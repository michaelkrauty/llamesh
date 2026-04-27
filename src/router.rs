use crate::config::Profile;
use crate::errors::AppError;
use crate::instance::Instance;
use crate::node_state::{NodeError, NodeState};
use axum::{
    body::Body,
    extract::State,
    http::{header::RETRY_AFTER, HeaderMap, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
    response::Json,
};
use bytes::Bytes;
use futures::{future::BoxFuture, Stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
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

async fn record_peer_status(state: &NodeState, peer_id: &str, status: StatusCode) {
    if peer_status_is_failure(status) {
        state.circuit_breaker.record_failure(peer_id).await;
    } else {
        state.circuit_breaker.record_success(peer_id).await;
    }
}

fn build_forward_headers(source: &HeaderMap, current_hops: usize, request_id: &str) -> HeaderMap {
    let mut forward_headers = HeaderMap::new();
    for (name, value) in source.iter() {
        let name_lower = name.as_str().to_lowercase();
        if matches!(
            name_lower.as_str(),
            "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "upgrade"
        ) {
            continue;
        }
        forward_headers.insert(name.clone(), value.clone());
    }
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

async fn send_peer_request(
    state: &NodeState,
    request: PeerRequest<'_>,
) -> Result<PeerResponse, AppError> {
    if let Some(noise_context) = state.noise_context.as_ref() {
        let timeout = (request.timeout_ms > 0).then(|| Duration::from_millis(request.timeout_ms));
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
                e.to_string(),
                "peer_forward_failed",
            )
        })?;

        let status = StatusCode::from_u16(response.head.status).map_err(|e| {
            AppError::internal_server_error(format!("Invalid peer response status: {e}"))
        })?;
        let mut response_headers = HeaderMap::new();
        for (name, value) in &response.head.headers {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                response_headers.insert(name, value);
            }
        }

        Ok(PeerResponse::Noise {
            status,
            headers: response_headers,
            body: Box::new(response.into_body_stream()),
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
                    format!("Failed to forward to peer {}: {e}", request.peer_id),
                    "peer_forward_failed",
                )
            })
    }
}

pub async fn list_models(
    State(state): State<Arc<NodeState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    crate::security::check_api_key_auth(state.config.auth.as_ref(), &headers)
        .map_err(AppError::authentication_error)?;

    let mut data = Vec::new();
    let current_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut seen_ids = HashSet::new();
    let owned_by = state.config.node_id.clone();

    let cookbook = state.cookbook.read().await;
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
            let entry_id = id.clone();

            // Look up metrics by args_hash
            let tps = if let Some(args_hash) = state.get_args_hash_for_key(&id).await {
                let hash_metrics = state.metrics.get_hash_metrics(&args_hash).await;
                hash_metrics.tokens_per_second()
            } else {
                0.0
            };

            // Get parsed model params from running instance (if any)
            let parsed_params = state
                .get_parsed_params_for_model(&model.name, &profile.id)
                .await;

            let metadata = json!({
                "model": model.name.as_str(),
                "model_description": model.description.as_deref(),
                "profile_id": profile.id.as_str(),
                "profile_description": profile.description.as_deref(),
                "idle_timeout_seconds": profile.idle_timeout_seconds,
                "estimated_tokens_per_second": tps,
                "parsed_model_params": parsed_params,
            });

            let model_obj = json!({
                "id": id,
                "object": "model",
                "created": current_time,
                "owned_by": owned_by.as_str(),
                "permission": [],
                "root": model.name.as_str(),
                "parent": null,
                "metadata": metadata.clone(),
            });
            data.push(model_obj);
            seen_ids.insert(entry_id);
        }
    }
    drop(cookbook);

    if state.config.cluster.enabled {
        let peers = state.peers.read().await;
        for peer in peers.values() {
            for entry in &peer.supported_models {
                if seen_ids.insert(entry.clone()) {
                    data.push(json!({
                        "id": entry,
                        "object": "model",
                        "created": current_time,
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

    Ok(Json(json!({
        "object": "list",
        "data": data
    })))
}

pub async fn route_request(
    State(state): State<Arc<NodeState>>,
    req: Request<Body>,
) -> Result<impl IntoResponse, AppError> {
    let (parts, body) = req.into_parts();
    let path_only = parts.uri.path().to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let endpoint_kind = EndpointKind::from_path(&path_only);

    if state.draining.load(Ordering::Relaxed) {
        return Err(AppError::service_unavailable(
            "Node is draining",
            "draining",
        ));
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

    let model_req = json_body.get("model").and_then(|v| v.as_str());
    let model_to_use = model_req
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.config.default_model.clone());

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

        // Try local model resolution first
        let local_resolution = state.resolve_model(&model_to_use).await;

        // If local resolution fails, check if any peer supports this model
        if local_resolution.is_none() {
            if let Some(peer) = state.find_peer_for_model(&model_to_use).await {
                // Forward to the peer that supports this model
                info!(
                    event = "forward_unknown_model",
                    model = %model_to_use,
                    peer = %peer.node_id,
                    "Model not in local cookbook, forwarding to peer"
                );

                state.metrics.inc_requests();
                // RequestGuard ensures current_requests is decremented if this
                // handler is cancelled before we can install the body-level
                // cleanup below. Without it, a client disconnect during
                // `client_req.send().await` would leak the gauge monotonically.
                let guard = RequestGuard::new(state.clone());
                // Track this forward locally so concurrent routing decisions
                // see the peer's load before its next gossip tick.
                let peer_forward_guard = state.track_peer_forward(&peer.node_id).await;

                let forward_headers =
                    build_forward_headers(&parts.headers, current_hops, &request_id);
                let timeout_ms = state.config.model_defaults.max_request_duration_ms;

                match send_peer_request(
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
                )
                .await
                {
                    Ok(resp) => {
                        let status = match &resp {
                            PeerResponse::Http(resp) => resp.status(),
                            PeerResponse::Noise { status, .. } => *status,
                        };
                        record_peer_status(&state, &peer.node_id, status).await;

                        // Defer the dec_current_requests decrement until the
                        // response body finishes streaming to the client (or is
                        // dropped on client disconnect). CleanupStream::Drop
                        // spawns this future on end-of-stream or on drop,
                        // closing the cancellation window between headers
                        // arriving and the body fully flushing.
                        let cleanup_fut: BoxFuture<'static, ()> = {
                            let state = state.clone();
                            let mut peer_forward_guard = peer_forward_guard;
                            Box::pin(async move {
                                state.metrics.dec_current_requests();
                                peer_forward_guard.release();
                            })
                        };

                        // Hand off responsibility for dec from `guard` to
                        // `cleanup_fut` now that we're committed to returning
                        // the streaming response.
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
                    Err(e) => {
                        state.circuit_breaker.record_failure(&peer.node_id).await;
                        // guard Drop handles dec_current_requests on return.
                        state.metrics.inc_errors();
                        error!(peer = %peer.node_id, error = ?e, "Failed to forward to peer");
                        return Err(e);
                    }
                }
            }

            // No local resolution and no peer found
            return Err(AppError::model_not_found(format!(
                "Model '{model_to_use}' not found in local cookbook or any peer"
            )));
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
                .select_best_node(&model_name, &profile, prefer_local)
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
            if state.draining.load(Ordering::Relaxed) {
                state.metrics.inc_errors();
                hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                return Err(AppError::service_unavailable(
                    "Node is shutting down",
                    "draining",
                ));
            }

            // Model exists locally but no node can serve right now
            // Wait for capacity change and retry
            info!(
                event = "waiting_for_node",
                model = %model_name,
                profile = %profile.id,
                "No node available, waiting for cluster capacity"
            );
            state.capacity_notify.notified().await;
            // Loop back to retry select_best_node
        };

        if best_node.node_id == state.config.node_id {
            // Local routing
            match state
                .get_instance_for_model(&model_name, &profile, true)
                .await
            {
                Ok(instance_lock) => {
                    guard.set_instance(instance_lock.clone());

                    let (target_host, target_port) = {
                        let mut inst = instance_lock.write().await;
                        inst.last_activity = std::time::Instant::now();
                        (inst.host.clone(), inst.port)
                    };

                    let url = format!("http://{target_host}:{target_port}{path_and_query}");

                    info!("Forwarding locally to {}", url);

                    let mut headers = parts.headers.clone();
                    headers.remove(axum::http::header::HOST);
                    headers.remove(axum::http::header::CONTENT_LENGTH);
                    headers.remove(axum::http::header::TRANSFER_ENCODING);

                    // Inject authorization if the profile defines an API key
                    if let Some(api_key) = profile.get_api_key() {
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

                    match client_req.send().await {
                        Ok(resp) => {
                            let tokens_counter = Arc::new(AtomicU64::new(0));
                            let tokens_for_cleanup = tokens_counter.clone();

                            let cleanup_fut: BoxFuture<'static, ()> = {
                                let instance_lock = instance_lock.clone();
                                let state = state.clone();
                                let hash_metrics = hash_metrics.clone();

                                Box::pin(async move {
                                    let (model_name, profile_id, became_idle) = {
                                        let mut inst = instance_lock.write().await;
                                        inst.in_flight_requests =
                                            inst.in_flight_requests.saturating_sub(1);
                                        let idle = inst.in_flight_requests == 0;

                                        // ── Drain scheduling ────────────────────────────
                                        // Check if this instance should be drained for a
                                        // queued competitor that needs its VRAM.
                                        if !inst.draining.load(Ordering::Relaxed) {
                                            let dominated = state
                                                .has_queued_competitors_needing_eviction(
                                                    &inst.model_name,
                                                )
                                                .await;
                                            if dominated {
                                                let tenure_expired = inst
                                                    .evictable_after
                                                    .lock()
                                                    .map(|t| std::time::Instant::now() >= t)
                                                    .unwrap_or(false);
                                                let own_queue_empty = idle
                                                    && !state
                                                        .has_pending_for_model(
                                                            &inst.model_name,
                                                            &inst.profile_id,
                                                        )
                                                        .await;

                                                if tenure_expired || own_queue_empty {
                                                    inst.draining.store(true, Ordering::Relaxed);
                                                    info!(
                                                        event = "drain_triggered",
                                                        model = %inst.model_name,
                                                        profile = %inst.profile_id,
                                                        instance_id = %inst.id,
                                                        reason = if tenure_expired { "tenure_expired" } else { "queue_empty" },
                                                        "Instance marked draining for competing model"
                                                    );
                                                }
                                            }
                                        }

                                        (
                                            inst.model_name.clone(),
                                            inst.profile_id.clone(),
                                            idle,
                                        )
                                    };
                                    state.metrics.dec_current_requests();

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
                                    hash_metrics
                                        .total_latency_ms
                                        .fetch_add(duration, Ordering::Relaxed);
                                    hash_metrics.observe_latency(duration);
                                    hash_metrics.tokens_generated_total.fetch_add(
                                        tokens_for_cleanup.load(Ordering::Relaxed),
                                        Ordering::Relaxed,
                                    );
                                })
                            };

                            // Transfer responsibility to cleanup_fut
                            guard.complete();

                            Ok(if response_streaming {
                                build_streaming_response(resp, cleanup_fut, tokens_counter)
                            } else {
                                handle_non_streaming_response(resp, cleanup_fut, tokens_counter)
                                    .await
                            })
                        }
                        Err(e) => {
                            error!("Upstream error: {}", e);
                            state.metrics.inc_errors();
                            // Guard handles dec_current_requests and in_flight cleanup
                            hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);

                            Err(AppError::new(
                                StatusCode::BAD_GATEWAY,
                                format!("Upstream error: {e}"),
                                "upstream_error",
                            ))
                        }
                    }
                }
                Err(e) => {
                    // Guard handles dec_current_requests
                    state.metrics.inc_errors();
                    hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);

                    // If local spawning failed repeatedly, try forwarding to a peer
                    // before returning an error to the client.
                    if matches!(e, NodeError::SpawnFailuresExhausted) {
                        if let Some(peer) = state.find_peer_for_model(&model_to_use).await {
                            info!(
                                event = "local_spawn_failed_peer_fallback",
                                model = %model_name,
                                peer = %peer.node_id,
                                "Local spawn failures exhausted, falling back to peer"
                            );

                            let peer_forward_guard = state.track_peer_forward(&peer.node_id).await;
                            let forward_headers =
                                build_forward_headers(&parts.headers, current_hops, &request_id);

                            match send_peer_request(
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
                            )
                            .await
                            {
                                Ok(resp) => {
                                    let status = match &resp {
                                        PeerResponse::Http(resp) => resp.status(),
                                        PeerResponse::Noise { status, .. } => *status,
                                    };
                                    record_peer_status(&state, &peer.node_id, status).await;

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
                                            hash_metrics
                                                .total_latency_ms
                                                .fetch_add(duration, Ordering::Relaxed);
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
                                Err(peer_err) => {
                                    // peer_forward_guard drops on fall-through,
                                    // releasing the counter.
                                    state.circuit_breaker.record_failure(&peer.node_id).await;
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

                    match e {
                        NodeError::SpawnFailuresExhausted => Err(AppError::service_unavailable(
                            "All nodes failed to spawn instance for this model",
                            "spawn_failures_exhausted",
                        )
                        .with_header(RETRY_AFTER, HeaderValue::from_static("30"))),
                        NodeError::QueueFull => Err(AppError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Queue full",
                            "queue_full",
                        )
                        .with_header(RETRY_AFTER, HeaderValue::from_static("5"))),
                        NodeError::QueueTimeout => Err(AppError::service_unavailable(
                            "Queue timeout",
                            "queue_timeout",
                        )
                        .with_header(RETRY_AFTER, HeaderValue::from_static("5"))),
                        NodeError::InsufficientResources => Err(AppError::service_unavailable(
                            "Insufficient resources",
                            "no_capacity",
                        )
                        .with_header(RETRY_AFTER, HeaderValue::from_static("30"))),
                        NodeError::Other(inner) => {
                            let msg = inner.to_string();
                            if msg.contains("timed out") {
                                Err(AppError::new(
                                    StatusCode::REQUEST_TIMEOUT,
                                    "Request timed out",
                                    "request_timeout",
                                ))
                            } else {
                                Err(AppError::internal_server_error(msg))
                            }
                        }
                        _ => Err(AppError::internal_server_error(e.to_string())),
                    }
                }
            }
        } else {
            // Remote routing
            info!("Forwarding remotely to peer {}", best_node.node_id);
            // Track this forward locally so subsequent routing decisions reflect
            // it instantly rather than waiting up to `gossip_interval_seconds`
            // for the peer's next gossip tick. Guard is released in the cleanup
            // future (success) or on function return (error).
            let peer_forward_guard = state.track_peer_forward(&best_node.node_id).await;
            let forward_headers =
                build_forward_headers(&parts.headers, current_hops, &request_id);

            match send_peer_request(
                &state,
                PeerRequest {
                    peer_id: &best_node.node_id,
                    peer_address: &best_node.address,
                    method: parts.method.clone(),
                    path_and_query: &path_and_query,
                    headers: forward_headers,
                    body: forward_body.clone(),
                    timeout_ms,
                },
            )
            .await
            {
                Ok(resp) => {
                    let status = match &resp {
                        PeerResponse::Http(resp) => resp.status(),
                        PeerResponse::Noise { status, .. } => *status,
                    };
                    record_peer_status(&state, &best_node.node_id, status).await;

                    let tokens_counter = Arc::new(AtomicU64::new(0));
                    let tokens_for_cleanup = tokens_counter.clone();

                    let cleanup_fut: BoxFuture<'static, ()> = {
                        let state = state.clone();
                        let hash_metrics = hash_metrics.clone();
                        // Move the peer-forward guard into the cleanup future so
                        // it's decremented exactly when the response completes.
                        let mut peer_forward_guard = peer_forward_guard;
                        Box::pin(async move {
                            state.metrics.dec_current_requests();
                            peer_forward_guard.release();
                            let duration = start_time.elapsed().as_millis() as u64;
                            hash_metrics
                                .total_latency_ms
                                .fetch_add(duration, Ordering::Relaxed);
                            hash_metrics.tokens_generated_total.fetch_add(
                                tokens_for_cleanup.load(Ordering::Relaxed),
                                Ordering::Relaxed,
                            );
                        })
                    };

                    // Transfer responsibility to cleanup_fut
                    guard.complete();

                    Ok(peer_response_to_client(
                        resp,
                        response_streaming,
                        cleanup_fut,
                        tokens_counter,
                    )
                    .await)
                }
                Err(e) => {
                    // peer_forward_guard drops here, releasing the counter.
                    // Record failure for circuit breaker
                    state
                        .circuit_breaker
                        .record_failure(&best_node.node_id)
                        .await;

                    warn!(
                        peer = %best_node.node_id,
                        address = %best_node.address,
                        path = %path_and_query,
                        error = ?e,
                        "Peer forwarding failed, waiting for capacity to retry"
                    );

                    // Wait for cluster capacity change and retry
                    // This allows the circuit breaker to open, other peers to become available,
                    // or the failed peer to recover
                    state.capacity_notify.notified().await;

                    // Re-select best node (might be different now due to circuit breaker)
                    let retry_node = loop {
                        if let Some(node) = state
                            .select_best_node(&model_name, &profile, prefer_local)
                            .await
                        {
                            break node;
                        }
                        // No node available, wait again
                        info!(
                            event = "retry_waiting",
                            model = %model_name,
                            "Retry waiting for node after peer failure"
                        );
                        state.capacity_notify.notified().await;
                    };

                    // Note: Full automatic retry would require restructuring route_request()
                    // to use a top-level loop. For now, we wait for capacity and return
                    // an error - the client can retry and get routed to the new best node.
                    // This is "soft" never-reject: we don't fail immediately, but eventually
                    // ask the client to retry after waiting for cluster state to stabilize.
                    state.metrics.inc_errors();
                    hash_metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    info!(
                        original_peer = %best_node.node_id,
                        retry_peer = %retry_node.node_id,
                        "Peer forwarding failed after wait, suggesting client retry"
                    );
                    Err(AppError::service_unavailable(
                        format!("Peer {} temporarily unavailable, please retry", best_node.node_id),
                        "peer_unavailable_retry",
                    ))
                }
            }
        }
    }
    .instrument(span)
    .await
}

async fn handle_non_streaming_response(
    resp: reqwest::Response,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
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
            error!("Failed to read upstream response body: {}", e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Failed to read upstream response"))
                .unwrap_or_else(|_| Response::new(Body::from("Internal error")));
        }
    };

    // Parse for usage
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

    let mut response = Response::new(Body::from(bytes));
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

fn build_streaming_response(
    resp: reqwest::Response,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
) -> Response<Body> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let stream = CleanupStream::new(resp.bytes_stream(), cleanup, tokens_generated);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn build_streaming_response_from_parts<S, E>(
    status: StatusCode,
    headers: HeaderMap,
    stream: S,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
) -> Response<Body>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    let stream = CleanupStream::new(stream, cleanup, tokens_generated);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

async fn handle_non_streaming_body(
    status: StatusCode,
    headers: HeaderMap,
    bytes: Bytes,
    cleanup: BoxFuture<'static, ()>,
    tokens_generated: Arc<AtomicU64>,
) -> Response<Body> {
    let _cleanup_guard = AutoCleanup::new(cleanup);

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

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
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
                build_streaming_response(resp, cleanup, tokens_generated)
            } else {
                handle_non_streaming_response(resp, cleanup, tokens_generated).await
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
                )
            } else {
                match body
                    .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())))
                    .try_fold(Vec::new(), |mut acc, chunk| async move {
                        acc.extend_from_slice(&chunk);
                        Ok(acc)
                    })
                    .await
                {
                    Ok(bytes) => {
                        handle_non_streaming_body(
                            status,
                            headers,
                            Bytes::from(bytes),
                            cleanup,
                            tokens_generated,
                        )
                        .await
                    }
                    Err(e) => {
                        error!("Failed to read peer response body: {}", e);
                        let _cleanup_guard = AutoCleanup::new(cleanup);
                        Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from("Failed to read peer response"))
                            .unwrap_or_else(|_| Response::new(Body::from("Internal error")))
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
}

impl<S, E> CleanupStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    fn new(inner: S, cleanup: BoxFuture<'static, ()>, tokens_generated: Arc<AtomicU64>) -> Self {
        Self {
            inner: Box::pin(inner),
            cleanup: Some(cleanup),
            tokens_generated,
            buffer: Vec::new(),
            token_counting_disabled: false,
            cleanup_executed: false,
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
                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                            if !content.is_empty() {
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
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
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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
}
