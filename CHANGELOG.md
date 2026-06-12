# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.12.0] - 2026-06-12

### Added

- Cleartext HTTP/2 (h2c) support: connections beginning with the HTTP/2
  prior-knowledge preface are now served by the HTTP/2 stack. The protocol
  detector already classified them as HTTP/2, but the accept loop handed them
  to the HTTP/1 connection handler, so every h2c client failed with a framing
  error. HTTP/2 connections are kept healthy with ping-based keep-alive (a
  PING after `http.idle_timeout_seconds` without frames) in place of HTTP/1's
  header read timeout. HTTP/1.1, TLS, and Noise handling are unchanged. (#83)

## [1.11.2] - 2026-06-12

### Documentation

- Added `SECURITY.md` (security model summary, private vulnerability
  reporting via GitHub, supported versions, scope) and `CONTRIBUTING.md`
  (build and test instructions, quality gates, pull request and versioning
  conventions, persistence-compatibility rules). README links both.
  Private vulnerability reporting is enabled on the repository. (#81)

## [1.11.1] - 2026-06-12

### Fixed

- The circuit breaker now recovers — and keeps blocking — through the code
  paths production actually exercises. Its recovery transitions lived only in
  an async entry point that no request path calls; the real gate is the
  synchronous peer-candidate check, which never transitions state. As a
  result an opened circuit stopped blocking permanently once its first
  backoff window elapsed (failures while open never restarted the window or
  escalated the backoff, so a still-down peer was retried by every request),
  and never closed again either (successes while open were discarded, leaving
  the circuit reported as open in metrics indefinitely and the backoff
  escalation counter never reset). "Open with backoff elapsed" is now treated
  as the recovery probe phase: candidate filtering stays a read-only
  eligibility check, the probe slot is claimed at the dispatch commit point
  (at most one probe per `cluster.circuit_breaker.half_open_probe_interval_ms`,
  default 1000ms, 0 admits every dispatch — this also implements the
  previously unenforced "limited probes" half-open contract, without letting
  candidate scans burn the probe slot of peers they never dispatch to), and
  only results attributed to a claimed probe drive recovery: a probe success
  advances the circuit toward half-open/closed and a probe failure restarts
  the block window with escalated backoff, while late responses from
  requests sent before the circuit opened leave the state unchanged whether
  they arrive during or after the backoff window. Gossip claims the probe
  slot when available, so an idle cluster recovers a returned peer within a
  gossip round even with no client traffic, and recovery transitions wake
  routing waiters parked on capacity notifications so already-queued
  requests retry immediately. (#79)

## [1.11.0] - 2026-06-12

### Added

- Queue observability on the metrics surface. `proxy_queue_length` reports
  the current number of requests waiting across all model queues (the same
  value as `total_queue_length` in `/cluster/nodes`, previously not
  scrapeable), refreshed at scrape time. `proxy_queue_drops_total{reason}`
  counts requests dropped from queues by reason — `full` (per-model
  `max_queue_size_per_model` reached), `timeout` (queue wait exceeded its
  limit), and `global_limit` (node-wide `max_total_queue_entries` reached) —
  mirroring the existing `queue_drop` log events, which previously were the
  only record of client-visible queue rejections. The drop counters are
  cumulative and persist across restarts; both metrics also appear in
  `/metrics/json`. (#77)

## [1.10.9] - 2026-06-12

### Fixed

- Protocol detection no longer misclassifies connections whose first TCP
  segment carries fewer bytes than the protocol signature needs. Detection
  peeked the socket once and classified whatever had arrived: a fragmented
  TLS ClientHello delivering only its first byte (`0x16`) was classified as
  a Noise handshake, and a lone `P` was classified as HTTP/1.x even when it
  began an HTTP/2 connection preface (`PRI `). Detection now waits for more
  bytes whenever the peeked prefix is a proper prefix of more than one
  signature, still bounded by the existing `protocol_detect_timeout_ms`
  (clients that stall mid-prefix are closed at the timeout, as before).
  Unambiguous prefixes are still classified immediately from the first
  byte. (#75)

### Documentation

- README endpoint table: added `GET /version` and the `/health`,
  `/v1/reranking`, and `/rerank` aliases.

## [1.10.8] - 2026-06-12

### Changed

- Cleared the codebase's standing static-analysis debt; `cargo fmt --check`
  and `cargo clippy` now pass with no findings. `attempt_peer_forward` takes
  the `PeerRequest` it previously assembled internally instead of nine loose
  parameters (the repository's only clippy warning), and a stale formatting
  drift in the connection module was reformatted. No behavior change. (#73)

## [1.10.7] - 2026-06-12

### Fixed

- Concurrent requests can no longer waste a llama-server spawn racing for the
  same capacity slot. The capacity checks (per-profile `max_instances` and
  node-wide `max_instances_per_node`) were enforced again only *after* a new
  process had been spawned, because the instances lock is released during the
  slow spawn itself; two concurrent requests for the same profile could both
  pass the checks, both spawn, and the loser killed its just-spawned process
  at insertion time (`spawn_profile_race_detected`), wasting a process exec
  and partial model load. Spawns now take an in-flight capacity reservation
  while still holding the instances lock, making check-and-reserve atomic:
  the second contender observes the reservation and queues immediately —
  before spawning — and is served by the winner's instance as before.
  Reservations are RAII-released on every failure path (including request
  cancellation) and handed off when the instance is inserted into the map.
  Because queued contenders now depend on the in-flight spawn resolving,
  every resolution wakes them: an abandoned reservation (spawn failure or
  cancellation) notifies all queues, since capacity freed without any
  instance reaching the map and no later event would do it; an instance
  becoming ready wakes queued waiters for its model and profile up to its
  spare concurrency slots; and a failed startup triggers an immediate
  eviction pass (which removes the dead instance and notifies all queues)
  instead of leaving waiters to the next periodic eviction tick; and a
  request that enqueues after being gated by a reservation re-checks the
  gate once it is visible in the queue, closing the window where an abandon
  notification fires before the loser has enqueued. The spawning request's
  own concurrency slot is also now claimed before the instance is shared,
  closing a window where a concurrently attaching request's increment could
  be overwritten and the instance permanently undercounted. The post-spawn
  race detection remains as defense in depth, but is no longer expected to
  fire. (#71)

## [1.10.6] - 2026-06-12

### Fixed

- A rebuild check that finds nothing to change no longer drains running
  instances. When `update_and_build` verified an already-built binary for the
  current commit, it unconditionally re-pointed the symlink and signaled a
  binary swap — even when the symlink already pointed at exactly that binary.
  Since every swap signal drains all running instances (they stop accepting
  new requests and are recycled onto the "new" binary), the scheduled update
  check evicted warm instances on every interval where upstream had no new
  commits, forcing pointless model reloads. The swap (and its drain) is now
  skipped when the live symlink already resolves to the verified binary.
  Builds land in immutable per-commit directories, so an identical resolved
  path means running instances already use exactly that binary; the
  rebuild-after-failed-verification path is unaffected and still signals a
  swap, since it replaces the binary content in place. (#69)

## [1.10.5] - 2026-06-12

### Fixed

- The initial llama.cpp build at startup is now retried with backoff on
  failure. Previously the startup build was attempted exactly once; when a
  node booted before its network or DNS was fully up, a transient `git fetch`
  failure left the node reporting an `unknown` llama.cpp version — and running
  without any update check — until the next scheduled update check, which with
  the common daily interval could be a whole day away. The build is now
  retried over roughly the first twenty minutes after startup (delays of 10s,
  30s, 1m, 2m, 5m, and 10m), which comfortably covers boot-time network
  bring-up. The rebuild lock is released between attempts so the manual
  rebuild endpoint remains usable, and the retry loop stops early if a
  concurrent rebuild (for example one triggered via the API) succeeds in the
  meantime, since redundantly re-running the build would re-signal a binary
  swap and needlessly drain freshly spawned instances. (#67)

## [1.10.4] - 2026-06-11

### Fixed

- Build command failures now include the failure reason. When a command in the
  llama.cpp build pipeline failed (`git clone`/`fetch`/`checkout`, `cmake`
  configure, `cmake --build`), the error logged with the `llama_build_failure`
  event — and surfaced as `last_build_error` in the build status API — contained
  only the command line, with no exit status and none of the process output. The
  actual cause (for example a CMake configure error or a transient `git fetch`
  network failure) was visible only in the parent process's raw stdout/stderr
  stream, requiring manual correlation with supervisor logs to diagnose.
  `run_command` now captures the child's stdout and stderr, streams each line
  through to the parent's stdout/stderr as before, and retains a bounded tail
  (40 lines, long lines truncated) that is included in the error along with the
  exit status. Successful builds behave exactly as before. (#65)

## [1.10.3] - 2026-05-31

### Fixed

- mDNS peer removal no longer evicts the wrong peer. When a peer's mDNS service
  departed, the `ServiceRemoved` handler removed peers with
  `fullname.contains(node_id)` — a **substring** test against the departing
  service's full name (`<node_id>.<service_type>`, e.g.
  `node1._llama-mesh._tcp.local.`). Because the match was a substring rather
  than the exact instance label, a single departure could drop unrelated,
  still-alive peers: a node whose id is a substring of the departing node's id
  (e.g. `node` when `node1` leaves), or — worse — a node whose id appears inside
  the service type carried by **every** full name (e.g. a node named `tcp` or
  `local`), which would then be evicted on every departure. The handler now
  recovers the departing instance's `node_id` by stripping the service-type
  suffix and matches peers exactly (a full-name match is also accepted, covering
  peers discovered without a `node_id` TXT record). Only the peer whose service
  actually departed is removed. This affects clusters using mDNS discovery
  (enabled by default); explicit configured peers were never matched by this
  path and are unaffected.

## [1.10.2] - 2026-05-31

### Security

- API key authentication now compares the presented key against the configured
  keys in **constant time**, matching how the cluster handshake token is already
  verified (`noise::handshake::verify_token`). The previous check used
  `Vec::contains` — a short-circuiting `==` on `String` — whose running time
  depends on how many leading bytes of the candidate match a configured key.
  Comparing a bearer secret this way is a timing side channel (low severity in
  practice, since exploiting it requires measuring response timing remotely,
  across TLS) and was inconsistent with the constant-time token comparison used
  for inter-node authentication. The comparison now runs over every configured
  key without short-circuiting and uses a byte-wise constant-time equality, so a
  partial match is not distinguishable from a full mismatch by response timing.
  No behavior or configuration change: valid keys authorize and invalid keys are
  rejected exactly as before, and no new dependency is introduced.

## [1.10.1] - 2026-05-31

### Fixed

- A request that fails to start an instance locally but is then **successfully**
  served by a peer (the spawn-failure peer-fallback path) is no longer counted as
  an error. When local `get_instance_for_model` returned `SpawnFailuresExhausted`,
  the error counters — the global `proxy_errors_total` and the per-model
  `proxy_hash_errors_total` — were incremented eagerly, *before* the peer fallback
  was attempted. If a peer then served the request and the client received a
  successful response, the request was still recorded as an error. The counters
  are now incremented only when the client ultimately receives an error: a request
  served successfully by a peer is no longer counted, while a peer response with
  an error status (e.g. a `503`), and every error-return path where no peer served
  the request, are still counted — exactly once each. This prevents inflated error
  rates — and the false-positive error-rate alerts they cause — on a node whose
  local spawns are failing while peers continue to serve the model.

## [1.10.0] - 2026-05-30

### Added

- `GET /metrics` now exposes `proxy_node_max_vram_mb` and
  `proxy_node_max_sysmem_mb`, gauges reporting this node's configured VRAM and
  system-memory admission guardrails (`max_vram_mb` / `max_sysmem_mb`). Admission
  spawns a new instance only while `effective + required <= max_*`, so the
  configured maximum is the denominator for any "how close am I to the
  guardrail" calculation. Previously `/metrics` exposed the numerator
  (`proxy_node_effective_vram_mb` / `proxy_node_effective_sysmem_mb`) but not the
  limit, so operators could not compute guardrail utilization
  (`proxy_node_effective_vram_mb / proxy_node_max_vram_mb`) or headroom in PromQL
  without hard-coding the configured value into every query and alert. The limit
  is also distinct from `proxy_node_device_vram_total_mb` (physical device VRAM):
  the guardrail is typically set below physical capacity to leave headroom, so
  the device total cannot substitute for it. These limits were already surfaced
  per node in `/cluster/nodes` (`max_vram`, `max_sysmem`); this adds them to the
  Prometheus scrape surface. The same two values are added to the
  `node_resources` object in the `/metrics/json` snapshot. The gauges are
  refreshed from node config on every scrape (the metrics handler recomputes the
  resource snapshot before rendering). This is additive and
  backward-compatible: the new `node_resources.max_vram_mb` /
  `node_resources.max_sysmem_mb` snapshot fields are `#[serde(default)]`, so
  metrics snapshots written by older versions still load and historical counters
  are preserved across the upgrade.

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
