# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.2.0] - 2026-04-24

### Added

- Cookbook profiles now support `enabled: false` independently from their
  parent model, allowing one profile to be hidden, unroutable, and omitted
  from cluster advertisements while sibling profiles remain available.
- Optional per-profile `estimated_vram_mb` and `estimated_sysmem_mb` cookbook
  fields provide cold-start admission hints until runtime memory sampling learns
  measured peaks for a launch-args hash.
- Node resource metrics now expose llamesh-tracked VRAM/sysmem, device-wide VRAM,
  external VRAM, effective admission-control usage, and GPU telemetry
  availability in Prometheus and JSON metrics.

### Fixed

- VRAM admission now accounts for device-wide GPU memory usage reported by NVML,
  including memory used by processes outside llamesh. External VRAM is treated
  as non-evictable usage, so spawn and eviction decisions no longer assume the
  configured `max_vram_mb` budget is entirely available to llamesh-managed
  `llama-server` instances.
- Cluster gossip now advertises VRAM availability after external GPU consumers
  are included, preventing peers from routing cold starts to nodes whose real
  GPU headroom is already consumed.

## [1.1.3] - 2026-04-21

### Fixed

- Cluster load balancing no longer oscillates between nodes on a ~gossip-period
  cycle. Concurrent bursts for a model enabled on multiple nodes were all
  piling onto a single node until gossip caught up, then all piling onto the
  other — leaving one GPU idle while the other was saturated. Root cause: the
  routing score in `select_best_node` consumed `peer.current_requests` from
  the last gossip snapshot (stale by up to `gossip_interval_seconds`), so
  every decision inside a burst saw the same "idle peer" view and committed
  en masse to the same node. Additionally, `self.current_requests` is an
  edge counter that includes requests this node has forwarded away (which
  don't use local GPU), causing self to look busier than it really is.

  Fix: added a local per-peer `pending_forwards` counter that increments
  instantly when a request is forwarded and decrements when it completes (via
  `PeerForwardGuard`'s `Drop` so cancellation/error paths are covered). The
  scoring function now consumes an `effective_current_requests` that is
  `peer.current_requests + pending_forwards_to_peer` for peers, and
  `self.current_requests - total_pending_forwards` for self. Bursts now
  diversify across nodes within a single dispatch, and self is scored by
  actual local processing rather than edge traffic.

## [1.1.2] - 2026-04-19

### Fixed

- Cookbook hot-reload now drains instances that no longer match the cookbook.
  Previously, disabling a model (or removing a profile, or changing its
  `llama_server_args`) left the running `llama-server` process alive
  indefinitely, continuing to hold VRAM/sysmem even though the local node no
  longer advertised the model. Reload now marks such instances `draining`
  (graceful: in-flight requests finish, no new dispatches) and the standard
  eviction path terminates them as soon as they go idle. Emits a new
  `instance_orphaned_by_reload` event with `reason` field
  (`model_or_profile_missing_or_disabled` | `args_changed`). As
  defense-in-depth, the idle-eviction loop now also drains immediately when
  a profile no longer resolves or its args_hash has drifted, replacing the
  previous 600-second fallback timeout.
- Cookbook watcher now survives rename-based edits. `sed -i`, most editors'
  save-atomic, and IDE autosave all replace the file via `rename(tmp,
  cookbook.yaml)`, which silently invalidated the inotify watch because the
  watch was bound to the original file's inode. A single rename-based save
  would fire one final event, then the watcher stopped receiving anything —
  subsequent cookbook changes required a restart. The watcher now watches
  the parent directory and filters events by the cookbook's filename, so
  both in-place and rename-based writes keep firing reload events
  indefinitely.

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
