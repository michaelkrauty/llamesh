//! Socket-free coverage of complete peer-response classification. Transport and
//! route-loop integration are intentionally exercised by separate fixtures.
use super::*;
use crate::build_manager::BuildManager;
use crate::circuit_breaker::{CircuitState, DispatchDecision};
use crate::config::{Cookbook, NodeConfig};
use crate::node_state::PeerState;
use std::collections::HashSet;

async fn state(backoff_ms: u64) -> NodeState {
    let mut config: NodeConfig = serde_yaml::from_str(
        r#"
node_id: local
listen_addr: 127.0.0.1:0
max_vram_mb: 1024
max_sysmem_mb: 1024
default_model: fixture
metrics_path: /dev/null
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 1
  max_instances_per_model: 1
llama_cpp:
  repo_url: https://example.invalid/fixture
  branch: fixture
  auto_update_interval_seconds: 0
  enabled: false
cluster:
  enabled: true
  gossip_interval_seconds: 5
  discovery: {mdns: false}
  circuit_breaker:
    failure_threshold: 2
    success_threshold: 2
    half_open_probe_interval_ms: 0
http:
  request_body_limit_bytes: 1024
  idle_timeout_seconds: 60
"#,
    )
    .unwrap();
    config.cluster.circuit_breaker.open_duration_base_ms = backoff_ms;
    config.cluster.circuit_breaker.open_duration_max_ms = backoff_ms;
    let build_manager = BuildManager::new(config.llama_cpp.clone());
    NodeState::new(config, Cookbook { models: vec![] }, build_manager)
        .await
        .unwrap()
}

fn response(status: u16, body: &str) -> BufferedPeerResponse {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "5".parse().unwrap());
    headers.append("x-fixture", "first".parse().unwrap());
    headers.append("x-fixture", "second".parse().unwrap());
    headers.insert(
        "x-opaque",
        axum::http::HeaderValue::from_bytes(b"\x80").unwrap(),
    );
    BufferedPeerResponse {
        status: StatusCode::from_u16(status).unwrap(),
        headers,
        body: Bytes::copy_from_slice(body.as_bytes()),
    }
}

fn assert_preserved(actual: BufferedPeerResponse, expected: &BufferedPeerResponse) {
    assert_eq!(actual.status, expected.status);
    assert_eq!(actual.headers, expected.headers);
    assert_eq!(actual.body, expected.body);
}

#[tokio::test]
async fn complete_peer_response_classifier_requires_exact_drain_attribution() {
    // Each row starts from a clean breaker so earlier rows cannot obscure the
    // classification or its ordinary status accounting.
    let rows = [
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
            "drain",
        ),
        (
            503,
            r#" { "error": {"type":"draining", "origin_node_id":"peer"}, "extra":1 } "#,
            "drain",
        ),
        (503, r#"{"error":{"type":"draining"}}"#, "surface"),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":"other"}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":"Peer"}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":"peer "}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":null}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":7}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":["peer"]}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":{"id":"peer"}}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":true}}"#,
            "surface",
        ),
        (
            503,
            r#"{"error":{"type":"Draining","origin_node_id":"peer"}}"#,
            "surface",
        ),
        (503, r#"{"error":{"origin_node_id":"peer"}}"#, "surface"),
        (
            503,
            r#"{"error":{"type":503,"origin_node_id":"peer"}}"#,
            "surface",
        ),
        (503, r#"{"error":null}"#, "surface"),
        (
            503,
            r#"{"error":[{"type":"draining","origin_node_id":"peer"}]}"#,
            "surface",
        ),
        (
            503,
            r#"{"type":"draining","origin_node_id":"peer"}"#,
            "surface",
        ),
        (503, "not JSON\0", "surface"),
        (
            503,
            r#"{"error":{"type":"draining","origin_node_id":"peer"}} trailing"#,
            "surface",
        ),
        (
            500,
            r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
            "surface",
        ),
        (
            200,
            r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
            "surface",
        ),
        (
            400,
            r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
            "surface",
        ),
        (503, r#"{"error":{"type":"backend_failure"}}"#, "surface"),
        (503, r#"{"error":{"type":"queue_full"}}"#, "heal"),
        (500, r#"{"error":{"type":"no_capacity"}}"#, "heal"),
        (400, r#"{"error":{"type":"queue_full"}}"#, "surface"),
    ];
    for (status, body, expected) in rows {
        let state = state(60_000).await;
        let original = response(status, body);
        let outcome =
            classify_buffered_peer_response(&state, "peer", false, response(status, body)).await;
        match (expected, outcome) {
            ("drain", ForwardOutcome::Draining) => {}
            ("surface", ForwardOutcome::Surface(actual)) => assert_preserved(actual, &original),
            (
                "heal",
                ForwardOutcome::Heal {
                    reason,
                    response: actual,
                },
            ) => {
                let json: Value = serde_json::from_str(body).unwrap();
                assert_eq!(reason, json["error"]["type"].as_str().unwrap());
                assert_preserved(actual, &original);
            }
            _ => panic!(
                "unexpected classification for status {status}, body {body}, expected {expected}"
            ),
        }
    }
}

async fn insert_peer(state: &NodeState, id: &str, queue: usize) {
    let peer: PeerState = serde_json::from_value(serde_json::json!({
        "node_id": id, "address": "http://fixture.invalid", "version": "fixture",
        "last_seen": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        "supported_models": ["remote:default"], "loaded_models": ["remote:default"],
        "active_instances": 1, "max_instances": 8, "current_requests": 0,
        "available_vram": 1024, "available_sysmem": 1024,
        "total_queue_length": queue, "ready": true
    })).unwrap();
    state.peers.write().await.insert(id.to_owned(), peer);
}

#[tokio::test]
async fn complete_drain_responses_leave_breaker_and_peer_table_unchanged() {
    for body in [
        r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
        r#"{"error":{"type":"draining","origin_node_id":"foreign"}}"#,
        r#"{"error":{"type":"draining"}}"#,
        r#"{"error":{"type":"draining","origin_node_id":false}}"#,
    ] {
        for stage in ["near-open", "open", "half-open"] {
            let state = state(if stage == "open" { 60_000 } else { 0 }).await;
            insert_peer(&state, "peer", 7).await;
            state.circuit_breaker.record_failure("peer", false).await;
            let probe_ticket = if stage == "near-open" {
                false
            } else {
                state.circuit_breaker.record_failure("peer", false).await;
                if stage == "half-open" {
                    assert_eq!(
                        state.circuit_breaker.try_claim_dispatch_sync("peer"),
                        DispatchDecision::Admit { probe_ticket: true }
                    );
                    assert!(state.circuit_breaker.record_success("peer", true).await);
                    assert_eq!(
                        state.circuit_breaker.get_state("peer").await,
                        CircuitState::HalfOpen
                    );
                }
                // An open circuit may still receive a late pre-open response.
                stage == "half-open"
            };
            let before = state.circuit_breaker.get_all_states().await;
            let peers_before = serde_json::to_value(&*state.peers.read().await).unwrap();
            for _ in 0..3 {
                let outcome = classify_buffered_peer_response(
                    &state,
                    "peer",
                    probe_ticket,
                    response(503, body),
                )
                .await;
                if body.contains("\"origin_node_id\":\"peer\"") {
                    assert!(matches!(outcome, ForwardOutcome::Draining));
                } else {
                    let ForwardOutcome::Surface(actual) = outcome else {
                        panic!("unattributable drain must surface")
                    };
                    assert_preserved(actual, &response(503, body));
                }
            }
            assert_eq!(
                state.circuit_breaker.get_all_states().await,
                before,
                "{stage}, {body}"
            );
            assert_eq!(
                serde_json::to_value(&*state.peers.read().await).unwrap(),
                peers_before
            );
            if stage == "near-open" {
                // No hidden success reset: the next ordinary failure must open.
                state.circuit_breaker.record_failure("peer", false).await;
                assert_eq!(
                    state.circuit_breaker.get_state("peer").await,
                    CircuitState::Open
                );
            }
        }
    }
}

#[tokio::test]
async fn classified_drains_exclude_each_peer_once_despite_fresh_ready_advertisements() {
    let state = state(60_000).await;
    insert_peer(&state, "first", 0).await;
    insert_peer(&state, "second", 1).await;
    insert_peer(&state, "last", 2).await;
    let mut excluded = HashSet::new();
    for expected in ["first", "second"] {
        let selected = state
            .find_peer_for_model("remote", &excluded)
            .await
            .unwrap();
        assert_eq!(selected.node_id, expected);
        let body =
            serde_json::json!({"error":{"type":"draining", "origin_node_id":selected.node_id}})
                .to_string();
        assert!(matches!(
            classify_buffered_peer_response(&state, &selected.node_id, false, response(503, &body))
                .await,
            ForwardOutcome::Draining
        ));
        excluded.insert(selected.node_id.clone());
        // Simulate a fresh, still-ready advertisement after the rejection.
        insert_peer(&state, &selected.node_id, 0).await;
    }
    let last = state
        .find_peer_for_model("remote", &excluded)
        .await
        .unwrap();
    assert_eq!(last.node_id, "last");
    let body = "{\"choices\":[]}";
    let ForwardOutcome::Surface(actual) =
        classify_buffered_peer_response(&state, &last.node_id, false, response(200, body)).await
    else {
        panic!("success must surface")
    };
    assert_preserved(actual, &response(200, body));
    excluded.insert(last.node_id);
    assert!(state
        .find_peer_for_model("remote", &excluded)
        .await
        .is_none());
    assert_eq!(excluded.len(), 3);
}

#[tokio::test]
async fn drain_lookalikes_and_other_errors_keep_normal_status_accounting() {
    for (status, body) in [
        (
            500,
            r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
        ),
        (503, r#"{"error":{"type":"backend_failure"}}"#),
        (503, r#"{"error":{"type":"queue_full"}}"#),
        (503, r#"{"error":{"type":null,"origin_node_id":"peer"}}"#),
        (503, "malformed JSON"),
    ] {
        let state = state(60_000).await;
        state.circuit_breaker.record_failure("peer", false).await;
        assert_eq!(
            state.circuit_breaker.get_state("peer").await,
            CircuitState::Closed
        );
        classify_buffered_peer_response(&state, "peer", false, response(status, body)).await;
        assert_eq!(
            state.circuit_breaker.get_state("peer").await,
            CircuitState::Open,
            "{status}: {body}"
        );
    }
    // Drain-shaped successful content is still a success and clears previous
    // consecutive failures; the body cannot confer drain semantics on a 200.
    let state = state(60_000).await;
    state.circuit_breaker.record_failure("peer", false).await;
    let body = r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#;
    let ForwardOutcome::Surface(actual) =
        classify_buffered_peer_response(&state, "peer", false, response(200, body)).await
    else {
        panic!("200 must surface")
    };
    assert_preserved(actual, &response(200, body));
    state.circuit_breaker.record_failure("peer", false).await;
    assert_eq!(
        state.circuit_breaker.get_state("peer").await,
        CircuitState::Closed
    );
    state.circuit_breaker.record_failure("peer", false).await;
    assert_eq!(
        state.circuit_breaker.get_state("peer").await,
        CircuitState::Open
    );
}

fn assert_retryable_timeout(error: &AppError) {
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.error.type_, "queue_timeout");
    assert_eq!(error.error.origin_node_id, None);
    assert_eq!(error.headers.as_ref().unwrap()[RETRY_AFTER], "5");
}

#[tokio::test(start_paused = true)]
async fn immediate_retry_preflight_keeps_original_deadline_and_counts_timeout_once() {
    let state = state(60_000).await;
    let hash_metrics = state.metrics.get_hash_metrics("fixture").await;
    let started = tokio::time::Instant::now();
    let deadline = Some(started + Duration::from_millis(50));
    for elapsed in [0, 20, 20] {
        tokio::time::advance(Duration::from_millis(elapsed)).await;
        routing_preflight(&state, &None, deadline, "remote", Some(&hash_metrics)).unwrap();
    }
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 0);
    assert_eq!(hash_metrics.errors_total.load(Ordering::Relaxed), 0);
    tokio::time::advance(Duration::from_millis(10)).await;
    let error =
        routing_preflight(&state, &None, deadline, "remote", Some(&hash_metrics)).unwrap_err();
    assert_retryable_timeout(&error);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_millis(50)
    );
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(hash_metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn preflight_unlimited_allows_retries_but_local_drain_precedes_expired_deadline() {
    let state = state(60_000).await;
    let hash_metrics = state.metrics.get_hash_metrics("fixture").await;
    routing_preflight(&state, &None, None, "remote", Some(&hash_metrics)).unwrap();
    let expired = Some(tokio::time::Instant::now());
    tokio::time::advance(Duration::from_secs(86400)).await;
    routing_preflight(&state, &None, None, "remote", Some(&hash_metrics)).unwrap();
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 0);
    state.draining.store(true, Ordering::Relaxed);
    let error =
        routing_preflight(&state, &None, expired, "remote", Some(&hash_metrics)).unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.error.type_, "draining");
    assert_eq!(error.error.origin_node_id.as_deref(), Some("local"));
    assert_eq!(error.headers.as_ref().unwrap()[RETRY_AFTER], "5");
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(hash_metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn classified_drain_exhaustion_wait_uses_remaining_original_queue_budget() {
    let state = state(60_000).await;
    let hash_metrics = state.metrics.get_hash_metrics("fixture").await;
    insert_peer(&state, "first", 0).await;
    insert_peer(&state, "second", 1).await;
    let started = tokio::time::Instant::now();
    let deadline = Some(started + Duration::from_millis(100));
    let mut excluded = HashSet::new();
    for expected in ["first", "second"] {
        routing_preflight(&state, &None, deadline, "remote", Some(&hash_metrics)).unwrap();
        let peer = state
            .find_peer_for_model("remote", &excluded)
            .await
            .unwrap();
        assert_eq!(peer.node_id, expected);
        let body = serde_json::json!({"error":{"type":"draining", "origin_node_id":peer.node_id}})
            .to_string();
        assert!(matches!(
            classify_buffered_peer_response(&state, &peer.node_id, false, response(503, &body))
                .await,
            ForwardOutcome::Draining
        ));
        excluded.insert(peer.node_id);
        tokio::time::advance(Duration::from_millis(30)).await;
    }
    // Subscribe before inspecting capacity, just as the production caller does.
    let capacity = state.capacity_notify.notified();
    assert!(state
        .find_peer_for_model("remote", &excluded)
        .await
        .is_none());
    let wait = wait_for_capacity_or_disconnect(
        capacity,
        &None,
        deadline,
        &state,
        "remote",
        Some(&hash_metrics),
    );
    tokio::pin!(wait);
    assert!(futures::poll!(&mut wait).is_pending());
    tokio::time::advance(Duration::from_millis(39)).await;
    assert!(futures::poll!(&mut wait).is_pending());
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 0);
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = wait.await.unwrap_err();
    assert_retryable_timeout(&error);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_millis(100)
    );
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(hash_metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 1);
    assert_eq!(excluded.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn unlimited_capacity_wait_can_be_canceled_without_timeout_accounting() {
    let state = Arc::new(state(60_000).await);
    let hash_metrics = state.metrics.get_hash_metrics("fixture").await;
    let task_state = state.clone();
    let task_metrics = hash_metrics.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let capacity = task_state.capacity_notify.notified();
        ready_tx.send(()).unwrap();
        wait_for_capacity_or_disconnect(
            capacity,
            &None,
            None,
            &task_state,
            "remote",
            Some(&task_metrics),
        )
        .await
    });
    ready_rx.await.unwrap();
    tokio::time::advance(Duration::from_secs(86400)).await;
    assert!(!task.is_finished());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 0);
    assert_eq!(hash_metrics.errors_total.load(Ordering::Relaxed), 0);
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 0);
}

fn http_peer_response(body: &str, complete: bool, status: u16) -> PeerResponse {
    let expected = response(status, body);
    let chunks = if complete {
        // Split JSON across chunks so the HTTP adapter must buffer it first.
        let midpoint = body.len() / 2;
        vec![
            Ok(Bytes::copy_from_slice(&body.as_bytes()[..midpoint])),
            Ok(Bytes::copy_from_slice(&body.as_bytes()[midpoint..])),
        ]
    } else {
        // Even a whole drain-looking JSON object is inconclusive until EOF.
        vec![
            Ok(Bytes::copy_from_slice(body.as_bytes())),
            Err(std::io::Error::other("fixture body interrupted before EOF")),
        ]
    };
    let mut response = Response::new(reqwest::Body::wrap_stream(futures::stream::iter(chunks)));
    *response.status_mut() = expected.status;
    *response.headers_mut() = expected.headers;
    PeerResponse::Http(reqwest::Response::from(response))
}

#[tokio::test]
async fn http_adapter_buffers_requested_stream_errors_before_classifying_drain() {
    for stream_requested in [false, true] {
        for (body, direct) in [
            (
                r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#,
                true,
            ),
            (r#" { "error": { "type": "draining" } } "#, false),
            (
                r#"{"error":{"type":"draining","origin_node_id":"foreign"}}"#,
                false,
            ),
        ] {
            let state = state(60_000).await;
            state.circuit_breaker.record_failure("peer", false).await;
            let result = classify_peer_response(
                &state,
                "peer",
                false,
                http_peer_response(body, true, 503),
                stream_requested,
            )
            .await;
            if direct {
                assert!(matches!(result, ForwardOutcome::Draining));
            } else {
                let ForwardOutcome::Surface(actual) = result else {
                    panic!("legacy/foreign drain must surface")
                };
                assert_preserved(actual, &response(503, body));
            }
            assert_eq!(
                state.circuit_breaker.get_state("peer").await,
                CircuitState::Closed
            );
            state.circuit_breaker.record_failure("peer", false).await;
            assert_eq!(
                state.circuit_breaker.get_state("peer").await,
                CircuitState::Open
            );
        }
    }
}

#[tokio::test]
async fn http_adapter_incomplete_drain_body_is_transport_failure_counted_once() {
    for stream_requested in [false, true] {
        let state = state(60_000).await;
        let body = r#"{"error":{"type":"draining","origin_node_id":"peer"}}"#;
        let result = classify_peer_response(
            &state,
            "peer",
            false,
            http_peer_response(body, false, 503),
            stream_requested,
        )
        .await;
        let ForwardOutcome::Transport(error) = result else {
            panic!("incomplete body cannot prove draining")
        };
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.error.type_, "peer_body_read_failed");
        assert!(error
            .error
            .message
            .contains("fixture body interrupted before EOF"));
        // Threshold is two: a duplicate result would already have opened;
        // absent failure accounting would remain closed after the next failure.
        assert_eq!(
            state.circuit_breaker.get_state("peer").await,
            CircuitState::Closed
        );
        state.circuit_breaker.record_failure("peer", false).await;
        assert_eq!(
            state.circuit_breaker.get_state("peer").await,
            CircuitState::Open
        );
    }
}

#[tokio::test]
async fn http_adapter_successful_stream_keeps_body_lazy_and_preserves_shape() {
    let state = state(0).await;
    state.circuit_breaker.record_failure("peer", false).await;
    state.circuit_breaker.record_failure("peer", false).await;
    assert_eq!(
        state.circuit_breaker.try_claim_dispatch_sync("peer"),
        DispatchDecision::Admit { probe_ticket: true }
    );
    let polls = Arc::new(AtomicUsize::new(0));
    let body_polls = polls.clone();
    let payload = "data: {\"choices\":[]}\n\ndata: [DONE]\n\n";
    let expected = response(200, payload);
    let body = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
        payload.as_bytes(),
    ))])
    .inspect(move |_| {
        body_polls.fetch_add(1, Ordering::Relaxed);
    });
    let mut http = Response::new(reqwest::Body::wrap_stream(body));
    *http.status_mut() = expected.status;
    *http.headers_mut() = expected.headers.clone();
    let result = classify_peer_response(
        &state,
        "peer",
        true,
        PeerResponse::Http(reqwest::Response::from(http)),
        true,
    )
    .await;
    let ForwardOutcome::StreamingSurface(PeerResponse::Http(actual)) = result else {
        panic!("requested successful stream must stay streaming")
    };
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    assert_eq!(actual.status(), expected.status);
    assert_eq!(actual.headers(), &expected.headers);
    assert_eq!(actual.bytes().await.unwrap(), expected.body);
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    // Streaming headers cannot recover an open circuit, even for a probe.
    assert_eq!(
        state.circuit_breaker.get_state("peer").await,
        CircuitState::Open
    );
}

// Keep the existing healability regressions on the same production parser
// and classifier used for forwarded responses.
pub(super) async fn is_healable_error_body(body: &[u8]) -> bool {
    let state = state(60_000).await;
    matches!(
        classify_buffered_peer_response(
            &state,
            "peer",
            false,
            BufferedPeerResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                headers: HeaderMap::new(),
                body: Bytes::copy_from_slice(body),
            },
        )
        .await,
        ForwardOutcome::Heal { .. }
    )
}
