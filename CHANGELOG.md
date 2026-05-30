# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.9.0] - 2026-05-30

### Added

- `GET /metrics` now exposes `proxy_node_active_instances`, a gauge reporting the
  number of llama-server instances this node is currently managing (spawned and
  not yet evicted). It matches the `active_instances` field already shown per
  node in `/cluster/nodes`, but as a scrapeable Prometheus time series it lets
  operators dashboard and alert on instance count and churn directly — for
  example, catching a node that is backing up requests while running zero
  instances (`proxy_queue_pending_tokens > 0` with `proxy_node_active_instances
  == 0`), or watching spawn/evict thrash. The same count is added to the
  `node_resources` object in the `/metrics/json` snapshot. The gauge is
  recomputed from live instance state on every scrape (the metrics handler
  refreshes the node resource snapshot before rendering), so it does not go
  stale. This is additive and backward-compatible: the new
  `node_resources.active_instances` snapshot field is `#[serde(default)]`, so
  metrics snapshots written by older versions still load and historical counters
  are preserved across the upgrade.

## [1.8.3] - 2026-05-30

### Fixed

- Cluster gossip settings are now validated at startup when cluster mode is
  enabled. `gossip_interval_seconds` (a required field with no default) and
  `max_concurrent_gossip` previously deserialized cleanly even when set to `0`,
  but both break the gossip loop at runtime: a `0` interval makes
  `tokio::time::interval` panic (`"interval period must be non-zero"`), killing
  the gossip task so the node silently never advertises itself or polls peers;
  a `0` `max_concurrent_gossip` leaves the gossip semaphore with no permits, so
  every outbound gossip attempt times out and is skipped. `NodeConfig::validate()`
  now rejects either value being `0` while `cluster.enabled` is true, failing
  fast with a message that names the offending setting, instead of surfacing the
  misconfiguration as a runtime panic or a silent mesh dropout. The checks are
  gated on `cluster.enabled` because the gossip loop — and these fields — are
  unused when cluster mode is off. Startup-only, consistent with the other node
  config validations (node config is not hot-reloaded).

## [1.8.2] - 2026-05-30

### Fixed

- `llama_cpp_ports` is now validated at startup. A configured-but-unusable port
  set — a range with `start > end` (an empty `RangeInclusive`), or a
  `llama_cpp_ports` block with no `ports` and no `ranges` — previously loaded
  cleanly but left the port pool empty, so every instance spawn failed at
  runtime with `No available ports` and no indication that the port
  configuration was the cause. `NodeConfig::validate()` now rejects an inverted
  range and a port set that yields no usable ports, failing fast with a clear
  message that identifies the offending setting. Omitting `llama_cpp_ports`
  entirely (OS-assigned ephemeral ports) remains valid.

## [1.8.1] - 2026-05-30

### Fixed

- The cookbook now rejects duplicate `model:profile` identifiers at load time
  instead of silently shadowing one of them. `build_model_index` keys the model
  index by a case-insensitive `"<model>:<profile>"` string in a `HashMap`, so two
  entries that resolve to the same key — a repeated profile `id` within a model,
  or a model `name` reused with an overlapping profile id (matching is
  case-insensitive) — would overwrite each other, leaving one configuration
  unreachable with no error or warning at startup or on hot-reload.
  `Cookbook::validate()` now detects this collision among the enabled
  `(model, profile)` pairs that the index actually builds and fails with an error
  naming both conflicting identifiers. Because hot-reload validates before
  swapping the index, a live edit that introduces a duplicate is rejected and the
  previously loaded cookbook keeps serving.

## [1.8.0] - 2026-05-30

### Added

- The `parsed_model_params` reported per profile by `GET /v1/models` now includes
  `n_slots` — the number of parallel decode slots the running instance
  initialized, i.e. the number of requests it can serve concurrently. This is
  parsed from llama-server's `initializing slots, n_slots = <N>` startup line,
  which (like the `new slot, n_ctx = <N>` line added in 1.7.0) is present at the
  default log verbosity. When a profile does not pin `--parallel`/`-np`,
  llama-server resolves the slot count automatically, so the running instance's
  real concurrency can differ from the proxy's configured
  `max_concurrent_requests_per_instance`; surfacing `n_slots` makes that
  divergence observable through the API. This is observability only — admission
  and slot-aware scheduling are unchanged, and the field is an additive,
  backward-compatible addition to the JSON response (absent as `null` until an
  instance is running).

## [1.7.0] - 2026-05-30

### Added

- The `parsed_model_params` reported per profile by `GET /v1/models` now includes
  `n_ctx` — the effective per-request context window the running instance is
  serving (parsed from llama-server's `new slot, n_ctx = <N>` line). This is the
  context length a client can actually use, which may be smaller than the model's
  trained maximum (`n_ctx_train`) when an instance is launched with a reduced
  context.

### Fixed

- Restore model-parameter parsing against current `llama-server` builds. Recent
  llama.cpp releases (build ~9425) raised the verbosity of the `print_info:`
  model-metadata block above the default log threshold, so it no longer appears
  in a default-verbosity startup log. The startup-log parser recognized only that
  block, so on current builds it extracted nothing and logged `Failed to parse
  model params from startup log` on essentially every instance spawn, while
  `parsed_model_params` in `/v1/models` was always null. The parser now also reads
  the fields that remain available at default verbosity — the effective context
  window (`new slot, n_ctx = <N>`) and the trained context length (from the
  `n_ctx_seq (<x>) < n_ctx_train (<N>)` capacity warning) — so model params are
  observable again without raising llama-server verbosity. Existing `print_info:`
  parsing is retained (its regex already tolerates the new timestamp/severity log
  prefix), so older builds and instances launched with raised verbosity continue
  to yield the full metadata set. Because `n_ctx` is present in every healthy
  startup log regardless of verbosity, the "failed to parse" warning is now
  emitted only when a startup log is genuinely unparseable rather than on every
  spawn.

## [1.6.1] - 2026-05-29

### Fixed

- Peer version-mismatch logging is now edge-triggered instead of repeating on
  every gossip round. `process_gossip_message` runs once per inbound gossip
  (every `gossip_interval_seconds` per peer), and previously emitted a `WARN`
  whenever a peer's version differed from the local node's — so a single
  version-skewed peer produced one identical warning per gossip interval for the
  entire duration of the skew (for example, throughout a rolling upgrade). The
  node now logs a peer's version skew only when that peer's version is first seen
  or changes, and clears the record once the peer's version matches again so a
  later mismatch is reported afresh. Both the `warn` and `reject_*`
  `version_mismatch_action` paths are deduplicated. The dedup state is tracked
  locally and is not part of the gossip wire format, so peer metadata exchanged
  between nodes is unchanged.

## [1.6.0] - 2026-05-29

### Added

- The cluster view (`GET /cluster/nodes`) now reports each node's
  `llama_cpp_version` — the llama.cpp commit of the binary currently serving on
  that node, or `"unknown"` if no build has been recorded. The value rides the
  existing gossip channel, so any single node's `/cluster/nodes` response carries
  the llama.cpp commit for every reachable peer. This makes llama.cpp version
  skew observable cluster-wide from one endpoint, without scraping each node's
  `/metrics` individually. The field mirrors the commit already exposed by the
  `proxy_build_info` metric and the `/metrics/json` snapshot, and it reflects the
  running binary live after an auto-rebuild swaps it. The gossip field is
  additive and deserializes with a default, so a rolling upgrade interoperates:
  peers that predate the field are recorded as `"unknown"`, and older nodes
  ignore the field sent by upgraded peers.

### Fixed

- `/cluster/nodes` now actually surfaces `llama_cpp_version`. The 1.5.0 changelog
  described the llama.cpp commit as "surfaced in ... `/cluster/nodes`", but the
  field was never present on the cluster/gossip node representation — the view
  exposed only the proxy `version`. Every node entry now includes the commit.

## [1.5.0] - 2026-05-29

### Added

- Add the `llama_cpp_version` label to the `proxy_build_info` Prometheus metric,
  bringing the scrape surface to parity with the `/metrics/json` snapshot (which
  already carries both `version` and `llama_cpp_version`). The gauge now renders
  as `proxy_build_info{version="x.y.z",llama_cpp_version="<commit>"} 1`, so
  monitoring can track llama.cpp version skew across a cluster (e.g.
  `count by (llama_cpp_version) (proxy_build_info)`) — previously the llama.cpp
  commit was only observable via `/cluster/nodes` or the JSON snapshot, not the
  scrape layer. The label reflects the running binary and updates live after an
  auto-rebuild swaps it. The value is run through the label-value escaper for
  safety; the existing `version` label is unchanged, so queries such as
  `count by (version) (proxy_build_info)` keep working.

### Fixed

- `llama_cpp_version` (surfaced in `/metrics`, `/metrics/json`, and
  `/cluster/nodes`) now reflects the llama.cpp binary that is actually running.
  The reported commit was previously recorded at git-checkout time — before the
  build and binary swap — so during an auto-rebuild it briefly advertised the
  new commit while the old binary was still serving, and a failed build left it
  pinned to a commit that never ran. The version is now recorded only after the
  binary swap succeeds.

## [1.4.0] - 2026-05-29

### Added

- Expose the llamesh proxy version through the metrics surface. The Prometheus
  endpoint now renders a `proxy_build_info{version="x.y.z"} 1` constant gauge
  (the conventional `*_build_info` pattern), and the JSON metrics snapshot
  (`/metrics/json`) gains a top-level `version` field alongside the existing
  `llama_cpp_version`. This lets monitoring track proxy version rollout and skew
  across a cluster (e.g. `count by (version) (proxy_build_info)`) — previously a
  node could report its peers' versions via `/cluster/nodes` but not surface its
  own to the scrape layer. The new snapshot field is `#[serde(default)]`, so
  metrics files written by older versions still load.

## [1.3.6] - 2026-05-04

### Fixed

- Prevent CLOSE-WAIT socket leak that exhausts file descriptors. Request
  handlers waiting for cluster capacity (`capacity_notify`) now detect client
  disconnect by polling the TCP socket state via `getsockopt(TCP_INFO)` every
  5 seconds. When the socket enters CLOSE-WAIT (client sent FIN), the handler
  exits and all resources are released through existing Drop guards. Also
  unconditionally set hyper's timer and `header_read_timeout` on HTTP
  connections. No timeouts are imposed — requests still wait indefinitely when
  `max_request_duration_ms=0`, as long as the client remains connected.

## [1.3.5] - 2026-05-01

### Fixed

- Plug a slot leak in `get_instance_for_model` that could prevent an instance
  from ever completing its drain. `try_get_or_spawn` increments
  `in_flight_requests` synchronously when it returns Ok, but the caller then
  awaits readiness on the instance through several `.await` points (token
  release, status reads, `ready_signal.notified()`). A future cancelled at
  any of those awaits would leak the slot, leaving `in_flight_requests > 0`
  with no request actively running. Drain blocks on `in_flight_requests == 0`,
  so a leaked slot would pin the instance's VRAM forever. Fix: wrap the slot
  in a new `SlotReleaseGuard` RAII type. Drop spawns the decrement and queue
  notifications; the existing happy-path `Ok(inst)` returns now go through
  `slot_guard.detach()` to transfer responsibility to the router's
  `RequestGuard` / `cleanup_fut` chain. Removes the manual decrements that
  previously only handled the explicit `Failed` paths.

## [1.3.4] - 2026-04-29

### Fixed

- Mesh forwarding now waits for cluster capacity instead of surfacing peer
  queue saturation to clients. When a peer responds with a healable 503
  (`queue_timeout`, `queue_full`, `no_capacity`, `insufficient_resources`,
  `peer_unavailable_retry`, `spawn_failures_exhausted`), the forwarder drops
  the buffered response, awaits a capacity-changed signal, re-selects the best
  peer, and retries. Transport errors against a peer follow the same patient
  retry path. Both the unknown-model forward and the cookbook-known
  remote-routing forward share the new behaviour, and `forward_heal_wait` /
  `forward_transport_retry` events surface in the log each time the loop
  iterates.

## [1.3.3] - 2026-04-28

### Fixed

- Queue priority tokens are now owned by cancellation-safe permits after a
  waiter is dequeued. If the waiting request is cancelled before it acquires
  capacity or requeues, the pending token is removed automatically so stale
  fairness state cannot permanently block fresh dispatch.
- Local response cleanup now releases request and instance accounting before
  awaiting drain or queue state. This preserves graceful drain scheduling while
  avoiding a lock-order inversion that could leave an idle backend appearing
  full after cancelled or completed non-streaming work.
- Non-streaming Noise peer responses now install cleanup before buffering the
  peer body, so client cancellation after peer headers arrive cannot leak node
  request accounting or peer-forward slots.
- Drain cancellation checks no longer hold individual instance locks while
  awaiting queue state.

## [1.3.2] - 2026-04-27

### Fixed

- Treat `stream: true` as a streaming response only on text-generation
  endpoints. Embedding and rerank requests now use non-streaming cleanup and
  strip the unsupported `stream` flag before forwarding, preventing stuck
  request accounting from keeping an idle embedding instance alive.
- Request queues now prune abandoned waiters and wake queued requests when an
  existing ready instance has available capacity before rejecting a fresh
  request as `queue_full`.

## [1.3.1] - 2026-04-24

### Fixed

- Enforce profile `max_instances` after concurrent spawn races so simultaneous
  requests cannot exceed the configured per-profile instance cap.

## [1.3.0] - 2026-04-24

### Added

- Peer forwarding now supports the Noise transport, including streamed response
  bodies, so inter-node request routing uses the same encrypted channel as
  cluster gossip.
- The repository now declares Rust 1.88 as its minimum supported Rust version.

### Changed

- Noise identity keys now use X25519 key material with `noise25519:` public key
  identifiers. Existing `ed25519:` known-peer entries are normalized for
  compatibility.
- Dependency set was refreshed to remove vulnerable or obsolete transitive
  dependencies, including the legacy Hyper 0.14 stack.

### Fixed

- `/cluster/gossip` rejects plain HTTP when Noise is enabled and cluster mTLS
  is not configured, preventing accidental unencrypted peer metadata exchange.
- Peer circuit breakers now distinguish successful and failed peer responses,
  including failures returned through Noise transport.
- Request queue cancellation and timeout paths now remove abandoned waiters and
  pending queue tokens instead of leaving stale queue state behind.
- Auth checks now consistently protect `/v1/models`, `/metrics`, and
  `/metrics/json` when API-key auth is enabled.
- Release-mode port-pool tests no longer depend on ports in the kernel's
  ephemeral range.
- Integration tests now prefer Cargo-provided binaries instead of stale
  `target/release` artifacts.

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
