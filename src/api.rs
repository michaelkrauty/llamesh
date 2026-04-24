use crate::cluster;
use crate::config::NodeConfig;
use crate::health;
use crate::metrics;
use crate::node_state::{NodeState, PeerState};
use crate::protocol_detect::{self, DetectedProtocol};
use crate::router;
use crate::security::{self, PeerIdentity};
use axum::body::Body;
use axum::http::HeaderName;
use axum::http::Request;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use http_body_util::BodyExt;
use hyper_util::rt::TokioTimer;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::signal;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{Instrument, Span};

// ... [Handlers remain unchanged, skipped for brevity] ...
async fn metrics_handler(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    check_auth(&state, &headers)?;
    let _ = state.calculate_resource_snapshot().await;
    Ok(metrics::render_prometheus_with_circuit_breaker(
        &state.metrics,
        Some(&state.circuit_breaker),
    )
    .await)
}

async fn version_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION")
    }))
}

fn check_auth(
    state: &NodeState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    security::check_api_key_auth(state.config.auth.as_ref(), headers).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": {
                    "message": "Unauthorized",
                    "type": "authentication_error",
                    "param": null,
                    "code": null
                }
            })),
        )
    })
}

/// Waits for SIGINT/SIGTERM, sets draining=true, notifies waiters, then returns.
async fn wait_for_shutdown_signal(state: Arc<NodeState>) {
    #[cfg(unix)]
    {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut terminate_stream) => {
                tokio::select! {
                    _ = signal::ctrl_c() => {},
                    _ = terminate_stream.recv() => {},
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to install SIGTERM handler: {}, using ctrl_c only",
                    e
                );
                let _ = signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!("Failed to install Ctrl+C handler: {}", e);
            // Fall through - shutdown will happen via other means
        }
    }

    tracing::info!("Signal received, starting graceful shutdown");
    state
        .draining
        .store(true, std::sync::atomic::Ordering::Relaxed);
    state.shutdown_notify.notify_waiters();
    // Wake any requests blocked on capacity_notify so they can check
    // the draining flag and exit instead of blocking shutdown.
    state.capacity_notify.notify_waiters();
}

/// Spawns a background task that force-exits on second signal.
fn spawn_force_exit_listener() {
    tokio::spawn(async {
        #[cfg(unix)]
        {
            match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                Ok(mut terminate_stream) => {
                    tokio::select! {
                        _ = signal::ctrl_c() => {},
                        _ = terminate_stream.recv() => {},
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to install SIGTERM handler for force-exit: {}", e);
                    let _ = signal::ctrl_c().await;
                }
            }
        }

        #[cfg(not(unix))]
        {
            if let Err(e) = signal::ctrl_c().await {
                tracing::error!("Failed to install Ctrl+C handler for force-exit: {}", e);
                return;
            }
        }

        tracing::info!("Second signal received, forcing immediate shutdown");
        std::process::exit(0);
    });
}

async fn metrics_json_handler(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> Result<Json<metrics::MetricsSnapshot>, (StatusCode, Json<serde_json::Value>)> {
    check_auth(&state, &headers)?;
    let _ = state.calculate_resource_snapshot().await;
    let version = state.build_manager.get_version().await;
    let build_status = state.build_manager.build_status();
    Ok(Json(
        state
            .metrics
            .snapshot(state.config.node_id.clone(), version, Some(build_status))
            .await,
    ))
}

async fn cluster_nodes_handler(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    check_auth(&state, &headers)?;

    let peers = state.peers.read().await;
    let mut nodes: HashMap<String, PeerState> = peers
        .iter()
        .map(|(node_id, peer)| (node_id.clone(), peer.clone()))
        .collect();

    // Add self
    let self_node = state.get_self_peer_state().await;
    nodes.insert(self_node.node_id.clone(), self_node);

    Ok(Json(serde_json::json!({
        "nodes": nodes
    })))
}

async fn rebuild_llama_handler(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> impl axum::response::IntoResponse {
    if let Err(e) = check_auth(&state, &headers) {
        return e.into_response();
    }

    let build_status = state.build_manager.build_status();

    // Trigger rebuild in background if lock available
    if let Ok(permit) = state.rebuild_lock.clone().try_lock_owned() {
        tokio::spawn(async move {
            let _permit = permit;
            match state.build_manager.update_and_build().await {
                Ok(_) => tracing::info!("Manual rebuild completed successfully"),
                Err(e) => {
                    tracing::error!(event = "llama_build_failure", error = %e, "Manual rebuild failed")
                }
            }
        });

        (
            axum::http::StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "accepted",
                "message": "Rebuild triggered in background",
                "build_status": build_status
            })),
        )
            .into_response()
    } else {
        (
            axum::http::StatusCode::CONFLICT,
            Json(serde_json::json!({
                "status": "error",
                "message": "Rebuild already in progress",
                "build_status": build_status
            })),
        )
            .into_response()
    }
}

async fn prewarm_handler(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl axum::response::IntoResponse {
    if let Err(e) = check_auth(&state, &headers) {
        return e.into_response();
    }

    let model_req = body.get("model").and_then(|v| v.as_str());

    if let Some(model_str) = model_req {
        if let Some((model_name, profile)) = state.resolve_model(model_str).await {
            match state
                .get_instance_for_model(&model_name, &profile, false)
                .await
            {
                Ok(_) => {
                    return (
                        axum::http::StatusCode::OK,
                        Json(serde_json::json!({"status": "ok", "message": "Model prewarmed"})),
                    )
                        .into_response()
                }
                Err(e) => {
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({"status": "error", "message": e.to_string()})),
                    )
                        .into_response()
                }
            }
        }
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"status": "error", "message": "Model not found"})),
        )
            .into_response();
    }
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"status": "error", "message": "Missing model field"})),
    )
        .into_response()
}

struct TlsStreamWrapper {
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    identity: PeerIdentity,
    remote_addr: std::net::SocketAddr,
}

impl axum::extract::connect_info::Connected<&TlsStreamWrapper> for PeerIdentity {
    fn connect_info(target: &TlsStreamWrapper) -> PeerIdentity {
        target.identity.clone()
    }
}

impl axum::extract::connect_info::Connected<&TlsStreamWrapper> for std::net::SocketAddr {
    fn connect_info(target: &TlsStreamWrapper) -> std::net::SocketAddr {
        target.remote_addr
    }
}

impl AsyncRead for TlsStreamWrapper {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsStreamWrapper {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub async fn start_server(config: NodeConfig, node_state: NodeState) -> anyhow::Result<()> {
    let state = Arc::new(node_state);
    let addr_str = config.listen_addr.clone();
    let parent_span = Span::current();

    let app = Router::new()
        .route("/healthz", get(health::healthz))
        .route("/health", get(health::healthz)) // Alias for llama-server compatibility
        .route("/readyz", get(health::readyz))
        .route("/version", get(version_handler))
        .route("/metrics", get(metrics_handler))
        .route("/metrics/json", get(metrics_json_handler))
        .route("/cluster/nodes", get(cluster_nodes_handler))
        .route("/admin/rebuild-llama", post(rebuild_llama_handler))
        .route("/admin/prewarm", post(prewarm_handler))
        .route("/v1/chat/completions", post(router::route_request))
        .route("/v1/completions", post(router::route_request))
        .route("/v1/embeddings", post(router::route_request))
        .route("/v1/rerank", post(router::route_request))
        .route("/v1/reranking", post(router::route_request)) // Alias
        .route("/rerank", post(router::route_request)) // Alias
        .route("/v1/models", get(router::list_models))
        .route("/cluster/gossip", post(cluster::handle_gossip))
        .with_state(state.clone());

    let addr: SocketAddr = addr_str.parse()?;
    tracing::info!("Listening on {}", addr);

    // Prepare optional TLS acceptor
    let tls_acceptor =
        if let Some(tls_config) = config.server_tls.as_ref().filter(|tls| tls.enabled) {
            tracing::info!("TLS enabled for API connections");
            let rustls_config =
                security::load_server_config(tls_config, config.cluster_tls.as_ref()).await?;
            Some(TlsAcceptor::from(Arc::new(rustls_config)))
        } else {
            None
        };

    // Check if noise is enabled for cluster traffic
    let noise_enabled = config.cluster.enabled && config.cluster.noise.enabled;
    if noise_enabled {
        tracing::info!("Noise Protocol enabled for cluster connections (single-port multiplexing)");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    spawn_signal_listener(state.clone());

    // Unified accept loop with protocol detection
    loop {
        let accept_fut = listener.accept();
        let shutdown_fut = state.shutdown_notify.notified();

        let (tcp_stream, remote_addr) = tokio::select! {
            res = accept_fut => {
                match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("Accept failed: {}", e);
                        continue;
                    }
                }
            }
            _ = shutdown_fut => {
                tracing::info!("Shutdown signal received in accept loop");
                break;
            }
        };

        if state.draining.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        // Detect protocol from first bytes (with timeout to prevent connection exhaustion)
        let detect_timeout = Duration::from_millis(config.http.protocol_detect_timeout_ms);
        let protocol = match timeout(detect_timeout, protocol_detect::detect(&tcp_stream)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::debug!("Protocol detection failed: {}", e);
                continue;
            }
            Err(_) => {
                tracing::debug!(
                    remote_addr = %remote_addr,
                    "Protocol detection timed out, closing connection"
                );
                continue;
            }
        };

        let state = state.clone();
        let app = app.clone();
        let tls_acceptor = tls_acceptor.clone();
        let http_config = config.http.clone();
        let span = parent_span.clone();

        tokio::spawn(
            async move {
                match protocol {
                    DetectedProtocol::Tls => {
                        // TLS connection - do TLS handshake then HTTP
                        if let Some(acceptor) = tls_acceptor {
                            handle_tls_connection(
                                tcp_stream,
                                remote_addr,
                                acceptor,
                                app,
                                http_config,
                            )
                            .await;
                        } else {
                            tracing::warn!("TLS connection received but TLS not configured");
                        }
                    }
                    DetectedProtocol::Http | DetectedProtocol::Http2 => {
                        // Plain HTTP connection
                        handle_http_connection(tcp_stream, remote_addr, app, http_config).await;
                    }
                    DetectedProtocol::Noise => {
                        // Noise Protocol - cluster traffic
                        handle_noise_connection(tcp_stream, remote_addr, state).await;
                    }
                }
            }
            .instrument(span),
        );
    }

    tracing::info!("Shutdown signal received, stopping accept loop");
    wait_for_drain(&state).await;
    Ok(())
}

/// Handle a TLS connection (API traffic)
async fn handle_tls_connection(
    tcp_stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    acceptor: TlsAcceptor,
    app: Router<()>,
    http_config: crate::config::HttpConfig,
) {
    let tls_stream = match acceptor.accept(tcp_stream).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("TLS handshake failed: {}", e);
            return;
        }
    };

    let certs = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .map(|c| c.to_vec())
        .unwrap_or_default();
    let identity = security::extract_peer_identity(&certs);

    let wrapper = TlsStreamWrapper {
        stream: tls_stream,
        identity,
        remote_addr,
    };

    let mut make_service = app.into_make_service_with_connect_info::<PeerIdentity>();
    let service = match make_service.call(&wrapper).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to create service: {}", e);
            return;
        }
    };

    serve_http_connection(hyper_util::rt::TokioIo::new(wrapper), service, http_config).await;
}

/// Handle a plain HTTP connection (API traffic)
async fn handle_http_connection(
    tcp_stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    app: Router<()>,
    http_config: crate::config::HttpConfig,
) {
    let mut make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    let service = match make_service.call(remote_addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to create service: {}", e);
            return;
        }
    };

    serve_http_connection(
        hyper_util::rt::TokioIo::new(tcp_stream),
        service,
        http_config,
    )
    .await;
}

/// Serve HTTP over any IO type
async fn serve_http_connection<I, S>(io: I, service: S, http_config: crate::config::HttpConfig)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    S: tower::Service<
            axum::http::Request<axum::body::Body>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    let hyper_service =
        hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let mut service = service.clone();
            async move {
                let (parts, body) = req.into_parts();
                let req = Request::from_parts(parts, axum::body::Body::new(body));
                service.call(req).await
            }
        });

    let mut builder = hyper::server::conn::http1::Builder::new();
    if http_config.idle_timeout_seconds > 0 {
        builder.timer(TokioTimer::new());
        builder.header_read_timeout(Duration::from_secs(http_config.idle_timeout_seconds));
    }

    if let Err(err) = builder.serve_connection(io, hyper_service).await {
        tracing::debug!("Connection error: {}", err);
    }
}

/// Handle a Noise Protocol connection (cluster traffic)
async fn handle_noise_connection(
    tcp_stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    state: Arc<NodeState>,
) {
    // Check if noise is configured
    let noise_context = match &state.noise_context {
        Some(ctx) => ctx.clone(),
        None => {
            tracing::warn!(
                remote_addr = %remote_addr,
                "Noise connection received but noise not configured"
            );
            return;
        }
    };

    // Perform noise handshake with timeout to prevent stalled connections
    // from holding a Tokio task indefinitely
    let mut tcp_stream = tcp_stream;
    let session = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::noise::handshake::respond(
            &mut tcp_stream,
            &noise_context.keypair,
            &noise_context.cluster_token,
            &state.config.node_id,
        ),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!(
                remote_addr = %remote_addr,
                error = %e,
                "Noise handshake failed"
            );
            return;
        }
        Err(_) => {
            tracing::warn!(
                remote_addr = %remote_addr,
                "Noise handshake timed out after 30s"
            );
            return;
        }
    };

    // Verify peer identity via TOFU/allowed_keys
    let peer_pubkey_display = format!(
        "noise25519:{}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            session.peer_public_key()
        )
    );

    let peer_node_id = session.peer_node_id().unwrap_or("unknown").to_string();

    if let Err(e) = crate::noise::known_peers::verify_peer(
        &noise_context.known_peers,
        &peer_node_id,
        &peer_pubkey_display,
        &noise_context.config,
    ) {
        tracing::warn!(
            remote_addr = %remote_addr,
            peer_node_id = %peer_node_id,
            error = %e,
            "Peer verification failed"
        );
        return;
    }

    tracing::debug!(
        remote_addr = %remote_addr,
        peer_node_id = %peer_node_id,
        "Noise connection established"
    );

    // Handle requests over the encrypted channel
    let mut session = session;
    let mut tcp_stream = tcp_stream;

    loop {
        // Read encrypted HTTP request
        let request =
            match crate::noise::transport::recv_request(&mut session, &mut tcp_stream).await {
                Ok(req) => req,
                Err(e) => {
                    // Connection closed or error
                    tracing::debug!(
                        remote_addr = %remote_addr,
                        peer_node_id = %peer_node_id,
                        error = %e,
                        "Noise connection ended"
                    );
                    break;
                }
            };

        tracing::debug!(
            remote_addr = %remote_addr,
            peer_node_id = %peer_node_id,
            method = %request.method,
            path = %request.path,
            "Received request over Noise"
        );

        let response = match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/cluster/gossip") => {
                handle_noise_gossip(&state, request.body.as_ref(), &peer_node_id, remote_addr).await
            }
            _ => handle_noise_http_request(state.clone(), request).await,
        };

        if let Err(e) = send_noise_response(&mut session, &mut tcp_stream, response).await {
            tracing::debug!(
                remote_addr = %remote_addr,
                error = %e,
                "Failed to send Noise response"
            );
            break;
        }
    }
}

async fn handle_noise_gossip(
    state: &Arc<NodeState>,
    body: &[u8],
    peer_node_id: &str,
    remote_addr: SocketAddr,
) -> axum::response::Response {
    match serde_json::from_slice::<cluster::GossipMessage>(body) {
        Ok(msg) => {
            if msg.origin.node_id != peer_node_id {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "Peer node_id mismatch"})),
                )
                    .into_response();
            }

            cluster::process_gossip_message(state, msg, Some(remote_addr)).await;
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse gossip message over Noise");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid JSON: {}", e)})),
            )
                .into_response()
        }
    }
}

async fn handle_noise_http_request(
    state: Arc<NodeState>,
    request: crate::noise::transport::NoiseRequest,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(request.method.as_str())
        .uri(request.path.as_str());

    if let Some(headers) = builder.headers_mut() {
        for (name, value) in request.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(&value),
            ) {
                headers.insert(name, value);
            }
        }
    }

    let req = match builder.body(Body::from(request.body)) {
        Ok(req) => req,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid request: {}", e)})),
            )
                .into_response()
        }
    };

    match router::route_request(State(state), req).await {
        Ok(resp) => resp.into_response(),
        Err(err) => err.into_response(),
    }
}

async fn send_noise_response(
    session: &mut crate::noise::handshake::NoiseSession,
    stream: &mut tokio::net::TcpStream,
    response: axum::response::Response,
) -> crate::noise::Result<()> {
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("");
    let (parts, mut body) = response.into_parts();

    crate::noise::transport::send_response_head(
        session,
        stream,
        status.as_u16(),
        status_text,
        &parts.headers,
    )
    .await?;

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| crate::noise::NoiseError::Transport(e.to_string()))?;
        if let Some(data) = frame.data_ref() {
            if !data.is_empty() {
                crate::noise::transport::send_body_chunk(session, stream, data).await?;
            }
        }
    }

    crate::noise::transport::send_body_end(session, stream).await
}

async fn wait_for_drain(state: &NodeState) {
    let grace = state.config.shutdown_grace_period_seconds;
    let start = std::time::Instant::now();
    let grace_duration = Duration::from_secs(grace);

    loop {
        if state.is_idle().await {
            tracing::info!("Node is idle (no requests/queues), finish shutdown");
            break;
        }
        if start.elapsed() >= grace_duration {
            tracing::warn!("Shutdown grace period exceeded, forcing shutdown");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn spawn_signal_listener(state: Arc<NodeState>) {
    let span = Span::current();
    tokio::spawn(
        async move {
            wait_for_shutdown_signal(state).await;
            spawn_force_exit_listener();
        }
        .instrument(span),
    );
}
