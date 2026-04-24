use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use futures::stream::{self};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::sleep;

#[derive(Parser, Debug, Clone)]
#[command(ignore_errors = true)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    #[arg(short, long, default_value = "mock-model")]
    model: String,
    #[arg(long)]
    alias: Option<String>,
    #[arg(short = 'c', long, default_value_t = 2048)]
    ctx_size: usize,
    #[arg(short = 'b', long, default_value_t = 512)]
    batch_size: usize,
    #[arg(long)]
    slots: Option<usize>,
    #[arg(long, short = 'n', alias = "parallel")]
    np: Option<usize>,

    #[arg(long)]
    embedding: bool,
    #[arg(long)]
    rerank: bool,
    #[arg(long, short = 'f', alias = "flash-attn")]
    fa: bool,
    #[arg(long)]
    kv_unified: bool,
    #[arg(long)]
    api_key: Option<String>,
    // llama-server args we ignore but need to accept
    #[arg(long)]
    fit: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Slot {
    id: usize,
    state: i32, // 0 = idle, 1 = busy
    prompt: String,
}

struct AppState {
    slots: Mutex<Vec<Slot>>,
    args: Args,
    start_time: SystemTime,
    total_requests: Mutex<usize>,
}

async fn auth_middleware(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(expected_key) = &state.args.api_key {
        let auth_header = req
            .headers()
            .get("Authorization")
            .and_then(|h| h.to_str().ok());

        let expected_header = format!("Bearer {expected_key}");

        if auth_header != Some(&expected_header) {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Unauthorized"})),
            )
                .into_response();
        }
    }
    next.run(req).await
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let num_slots = args.slots.or(args.np).unwrap_or(1);

    let mut slots = Vec::new();
    for i in 0..num_slots {
        slots.push(Slot {
            id: i,
            state: 0,
            prompt: String::new(),
        });
    }

    let state = Arc::new(AppState {
        slots: Mutex::new(slots),
        args: args.clone(),
        start_time: SystemTime::now(),
        total_requests: Mutex::new(0),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/props", get(props))
        .route("/slots", get(get_slots))
        .route("/metrics", get(metrics))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/rerank", post(rerank))
        .route("/tokenize", post(tokenize))
        .route("/detokenize", post(detokenize))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse().unwrap();
    println!("Starting mock llama-server on {addr} with {num_slots} slots");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind to {addr}: {e}");
            std::process::exit(1);
        }
    };
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

async fn models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let created = state
        .start_time
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    Json(serde_json::json!({
        "object": "list",
        "data": [
            {
                "id": "mock-model",
                "object": "model",
                "created": created,
                "owned_by": "mock-server"
            }
        ]
    }))
}

async fn props(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let slots = state.slots.lock().unwrap();
    Json(serde_json::json!({
        "default_generation_settings": {
            "n_ctx": state.args.ctx_size,
            "seed": -1,
            "temp": 0.8,
            "top_k": 40,
            "top_p": 0.95
        },
        "total_slots": slots.len()
    }))
}

async fn get_slots(State(state): State<Arc<AppState>>) -> Json<Vec<Slot>> {
    let slots = state.slots.lock().unwrap();
    Json(slots.clone())
}

async fn metrics(State(state): State<Arc<AppState>>) -> String {
    let uptime = state.start_time.elapsed().unwrap().as_secs_f64();
    let requests = *state.total_requests.lock().unwrap();
    let slots = state.slots.lock().unwrap();
    let idle = slots.iter().filter(|s| s.state == 0).count();
    let busy = slots.iter().filter(|s| s.state == 1).count();

    format!(
        "# HELP llama_server_uptime_seconds Uptime of the server\n\
         # TYPE llama_server_uptime_seconds gauge\n\
         llama_server_uptime_seconds {uptime}\n\
         # HELP llama_server_requests_total Total number of requests\n\
         # TYPE llama_server_requests_total counter\n\
         llama_server_requests_total {requests}\n\
         # HELP llama_server_slots_idle Number of idle slots\n\
         # TYPE llama_server_slots_idle gauge\n\
         llama_server_slots_idle {idle}\n\
         # HELP llama_server_slots_processing Number of processing slots\n\
         # TYPE llama_server_slots_processing gauge\n\
         llama_server_slots_processing {busy}\n"
    )
}

#[derive(Deserialize)]
struct ChatRequest {
    model: Option<String>,
    stream: Option<bool>,
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    {
        let mut n = state.total_requests.lock().unwrap();
        *n += 1;
    }

    let slot_id = {
        let mut slots = state.slots.lock().unwrap();
        if let Some(slot) = slots.iter_mut().find(|s| s.state == 0) {
            slot.state = 1;
            Some(slot.id)
        } else {
            None
        }
    };

    if slot_id.is_none() {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": {
                    "message": "Server is busy, no slots available",
                    "type": "server_error",
                    "code": 503
                }
            })),
        )
            .into_response();
    }
    let slot_id = slot_id.unwrap();

    let model = req
        .model
        .clone()
        .unwrap_or_else(|| "mock-model".to_string());
    let stream_mode = req.stream.unwrap_or(false);

    let state_clone = state.clone();

    // Simulate processing delay
    let delay_ms = rand::rng().random_range(100..500);
    sleep(Duration::from_millis(delay_ms)).await;

    if stream_mode {
        let stream = stream::unfold(
            (0, slot_id, state_clone, model),
            |(step, s_id, st, m)| async move {
                if step == 0 {
                    // First delay already happened
                } else {
                    let ms = rand::rng().random_range(50..200);
                    sleep(Duration::from_millis(ms)).await;
                }

                let content = "This is a mock response from the dummy server.";
                let words: Vec<&str> = content.split_whitespace().collect();

                if step < words.len() {
                    let word = words[step];
                    let chunk = serde_json::json!({
                        "id": "chatcmpl-mock",
                        "object": "chat.completion.chunk",
                        "created": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        "model": m,
                        "choices": [{
                            "delta": {"content": format!("{} ", word)},
                            "index": 0,
                            "finish_reason": null
                        }]
                    });

                    Some((
                        Ok::<_, Infallible>(Event::default().data(chunk.to_string())),
                        (step + 1, s_id, st, m),
                    ))
                } else if step == words.len() {
                    let chunk = serde_json::json!({
                        "id": "chatcmpl-mock",
                        "object": "chat.completion.chunk",
                        "created": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        "model": m,
                        "choices": [{
                            "delta": {},
                            "index": 0,
                            "finish_reason": "stop"
                        }]
                    });
                    Some((
                        Ok::<_, Infallible>(Event::default().data(chunk.to_string())),
                        (step + 1, s_id, st, m),
                    ))
                } else if step == words.len() + 1 {
                    let usage_chunk = serde_json::json!({
                        "id": "chatcmpl-mock",
                        "object": "chat.completion.chunk",
                        "created": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        "model": m,
                        "choices": [],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
                    });
                    Some((
                        Ok::<_, Infallible>(Event::default().data(usage_chunk.to_string())),
                        (step + 1, s_id, st, m),
                    ))
                } else if step == words.len() + 2 {
                    Some((
                        Ok::<_, Infallible>(Event::default().data("[DONE]")),
                        (step + 1, s_id, st, m),
                    ))
                } else {
                    // Cleanup slot
                    let mut slots = st.slots.lock().unwrap();
                    if let Some(slot) = slots.iter_mut().find(|s| s.id == s_id) {
                        slot.state = 0;
                    }
                    None
                }
            },
        );

        Sse::new(stream).into_response()
    } else {
        // Deterministic short path when slow-body is engaged, so tests can
        // predict when headers reach the client and time their cancellation.
        let slow_body_ms = std::env::var("MOCK_SLOW_BODY_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        if slow_body_ms.is_none() {
            let ms = rand::rng().random_range(500..2000);
            sleep(Duration::from_millis(ms)).await;
        }

        let response = serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            "model": model,
            "choices": [{
                "message": {"role": "assistant", "content": "This is a mock response from the dummy server."},
                "index": 0,
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
        });

        // Cleanup slot
        {
            let mut slots = state.slots.lock().unwrap();
            if let Some(slot) = slots.iter_mut().find(|s| s.id == slot_id) {
                slot.state = 0;
            }
        }

        // Test hook: `MOCK_SLOW_BODY_MS=<n>` streams the non-streaming JSON body
        // as two halves with an `n`-millisecond pause between them. Lets tests
        // trigger a client cancel landing inside `resp.bytes().await` on the
        // proxy, rather than during `send().await`.
        if let Some(delay_ms) = slow_body_ms {
            let body_bytes = serde_json::to_vec(&response).expect("serialize mock response");
            let total_len = body_bytes.len();
            let mid = total_len / 2;
            let (head, tail) = body_bytes.split_at(mid);
            let first = Bytes::copy_from_slice(head);
            let second = Bytes::copy_from_slice(tail);
            let stream = stream::unfold(
                (0u8, first, second, delay_ms),
                |(step, first, second, delay_ms)| async move {
                    match step {
                        0 => Some((
                            Ok::<Bytes, Infallible>(first.clone()),
                            (1, first, second, delay_ms),
                        )),
                        1 => {
                            sleep(Duration::from_millis(delay_ms)).await;
                            Some((
                                Ok::<Bytes, Infallible>(second.clone()),
                                (2, first, second, delay_ms),
                            ))
                        }
                        _ => None,
                    }
                },
            );
            // Set Content-Length explicitly so downstream consumers (the proxy
            // or the test client) know the body ends at a specific byte count.
            // Otherwise hyper uses chunked transfer for Body::from_stream, and
            // the proxy's subsequent re-wrap into `Body::from(bytes)` produces
            // a header/body mismatch.
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("content-length", total_len.to_string())
                .body(Body::from_stream(stream))
                .expect("build slow-body response");
        }

        Json(response).into_response()
    }
}

async fn embeddings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let slot_id = {
        let mut slots = state.slots.lock().unwrap();
        if let Some(slot) = slots.iter_mut().find(|s| s.state == 0) {
            slot.state = 1;
            Some(slot.id)
        } else {
            None
        }
    };

    if slot_id.is_none() {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "busy"})),
        )
            .into_response();
    }

    sleep(Duration::from_millis(100)).await;

    let response = serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "embedding": vec![0.1; 10], "index": 0}],
        "model": "mock-embedding",
        "usage": {"prompt_tokens": 5, "total_tokens": 5}
    });

    {
        let mut slots = state.slots.lock().unwrap();
        if let Some(slot) = slots.iter_mut().find(|s| s.id == slot_id.unwrap()) {
            slot.state = 0;
        }
    }

    Json(response).into_response()
}

async fn rerank() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "id": "rerank-mock",
        "model": "mock-rerank-model",
        "results": [
            {"index": 0, "relevance_score": 0.9, "document": {"text": "Doc 1"}},
            {"index": 1, "relevance_score": 0.1, "document": {"text": "Doc 2"}}
        ],
        "usage": {"total_tokens": 10}
    }))
}

async fn tokenize() -> Json<serde_json::Value> {
    Json(serde_json::json!({"tokens": [1, 2, 3]}))
}

async fn detokenize() -> Json<serde_json::Value> {
    Json(serde_json::json!({"content": "mock text"}))
}
