use crate::config::Profile;
use crate::connection::ConnectionHandle;
use crate::errors::AppError;
use crate::instance::Instance;
use crate::metrics::{HashMetrics, Metrics};
use crate::node_state::{NodeError, NodeState};
use crate::util::error_chain;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{
        header::{CONNECTION, CONTENT_LENGTH, HOST, RETRY_AFTER},
        HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode,
    },
    response::IntoResponse,
    response::Json,
    Extension,
};
use bytes::Bytes;
use futures::{future::BoxFuture, Stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{error, info, warn, Instrument};

const DISCONNECT_POLL_INTERVAL: Duration = Duration::from_secs(5);

fn node_error_to_app_error(error: NodeError) -> AppError {
    match error {
        NodeError::Draining => AppError::node_draining(),
        NodeError::SpawnFailuresExhausted => AppError::service_unavailable(
            "All nodes failed to spawn instance for this model",
            "spawn_failures_exhausted",
        )
        .with_header(RETRY_AFTER, HeaderValue::from_static("30")),
        NodeError::QueueFull => {
            AppError::new(StatusCode::SERVICE_UNAVAILABLE, "Queue full", "queue_full")
                .with_header(RETRY_AFTER, HeaderValue::from_static("5"))
        }
        NodeError::QueueTimeout => AppError::service_unavailable("Queue timeout", "queue_timeout")
            .with_header(RETRY_AFTER, HeaderValue::from_static("5")),
        NodeError::InsufficientResources => {
            AppError::service_unavailable("Insufficient resources", "no_capacity")
                .with_header(RETRY_AFTER, HeaderValue::from_static("30"))
        }
        NodeError::ModelNotFound(model) => {
            AppError::model_not_found(format!("Model not found: {model}"))
        }
        NodeError::Other(inner) => {
            let msg = inner.to_string();
            if msg.contains("timed out") {
                AppError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "Request timed out",
                    "request_timeout",
                )
            } else {
                AppError::internal_server_error(msg)
            }
        }
        _ => AppError::internal_server_error(error.to_string()),
    }
}

/// Wait for a capacity notification, but bail early if the client has
/// disconnected. Returns `Err` if the client is gone.
///
/// The caller creates `capacity` from `capacity_notify.notified()` before it
/// checks the state the wait depends on, such as the draining flag:
/// `notify_waiters` wakes only the waiters that already exist.
async fn wait_for_capacity_or_disconnect(
    capacity: tokio::sync::futures::Notified<'_>,
    conn: &Option<ConnectionHandle>,
) -> Result<(), AppError> {
    match conn {
        Some(handle) => {
            tokio::select! {
                _ = capacity => Ok(()),
                _ = crate::connection::poll_client_disconnect(handle, DISCONNECT_POLL_INTERVAL) => {
                    info!(event = "client_disconnected", "Client disconnected during capacity wait");
                    Err(AppError::client_disconnected())
                }
            }
        }
        None => {
            capacity.await;
            Ok(())
        }
    }
}

/// Await an upstream network operation, aborting early if the client's
/// connection closes first.
///
/// Wraps a peer forward or a local generation so an abandoned request stops
/// consuming cluster resources — a queued instance slot, a peer connection, an
/// in-flight generation — instead of running to completion for a client that
/// has already gone away. Returns the operation's output, or
/// `AppError::client_disconnected()` if the client disconnects first. With no
/// connection handle (`None`) the future is awaited without disconnect
/// detection (e.g. an internally-originated request that has no client socket).
///
/// Dropping `fut` on disconnect is intentional: it cancels the wrapped
/// operation, closing the per-request peer/backend connection so the abort
/// propagates one hop downstream (a forwarding node closing its connection lets
/// the serving node observe the disconnect in turn). The caller's
/// `RequestGuard`/`PeerForwardGuard` release the request's in-flight and metric
/// accounting on the early return exactly as they do on any other error path,
/// and a circuit-breaker probe left unrecorded is benign (at worst the peer's
/// next recovery probe waits one interval).
async fn await_unless_client_gone<F, T>(
    conn: &Option<ConnectionHandle>,
    poll_interval: Duration,
    stage: &'static str,
    fut: F,
) -> Result<T, AppError>
where
    F: std::future::Future<Output = T>,
{
    match conn {
        Some(handle) => {
            tokio::select! {
                biased;
                res = fut => Ok(res),
                _ = crate::connection::poll_client_disconnect(handle, poll_interval) => {
                    info!(event = "client_disconnected", stage = stage, "Client disconnected during upstream wait");
                    Err(AppError::client_disconnected())
                }
            }
        }
        None => Ok(fut.await),
    }
}

struct RequestGuard {
    state: Arc<NodeState>,
    // For local instances
    instance_lock: Option<Arc<RwLock<Instance>>>,
    completed: bool,
}

impl RequestGuard {
    fn new(state: Arc<NodeState>) -> Self {
        Self {
            state,
            instance_lock: None,
            completed: false,
        }
    }

    fn set_instance(&mut self, inst: Arc<RwLock<Instance>>) {
        self.instance_lock = Some(inst);
    }

    fn complete(mut self) {
        self.completed = true;
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.state.metrics.dec_current_requests();
            if let Some(lock) = &self.instance_lock {
                let lock = lock.clone();
                let state = self.state.clone();
                tokio::spawn(async move {
                    let (model, profile, became_idle) = {
                        let mut inst = lock.write().await;
                        inst.in_flight_requests = inst.in_flight_requests.saturating_sub(1);
                        (
                            inst.model_name.clone(),
                            inst.profile_id.clone(),
                            inst.in_flight_requests == 0,
                        )
                    };
                    if became_idle {
                        state.notify_all_queues().await;
                    } else {
                        state.notify_queue(&model, &profile).await;
                    }
                });
            }
        }
    }
}

enum PeerResponse {
    Http(reqwest::Response),
    Noise {
        status: StatusCode,
        headers: HeaderMap,
        body: Box<crate::noise::transport::NoiseBodyStream>,
    },
}

struct PeerRequest<'a> {
    peer_id: &'a str,
    peer_address: &'a str,
    method: Method,
    path_and_query: &'a str,
    headers: HeaderMap,
    body: Bytes,
    timeout_ms: u64,
}

fn peer_status_is_failure(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
}

async fn record_peer_status(
    state: &NodeState,
    peer_id: &str,
    status: StatusCode,
    probe_ticket: bool,
) {
    if peer_status_is_failure(status) {
        state
            .circuit_breaker
            .record_failure(peer_id, probe_ticket)
            .await;
    } else if state
        .circuit_breaker
        .record_success(peer_id, probe_ticket)
        .await
    {
        // The circuit just advanced toward recovery. Wake routing loops that
        // parked on capacity_notify after filtering this peer out, so they
        // retry (or send the next recovery probe) immediately instead of
        // waiting for an unrelated capacity event.
        state.capacity_notify.notify_waiters();
    }
}

/// Error `type` strings that signal a peer is patient-but-currently-saturated.
/// Receiving one of these means the peer would be able to serve the request
/// after some cluster state change (capacity freed, queue drained, gossip
/// update). The forwarder treats them as healable and waits for capacity
/// instead of surfacing the 503 to the client.
const HEALABLE_PEER_ERROR_TYPES: &[&str] = &[
    "queue_timeout",
    "queue_full",
    "no_capacity",
    "insufficient_resources",
    "peer_unavailable_retry",
    "spawn_failures_exhausted",
];

/// Inspect a buffered peer error body to decide whether the failure is
/// healable. A healable response is one where retrying after a cluster
/// capacity change is likely to succeed.
fn is_healable_error_body(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(error_type) = value
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|t| t.as_str())
    else {
        return false;
    };
    HEALABLE_PEER_ERROR_TYPES.contains(&error_type)
}

/// Outcome of attempting to forward a request to a peer.
enum ForwardOutcome {
    /// Peer responded with a status the client should see. Body bytes are
    /// already buffered so the caller can build a fresh response.
    Surface {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    },
    /// Peer streamed a successful response. The streaming response is
    /// constructed by the caller.
    StreamingSurface(PeerResponse),
    /// Peer reported a healable failure. Caller should wait for capacity and
    /// retry (possibly against a different peer).
    Heal { reason: String },
    /// Peer transport failed (network error, handshake, etc.). Caller should
    /// record a circuit-breaker failure and may retry.
    Transport(AppError),
    /// The circuit breaker declined to admit this request: the peer's
    /// circuit is recovering and the probe slot for this interval is taken.
    /// Not a peer failure — the caller should wait for capacity and retry.
    ProbeDenied,
}

/// Forward a request to a peer and classify the outcome. Buffers non-streaming
/// 5xx response bodies so the caller can decide whether to surface them or
/// retry. Streaming and 2xx/4xx responses are handed back unbuffered for
/// pass-through to the client.
async fn attempt_peer_forward(
    state: &NodeState,
    request: PeerRequest<'_>,
    response_streaming: bool,
) -> ForwardOutcome {
    let peer_id = request.peer_id;
    // Circuit-breaker admission happens here, at the commit point, rather
    // than during candidate filtering: filtering scans peers that are never
    // dispatched to, and must not burn a recovering peer's probe slot. The
    // returned probe ticket attributes THIS dispatch's result; every
    // success/failure recording below carries it, and the dispatch loops do
    // not record outcomes themselves.
    let probe_ticket = match state.circuit_breaker.try_claim_dispatch_sync(peer_id) {
        crate::circuit_breaker::DispatchDecision::Admit { probe_ticket } => probe_ticket,
        crate::circuit_breaker::DispatchDecision::Deny => return ForwardOutcome::ProbeDenied,
    };
    let resp = match send_peer_request(state, request, response_streaming).await {
        Ok(resp) => resp,
        Err(e) => {
            state
                .circuit_breaker
                .record_failure(peer_id, probe_ticket)
                .await;
            return ForwardOutcome::Transport(e);
        }
    };

    let status = match &resp {
        PeerResponse::Http(r) => r.status(),
        PeerResponse::Noise { status, .. } => *status,
    };

    // Pass through streaming successful responses without buffering. A 5xx
    // never opens a stream because the peer commits to its response shape
    // before opening one — error responses are always non-streaming JSON.
    // Streaming successes are recorded WITHOUT the probe ticket: only the
    // headers have arrived, the stream can still fail mid-body, and a stream
    // failure is never recorded — so headers alone must not drive a
    // recovering circuit toward closed. Recovery is carried by non-streaming
    // probes and the gossip prober instead.
    if response_streaming && status.is_success() {
        record_peer_status(state, peer_id, status, false).await;
        return ForwardOutcome::StreamingSurface(resp);
    }

    // For non-streaming or any error response, buffer the body so we can
    // either inspect it (for healing) or hand it to the client.
    let (status, response_headers, body_bytes) = match resp {
        PeerResponse::Http(r) => {
            let headers = r.headers().clone();
            let bytes = match r.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    state
                        .circuit_breaker
                        .record_failure(peer_id, probe_ticket)
                        .await;
                    return ForwardOutcome::Transport(AppError::new(
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "Failed to read peer response body from {peer_id}: {}",
                            error_chain(&e)
                        ),
                        "peer_body_read_failed",
                    ));
                }
            };
            (status, headers, bytes)
        }
        PeerResponse::Noise {
            status,
            headers,
            body,
        } => {
            let body = *body;
            let bytes = match body
                .map(|chunk| chunk.map_err(|e| std::io::Error::other(error_chain(&e))))
                .try_fold(Vec::new(), |mut acc, chunk| async move {
                    acc.extend_from_slice(&chunk);
                    Ok(acc)
                })
                .await
            {
                Ok(b) => Bytes::from(b),
                Err(e) => {
                    state
                        .circuit_breaker
                        .record_failure(peer_id, probe_ticket)
                        .await;
                    return ForwardOutcome::Transport(AppError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Failed to read Noise peer response body from {peer_id}: {e}"),
                        "peer_body_read_failed",
                    ));
                }
            };
            (status, headers, bytes)
        }
    };

    record_peer_status(state, peer_id, status, probe_ticket).await;

    if status.is_server_error() && is_healable_error_body(&body_bytes) {
        let reason = serde_json::from_slice::<Value>(&body_bytes)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| status.as_u16().to_string());
        return ForwardOutcome::Heal { reason };
    }

    ForwardOutcome::Surface {
        status,
        headers: response_headers,
        body: body_bytes,
    }
}

/// Header fields that describe a single connection rather than the message
/// (RFC 9110 §7.6.1), plus the obsolete `Proxy-Connection`. The proxy's client,
/// peer, and `llama-server` connections each negotiate their own persistence,
/// framing, and upgrades, so these fields are never relayed in either direction.
const HOP_BY_HOP_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Remove the hop-by-hop fields from a message about to be relayed: the fixed
/// set above and any field its sender listed in `Connection`. Relaying them
/// would apply one connection's state to another; for example, the
/// `Connection: close` that `llama-server` sends on the last response of a
/// pooled connection would also close the client's connection to the proxy.
fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
}

/// Build the headers of a request forwarded to a local `llama-server` instance.
///
/// `llama-server` closes its connection after every streamed response without
/// announcing it: its stream provider reports the finished stream as cancelled,
/// so cpp-httplib drops the connection right after the terminal chunk although
/// the response advertised keep-alive. A pooled connection reused at that moment
/// (as it is by a queued request, which is dispatched as soon as the stream
/// releases its slot) is already closing, and the request fails without a
/// response. A streamed request therefore asks for `Connection: close`, keeping
/// its connection out of the pool; the server would discard it anyway.
pub(crate) fn build_local_upstream_headers(
    source: &HeaderMap,
    response_streaming: bool,
) -> HeaderMap {
    let mut headers = source.clone();
    remove_hop_by_hop_headers(&mut headers);
    // Set by the client for the outbound request.
    headers.remove(HOST);
    headers.remove(CONTENT_LENGTH);
    if response_streaming {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    }
    headers
}

fn build_forward_headers(source: &HeaderMap, current_hops: usize, request_id: &str) -> HeaderMap {
    // A clone keeps every value of a repeated header (e.g. multiple Cookie
    // lines), which a per-pair `insert` would collapse to the last one.
    let mut forward_headers = source.clone();
    remove_hop_by_hop_headers(&mut forward_headers);
    // Set by the outbound transport for the forwarded request.
    forward_headers.remove(HOST);
    forward_headers.remove(CONTENT_LENGTH);
    forward_headers.insert(
        axum::http::header::HeaderName::from_static("x-llama-mesh-hops"),
        HeaderValue::from_str(&(current_hops + 1).to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("1")),
    );
    if let Ok(value) = HeaderValue::from_str(request_id) {
        forward_headers.insert(
            axum::http::header::HeaderName::from_static("x-request-id"),
            value,
        );
    }
    forward_headers
}

/// Build the client-facing response header map from a peer's parsed response
/// headers. The Noise transport delivers headers as `(name, value)` pairs (one
/// per header line), so this rebuilds a `HeaderMap` from them — using `append`
/// so a repeated response header (e.g. multiple `Set-Cookie` lines) keeps every
/// value. The reqwest forward path clones the upstream `HeaderMap` wholesale and
/// so already preserves repeated values; this keeps the Noise path consistent.
fn build_peer_response_headers(raw: &[(String, String)]) -> HeaderMap {
    let mut response_headers = HeaderMap::new();
    for (name, value) in raw {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            response_headers.append(name, value);
        }
    }
    response_headers
}

async fn send_peer_request(
    state: &NodeState,
    request: PeerRequest<'_>,
    streaming: bool,
) -> Result<PeerResponse, AppError> {
    if let Some(noise_context) = state.noise_context.as_ref() {
        let timeout = (request.timeout_ms > 0).then(|| Duration::from_millis(request.timeout_ms));
        // Bound silence between peer response body chunks (mirrors the local
        // upstream read timeout) so a stalled peer cannot hold this node's slot.
        // Only for streaming responses: a non-streaming forward buffers the peer
        // body inside `attempt_peer_forward`, where a read failure is retried
        // (re-forwarded), so an inactivity abort there would loop rather than
        // release the slot. A streaming response is surfaced to the client
        // before its body is read, so an aborted body simply ends the client
        // stream and releases the slot through the normal cleanup path.
        let body_inactivity = (streaming && state.config.upstream_read_timeout_ms > 0)
            .then(|| Duration::from_millis(state.config.upstream_read_timeout_ms));
        let response = crate::noise::transport::request(
            noise_context,
            crate::noise::transport::OutboundNoiseRequest {
                peer_base_url: request.peer_address,
                expected_peer_node_id: Some(request.peer_id),
                method: request.method.as_str(),
                path: request.path_and_query,
                headers: &request.headers,
                body: &request.body,
                timeout_duration: timeout,
            },
        )
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::BAD_GATEWAY,
                error_chain(&e),
                "peer_forward_failed",
            )
        })?;

        let status = StatusCode::from_u16(response.head.status).map_err(|e| {
            AppError::internal_server_error(format!("Invalid peer response status: {e}"))
        })?;
        let response_headers = build_peer_response_headers(&response.head.headers);

        Ok(PeerResponse::Noise {
            status,
            headers: response_headers,
            body: Box::new(response.into_body_stream(body_inactivity)),
        })
    } else {
        let base = request.peer_address.trim_end_matches('/');
        let url = format!("{base}{}", request.path_and_query);
        let mut client_req = state
            .cluster_client
            .request(request.method, &url)
            .headers(request.headers)
            .body(request.body);

        if request.timeout_ms > 0 {
            client_req = client_req.timeout(Duration::from_millis(request.timeout_ms));
        }

        client_req
            .send()
            .await
            .map(PeerResponse::Http)
            .map_err(|e| {
                AppError::new(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "Failed to forward to peer {}: {}",
                        request.peer_id,
                        error_chain(&e)
                    ),
                    "peer_forward_failed",
                )
            })
    }
}

/// Build the OpenAI-style model objects this node advertises: every enabled
/// local `model:profile`, plus any models advertised by cluster peers. Shared by
/// `list_models` (`GET /v1/models`) and `get_model` (`GET /v1/models/:model`) so
/// the two endpoints never disagree on the catalog.
async fn collect_models(state: &NodeState) -> Vec<Value> {
    let mut data = Vec::new();
    // Stable per-model `created`: OpenAI clients treat it as a fixed creation
    // timestamp, so report the node start time instead of the wall clock at
    // request time, which would otherwise change on every call.
    let created = state.started_at_unix;
    let mut seen_ids = HashSet::new();
    let owned_by = state.config.node_id.clone();

    // Used to withhold parsed params (persisted, or from a lingering old
    // instance) recorded under a different llama.cpp binary than the one new
    // instances would currently be spawned from.
    let llama_cpp_version = state.current_llama_cpp_version().await;

    // Phase 1: snapshot the local catalog under the cookbook read lock,
    // computing each profile's args_hash inline (we already hold the profile).
    // The per-profile metric and instance lookups in phase 2 must not run while
    // this guard is held — `get_args_hash_for_key` re-locks the cookbook, and
    // tokio's write-preferring `RwLock` can deadlock that nested read against a
    // queued hot-reload — so gather everything they need here and release first.
    struct LocalProfile {
        id: String,
        model_name: String,
        model_description: Option<String>,
        profile_id: String,
        profile_description: Option<String>,
        idle_timeout_seconds: u64,
        args_hash: String,
    }
    let local_profiles: Vec<LocalProfile> = {
        let cookbook = state.cookbook.read().await;
        let mut rows = Vec::new();
        for model in &cookbook.models {
            if !model.enabled {
                continue;
            }
            for profile in &model.profiles {
                if !profile.enabled {
                    continue;
                }
                let is_default = profile.id == "default";
                // Use bare model name for default profile, otherwise model:profile
                let id = if is_default {
                    model.name.clone()
                } else {
                    format!("{}:{}", model.name, profile.id)
                };
                rows.push(LocalProfile {
                    id,
                    model_name: model.name.clone(),
                    model_description: model.description.clone(),
                    profile_id: profile.id.clone(),
                    profile_description: profile.description.clone(),
                    idle_timeout_seconds: profile.idle_timeout_seconds,
                    args_hash: crate::node_state::args_hash_for_profile(profile),
                });
            }
        }
        rows
    };

    // Phase 2: the cookbook guard is released, so it is safe to take the metric
    // and instance locks without risking the nested-read deadlock above.
    for row in local_profiles {
        // Look up metrics by args_hash
        let hash_metrics = state.metrics.get_hash_metrics(&row.args_hash).await;
        let tps = hash_metrics.tokens_per_second();
        let persisted_params = hash_metrics.get_parsed_params(llama_cpp_version.as_deref());

        // Parsed model params: prefer a running, still-current instance
        // (freshest), fall back to the params persisted from the last
        // instance startup.
        let parsed_params = state
            .get_parsed_params_for_model(
                &row.model_name,
                &row.profile_id,
                llama_cpp_version.as_deref(),
            )
            .await
            .or(persisted_params);

        let metadata = json!({
            "model": row.model_name.clone(),
            "model_description": row.model_description,
            "profile_id": row.profile_id,
            "profile_description": row.profile_description,
            "idle_timeout_seconds": row.idle_timeout_seconds,
            "estimated_tokens_per_second": tps,
            "parsed_model_params": parsed_params,
        });

        data.push(json!({
            "id": row.id.clone(),
            "object": "model",
            "created": created,
            "owned_by": owned_by.as_str(),
            "permission": [],
            "root": row.model_name,
            "parent": null,
            "metadata": metadata,
        }));
        seen_ids.insert(row.id);
    }

    if state.config.cluster.enabled {
        let peers = state.peers.read().await;
        for peer in peers.values() {
            // Peer-owned models report the peer's own start time so the value is
            // consistent across every front-end node serving them; fall back to
            // this node's start time for older peers that don't advertise it.
            let peer_created = if peer.started_at_unix > 0 {
                peer.started_at_unix
            } else {
                created
            };
            for entry in &peer.supported_models {
                if seen_ids.insert(entry.clone()) {
                    data.push(json!({
                        "id": entry,
                        "object": "model",
                        "created": peer_created,
                        "owned_by": peer.node_id,
                        "permission": [],
                        "root": entry.split(':').next().unwrap_or(entry),
                        "parent": null,
                        "metadata": {
                            "source": "cluster",
                            "advertised_by": peer.node_id,
                            "cluster_url": peer.address
                        }
                    }));
                }
            }
        }
    }

    data
}

pub async fn list_models(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    crate::security::check_api_key_auth(state.config.auth.as_ref(), &headers)
        .map_err(AppError::authentication_error)?;

    let data = collect_models(&state).await;
    Ok(Json(json!({
        "object": "list",
        "data": data
    })))
}

/// `GET /v1/models/{model}` — the OpenAI "retrieve model" endpoint. Returns the
/// single model object whose `id` matches, or 404 if this node neither serves
/// the model nor sees a peer advertising it. The match is exact on the
/// advertised id, so a non-default profile must be requested as `model:profile`.
/// Registered as a catch-all so ids containing `/` resolve to the same object
/// `/v1/models` lists.
pub async fn get_model(
    State(state): State<Arc<NodeState>>,
    Path(model): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    crate::security::check_api_key_auth(state.config.auth.as_ref(), &headers)
        .map_err(AppError::authentication_error)?;

    // A catch-all capture can carry a leading slash; strip it so the match is
    // exact against the advertised id (which never starts with `/`).
    let model = model.trim_start_matches('/');
    let data = collect_models(&state).await;
    data.into_iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model))
        .map(Json)
        .ok_or_else(|| {
            AppError::model_not_found(format!(
                "Model '{model}' not found in local cookbook or any peer"
            ))
            .with_param("model")
        })
}

/// Resolve the request's `model` field to the identifier to route on.
///
/// Per the API spec, an *omitted* `model` substitutes the configured
/// `default_model`, so an absent field returns `Some(default_model)`. A `model`
/// that is present but is not a non-empty string — including JSON `null` —
/// returns `None`: silently falling back to the default would route to a
/// different model than the caller named, masking the mistake, so the caller
/// turns `None` into a 400 `invalid_request_error`. Only an absent field, not a
/// present `null`, takes the default path.
fn resolve_requested_model(json_body: &Value, default_model: &str) -> Option<String> {
    match json_body.get("model") {
        None => Some(default_model.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        // Present but not a usable string (null, number, bool, object, array,
        // or empty string): a client error, not an implicit default request.
        Some(_) => None,
    }
}

pub async fn route_request(
    State(state): State<Arc<NodeState>>,
    conn_handle: Option<Extension<ConnectionHandle>>,
    req: Request<Body>,
) -> Result<impl IntoResponse, AppError> {
    let conn_handle = conn_handle.map(|Extension(h)| h);
    let (parts, body) = req.into_parts();
    let path_only = parts.uri.path().to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let endpoint_kind = EndpointKind::from_path(&path_only);

    if state.draining.load(Ordering::Relaxed) {
        return Err(AppError::node_draining());
    }

    // Auth Check
    crate::security::check_api_key_auth(state.config.auth.as_ref(), &parts.headers)
        .map_err(AppError::authentication_error)?;

    let body_timeout = Duration::from_millis(state.config.http.body_read_timeout_ms);
    let bytes = timeout(
        body_timeout,
        axum::body::to_bytes(body, state.config.http.request_body_limit_bytes),
    )
    .await
    .map_err(|_| AppError::request_timeout("Request body read timed out"))?
    .map_err(|e| AppError::invalid_request(format!("Body too large or error: {e}")))?;

    let json_body: Value =
        serde_json::from_slice(&bytes).map_err(|_| AppError::invalid_request("Invalid JSON"))?;

    let model_to_use = resolve_requested_model(&json_body, &state.config.default_model)
        .ok_or_else(|| {
            AppError::invalid_request("'model' must be a non-empty string").with_param("model")
        })?;

    // Only text-generation endpoints have a streaming response shape. Some
    // OpenAI clients include `stream` on all requests; embeddings/rerank must
    // still use non-streaming cleanup semantics.
    let client_stream_requested = json_body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let response_streaming =
        response_streaming_for_endpoint(endpoint_kind, client_stream_requested);
    let forward_body =
        request_body_for_endpoint(&json_body, endpoint_kind, client_stream_requested, &bytes);

    let session_id = parts
        .headers
        .get("x-session-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ulid::Ulid::new().to_string());

    // Use incoming request ID if provided, otherwise generate new one
    let request_id = parts
        .headers
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ulid::Ulid::new().to_string());

    // Loop prevention check - reject malformed hop headers to prevent bypass
    let current_hops = match parts.headers.get("x-llama-mesh-hops") {
        Some(header) => {
            let header_str = header.to_str().map_err(|_| {
                AppError::invalid_request("Invalid x-llama-mesh-hops header encoding")
            })?;
            header_str
                .parse::<usize>()
                .map_err(|_| AppError::invalid_request("Invalid x-llama-mesh-hops header value"))?
        }
        None => 0,
    };

    if current_hops > state.config.max_hops {
        return Err(AppError::invalid_request("Too many hops (loop detected)"));
    }

    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        session_id = %session_id,
        model = %model_to_use,
        node_id = %state.config.node_id
    );

    async move {
        info!("Received request");

        // Try local model resolution first. The cookbook version is sampled
        // before resolving, so a reload after this point makes local queueing
        // re-resolve the profile (see `get_instance_for_model`).
        let resolved_cookbook_version = state.cookbook_version.load(Ordering::SeqCst);
        let local_resolution = state.resolve_model(&model_to_use).await;

        // If local resolution fails, check if any peer supports this model.
        // The forwarder is patient: a peer that responds with a healable 503
        // (queue timeout, queue full, no capacity, etc.) is treated as
        // "saturated for now" and the request waits for cluster capacity to
        // change before retrying instead of surfacing the failure to the
        // client.
        if local_resolution.is_none() {
            // Counted once for the whole forwarding effort; cleanup_fut /
            // guard Drop decrements exactly once when we either commit the
            // response stream or bail out.
            state.metrics.inc_requests();
            let guard = RequestGuard::new(state.clone());

            let forward_headers =
                build_forward_headers(&parts.headers, current_hops, &request_id);
            let timeout_ms = state.config.model_defaults.max_request_duration_ms;

            // Set when a dispatch was probe-denied: the immediate retry
            // excludes that peer so a healthy alternative is reached without
            // waiting out the probe interval. Consumed by the next selection.
            let mut exclude_peer: Option<String> = None;
            let mut last_probe_denied: Option<String> = None;

            loop {
                // Every pass after the first follows a wait for capacity, and
                // the drain may have begun meanwhile; do not forward again.
                if state.draining.load(Ordering::Relaxed) {
                    state.metrics.inc_errors();
                    return Err(AppError::node_draining());
                }
                let selection_exclude = exclude_peer.take();
                let Some(peer) = state
                    .find_peer_for_model(&model_to_use, selection_exclude.as_deref())
                    .await
                else {
                    // No peer currently supports this model. Either it really
                    // does not exist anywhere, or every peer that advertises
                    // it is currently filtered (open circuit, draining,
                    // stale). For the latter, wait for cluster state to
                    // change before giving up.
                    let capacity = state.capacity_notify.notified();
                    if state.draining.load(Ordering::Relaxed) {
                        state.metrics.inc_errors();
                        return Err(AppError::node_draining());
                    }
                    // We can only distinguish "no one will ever serve this"
                    // from "no one can serve it right now" by checking
                    // whether any peer ever advertised the model. If the
                    // model has never been seen on any peer, fail fast with
                    // model_not_found. Otherwise wait.
                    if !state.any_peer_advertises_model(&model_to_use).await {
                        state.metrics.inc_errors();
                        return Err(AppError::model_not_found(format!(
                            "Model '{model_to_use}' not found in local cookbook or any peer"
                        )));
                    }
                    info!(
                        event = "forward_waiting_for_peer",
                        model = %model_to_use,
                        "All peers advertising model are currently unavailable, waiting for cluster capacity"
                    );
                    // During a probe-denial episode (the excluded recovering
                    // peer may be the only one advertising the model) the
                    // probe slot frees by time passing, so bound the wait by
                    // the probe deadline instead of relying on a Notify.
                    if last_probe_denied.is_some() {
                        let probe_deadline = Duration::from_millis(
                            state
                                .config
                                .cluster
                                .circuit_breaker
                                .half_open_probe_interval_ms
                                .max(250),
                        );
                        if let Ok(res) = tokio::time::timeout(
                            probe_deadline,
                            wait_for_capacity_or_disconnect(capacity, &conn_handle),
                        )
                        .await
                        {
                            res?;
                        }
                    } else {
                        wait_for_capacity_or_disconnect(capacity, &conn_handle).await?;
                    }
                    continue;
                };

                info!(
                    event = "forward_unknown_model",
                    model = %model_to_use,
                    peer = %peer.node_id,
                    "Model not in local cookbook, forwarding to peer"
                );

                // Track this forward locally so concurrent routing decisions
                // see the peer's load before its next gossip tick. This guard
                // is per-attempt: released when we retry, kept alive into
                // cleanup_fut on the attempt that commits.
                let peer_forward_guard = state.track_peer_forward(&peer.node_id).await;

                let outcome = await_unless_client_gone(
                    &conn_handle,
                    DISCONNECT_POLL_INTERVAL,
                    "peer_forward",
                    attempt_peer_forward(
                        &state,
                        PeerRequest {
                            peer_id: &peer.node_id,
                            peer_address: &peer.address,
                            method: parts.method.clone(),
                            path_and_query: &path_and_query,
                            headers: forward_headers.clone(),
                            body: forward_body.clone(),
                            timeout_ms,
                        },
                        response_streaming,
                    ),
                )
                .await?;

                match outcome {
                    ForwardOutcome::StreamingSurface(resp) => {
                        let cleanup_fut: BoxFuture<'static, ()> = {
                            let state = state.clone();
                            let mut peer_forward_guard = peer_forward_guard;
                            Box::pin(async move {
                                state.metrics.dec_current_requests();
                                peer_forward_guard.release();
                            })
                        };
                        guard.complete();
                        let tokens_counter = Arc::new(AtomicU64::new(0));
                        return Ok(peer_response_to_client(
                            resp,
                            response_streaming,
                            cleanup_fut,
                            tokens_counter,
                        )
                        .await);
                    }
                    ForwardOutcome::Surface {
                        status,
                        headers,
                        body,
                    } => {
                        let cleanup_fut: BoxFuture<'static, ()> = {
                            let state = state.clone();
                            let mut peer_forward_guard = peer_forward_guard;
                            Box::pin(async move {
                                state.metrics.dec_current_requests();
                                peer_forward_guard.release();
                            })
                        };
                        guard.complete();
                        let tokens_counter = Arc::new(AtomicU64::new(0));
                        let _cleanup_guard = AutoCleanup::new(cleanup_fut);
                        return Ok(build_non_streaming_body_response(
                            status,
                            headers,
                            body,
                            tokens_counter,
                        ));
                    }
                    ForwardOutcome::Heal { reason } => {
                        // peer_forward_guard drops here, releasing this
                        // attempt's edge-counter slot. The peer's
                        // circuit-breaker state was already updated inside
                        // attempt_peer_forward via record_peer_status.
                        info!(
                            event = "forward_heal_wait",
                            peer = %peer.node_id,
                            reason = %reason,
                            "Peer reported saturation, waiting for cluster capacity"
                        );
                        drop(peer_forward_guard);
                        let capacity = state.capacity_notify.notified();
                        if state.draining.load(Ordering::Relaxed) {
                            state.metrics.inc_errors();
                            return Err(AppError::node_draining());
                        }
                        wait_for_capacity_or_disconnect(capacity, &conn_handle).await?;
                        continue;
                    }
                    ForwardOutcome::ProbeDenied => {
                        tracing::debug!(
                            event = "forward_probe_denied",
                            peer = %peer.node_id,
                            "Peer circuit recovering and probe slot taken"
                        );
                        drop(peer_forward_guard);
                        let repeat =
                            last_probe_denied.as_deref() == Some(peer.node_id.as_str());
                        last_probe_denied = Some(peer.node_id.clone());
                        exclude_peer = Some(peer.node_id.clone());
                        if repeat {
                            // Selection keeps landing on this recovering
                            // peer: park until the next probe deadline. The
                            // slot frees by time passing, not only by
                            // capacity events.
                            let probe_deadline = Duration::from_millis(
                                state
                                    .config
                                    .cluster
                                    .circuit_breaker
                                    .half_open_probe_interval_ms
                                    .max(250),
                            );
                            let capacity = state.capacity_notify.notified();
                            if state.draining.load(Ordering::Relaxed) {
                                state.metrics.inc_errors();
                                return Err(AppError::node_draining());
                            }
                            if let Ok(res) = tokio::time::timeout(
                                probe_deadline,
                                wait_for_capacity_or_disconnect(capacity, &conn_handle),
                            )
                            .await
                            {
                                res?;
                            }
                        }
                        // Otherwise retry immediately, excluding this peer,
                        // so a healthy alternative is reached without delay.
                        continue;
                    }
                    ForwardOutcome::Transport(e) => {
                        // The failure was recorded (with this dispatch's
                        // probe attribution) inside attempt_peer_forward. The
                        // forwarder retries against any peer that comes back
                        // online (including this one once its circuit
                        // resets).
                        warn!(
                            event = "forward_transport_retry",
                            peer = %peer.node_id,
                            error = ?e,
                            "Peer transport failure, waiting for cluster capacity to retry"
                        );
                        drop(peer_forward_guard);
                        let capacity = state.capacity_notify.notified();
                        if state.draining.load(Ordering::Relaxed) {
                            state.metrics.inc_errors();
                            return Err(AppError::node_draining());
                        }
                        wait_for_capacity_or_disconnect(capacity, &conn_handle).await?;
                        continue;
                    }
                }
            }
        }

        let (model_name, profile) = local_resolution.ok_or_else(|| {
            AppError::internal_server_error("Unexpected missing local resolution")
        })?;

        if let Err(e) = ensure_profile_supports_endpoint(&profile, endpoint_kind, &model_to_use) {
            return Err(*e);
        }

        let model_key = format!("{}:{}", model_name, profile.id);
        let args_hash = state
            .get_args_hash_for_key(&model_key)
            .await
            .ok_or_else(|| AppError::internal_server_error("Failed to compute args_hash"))?;
        let hash_metrics = state.metrics.get_hash_metrics(&args_hash).await;
        hash_metrics.add_display_name(&model_key);
        hash_metrics.requests_total.fetch_add(1, Ordering::Relaxed);
        state.metrics.inc_requests();

        // Guard ensures we decrement metrics/in_flight on cancellation or error
        let mut guard = RequestGuard::new(state.clone());

        let start_time = std::time::Instant::now();

        let timeout_ms = profile
            .max_request_duration_ms
            .unwrap_or(state.config.model_defaults.max_request_duration_ms);

        // If request was already forwarded (hop count > 0), prefer local handling
        // to avoid ping-pong routing loops between nodes
        let prefer_local = current_hops > 0;

        // Cluster-aware routing: wait for capacity if model exists but no node available
        let best_node = loop {
            if let Some(node) = state
                .select_best_node(&model_name, &profile, prefer_local, None)
                .await
            {
                break node;
            }

            // No node available - check if model exists in local cookbook
            // If it does, wait for capacity (local node might be draining)
            // If it doesn't, the model truly doesn't exist anywhere → 404
            if state.resolve_model(&model_name).await.is_none() {
                // Model doesn't exist in local cookbook and no peer supports it
                state.metrics.inc_errors();
                hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                return Err(AppError::model_not_found(&model_name));
            }

            // If node is draining (shutdown in progress), don't wait — bail out
            let capacity = state.capacity_notify.notified();
            if state.draining.load(Ordering::Relaxed) {
                state.metrics.inc_errors();
                hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                return Err(AppError::node_draining());
            }

            // Model exists locally but no node can serve right now
            // Wait for capacity change and retry
            info!(
                event = "waiting_for_node",
                model = %model_name,
                profile = %profile.id,
                "No node available, waiting for cluster capacity"
            );
            wait_for_capacity_or_disconnect(capacity, &conn_handle).await?;
            // Loop back to retry select_best_node
        };

        if best_node.node_id == state.config.node_id {
            // Local routing — wrap in disconnect check so we don't block
            // forever inside get_instance_for_model's internal capacity waits
            // if the client has already gone.
            let instance_result = match &conn_handle {
                Some(handle) => {
                    tokio::select! {
                        res = state.get_instance_for_model(&model_name, &profile, true, resolved_cookbook_version) => res,
                        _ = crate::connection::poll_client_disconnect(handle, DISCONNECT_POLL_INTERVAL) => {
                            info!(event = "client_disconnected", "Client disconnected during instance acquisition");
                            return Err(AppError::client_disconnected());
                        }
                    }
                }
                None => {
                    state
                        .get_instance_for_model(
                            &model_name,
                            &profile,
                            true,
                            resolved_cookbook_version,
                        )
                        .await
                }
            };
            match instance_result {
                Ok(instance_lock) => {
                    guard.set_instance(instance_lock.clone());

                    let (target_host, target_port, streams_awaiting_downstream, backend_api_key) = {
                        let mut inst = instance_lock.write().await;
                        inst.last_activity = std::time::Instant::now();
                        (
                            inst.host.clone(),
                            inst.port,
                            inst.streams_awaiting_downstream.clone(),
                            inst.backend_api_key.clone(),
                        )
                    };

                    let url = format!("http://{target_host}:{target_port}{path_and_query}");

                    info!("Forwarding locally to {}", url);

                    let mut headers =
                        build_local_upstream_headers(&parts.headers, response_streaming);

                    // Send the key this instance was started with: after a
                    // cookbook reload, the profile this request resolved can
                    // name a different `--api-key` than a running instance.
                    if let Some(api_key) = backend_api_key {
                        let value =
                            axum::http::HeaderValue::from_str(&format!("Bearer {api_key}"))
                                .map_err(|e| {
                                    error!(
                                        "Failed to create Authorization header from API key: {}. \
                                     API key may contain invalid characters.",
                                        e
                                    );
                                    AppError::internal_server_error(
                                        "Invalid API key configuration for backend instance",
                                    )
                                })?;
                        headers.insert(axum::http::header::AUTHORIZATION, value);
                    }

                    let mut client_req = state
                        .http_client
                        .request(parts.method, &url)
                        .headers(headers)
                        .body(forward_body.clone());

                    if timeout_ms > 0 {
                        client_req = client_req.timeout(Duration::from_millis(timeout_ms));
                    }

                    match await_unless_client_gone(
                        &conn_handle,
                        DISCONNECT_POLL_INTERVAL,
                        "local_generation",
                        client_req.send(),
                    )
                    .await?
                    {
                        Ok(resp) => {
                            let tokens_counter = Arc::new(AtomicU64::new(0));
                            let tokens_for_cleanup = tokens_counter.clone();

                            let cleanup_fut: BoxFuture<'static, ()> = {
                                let instance_lock = instance_lock.clone();
                                let state = state.clone();
                                let hash_metrics = hash_metrics.clone();

                                Box::pin(async move {
                                    let (
                                        model_name,
                                        profile_id,
                                        instance_id,
                                        became_idle,
                                        was_draining,
                                        tenure_expired,
                                    ) = {
                                        let mut inst = instance_lock.write().await;
                                        inst.in_flight_requests =
                                            inst.in_flight_requests.saturating_sub(1);
                                        let idle = inst.in_flight_requests == 0;
                                        let was_draining = inst.draining.load(Ordering::Relaxed);
                                        let tenure_expired = inst
                                            .evictable_after
                                            .lock()
                                            .map(|t| std::time::Instant::now() >= t)
                                            .unwrap_or(false);
                                        (
                                            inst.model_name.clone(),
                                            inst.profile_id.clone(),
                                            inst.id.clone(),
                                            idle,
                                            was_draining,
                                            tenure_expired,
                                        )
                                    };
                                    state.metrics.dec_current_requests();

                                    // ── Drain scheduling ────────────────────────────
                                    // Do not hold the instance lock while checking queues:
                                    // NodeState's lock order requires queues before
                                    // individual instance locks.
                                    if !was_draining {
                                        let dominated = state
                                            .has_queued_competitors_needing_eviction(&model_name)
                                            .await;
                                        if dominated {
                                            let own_queue_empty = became_idle
                                                && !state
                                                    .has_pending_for_model(
                                                        &model_name,
                                                        &profile_id,
                                                    )
                                                    .await;

                                            if tenure_expired || own_queue_empty {
                                                let inst = instance_lock.write().await;
                                                if inst.id == instance_id
                                                    && !inst.draining.load(Ordering::Relaxed)
                                                {
                                                    inst.draining.store(true, Ordering::Relaxed);
                                                    inst.draining_for_competitor.store(true, Ordering::Relaxed);
                                                    info!(
                                                        event = "drain_triggered",
                                                        model = %model_name,
                                                        profile = %profile_id,
                                                        instance_id = %instance_id,
                                                        reason = if tenure_expired { "tenure_expired" } else { "queue_empty" },
                                                        "Instance marked draining for competing model"
                                                    );
                                                }
                                            }
                                        }
                                    }

                                    // Critical: If instance became idle, we must notify ALL queues.
                                    // Why? Because another model's queue might be blocked by MaxInstancesNode.
                                    // If we only notify this model's queue, requests for other models
                                    // will never wake up to check if they can now evict this idle instance.
                                    if became_idle {
                                        // Check if any drains can be cancelled (competitors may have
                                        // been handled by peers or timed out while we were draining).
                                        state.maybe_cancel_drains().await;
                                        state.notify_all_queues().await;
                                    } else {
                                        state.notify_queue(&model_name, &profile_id).await;
                                    }
                                    let duration = start_time.elapsed().as_millis() as u64;
                                    // `observe_latency` already accumulates
                                    // `total_latency_ms` (and records the p95
                                    // sample); adding it again here would
                                    // double-count the latency.
                                    hash_metrics.observe_latency(duration);
                                    hash_metrics.tokens_generated_total.fetch_add(
                                        tokens_for_cleanup.load(Ordering::Relaxed),
                                        Ordering::Relaxed,
                                    );
                                })
                            };

                            // Transfer responsibility to cleanup_fut
                            guard.complete();

                            // Count a local body-read failure (mid-stream, or a
                            // failed non-streaming body read) like a send error;
                            // peer paths pass None and account for it elsewhere.
                            let error_recorder =
                                Some((state.metrics.clone(), hash_metrics.clone()));
                            if response_streaming {
                                Ok(build_streaming_response(
                                    resp,
                                    cleanup_fut,
                                    tokens_counter,
                                    error_recorder,
                                    false,
                                    Some(streams_awaiting_downstream),
                                ))
                            } else {
                                // `send()` resolves at the response headers, so a
                                // slow non-streaming body would otherwise be read
                                // to completion for an already-gone client. Keep
                                // watching for disconnect across the body drain
                                // too; the `AutoCleanup` inside
                                // `handle_non_streaming_response` runs the cleanup
                                // if we bail mid-read.
                                await_unless_client_gone(
                                    &conn_handle,
                                    DISCONNECT_POLL_INTERVAL,
                                    "local_generation",
                                    handle_non_streaming_response(
                                        resp,
                                        cleanup_fut,
                                        tokens_counter,
                                        error_recorder,
                                    ),
                                )
                                .await
                            }
                        }
                        Err(e) => {
                            let detail = error_chain(&e);
                            error!("Upstream error: {}", detail);
                            state.metrics.inc_errors();
                            // Guard handles dec_current_requests and in_flight cleanup
                            hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);

                            Err(AppError::upstream_error(format!("Upstream error: {detail}")))
                        }
                    }
                }
                Err(e) => {
                    // Guard handles dec_current_requests. Error counters are NOT
                    // bumped eagerly here: a local failure may still be served by a
                    // peer below (peer fallback). They are incremented only when the
                    // client ultimately receives an error — based on the response
                    // status returned to the client on the fallback path below, or on
                    // the fall-through error-return paths after the fallback attempt —
                    // so a request a peer serves successfully is not counted.

                    // If local spawning failed repeatedly, try forwarding to a peer
                    // before returning an error to the client.
                    if matches!(e, NodeError::SpawnFailuresExhausted) {
                        // Same circuit-breaker commit-point admission as
                        // attempt_peer_forward: a recovering peer admits at
                        // most one probe per interval, and this dispatch
                        // bypasses attempt_peer_forward.
                        let fallback_peer = match state
                            .find_peer_for_model(&model_to_use, None)
                            .await
                        {
                            Some(peer) => {
                                match state.circuit_breaker.try_claim_dispatch_sync(&peer.node_id)
                                {
                                    crate::circuit_breaker::DispatchDecision::Admit {
                                        probe_ticket,
                                    } => Some((peer, probe_ticket)),
                                    crate::circuit_breaker::DispatchDecision::Deny => {
                                        tracing::debug!(
                                            event = "forward_probe_denied",
                                            peer = %peer.node_id,
                                            "Peer circuit recovering and probe slot taken; skipping spawn-failure fallback"
                                        );
                                        None
                                    }
                                }
                            }
                            None => None,
                        };
                        if let Some((peer, probe_ticket)) = fallback_peer {
                            info!(
                                event = "local_spawn_failed_peer_fallback",
                                model = %model_name,
                                peer = %peer.node_id,
                                "Local spawn failures exhausted, falling back to peer"
                            );

                            let peer_forward_guard = state.track_peer_forward(&peer.node_id).await;
                            let forward_headers =
                                build_forward_headers(&parts.headers, current_hops, &request_id);

                            match await_unless_client_gone(
                                &conn_handle,
                                DISCONNECT_POLL_INTERVAL,
                                "peer_forward",
                                send_peer_request(
                                    &state,
                                    PeerRequest {
                                        peer_id: &peer.node_id,
                                        peer_address: &peer.address,
                                        method: parts.method.clone(),
                                        path_and_query: &path_and_query,
                                        headers: forward_headers,
                                        body: forward_body.clone(),
                                        timeout_ms,
                                    },
                                    response_streaming,
                                ),
                            )
                            .await?
                            {
                                Ok(resp) => {
                                    let status = match &resp {
                                        PeerResponse::Http(resp) => resp.status(),
                                        PeerResponse::Noise { status, .. } => *status,
                                    };
                                    // Successes are recorded without the
                                    // probe ticket here: the body has not
                                    // been read yet (it streams to the
                                    // client below), so headers alone must
                                    // not drive recovery. Failure statuses
                                    // keep the ticket so a failed probe
                                    // escalates the block window.
                                    let attribution =
                                        probe_ticket && peer_status_is_failure(status);
                                    record_peer_status(&state, &peer.node_id, status, attribution)
                                        .await;

                                    let tokens_counter = Arc::new(AtomicU64::new(0));
                                    let tokens_for_cleanup = tokens_counter.clone();

                                    let cleanup_fut: BoxFuture<'static, ()> = {
                                        let state = state.clone();
                                        let hash_metrics = hash_metrics.clone();
                                        let mut peer_forward_guard = peer_forward_guard;
                                        Box::pin(async move {
                                            state.metrics.dec_current_requests();
                                            peer_forward_guard.release();
                                            let duration = start_time.elapsed().as_millis() as u64;
                                            hash_metrics.observe_forwarded_latency(duration);
                                            hash_metrics.tokens_generated_total.fetch_add(
                                                tokens_for_cleanup.load(Ordering::Relaxed),
                                                Ordering::Relaxed,
                                            );
                                        })
                                    };

                                    guard.complete();

                                    // Watch for client disconnect across the peer
                                    // body drain too. For a non-streaming response
                                    // peer_response_to_client buffers the body, so
                                    // without this an abandoned request would read
                                    // the full peer generation; its AutoCleanup
                                    // runs the cleanup if we bail mid-read. (A
                                    // streaming response returns here at the
                                    // headers and is torn down by its own
                                    // write-failure path.)
                                    let client_response = await_unless_client_gone(
                                        &conn_handle,
                                        DISCONNECT_POLL_INTERVAL,
                                        "peer_forward",
                                        peer_response_to_client(
                                            resp,
                                            response_streaming,
                                            cleanup_fut,
                                            tokens_counter,
                                        ),
                                    )
                                    .await?;

                                    // Count an error iff the client ultimately
                                    // receives an error response. `client_status` is
                                    // the peer's status passed through as-is, except
                                    // that a non-streaming body-read failure is turned
                                    // into a 502 by `peer_response_to_client`, which
                                    // this captures. A request the peer serves
                                    // successfully is not counted (counting it was the
                                    // over-count bug); a mid-stream failure after a
                                    // success status is not counted, consistent with
                                    // the local and direct-forward paths.
                                    let client_status = client_response.status();
                                    if client_status.is_client_error()
                                        || client_status.is_server_error()
                                    {
                                        state.metrics.inc_errors();
                                        hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                                    }

                                    return Ok(client_response);
                                }
                                Err(peer_err) => {
                                    // peer_forward_guard drops on fall-through,
                                    // releasing the counter.
                                    state
                                        .circuit_breaker
                                        .record_failure(&peer.node_id, probe_ticket)
                                        .await;
                                    warn!(
                                        peer = %peer.node_id,
                                        error = ?peer_err,
                                        "Peer fallback also failed"
                                    );
                                    // Fall through to return the original error
                                }
                            }
                        }
                    }

                    // Reached only when no peer returned a response (no peer
                    // available, or the fallback request itself failed): the request
                    // is returning an error to the client, so count it now. Responses
                    // the fallback peer did return are counted above based on their
                    // client-facing status, so each failed request is counted exactly
                    // once.
                    state.metrics.inc_errors();
                    hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);

                    Err(node_error_to_app_error(e))
                }
            }
        } else {
            // Remote routing — patient retry loop.
            //
            // The forwarder treats a peer's healable 503 (queue_timeout,
            // queue_full, no_capacity, ...) as "saturated for now" and waits
            // for cluster capacity to change before re-selecting and trying
            // again. This avoids surfacing transient peer saturation to the
            // client, which is the inverse of the "patient proxy" intent.
            //
            // Each iteration re-selects via `select_best_node` so that a
            // freed peer, a circuit reset, or a gossip update can route us
            // somewhere new without bouncing back to the original client.
            let forward_headers =
                build_forward_headers(&parts.headers, current_hops, &request_id);

            let mut current_node = best_node;
            // Set when a dispatch was probe-denied: the immediate re-select
            // excludes that peer so a healthy alternative is reached without
            // waiting out the probe interval. Consumed by the next selection.
            let mut exclude_peer: Option<String> = None;
            let mut last_probe_denied: Option<String> = None;
            loop {
                info!(
                    event = "forward_remote",
                    peer = %current_node.node_id,
                    "Forwarding remotely to peer"
                );

                let peer_forward_guard =
                    state.track_peer_forward(&current_node.node_id).await;

                let outcome = await_unless_client_gone(
                    &conn_handle,
                    DISCONNECT_POLL_INTERVAL,
                    "peer_forward",
                    attempt_peer_forward(
                        &state,
                        PeerRequest {
                            peer_id: &current_node.node_id,
                            peer_address: &current_node.address,
                            method: parts.method.clone(),
                            path_and_query: &path_and_query,
                            headers: forward_headers.clone(),
                            body: forward_body.clone(),
                            timeout_ms,
                        },
                        response_streaming,
                    ),
                )
                .await?;

                let mut probe_wait_deadline: Option<Duration> = None;
                let mut skip_wait = false;
                match outcome {
                    ForwardOutcome::StreamingSurface(resp) => {
                        let tokens_counter = Arc::new(AtomicU64::new(0));
                        let tokens_for_cleanup = tokens_counter.clone();
                        let cleanup_fut: BoxFuture<'static, ()> = {
                            let state = state.clone();
                            let hash_metrics = hash_metrics.clone();
                            let mut peer_forward_guard = peer_forward_guard;
                            Box::pin(async move {
                                state.metrics.dec_current_requests();
                                peer_forward_guard.release();
                                let duration = start_time.elapsed().as_millis() as u64;
                                hash_metrics.observe_forwarded_latency(duration);
                                hash_metrics.tokens_generated_total.fetch_add(
                                    tokens_for_cleanup.load(Ordering::Relaxed),
                                    Ordering::Relaxed,
                                );
                            })
                        };
                        guard.complete();
                        return Ok(peer_response_to_client(
                            resp,
                            response_streaming,
                            cleanup_fut,
                            tokens_counter,
                        )
                        .await);
                    }
                    ForwardOutcome::Surface {
                        status,
                        headers,
                        body,
                    } => {
                        let tokens_counter = Arc::new(AtomicU64::new(0));
                        let tokens_for_cleanup = tokens_counter.clone();
                        let cleanup_fut: BoxFuture<'static, ()> = {
                            let state = state.clone();
                            let hash_metrics = hash_metrics.clone();
                            let mut peer_forward_guard = peer_forward_guard;
                            Box::pin(async move {
                                state.metrics.dec_current_requests();
                                peer_forward_guard.release();
                                let duration = start_time.elapsed().as_millis() as u64;
                                hash_metrics.observe_forwarded_latency(duration);
                                hash_metrics.tokens_generated_total.fetch_add(
                                    tokens_for_cleanup.load(Ordering::Relaxed),
                                    Ordering::Relaxed,
                                );
                            })
                        };
                        guard.complete();
                        let _cleanup_guard = AutoCleanup::new(cleanup_fut);
                        return Ok(build_non_streaming_body_response(
                            status,
                            headers,
                            body,
                            tokens_counter,
                        ));
                    }
                    ForwardOutcome::Heal { reason } => {
                        info!(
                            event = "forward_heal_wait",
                            peer = %current_node.node_id,
                            reason = %reason,
                            "Peer reported saturation, waiting for cluster capacity"
                        );
                        drop(peer_forward_guard);
                    }
                    ForwardOutcome::ProbeDenied => {
                        tracing::debug!(
                            event = "forward_probe_denied",
                            peer = %current_node.node_id,
                            "Peer circuit recovering and probe slot taken"
                        );
                        drop(peer_forward_guard);
                        let repeat =
                            last_probe_denied.as_deref() == Some(current_node.node_id.as_str());
                        last_probe_denied = Some(current_node.node_id.clone());
                        exclude_peer = Some(current_node.node_id.clone());
                        if repeat {
                            // Selection keeps landing on this recovering
                            // peer: bound the wait below by the next probe
                            // deadline, since the slot frees by time passing.
                            probe_wait_deadline = Some(Duration::from_millis(
                                state
                                    .config
                                    .cluster
                                    .circuit_breaker
                                    .half_open_probe_interval_ms
                                    .max(250),
                            ));
                        } else {
                            // First denial: re-select immediately, excluding
                            // this peer, so a healthy alternative is reached
                            // without waiting out the probe interval.
                            skip_wait = true;
                        }
                    }
                    ForwardOutcome::Transport(e) => {
                        // Failure already recorded inside attempt_peer_forward
                        // with this dispatch's probe attribution.
                        warn!(
                            event = "forward_transport_retry",
                            peer = %current_node.node_id,
                            address = %current_node.address,
                            path = %path_and_query,
                            error = ?e,
                            "Peer forwarding failed, waiting for cluster capacity to retry"
                        );
                        drop(peer_forward_guard);
                    }
                }

                // A drain wakes capacity waiters once, when it begins, so a
                // wait registered after that could hold the drain open until
                // the client gave up. Subscribe first, then fail if the drain
                // has begun; the check after the wait covers a drain that
                // begins during it.
                let capacity = state.capacity_notify.notified();
                if state.draining.load(Ordering::Relaxed) {
                    state.metrics.inc_errors();
                    hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return Err(AppError::node_draining());
                }

                // Wait for cluster state to change (capacity freed, peer
                // recovered, gossip update, drain finished) before retrying.
                // Probe-denied attempts additionally wake at the next probe
                // deadline, since a probe slot frees by time passing — or
                // skip the wait entirely on a first denial, to re-select an
                // alternative peer immediately.
                if skip_wait {
                    // fall through to re-selection
                } else {
                    match probe_wait_deadline {
                    Some(deadline) => {
                        if let Ok(res) = tokio::time::timeout(
                            deadline,
                            wait_for_capacity_or_disconnect(capacity, &conn_handle),
                        )
                        .await
                        {
                            res?;
                        }
                    }
                    None => {
                        wait_for_capacity_or_disconnect(capacity, &conn_handle).await?;
                    }
                    }
                }

                if state.draining.load(Ordering::Relaxed) {
                    state.metrics.inc_errors();
                    hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    return Err(AppError::node_draining());
                }

                // Re-select. Could be the same peer (its circuit may have
                // closed or its queue drained), a different peer, or even
                // local routing if best_node now points to self. For
                // simplicity we keep the remote-only path here: if local
                // routing has become best after a wait, the original
                // top-level decision still stands and the request will reach
                // the same peer-or-better via this loop until the peer
                // actually serves it.
                current_node = loop {
                    let selection_exclude = exclude_peer.take();
                    if let Some(node) = state
                        .select_best_node(
                            &model_name,
                            &profile,
                            prefer_local,
                            selection_exclude.as_deref(),
                        )
                        .await
                    {
                        break node;
                    }
                    info!(
                        event = "retry_waiting",
                        model = %model_name,
                        "No node available during retry, waiting for cluster capacity"
                    );
                    let capacity = state.capacity_notify.notified();
                    if state.draining.load(Ordering::Relaxed) {
                        state.metrics.inc_errors();
                        hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        return Err(AppError::node_draining());
                    }
                    // During a probe-denial episode the excluded recovering
                    // peer may be the only candidate; its probe slot frees by
                    // time passing, so bound the wait by the probe deadline.
                    if last_probe_denied.is_some() {
                        let probe_deadline = Duration::from_millis(
                            state
                                .config
                                .cluster
                                .circuit_breaker
                                .half_open_probe_interval_ms
                                .max(250),
                        );
                        if let Ok(res) = tokio::time::timeout(
                            probe_deadline,
                            wait_for_capacity_or_disconnect(capacity, &conn_handle),
                        )
                        .await
                        {
                            res?;
                        }
                    } else {
                        wait_for_capacity_or_disconnect(capacity, &conn_handle).await?;
                    }
                };
            }
        }
    }
    .instrument(span)
    .await
}

/// Node-wide + per-hash error counters for the local request being served,
/// threaded into the response paths so a body-read failure that surfaces inside
/// the shared response builder (non-streaming) or after the handler returns
/// (streaming) is still counted on the local path. `None` on the cluster
/// peer-forward paths, which account for their failures separately (the
/// peer-fallback status check and `peer_body_read_failed`).
type BodyReadErrorRecorder = Option<(Arc<Metrics>, Arc<HashMetrics>)>;

/// Count one local body-read failure: node-wide `proxy_errors_total` plus the
/// per-hash `errors_total`, mirroring the local send()-error accounting. A
/// `None` recorder is a no-op.
fn record_local_body_read_error(recorder: &BodyReadErrorRecorder) {
    if let Some((metrics, hash_metrics)) = recorder {
        metrics.inc_errors();
        hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
    }
}

async fn handle_non_streaming_response(
    resp: reqwest::Response,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
    error_recorder: BodyReadErrorRecorder,
) -> Response<Body> {
    // `AutoCleanup` guarantees `cleanup` runs exactly once — either when this
    // function returns normally, or if the enclosing task is cancelled while
    // awaiting the body (e.g. client disconnect mid-read). Without this
    // guard, the cleanup future would be dropped unrun on cancellation,
    // leaking in_flight_requests on the instance and current_requests on the
    // node.
    let _cleanup_guard = AutoCleanup::new(cleanup);

    let status = resp.status();
    let headers = resp.headers().clone();

    // Read full body
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let detail = error_chain(&e);
            error!(
                event = "local_body_read_failed",
                "Failed to read upstream response body: {}", detail
            );
            record_local_body_read_error(&error_recorder);
            return AppError::upstream_error(format!(
                "Failed to read upstream response body: {detail}"
            ))
            .into_response();
        }
    };

    build_non_streaming_body_response(status, headers, bytes, tokens_generated)
}

fn build_non_streaming_body_response(
    status: StatusCode,
    headers: HeaderMap,
    bytes: Bytes,
    tokens_generated: Arc<AtomicU64>,
) -> Response<Body> {
    if let Ok(val) = serde_json::from_slice::<Value>(&bytes) {
        if let Some(usage) = val.get("usage") {
            if let Some(completion_tokens) = usage.get("completion_tokens").and_then(|v| v.as_u64())
            {
                info!(
                    "Counted {} tokens from non-streaming response",
                    completion_tokens
                );
                tokens_generated.fetch_add(completion_tokens, Ordering::Relaxed);
            }
        } else {
            info!("No usage field in response");
        }
    } else {
        info!("Failed to parse response as JSON");
    }

    relayed_response(status, headers, Body::from(bytes))
}

/// Build the client-facing response for a response relayed from a local
/// instance or a peer, without the upstream connection's hop-by-hop fields.
fn relayed_response(status: StatusCode, mut headers: HeaderMap, body: Body) -> Response<Body> {
    remove_hop_by_hop_headers(&mut headers);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Wraps a cleanup `BoxFuture` so it is guaranteed to be spawned exactly once,
/// even if the owning task is cancelled before it reaches its normal spawn
/// site. Mirrors the Drop-based cleanup semantics of `CleanupStream`, but for
/// non-streaming bodies where the cleanup would otherwise only run after a
/// body-read `await` that a cancellation can interrupt.
struct AutoCleanup {
    cleanup: Option<BoxFuture<'static, ()>>,
}

impl AutoCleanup {
    fn new(cleanup: BoxFuture<'static, ()>) -> Self {
        Self {
            cleanup: Some(cleanup),
        }
    }
}

impl Drop for AutoCleanup {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(cleanup);
                }
                Err(_) => {
                    // Runtime is shutting down — cleanup cannot execute.
                    // Same architectural limitation as CleanupStream's Drop.
                    let count = SKIPPED_STREAM_CLEANUPS.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::warn!(
                        skipped_cleanups = count,
                        "Response cleanup skipped: tokio runtime unavailable during drop. \
                         Request counters may be inaccurate during shutdown."
                    );
                }
            }
        }
    }
}

/// `downstream_wait` is the serving instance's count of streams waiting on
/// their clients (local responses only; see `DownstreamWait`).
fn build_streaming_response(
    resp: reqwest::Response,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
    error_recorder: BodyReadErrorRecorder,
    peer_stream: bool,
    downstream_wait: Option<Arc<AtomicUsize>>,
) -> Response<Body> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let mut stream = CleanupStream::new(
        resp.bytes_stream(),
        cleanup,
        tokens_generated,
        error_recorder,
        peer_stream,
    );
    stream.downstream_wait = downstream_wait.map(DownstreamWait::new);
    relayed_response(status, headers, Body::from_stream(stream))
}

fn build_streaming_response_from_parts<S, E>(
    status: StatusCode,
    headers: HeaderMap,
    stream: S,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
    peer_stream: bool,
) -> Response<Body>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    // Peer (Noise) stream: no local error recorder; a mid-stream abort is
    // counted in `PEER_STREAM_BODY_ABORTS` when `peer_stream` (a successful
    // streamed peer response — the otherwise-invisible 2xx-body-abort case).
    let stream = CleanupStream::new(stream, cleanup, tokens_generated, None, peer_stream);
    relayed_response(status, headers, Body::from_stream(stream))
}

async fn peer_response_to_client(
    response: PeerResponse,
    stream_requested: bool,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
) -> Response<Body> {
    match response {
        PeerResponse::Http(resp) => {
            if stream_requested {
                // Peer path: no local error recorder. Count a mid-stream abort in
                // `PEER_STREAM_BODY_ABORTS` only for a *successful* (2xx) streamed
                // response — the otherwise-invisible case. A non-2xx response is
                // already visible via its error status (this matters on the
                // spawn-failure fallback path, which streams whatever status the
                // peer returned; the primary path only surfaces 2xx).
                let peer_stream = resp.status().is_success();
                build_streaming_response(resp, cleanup, tokens_generated, None, peer_stream, None)
            } else {
                handle_non_streaming_response(resp, cleanup, tokens_generated, None).await
            }
        }
        PeerResponse::Noise {
            status,
            headers,
            body,
        } => {
            let body = *body;
            if stream_requested {
                build_streaming_response_from_parts(
                    status,
                    headers,
                    body,
                    cleanup,
                    tokens_generated,
                    status.is_success(),
                )
            } else {
                let _cleanup_guard = AutoCleanup::new(cleanup);
                match body
                    .map(|chunk| chunk.map_err(|e| std::io::Error::other(error_chain(&e))))
                    .try_fold(Vec::new(), |mut acc, chunk| async move {
                        acc.extend_from_slice(&chunk);
                        Ok(acc)
                    })
                    .await
                {
                    Ok(bytes) => build_non_streaming_body_response(
                        status,
                        headers,
                        Bytes::from(bytes),
                        tokens_generated,
                    ),
                    Err(e) => {
                        error!("Failed to read peer response body: {}", e);
                        AppError::upstream_error(format!("Failed to read peer response body: {e}"))
                            .into_response()
                    }
                }
            }
        }
    }
}

/// Maximum buffer size for token counting (16MB).
/// If exceeded, token counting is disabled but stream continues unaffected.
const MAX_TOKEN_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Counter for stream cleanups skipped during shutdown (when tokio runtime unavailable).
/// This is a known limitation - cleanup is async but Drop is sync.
pub static SKIPPED_STREAM_CLEANUPS: AtomicU64 = AtomicU64::new(0);

/// Counter for streams where token counting was disabled due to buffer overflow.
/// Exposed via metrics endpoint.
pub static TOKEN_COUNTING_DISABLED: AtomicU64 = AtomicU64::new(0);

/// Counter for streaming responses forwarded from a cluster peer whose body
/// aborted mid-stream (a connection reset, or — with `upstream_read_timeout_ms`
/// enabled — an inactivity timeout). The peer's 2xx response head is surfaced to
/// the client and recorded as a success before the body streams, so an abort
/// after that point is otherwise invisible: it is not a local body-read failure
/// (`proxy_errors_total`) and does not move the peer's circuit breaker. Exposed
/// as `proxy_peer_stream_body_aborts_total`; runtime-only (resets on restart).
pub static PEER_STREAM_BODY_ABORTS: AtomicU64 = AtomicU64::new(0);

struct CleanupStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    inner: Pin<Box<S>>,
    cleanup: Option<BoxFuture<'static, ()>>,
    tokens_generated: Arc<AtomicU64>,
    buffer: Vec<u8>,
    token_counting_disabled: bool,
    cleanup_executed: bool,
    /// Error counters for the local request (None on peer streams). Used to
    /// count a mid-stream body-read failure once.
    error_recorder: BodyReadErrorRecorder,
    /// Guards the body-read error count to exactly one per failed stream.
    error_recorded: bool,
    /// True when this stream forwards a cluster peer's response body. A
    /// mid-stream abort then increments `PEER_STREAM_BODY_ABORTS` (once).
    peer_stream: bool,
    /// Tracks whether a local instance's response is waiting on its client.
    downstream_wait: Option<DownstreamWait>,
}

/// Marks a streamed response from a local instance as waiting on its client
/// from the moment a chunk is handed to the downstream writer until the writer
/// polls for the next one. A writer stops polling only while it cannot send
/// what it already has, so a response left in this state is blocked on its
/// client, not on the instance. The count is shared with the instance, where
/// the wedged-instance watchdog uses it to avoid stopping an instance that is
/// idle only because a client stopped reading.
struct DownstreamWait {
    awaiting: Arc<AtomicUsize>,
    waiting: bool,
}

impl DownstreamWait {
    fn new(awaiting: Arc<AtomicUsize>) -> Self {
        Self {
            awaiting,
            waiting: false,
        }
    }

    /// The writer asked for the next chunk.
    fn polled(&mut self) {
        if std::mem::take(&mut self.waiting) {
            self.awaiting.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// A chunk was handed to the writer.
    fn handed_off(&mut self) {
        if !std::mem::replace(&mut self.waiting, true) {
            self.awaiting.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for DownstreamWait {
    fn drop(&mut self) {
        self.polled();
    }
}

impl<S, E> CleanupStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    fn new(
        inner: S,
        cleanup: BoxFuture<'static, ()>,
        tokens_generated: Arc<AtomicU64>,
        error_recorder: BodyReadErrorRecorder,
        peer_stream: bool,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            cleanup: Some(cleanup),
            tokens_generated,
            buffer: Vec::new(),
            token_counting_disabled: false,
            cleanup_executed: false,
            error_recorder,
            error_recorded: false,
            peer_stream,
            downstream_wait: None,
        }
    }
}

fn process_line(line: &[u8], counter: &AtomicU64) {
    let data_prefix = b"data:";
    let done_marker = b"[DONE]";

    let trimmed = crate::util::strip_leading_whitespace(line);
    if trimmed.starts_with(data_prefix) {
        let payload = &trimmed[data_prefix.len()..];
        let payload_trimmed = crate::util::strip_leading_whitespace(payload);

        if payload_trimmed.starts_with(done_marker) {
            return;
        }

        // Try to parse JSON to be more precise about token counting
        if let Ok(val) = serde_json::from_slice::<Value>(payload) {
            if let Some(usage) = val.get("usage") {
                if let Some(completion_tokens) =
                    usage.get("completion_tokens").and_then(|v| v.as_u64())
                {
                    counter.store(completion_tokens, Ordering::Relaxed);
                    // If we found usage, we assume this is the final stats chunk and we are done counting.
                    // Even if there were choices (unlikely with usage), the usage stat is authoritative for the whole stream.
                    return;
                }
            }

            if let Some(choices) = val.get("choices").and_then(|c| c.as_array()) {
                if let Some(choice) = choices.first() {
                    if let Some(delta) = choice.get("delta") {
                        // Count any delta carrying generated output: visible
                        // content, reasoning content (thinking models emit it in
                        // a separate field), or tool-call fragments. A
                        // content-only check undercounts streams from reasoning
                        // and tool-calling models whenever the upstream omits the
                        // authoritative `usage` chunk.
                        let nonempty_str = |key: &str| {
                            delta
                                .get(key)
                                .and_then(|v| v.as_str())
                                .is_some_and(|s| !s.is_empty())
                        };
                        let has_tool_calls = delta
                            .get("tool_calls")
                            .and_then(|v| v.as_array())
                            .is_some_and(|a| !a.is_empty());
                        if nonempty_str("content")
                            || nonempty_str("reasoning_content")
                            || has_tool_calls
                        {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        } else {
            // Fallback: count chunk as token if we can't parse
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl<S, E> Stream for CleanupStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(wait) = self.downstream_wait.as_mut() {
            wait.polled();
        }
        let poll = self.as_mut().poll_upstream(cx);
        if let (Poll::Ready(Some(Ok(_))), Some(wait)) = (&poll, self.downstream_wait.as_mut()) {
            wait.handed_off();
        }
        poll
    }
}

impl<S, E> CleanupStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error,
{
    /// Poll the upstream body, counting tokens and running cleanup at its end.
    fn poll_upstream(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, E>>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Skip token counting if disabled due to buffer overflow
                if self.token_counting_disabled {
                    return Poll::Ready(Some(Ok(bytes)));
                }

                let mut current_slice = bytes.as_ref();

                // If we have a buffer, we might be completing a line
                if !self.buffer.is_empty() {
                    if let Some(idx) = current_slice.iter().position(|&b| b == b'\n') {
                        // Check buffer size before extending
                        if self.buffer.len() + idx > MAX_TOKEN_BUFFER_SIZE {
                            tracing::warn!(
                                "Token counting buffer exceeded {}MB limit, disabling token counting",
                                MAX_TOKEN_BUFFER_SIZE / (1024 * 1024)
                            );
                            self.token_counting_disabled = true;
                            self.buffer.clear();
                            TOKEN_COUNTING_DISABLED.fetch_add(1, Ordering::Relaxed);
                            return Poll::Ready(Some(Ok(bytes)));
                        }

                        // Found a newline. Complete the line in buffer.
                        self.buffer.extend_from_slice(&current_slice[..idx]);
                        process_line(&self.buffer, &self.tokens_generated);
                        self.buffer.clear();

                        // Advance slice past the newline
                        if idx + 1 < current_slice.len() {
                            current_slice = &current_slice[idx + 1..];
                        } else {
                            current_slice = &[];
                        }
                    } else {
                        // Check buffer size before extending
                        if self.buffer.len() + current_slice.len() > MAX_TOKEN_BUFFER_SIZE {
                            tracing::warn!(
                                "Token counting buffer exceeded {}MB limit, disabling token counting",
                                MAX_TOKEN_BUFFER_SIZE / (1024 * 1024)
                            );
                            self.token_counting_disabled = true;
                            self.buffer.clear();
                            TOKEN_COUNTING_DISABLED.fetch_add(1, Ordering::Relaxed);
                            return Poll::Ready(Some(Ok(bytes)));
                        }

                        // No newline in this chunk. Append everything to buffer.
                        self.buffer.extend_from_slice(current_slice);
                        return Poll::Ready(Some(Ok(bytes)));
                    }
                }

                // Process remaining complete lines in current_slice
                let mut start = 0;
                while let Some(idx) = current_slice[start..].iter().position(|&b| b == b'\n') {
                    let end = start + idx;
                    let line = &current_slice[start..end];
                    process_line(line, &self.tokens_generated);
                    start = end + 1;
                }

                // Whatever is left is an incomplete line (or empty)
                if start < current_slice.len() {
                    let remaining = &current_slice[start..];
                    // Check buffer size before extending
                    if self.buffer.len() + remaining.len() > MAX_TOKEN_BUFFER_SIZE {
                        tracing::warn!(
                            "Token counting buffer exceeded {}MB limit, disabling token counting",
                            MAX_TOKEN_BUFFER_SIZE / (1024 * 1024)
                        );
                        self.token_counting_disabled = true;
                        self.buffer.clear();
                        TOKEN_COUNTING_DISABLED.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.buffer.extend_from_slice(remaining);
                    }
                }

                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(None) => {
                if let Some(cleanup) = self.cleanup.take() {
                    tokio::spawn(cleanup);
                    self.cleanup_executed = true;
                }
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                // A mid-stream body-read failure aborts the client stream. Count
                // it once: on the local path via the error recorder (like a
                // non-streaming body-read failure or a send error); on a peer
                // forward via `PEER_STREAM_BODY_ABORTS`, since the peer's 2xx
                // head was already surfaced/recorded as a success.
                if !self.error_recorded {
                    self.error_recorded = true;
                    record_local_body_read_error(&self.error_recorder);
                    if self.error_recorder.is_some() {
                        error!(
                            event = "local_stream_body_read_failed",
                            error = %error_chain(&e),
                            "Local streaming body read failed"
                        );
                    }
                    if self.peer_stream {
                        // Peer-attributed (not a node `proxy_errors_total` error);
                        // logged at warn like the other peer-forward failures.
                        PEER_STREAM_BODY_ABORTS.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            event = "peer_stream_body_aborted",
                            error = %error_chain(&e),
                            "Forwarded peer streaming body aborted mid-stream"
                        );
                    }
                }
                Poll::Ready(Some(Err(e)))
            }
            other => other,
        }
    }
}

impl<S, E> Drop for CleanupStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    fn drop(&mut self) {
        // Only run cleanup if it wasn't already executed in poll_next
        if !self.cleanup_executed {
            if let Some(cleanup) = self.cleanup.take() {
                // Use Handle::try_current() to check if runtime is available
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(cleanup);
                    }
                    Err(_) => {
                        // Runtime is shutting down - cleanup cannot be executed.
                        // This is an architectural limitation: Drop is sync but cleanup is async.
                        // During graceful shutdown, in-flight streams should complete before
                        // runtime shutdown. If they don't, metrics will be slightly off.
                        let count = SKIPPED_STREAM_CLEANUPS.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::warn!(
                            skipped_cleanups = count,
                            "Cleanup skipped: tokio runtime unavailable during drop. \
                             In-flight request count may be inaccurate during shutdown."
                        );
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointKind {
    Text,
    Embeddings,
    Rerank,
    Other,
}

impl EndpointKind {
    fn from_path(path: &str) -> Self {
        if path.starts_with("/v1/embeddings") {
            EndpointKind::Embeddings
        } else if path.starts_with("/v1/rerank") || path.starts_with("/rerank") {
            EndpointKind::Rerank
        } else if path.starts_with("/v1/chat/completions") || path.starts_with("/v1/completions") {
            EndpointKind::Text
        } else {
            EndpointKind::Other
        }
    }
}

fn response_streaming_for_endpoint(
    endpoint_kind: EndpointKind,
    client_stream_requested: bool,
) -> bool {
    client_stream_requested && matches!(endpoint_kind, EndpointKind::Text)
}

fn request_body_for_endpoint(
    json_body: &Value,
    endpoint_kind: EndpointKind,
    client_stream_requested: bool,
    original: &Bytes,
) -> Bytes {
    if !client_stream_requested
        || !matches!(
            endpoint_kind,
            EndpointKind::Embeddings | EndpointKind::Rerank
        )
    {
        return original.clone();
    }

    let Some(object) = json_body.as_object() else {
        return original.clone();
    };

    let mut sanitized = object.clone();
    sanitized.remove("stream");
    serde_json::to_vec(&Value::Object(sanitized))
        .map(Bytes::from)
        .unwrap_or_else(|_| original.clone())
}

fn ensure_profile_supports_endpoint(
    profile: &Profile,
    endpoint_kind: EndpointKind,
    requested_model: &str,
) -> Result<(), Box<AppError>> {
    match endpoint_kind {
        EndpointKind::Embeddings => {
            if profile.supports_embeddings() {
                Ok(())
            } else {
                Err(Box::new(
                    AppError::model_not_found(format!(
                        "Model '{requested_model}' is not configured for embeddings"
                    ))
                    .with_param("model"),
                ))
            }
        }
        EndpointKind::Rerank => {
            if profile.supports_rerank() {
                // We pass the reranking response through as-is.
                // If the upstream server returns a format that matches the client's expectation
                // (e.g. Cohere-compatible or Jina-compatible), it will work transparently.
                Ok(())
            } else {
                Err(Box::new(
                    AppError::model_not_found(format!(
                        "Model '{requested_model}' is not configured for reranking"
                    ))
                    .with_param("model"),
                ))
            }
        }
        EndpointKind::Text => {
            if profile.supports_text_mode() {
                Ok(())
            } else {
                Err(Box::new(
                    AppError::invalid_request(format!(
                        "Model '{requested_model}' is restricted to embeddings or rerank endpoints"
                    ))
                    .with_param("model"),
                ))
            }
        }
        EndpointKind::Other => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[tokio::test]
    async fn node_draining_error_returns_retryable_503() {
        let response = node_error_to_app_error(NodeError::Draining).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "5");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "draining");
    }

    #[tokio::test]
    async fn model_not_found_while_queued_returns_404() {
        let response = node_error_to_app_error(NodeError::ModelNotFound("queued:default".into()))
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "model_not_found");
    }

    // A live, connected server-side socket whose peer is still attached.
    async fn connected_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn await_unless_client_gone_without_handle_just_awaits() {
        // No connection handle (e.g. an internally-originated request): the
        // future is awaited with no disconnect detection.
        let out: Result<u32, AppError> =
            await_unless_client_gone(&None, Duration::from_millis(10), "test", async { 42 }).await;
        assert_eq!(out.unwrap(), 42);
    }

    #[tokio::test]
    async fn await_unless_client_gone_returns_output_when_still_connected() {
        // Client still attached: a completing future returns its value and is
        // never spuriously aborted as a disconnect.
        let (client, server) = connected_pair().await;
        let handle = Some(ConnectionHandle::new(server.as_raw_fd()));
        let out: Result<u32, AppError> =
            await_unless_client_gone(&handle, Duration::from_millis(20), "test", async { 7 }).await;
        assert_eq!(out.unwrap(), 7);
        drop(client);
        drop(server);
    }

    #[tokio::test]
    async fn await_unless_client_gone_aborts_when_client_disconnects() {
        // Client goes away while the operation is still pending: the wrapped
        // future is dropped and a 499 client_disconnected error is returned
        // instead of the request running to completion.
        let (client, server) = connected_pair().await;
        let handle = Some(ConnectionHandle::new(server.as_raw_fd()));
        drop(client); // client closes the connection
        tokio::time::sleep(Duration::from_millis(50)).await; // let the kernel process the FIN
        let never = std::future::pending::<u32>();
        let out: Result<u32, AppError> =
            await_unless_client_gone(&handle, Duration::from_millis(20), "test", never).await;
        let err = out.expect_err("should abort on client disconnect");
        assert_eq!(err.status.as_u16(), 499);
        assert_eq!(err.error.type_, "client_disconnected");
        drop(server);
    }

    #[test]
    fn resolve_requested_model_absent_uses_default() {
        let body = json!({"messages": []});
        assert_eq!(
            resolve_requested_model(&body, "default-m"),
            Some("default-m".to_string())
        );
    }

    #[test]
    fn resolve_requested_model_null_is_invalid() {
        // A present `null` is not the same as an omitted field: it is an
        // invalid value (None -> 400), not an implicit request for the default.
        let body = json!({"model": null});
        assert_eq!(resolve_requested_model(&body, "default-m"), None);
    }

    #[test]
    fn resolve_requested_model_string_is_used() {
        let body = json!({"model": "gpt-oss-20b:fast"});
        assert_eq!(
            resolve_requested_model(&body, "default-m"),
            Some("gpt-oss-20b:fast".to_string())
        );
    }

    #[test]
    fn resolve_requested_model_empty_string_is_invalid() {
        // An empty string is present-but-invalid, not "omitted": None, so the
        // caller returns a 400 rather than substituting the default.
        let body = json!({"model": ""});
        assert_eq!(resolve_requested_model(&body, "default-m"), None);
    }

    #[test]
    fn resolve_requested_model_non_string_is_invalid() {
        // A present-but-wrong-type model must not silently route to the
        // default — it is a client error (None -> 400 at the call site).
        for body in [
            json!({"model": 123}),
            json!({"model": true}),
            json!({"model": {"name": "x"}}),
            json!({"model": ["x"]}),
        ] {
            assert_eq!(
                resolve_requested_model(&body, "default-m"),
                None,
                "expected None for body {body:?}"
            );
        }
    }

    fn profile_with_args(args: &[&str]) -> Profile {
        Profile {
            id: "p".into(),
            description: None,
            enabled: true,
            model_path: Some("/tmp/model.gguf".into()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 10,
            max_instances: None,
            llama_server_args: args.iter().map(|s| s.to_string()).collect(),
            estimated_vram_mb: None,
            estimated_sysmem_mb: None,
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
            min_eviction_tenure_secs: None,
        }
    }

    #[test]
    fn test_ensure_profile_supports_embeddings() {
        let p = profile_with_args(&["--embedding"]);
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Embeddings, "m").is_ok());
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Text, "m").is_err());
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Rerank, "m").is_err());
    }

    #[test]
    fn test_is_healable_error_body_recognises_known_types() {
        for ty in HEALABLE_PEER_ERROR_TYPES {
            let body = format!(
                r#"{{"error": {{"message": "x", "type": "{ty}", "param": null, "code": null}}}}"#
            );
            assert!(
                is_healable_error_body(body.as_bytes()),
                "{ty} should be classified as healable"
            );
        }
    }

    #[test]
    fn test_is_healable_error_body_rejects_unknown_types() {
        let body =
            br#"{"error": {"message": "x", "type": "invalid_request_error", "param": null, "code": null}}"#;
        assert!(!is_healable_error_body(body.as_slice()));
    }

    #[test]
    fn test_is_healable_error_body_rejects_non_json() {
        assert!(!is_healable_error_body(b"upstream connect error: 503"));
        assert!(!is_healable_error_body(b""));
    }

    #[test]
    fn test_is_healable_error_body_rejects_missing_type() {
        let body = br#"{"error": {"message": "x"}}"#;
        assert!(!is_healable_error_body(body.as_slice()));
    }

    #[test]
    fn build_forward_headers_preserves_repeated_request_headers() {
        use axum::http::header::{HeaderName, AUTHORIZATION, CONNECTION, COOKIE, HOST};

        // A client can send the same header name more than once (e.g. multiple
        // Cookie lines, or repeated Accept-Encoding). HeaderMap::iter yields one
        // pair per value and HeaderMap::insert replaces all prior values, so all
        // but the last would be dropped before forwarding to a peer; append keeps
        // every value.
        let mut source = HeaderMap::new();
        source.append(COOKIE, HeaderValue::from_static("a=1"));
        source.append(COOKIE, HeaderValue::from_static("b=2"));
        source.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
        // Hop-by-hop headers must still be stripped.
        source.insert(HOST, HeaderValue::from_static("example.com"));
        source.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        // A stale hop count carried by the incoming request must be replaced by
        // the proxy's own count, not accumulated.
        source.insert(
            HeaderName::from_static("x-llama-mesh-hops"),
            HeaderValue::from_static("2"),
        );

        let forwarded = build_forward_headers(&source, 2, "req-123");

        // Both repeated cookie values survive, in order.
        let cookies: Vec<&str> = forwarded
            .get_all(COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);

        // Single-valued passthrough headers are preserved.
        assert_eq!(forwarded.get(AUTHORIZATION).unwrap(), "Bearer token");

        // Hop-by-hop headers are dropped.
        assert!(forwarded.get(HOST).is_none());
        assert!(forwarded.get(CONNECTION).is_none());

        // The proxy's hop count replaces any incoming value and stays single-valued.
        let hops: Vec<&HeaderValue> = forwarded
            .get_all(HeaderName::from_static("x-llama-mesh-hops"))
            .iter()
            .collect();
        assert_eq!(hops.len(), 1);
        assert_eq!(forwarded.get("x-llama-mesh-hops").unwrap(), "3");

        // The request id is set.
        assert_eq!(forwarded.get("x-request-id").unwrap(), "req-123");
    }

    /// Every hop-by-hop field, plus a custom field the sender marks as
    /// hop-by-hop by naming it in `Connection`.
    fn headers_with_hop_by_hop_fields() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.append(CONNECTION, HeaderValue::from_static("close"));
        headers.append(CONNECTION, HeaderValue::from_static("X-Hop-Scoped , te"));
        headers.insert("x-hop-scoped", HeaderValue::from_static("1"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5, max=100"));
        headers.insert("proxy-authenticate", HeaderValue::from_static("Basic"));
        headers.insert(
            "proxy-authorization",
            HeaderValue::from_static("Basic eA=="),
        );
        headers.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
        headers.insert("te", HeaderValue::from_static("trailers"));
        headers.insert("trailer", HeaderValue::from_static("x-checksum"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers
    }

    fn assert_no_hop_by_hop_fields(headers: &HeaderMap) {
        for name in HOP_BY_HOP_HEADERS.iter().chain(&["x-hop-scoped"]) {
            assert!(headers.get(*name).is_none(), "{name} was relayed");
        }
    }

    #[test]
    fn remove_hop_by_hop_headers_keeps_only_end_to_end_fields() {
        use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE};

        let mut headers = headers_with_hop_by_hop_fields();
        headers.append(COOKIE, HeaderValue::from_static("a=1"));
        headers.append(COOKIE, HeaderValue::from_static("b=2"));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        remove_hop_by_hop_headers(&mut headers);

        assert_no_hop_by_hop_fields(&headers);
        assert_eq!(headers.len(), 4, "unexpected fields: {headers:?}");
        let cookies: Vec<_> = headers.get_all(COOKIE).iter().collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        assert_eq!(headers[AUTHORIZATION], "Bearer token");
        assert_eq!(headers[CONTENT_TYPE], "application/json");
    }

    #[test]
    fn build_forward_headers_drops_hop_by_hop_fields() {
        let mut source = headers_with_hop_by_hop_fields();
        source.insert(HOST, HeaderValue::from_static("example.com"));
        source.insert(CONTENT_LENGTH, HeaderValue::from_static("2"));
        source.insert("x-end-to-end", HeaderValue::from_static("kept"));

        let forwarded = build_forward_headers(&source, 0, "req-1");

        assert_no_hop_by_hop_fields(&forwarded);
        assert!(forwarded.get(HOST).is_none());
        assert!(forwarded.get(CONTENT_LENGTH).is_none());
        assert_eq!(forwarded["x-end-to-end"], "kept");
        assert_eq!(forwarded["x-llama-mesh-hops"], "1");
    }

    #[test]
    fn build_local_upstream_headers_close_streamed_connections_only() {
        let mut source = headers_with_hop_by_hop_fields();
        source.insert(HOST, HeaderValue::from_static("example.com"));
        source.insert(CONTENT_LENGTH, HeaderValue::from_static("2"));
        source.insert("x-end-to-end", HeaderValue::from_static("kept"));

        // The client's own `Connection: close` is not forwarded.
        let sized = build_local_upstream_headers(&source, false);
        assert_no_hop_by_hop_fields(&sized);

        // A streamed request asks the instance to close its connection.
        let streamed = build_local_upstream_headers(&source, true);
        let connection: Vec<_> = streamed.get_all(CONNECTION).iter().collect();
        assert_eq!(connection, ["close"]);
        let mut rest = streamed.clone();
        rest.remove(CONNECTION);
        assert_no_hop_by_hop_fields(&rest);

        for headers in [sized, streamed] {
            assert!(headers.get(HOST).is_none());
            assert!(headers.get(CONTENT_LENGTH).is_none());
            assert_eq!(headers["x-end-to-end"], "kept");
        }
    }

    #[test]
    fn relayed_responses_drop_upstream_connection_fields() {
        use axum::http::header::CONTENT_TYPE;

        // What `llama-server` sends on the last response of a keep-alive
        // connection, alongside the fields a client must still receive.
        let mut upstream = headers_with_hop_by_hop_fields();
        upstream.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        upstream.insert("x-accel-buffering", HeaderValue::from_static("no"));

        let non_streaming = build_non_streaming_body_response(
            StatusCode::OK,
            upstream.clone(),
            Bytes::from_static(b"{}"),
            Arc::new(AtomicU64::new(0)),
        );
        let local_stream = {
            let mut response = axum::http::Response::new("data: {}\n\n");
            *response.headers_mut() = upstream.clone();
            build_streaming_response(
                reqwest::Response::from(response),
                Box::pin(async {}),
                Arc::new(AtomicU64::new(0)),
                None,
                false,
                None,
            )
        };
        let peer_stream = build_streaming_response_from_parts(
            StatusCode::OK,
            upstream,
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"data: {}\n\n"))]),
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            true,
        );

        for response in [non_streaming, local_stream, peer_stream] {
            let headers = response.headers();
            assert_no_hop_by_hop_fields(headers);
            assert_eq!(headers[CONTENT_TYPE], "text/event-stream");
            assert_eq!(headers["x-accel-buffering"], "no");
        }
    }

    #[test]
    fn build_peer_response_headers_preserves_repeated_set_cookie() {
        use axum::http::header::{CONTENT_TYPE, SET_COOKIE};

        // A peer's response can carry the same header more than once — most
        // commonly several Set-Cookie lines. The Noise transport delivers these
        // as separate (name, value) pairs; rebuilding with insert would keep
        // only the last, append keeps them all.
        let raw = vec![
            ("Set-Cookie".to_string(), "a=1".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Set-Cookie".to_string(), "b=2".to_string()),
        ];

        let headers = build_peer_response_headers(&raw);

        let cookies: Vec<&str> = headers
            .get_all(SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
    }

    #[test]
    fn test_ensure_profile_supports_rerank() {
        let p = profile_with_args(&["--rerank"]);
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Rerank, "m").is_ok());
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Text, "m").is_err());
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Embeddings, "m").is_err());
    }

    #[test]
    fn test_ensure_profile_supports_text() {
        let p = profile_with_args(&["-c", "123"]);
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Text, "m").is_ok());
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Embeddings, "m").is_err());
        assert!(ensure_profile_supports_endpoint(&p, EndpointKind::Rerank, "m").is_err());
    }

    #[test]
    fn test_endpoint_kind_detection() {
        assert!(matches!(
            EndpointKind::from_path("/v1/chat/completions"),
            EndpointKind::Text
        ));
        assert!(matches!(
            EndpointKind::from_path("/v1/completions"),
            EndpointKind::Text
        ));
        assert!(matches!(
            EndpointKind::from_path("/v1/embeddings"),
            EndpointKind::Embeddings
        ));
        assert!(matches!(
            EndpointKind::from_path("/v1/rerank"),
            EndpointKind::Rerank
        ));
        assert!(matches!(
            EndpointKind::from_path("/rerank"),
            EndpointKind::Rerank
        ));
        assert!(matches!(
            EndpointKind::from_path("/other"),
            EndpointKind::Other
        ));
    }

    #[test]
    fn test_response_streaming_is_text_endpoint_only() {
        assert!(response_streaming_for_endpoint(EndpointKind::Text, true));
        assert!(!response_streaming_for_endpoint(EndpointKind::Text, false));
        assert!(!response_streaming_for_endpoint(
            EndpointKind::Embeddings,
            true
        ));
        assert!(!response_streaming_for_endpoint(EndpointKind::Rerank, true));
        assert!(!response_streaming_for_endpoint(EndpointKind::Other, true));
    }

    #[test]
    fn test_request_body_strips_stream_from_non_text_endpoints() {
        let body = json!({
            "model": "embedding-model",
            "input": "hello",
            "stream": true
        });
        let original = Bytes::from(serde_json::to_vec(&body).unwrap());

        let sanitized = request_body_for_endpoint(&body, EndpointKind::Embeddings, true, &original);
        let sanitized_json: Value = serde_json::from_slice(&sanitized).unwrap();
        assert!(sanitized_json.get("stream").is_none());

        let text_body = request_body_for_endpoint(&body, EndpointKind::Text, true, &original);
        assert_eq!(text_body, original);
    }

    // Compile-time validation of buffer size bounds
    const _: () = {
        assert!(
            MAX_TOKEN_BUFFER_SIZE == 16 * 1024 * 1024,
            "Buffer should be 16MB"
        );
        assert!(
            MAX_TOKEN_BUFFER_SIZE >= 1024 * 1024,
            "Buffer should be at least 1MB"
        );
        assert!(
            MAX_TOKEN_BUFFER_SIZE <= 64 * 1024 * 1024,
            "Buffer should be at most 64MB"
        );
    };

    #[test]
    fn test_process_line_token_counting() {
        use std::sync::atomic::AtomicU64;

        let counter = AtomicU64::new(0);

        // Test SSE data line with content
        process_line(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Test usage data overrides counter
        process_line(b"data: {\"usage\":{\"completion_tokens\":42}}", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 42);

        // Test [DONE] marker doesn't increment
        let prev = counter.load(Ordering::Relaxed);
        process_line(b"data: [DONE]", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), prev);
    }

    #[test]
    fn test_process_line_counts_reasoning_content() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Thinking models stream reasoning in a separate `reasoning_content`
        // field; it is generated output and must be counted.
        process_line(
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Empty reasoning_content does not increment.
        process_line(
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\"}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_process_line_counts_tool_calls() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // A tool-call delta carries generated output and must be counted.
        process_line(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // An empty tool_calls array does not increment.
        process_line(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[]}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_process_line_malformed_json() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Malformed JSON should fallback to counting as 1 token
        process_line(b"data: {invalid json}", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_process_line_empty_content() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Empty content string should NOT increment
        process_line(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_process_line_missing_delta() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Missing delta field - no increment
        process_line(b"data: {\"choices\":[{}]}", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_process_line_empty_choices() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Empty choices array - no increment
        process_line(b"data: {\"choices\":[]}", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_process_line_non_data_line() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Non-data SSE lines should be ignored
        process_line(b"event: message", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        process_line(b": comment", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        process_line(b"", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_process_line_whitespace_handling() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(0);

        // Leading whitespace before "data:"
        process_line(
            b"  data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}",
            &counter,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_process_line_usage_zero_tokens() {
        use std::sync::atomic::AtomicU64;
        let counter = AtomicU64::new(10);

        // Usage with 0 tokens should set counter to 0
        process_line(b"data: {\"usage\":{\"completion_tokens\":0}}", &counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_endpoint_kind_edge_cases() {
        // Trailing slashes are accepted (starts_with match)
        assert!(matches!(
            EndpointKind::from_path("/v1/chat/completions/"),
            EndpointKind::Text
        ));

        // Query strings are accepted (starts_with match)
        assert!(matches!(
            EndpointKind::from_path("/v1/embeddings?model=test"),
            EndpointKind::Embeddings
        ));

        // Case sensitivity - paths are case-sensitive
        assert!(matches!(
            EndpointKind::from_path("/V1/EMBEDDINGS"),
            EndpointKind::Other
        ));

        // Empty path
        assert!(matches!(EndpointKind::from_path(""), EndpointKind::Other));

        // Partial match not accepted
        assert!(matches!(
            EndpointKind::from_path("/v1/embed"),
            EndpointKind::Other
        ));
    }

    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn auto_cleanup_runs_on_normal_drop() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        {
            let _guard = AutoCleanup::new(Box::pin(async move {
                ran_clone.store(true, Ordering::SeqCst);
            }));
            // scope ends here → guard drops → cleanup spawned on current runtime
        }
        // Yield long enough for the spawned cleanup task to run
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            ran.load(Ordering::SeqCst),
            "cleanup should run when AutoCleanup drops inside a running runtime"
        );
    }

    #[tokio::test]
    async fn auto_cleanup_runs_when_host_task_is_aborted() {
        // Simulates cancellation: a task that holds an AutoCleanup is aborted
        // mid-await. The cleanup must still spawn via Drop.
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();

        let handle = tokio::spawn(async move {
            let _guard = AutoCleanup::new(Box::pin(async move {
                ran_clone.store(true, Ordering::SeqCst);
            }));
            // Park forever so abort cancels mid-await — modelling a client
            // disconnecting while `resp.bytes().await` is still pending.
            std::future::pending::<()>().await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        handle.abort();
        // Give the cleanup task a chance to schedule and run
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            ran.load(Ordering::SeqCst),
            "cleanup should run via Drop even when the owning task is aborted"
        );
    }

    #[tokio::test]
    async fn record_local_body_read_error_counts_node_and_hash_once() {
        let metrics = Arc::new(Metrics::default());
        let hm = metrics.get_hash_metrics("h").await;
        record_local_body_read_error(&Some((metrics.clone(), hm.clone())));
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
        assert_eq!(hm.errors_total.load(Ordering::Relaxed), 1);
        // A None recorder (peer paths) is a no-op.
        record_local_body_read_error(&None);
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
        assert_eq!(hm.errors_total.load(Ordering::Relaxed), 1);
    }

    fn body_stream(
        items: Vec<Result<Bytes, std::io::Error>>,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
        futures::stream::iter(items)
    }

    #[tokio::test]
    async fn cleanup_stream_counts_local_body_read_error_once() {
        use futures::StreamExt;
        let metrics = Arc::new(Metrics::default());
        let hm = metrics.get_hash_metrics("h").await;
        let inner = body_stream(vec![
            Ok(Bytes::from("data: {}\n")),
            Err(std::io::Error::other("mid-stream reset")),
        ]);
        let mut s = CleanupStream::new(
            inner,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            Some((metrics.clone(), hm.clone())),
            false,
        );
        while s.next().await.is_some() {}
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
        assert_eq!(hm.errors_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cleanup_stream_counts_body_read_error_at_most_once() {
        use futures::StreamExt;
        let metrics = Arc::new(Metrics::default());
        let hm = metrics.get_hash_metrics("h").await;
        // Two consecutive errors must still count exactly one (one-shot guard).
        let inner = body_stream(vec![
            Err(std::io::Error::other("first")),
            Err(std::io::Error::other("second")),
        ]);
        let mut s = CleanupStream::new(
            inner,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            Some((metrics.clone(), hm.clone())),
            false,
        );
        while s.next().await.is_some() {}
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
        assert_eq!(hm.errors_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cleanup_stream_does_not_count_without_recorder() {
        use futures::StreamExt;
        // A non-peer stream with no recorder: a body error must not be counted.
        let metrics = Arc::new(Metrics::default());
        let inner = body_stream(vec![
            Ok(Bytes::from("x")),
            Err(std::io::Error::other("boom")),
        ]);
        let mut s = CleanupStream::new(
            inner,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            None,
            false,
        );
        while s.next().await.is_some() {}
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cleanup_stream_counts_peer_body_abort_once() {
        use futures::StreamExt;
        // A peer stream (peer_stream = true, no local recorder) increments the
        // peer-abort counter exactly once on a mid-stream error, and not on a
        // clean stream.
        let before = PEER_STREAM_BODY_ABORTS.load(Ordering::Relaxed);
        let aborting = body_stream(vec![
            Ok(Bytes::from("data: {}\n")),
            Err(std::io::Error::other("peer reset")),
            Err(std::io::Error::other("again")),
        ]);
        let mut s = CleanupStream::new(
            aborting,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            None,
            true,
        );
        while s.next().await.is_some() {}
        assert_eq!(
            PEER_STREAM_BODY_ABORTS.load(Ordering::Relaxed),
            before + 1,
            "a peer body abort counts exactly once"
        );

        let clean = body_stream(vec![Ok(Bytes::from("a")), Ok(Bytes::from("b"))]);
        let mut s = CleanupStream::new(
            clean,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            None,
            true,
        );
        while s.next().await.is_some() {}
        assert_eq!(
            PEER_STREAM_BODY_ABORTS.load(Ordering::Relaxed),
            before + 1,
            "a clean peer stream does not count"
        );
    }

    /// A body error whose cause is only reachable through `source()`.
    #[derive(Debug)]
    struct BodyError(std::io::Error);

    impl std::fmt::Display for BodyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("request or response body error")
        }
    }

    impl std::error::Error for BodyError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[tokio::test]
    async fn cleanup_stream_logs_the_cause_of_a_local_body_read_failure() {
        use futures::StreamExt;
        let capture = crate::test_support::capture_logs(tracing::Level::ERROR);

        let metrics = Arc::new(Metrics::default());
        let hm = metrics.get_hash_metrics("h").await;
        let reset = std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        );
        let inner =
            futures::stream::iter(vec![Ok(Bytes::from("data: {}\n")), Err(BodyError(reset))]);
        let mut s = CleanupStream::new(
            inner,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            Some((metrics, hm)),
            false,
        );
        while s.next().await.is_some() {}

        let logged = capture.contents();
        assert!(
            logged.contains("\"event\":\"local_stream_body_read_failed\""),
            "missing failure event: {logged}"
        );
        assert!(
            logged
                .contains("\"error\":\"request or response body error: connection reset by peer\""),
            "failure logged without its cause: {logged}"
        );
    }

    fn tracked_stream<S>(inner: S) -> (CleanupStream<S, std::io::Error>, Arc<AtomicUsize>)
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    {
        let awaiting = Arc::new(AtomicUsize::new(0));
        let mut stream = CleanupStream::new(
            inner,
            Box::pin(async {}),
            Arc::new(AtomicU64::new(0)),
            None,
            false,
        );
        stream.downstream_wait = Some(DownstreamWait::new(awaiting.clone()));
        (stream, awaiting)
    }

    #[tokio::test]
    async fn cleanup_stream_marks_chunks_awaiting_the_writer() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let (mut s, awaiting) = tracked_stream(rx);
        let waiting = || awaiting.load(Ordering::Relaxed);

        tx.unbounded_send(Ok(Bytes::from("data: {}\n"))).unwrap();
        assert!(s.next().await.is_some());
        assert_eq!(waiting(), 1, "chunk handed to the writer");
        assert!(futures::poll!(s.next()).is_pending());
        assert_eq!(waiting(), 0, "writer asked for more; waiting on upstream");

        tx.unbounded_send(Err(std::io::Error::other("reset")))
            .unwrap();
        assert!(matches!(s.next().await, Some(Err(_))));
        assert_eq!(waiting(), 0, "an error is not handed off");

        tx.unbounded_send(Ok(Bytes::from("data: {}\n"))).unwrap();
        assert!(s.next().await.is_some());
        assert_eq!(waiting(), 1);
        drop(s);
        assert_eq!(waiting(), 0, "dropping the response releases the mark");

        let (mut s, awaiting) = tracked_stream(body_stream(vec![Ok(Bytes::from("x"))]));
        while s.next().await.is_some() {}
        assert_eq!(awaiting.load(Ordering::Relaxed), 0, "end of stream");
    }

    /// Serve one HTTP/1.1 response with a tracked streaming body through hyper,
    /// the same stack that serves API clients.
    async fn serve_tracked_stream<S>(inner: S) -> (std::net::SocketAddr, Arc<AtomicUsize>)
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    {
        let (stream, awaiting) = tracked_stream(inner);
        let body = std::sync::Mutex::new(Some(Body::from_stream(stream)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let service = hyper::service::service_fn(move |_req| {
                let body = body.lock().unwrap().take().expect("a single request");
                async move { Ok::<_, std::convert::Infallible>(Response::new(body)) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                .await;
        });
        (addr, awaiting)
    }

    async fn request_stream(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n")
            .await
            .unwrap();
        client
    }

    /// Wait until `condition` has held continuously for `stable_for`.
    async fn wait_until_stable(condition: impl Fn() -> bool, stable_for: Duration) {
        timeout(Duration::from_secs(10), async {
            let mut since = None;
            loop {
                if !condition() {
                    since = None;
                } else if since.get_or_insert_with(std::time::Instant::now).elapsed() >= stable_for
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition did not hold");
    }

    #[tokio::test]
    async fn stream_blocked_on_a_stalled_client_stays_awaiting_downstream() {
        // An upstream that always has more output, like a generating instance,
        // and a client that never reads the response.
        let inner = futures::stream::repeat_with(|| Ok(Bytes::from(vec![b'x'; 16 * 1024])));
        let (addr, awaiting) = serve_tracked_stream(inner).await;
        let client = request_stream(addr).await;
        // Once socket buffers fill, the server stops polling for more.
        wait_until_stable(
            || awaiting.load(Ordering::Relaxed) == 1,
            Duration::from_millis(300),
        )
        .await;
        // A client that disconnects releases the stream.
        drop(client);
        wait_until_stable(
            || awaiting.load(Ordering::Relaxed) == 0,
            Duration::from_millis(50),
        )
        .await;
    }

    #[tokio::test]
    async fn stream_waiting_on_a_silent_upstream_is_not_awaiting_downstream() {
        use tokio::io::AsyncReadExt;
        // An upstream that sends one chunk and then goes silent, like a hung
        // instance, and a client that reads everything it is sent.
        let inner = futures::stream::iter([Ok(Bytes::from("data: first\n\n"))])
            .chain(futures::stream::pending());
        let (addr, awaiting) = serve_tracked_stream(inner).await;
        let mut client = request_stream(addr).await;
        let mut received = Vec::new();
        timeout(Duration::from_secs(10), async {
            while !received.windows(11).any(|w| w == b"data: first") {
                let mut buf = [0u8; 4096];
                let n = client.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "server closed the stream");
                received.extend_from_slice(&buf[..n]);
            }
        })
        .await
        .expect("chunk not delivered");
        wait_until_stable(
            || awaiting.load(Ordering::Relaxed) == 0,
            Duration::from_millis(300),
        )
        .await;
    }
}
