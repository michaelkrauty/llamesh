# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.1.1] - 2026-04-16

### Fixed

- `in_flight_requests` no longer leaks when a non-streaming request is
  cancelled mid body-read (e.g. client disconnect, client timeout). The
  cleanup future passed into `handle_non_streaming_response` is now wrapped
  in an `AutoCleanup` guard whose `Drop` spawns it unconditionally, mirroring
  the semantics `CleanupStream` already provides for streaming responses.
  Previously, cancellation during `resp.bytes().await` dropped the cleanup
  future unrun, leaving the instance's counter stuck above zero — the
  instance would then never satisfy `in_flight_requests == 0` and would
  refuse to idle-evict, holding VRAM and sysmem indefinitely under any
  workload that experiences cancellations.
- `current_requests` no longer leaks in the forward-to-peer path when the
  local node has no entry for the requested model. The `inc_requests()` call
  at the top of the path had no matching `RequestGuard`, so a cancellation
  between there and the success-path `dec_current_requests()` leaked the
  gauge monotonically. A `RequestGuard` now covers the path; on cancellation
  its `Drop` decrements. On the success path, the decrement is deferred via
  `CleanupStream` until the response body fully streams to the client (or is
  dropped on client disconnect), closing a second window where headers had
  arrived but the body was still in flight.

## [1.1.0] - 2026-03-31

### Added

- **Drain scheduling for GPU time-sharing.** When a request arrives for a model
  that needs VRAM held by another model, the incumbent is now drained (stops
  accepting new requests) and evicted once its in-flight requests complete.
  Previously, the incumbent held the GPU indefinitely as long as it had any
  demand, causing starvation for competing models.

  - New `min_eviction_tenure_secs` config option (default: 15) controls the
    minimum time a model must serve before it can be forcibly drained by a
    competitor. Configurable globally in `model_defaults` or per-profile.
  - Running requests are never interrupted. The drain only prevents new
    dispatches; in-flight requests complete naturally, however long they take.
  - Drains are reversible: if the competing requests are handled elsewhere
    (e.g., by a peer node or timeout), the drain is cancelled and the
    incumbent resumes serving.

- **Instant gossip.** Significant state changes (instance ready, instance
  stopped, cookbook reload) now trigger an immediate gossip round to peers,
  complementing the periodic gossip interval. Peers learn about capacity
  changes within milliseconds instead of waiting for the next gossip tick.

## [1.0.2] - 2026-03-31

### Fixed

- Cookbook hot-reload now wakes queued requests. Previously, when a model's
  resource estimate exceeded `max_vram_mb`, the request was queued with
  `insufficient_resources` and subsequent requests piled up behind it for
  fairness. If a cookbook reload reduced the resource requirements (e.g. by
  lowering context length), the queued requests were never re-evaluated,
  permanently jamming the queue until restart.

## [1.0.1] - 2026-03-26

### Fixed

- Nodes no longer stop serving requests during llama.cpp rebuilds. Builds use
  isolated per-commit directories; the existing binary symlink remains valid
  throughout, so `can_serve()` now depends only on binary existence, not the
  build flag.

## [1.0.0] - 2026-03-19

Initial public release.

### Added

- **OpenAI-compatible API** — `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/models` endpoints
- **Automatic instance management** — on-demand `llama-server` spawn, health monitoring, and idle eviction
- **Multi-node mesh clustering** — zero-config LAN discovery via mDNS or explicit WAN peer configuration
- **Noise Protocol encryption** — authenticated, encrypted inter-node communication
- **Model profiles** — multiple profiles per model with different `llama-server` args (e.g. `fast` vs `quality`)
- **Resource guardrails** — VRAM and system memory tracking to prevent OOM
- **Hot-reload cookbook** — add or modify models without restarting the proxy
- **Auto-build llama.cpp** — clones, builds, smoke tests, and atomically swaps binaries
- **Hugging Face integration** — automatic model downloads via `hf_repo`/`hf_file`
- **SSE streaming** — streaming with backpressure, forwarded verbatim from llama-server
- **Circuit breaker** — automatic peer failure detection and recovery
- **Slot-aware routing** — routes requests to the instance with the most available slots
- **Request queueing** — queues requests when all slots are busy, with configurable timeouts
- **TLS/mTLS support** — optional TLS termination and mutual TLS for clients
- **API key authentication** — configurable API key validation for client requests
- **Prometheus metrics** — `/metrics` endpoint plus JSON snapshots at `/metrics/json`
- **Health probes** — `/healthz` and `/readyz` for load balancer and orchestrator integration
- **Log analysis tooling** — scripts for error triage, request tracing, instance lifecycle, and cluster health
- **Systemd deployment** — install script and service template
