# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.19.3] - 2026-06-21

### Changed

- Cleared the remaining `cargo clippy --all-targets` warnings so the full-target
  lint is a clean gate (previously only `--bin llamesh` was clean): a test
  helper's complex boxed-future return type is now a named type alias, and the
  non-empty `SCHEDULED_UPDATE_RETRY_DELAYS` invariant is enforced by a
  compile-time assertion next to the constant (stronger than the former runtime
  test, which clippy flagged as an always-true `const` check). No runtime
  behavior change.

## [1.19.2] - 2026-06-20

### Fixed

- `upstream_read_timeout_ms` now also bounds the response body of a *streaming*
  request forwarded to a cluster peer over the Noise transport, extending the
  in-flight-slot-leak protection it already provides for local instances
  (addresses #138). A node that streams a forwarded request from a peer which
  then goes silent mid-response previously held its in-flight slot forever (the
  peer body read had no inactivity bound), which could hang a graceful drain
  just like the original local-stall bug. When `upstream_read_timeout_ms > 0`,
  the Noise peer response-body read is now bounded by the inactivity timeout,
  applied per read and reset on every byte received, so it bounds connection
  silence (a slow-but-active stream, even within one large frame, is never
  aborted); a stalled peer aborts the stream and releases the slot through the
  normal cleanup path. The gossip Noise body drain is likewise bounded by the
  existing gossip timeout, and a gossip 2xx response whose body cannot be
  drained is now recorded as a failed exchange rather than a success. Scoped to
  streaming forwards: a non-streaming peer
  forward buffers the body and retries on read failure, so an inactivity abort
  there would re-forward rather than release the slot; bounding that path (and
  the reqwest cluster transport) is left to a follow-up. Disabled by default
  (`0`), so behavior is unchanged unless the timeout is enabled.

## [1.19.1] - 2026-06-20

### Fixed

- Local upstream body-read failures are now counted in the error metrics
  (`proxy_errors_total` and the per-hash `errors_total`), for both the
  non-streaming and streaming paths. Previously only `send()` failures were
  counted on the local path: a body-read failure — a connection reset mid-body,
  or (with `upstream_read_timeout_ms` enabled, or when the wedge-detector stops a
  hung instance) a read-inactivity error — returned a 502 / aborted the client
  stream without incrementing any error counter, so it was invisible to
  `proxy_errors_total` and per-hash error metrics. The count fires exactly once
  per failed request (guarded against per-chunk double counting on the streaming
  path) and only on the local path; the cluster peer-forward paths already
  account for their failures separately and are unchanged.

## [1.19.0] - 2026-06-20

### Added

- New opt-in `wedge_detector` watchdog that stops a local `llama-server`
  instance which holds a request slot while flat at ~0% CPU and ~0% GPU. A
  request whose instance accepts it and then goes silent keeps
  `in_flight_requests > 0` forever: the instance is never idle-evictable
  (eviction requires no in-flight requests) and the held slot keeps
  `current_requests > 0`, so a graceful drain hangs until the process is killed
  by hand. When enabled, a background task samples each Ready, slot-holding
  instance's per-process CPU (procfs) and GPU (NVML `sm_util`) every
  `sample_interval_ms`; an instance flat on both while holding a slot
  continuously for `window_ms` (default 10 minutes) is stopped, which makes the
  stuck upstream call error out and releases the slot through the normal
  request-cleanup path — the same effect as an operator killing the PID.
  Unlike a wall-clock timeout, the activity signal distinguishes a slow-but-
  active generation (CPU or GPU busy) from a true hang (both flat), so it never
  aborts a legitimately long or non-streaming request; a GPU-bound decode
  reports `sm_util > 0` and is never flagged. A streaming response stays active
  as long as its client keeps consuming (each chunk drives more generation);
  only a stream whose client consumes nothing for the full window is treated as
  wedged. On a GPU node an absent
  per-process GPU reading is trusted as "idle" only after per-process
  utilization has been observed working on that node at least once and the
  current NVML sweep is not degraded; a node with no NVML GPU is treated as
  "GPU activity unknown" (so a non-NVIDIA GPU is never wrongly flagged) unless
  `wedge_detector.cpu_only` is set to assert the node has no GPU. Disabled by
  default — the per-process GPU signal is hardware-dependent — and complements
  `upstream_read_timeout_ms` rather than replacing it. Kills are counted by the
  new `proxy_wedged_instances_killed_total` metric and logged as
  `instance_wedged`.

## [1.18.7] - 2026-06-20

### Added

- New `upstream_read_timeout_ms` option: an inactivity (read) timeout for
  outbound requests to a local `llama-server`. A stalled upstream — one that
  accepts a request and then goes silent (connection held open, no further
  bytes) — otherwise holds its in-flight slot forever, so `current_requests`
  never returns to zero and a graceful drain can hang until the process is
  killed by hand. With this set, the request is aborted after the configured
  silence and the slot is released. It is a per-chunk timer (reqwest resets it
  on every response chunk), so an actively-streaming generation is unaffected.
  Disabled by default (`0`): the timer also covers the wait for the first body
  bytes, and a `stream: false` response only arrives once generation finishes,
  so a non-zero value shorter than a request's full generation time would abort
  a legitimate request — enable it only for deployments whose requests stream
  tokens regularly. The cluster peer-forward paths share the exposure and are
  tracked separately.

## [1.18.6] - 2026-06-19

### Fixed

- Noise HTTP frame parsing now locates the header/body boundary on the raw
  bytes instead of a UTF-8-lossy view. `parse_http_request` and
  `parse_http_response_head` found the `\r\n\r\n` boundary in a
  `String::from_utf8_lossy(data)` and then applied that offset to slice the body
  out of the original `data`. The two offset spaces diverge if any byte before
  the boundary is invalid UTF-8 — `from_utf8_lossy` expands each to a 3-byte
  U+FFFD — so a non-ASCII header byte would shift the body slice and corrupt the
  forwarded request/response body. The peers that build these frames only emit
  ASCII headers, so this was not reachable in practice, but the parser is now
  robust to its byte input regardless. Parsing of valid ASCII headers is
  unchanged.

## [1.18.5] - 2026-06-19

### Documentation

- Corrected two reference values in the example config and `SPEC.md` that did
  not match the code. Under "Local paths (defaults shown)", `llama_cpp.binary_path`
  carried a stray `build/` segment (`./llama.cpp/build/bin/llama-server`) where
  the actual default is `./llama.cpp/bin/llama-server`; the sibling `repo_path`
  and `build_path` already matched their defaults, so deleting the line yielded a
  different path than the comment promised. And the idle-eviction "fallback
  timeout" prose in `SPEC.md` said 600 seconds where the default is 300 (the same
  document's example config already noted "default 300"). The
  example-validation test now asserts the documented local paths equal their
  `default_*()` functions, so they cannot silently drift again.

## [1.18.4] - 2026-06-19

### Fixed

- Repeated headers are no longer dropped when relaying between clients and
  cluster peers. Several places rebuilt a `HeaderMap` from a header list (or
  another map) with `HeaderMap::insert` while iterating; because `iter` yields a
  separate entry per value and `insert` replaces all prior values for a name, a
  header sent more than once lost every value but the last. This affected the
  outgoing request copy (`build_forward_headers`), the encrypted-peer request
  receiver that rebuilds the Axum request, and the encrypted-peer response relay,
  so a request with several `Cookie` lines — or a response with several
  `Set-Cookie` lines — was silently collapsed to a single value end to end. All
  three now use `append`, which preserves every value; the proxy-set
  `x-llama-mesh-hops` and `x-request-id` request headers stay single-valued and
  authoritative. The reqwest forward path already cloned the upstream map and was
  unaffected. Single-valued headers (the common case, including `Authorization`)
  are unchanged.

## [1.18.3] - 2026-06-19

### Fixed

- Config validation now rejects a zero `max_instances_per_node` at startup. The
  field caps how many `llama-server` instances a node will spawn locally, and a
  zero cap means it can never spawn one — fatal in any mode. A standalone node
  can then serve nothing; a clustered node still self-routes a request for a
  model in its own cookbook (local selection is gated on whether the node can
  serve locally, not on the cap, and the zero-capacity skip only applies to
  peers) and times out on the unsatisfiable cap instead of forwarding to a peer.
  It deserialized cleanly (the field has a default), degrading the node silently
  at runtime, so it is now caught at startup with a clear message, like the
  existing gossip, port, circuit-breaker, and HTTP-limit checks. A
  forwarding-only node instead keeps an empty local cookbook (so it never
  self-routes) and a non-zero cap that is never reached.

## [1.18.2] - 2026-06-15

### Fixed

- Config validation now rejects degenerate per-peer circuit breaker settings at
  startup (when cluster mode and the breaker are both enabled). A zero
  `failure_threshold` or `success_threshold` would open or close a peer's
  circuit without the intended number of failures or recovery probes; a zero
  `open_duration_base_ms` or `open_duration_max_ms` collapses the open-state
  backoff to nothing, so a failing peer is re-probed on every request with no
  spacing; and a base backoff greater than the max is immediately capped, so the
  exponential backoff never grows. Each deserialized cleanly but degraded peer
  failure handling at runtime, so they are now caught with a clear message, like
  the existing gossip, port, and HTTP-limit checks. `half_open_probe_interval_ms`
  is exempt, since `0` is a documented value that disables probe gating. As
  defense in depth, the backoff multiplication is now saturating, so a large
  (but valid) base can no longer overflow and wrap to a near-zero backoff.

## [1.18.1] - 2026-06-15

### Fixed

- Upstream and peer response-body read failures now return the standard OpenAI
  error envelope instead of a bare plaintext body. When a local `llama-server`
  or a cluster peer accepted a non-streaming request and sent `200` headers but
  then failed to deliver the full body (e.g. the backend process was killed
  mid-response, or the connection reset before the body finished), the proxy
  replied with `502` and a raw string (`"Failed to read upstream response"` /
  `"Failed to read peer response"`) carrying no `Content-Type: application/json`
  and no `error` object. OpenAI-compatible clients deserialize the body as JSON,
  so they saw a parse error instead of the real failure. Both paths now return
  `{"error":{"message","type":"upstream_error","param","code"}}` like every
  other error, unified behind a single `AppError::upstream_error()` constructor
  (which the existing local upstream-error path now also uses).

## [1.18.0] - 2026-06-15

### Fixed

- Per-hash total latency is now persisted directly in the metrics snapshot
  instead of being reconstructed from the rounded `avg_latency_ms` on restart.
  It was the only counter in the snapshot stored as a derived value: on load the
  proxy rebuilt it as `avg_latency_ms * requests_total`, and because that product
  is not exact in floating point, the `proxy_hash_total_latency_ms` counter could
  shift by a millisecond across a restart — and with it `tokens_per_second()`,
  which feeds `estimated_tokens_per_second` on `/v1/models`. The snapshot now
  stores the raw counter, so it survives a restart exactly; `avg_latency_ms` is
  retained as a read-only convenience derived from it.

### Added

- The `/metrics/json` per-hash entries now include `total_latency_ms`, the raw
  latency counter behind `avg_latency_ms` and the Prometheus
  `proxy_hash_total_latency_ms` series. Snapshots written by older versions that
  lack the field still load — the value is reconstructed from `avg_latency_ms`
  for backward compatibility.

## [1.17.0] - 2026-06-15

### Added

- Process start time is now exposed for uptime and restart detection. The
  `/metrics` endpoint emits the canonical, un-prefixed Prometheus gauge
  `process_start_time_seconds` (the Unix timestamp when the process started),
  which standard tooling queries by exact name — Grafana uptime panels and
  alerting rules compute `time() - process_start_time_seconds`, and a change in
  its value flags a restart. It deliberately keeps the conventional unprefixed
  name rather than the `proxy_` prefix used by this proxy's own metrics, so
  existing dashboards work unmodified. The `/metrics/json` snapshot carries the
  same value as `started_at_unix`, matching the field already reported by
  `/cluster/nodes`. The new snapshot field is `#[serde(default)]`, so an
  in-place upgrade from an older `node-metrics.json` keeps loading.

## [1.16.4] - 2026-06-14

### Fixed

- `503` responses returned while a node is draining for shutdown now carry a
  `Retry-After` header, like the other retryable `503`s (queue full/timeout, no
  capacity, spawn failures). Previously a draining node rejected new requests
  with a bare `503`, giving OpenAI-compatible clients no backoff hint during a
  rolling restart. Draining is transient — the node is restarting and should
  accept requests again shortly — so it now returns `Retry-After: 5`, matching
  the transient-condition tier. The four draining rejection sites are unified
  behind a single `AppError::node_draining()` constructor, so they share one
  message, type, and retry hint instead of drifting (two had previously diverged
  to "Node is shutting down" versus "Node is draining"). The error-model section
  of the spec now documents the per-condition `Retry-After` values.

## [1.16.3] - 2026-06-14

### Fixed

- Bound the two outbound HTTP requests that lacked a timeout, so an
  unresponsive endpoint that accepts a connection but never replies can no
  longer stall the proxy. The instance readiness health probe now applies a
  per-request timeout: previously a `llama-server` that accepted the connection
  but never answered `/health` could block the readiness loop indefinitely, past
  the overall startup deadline, holding the spawn slot. The plaintext (non-Noise)
  cluster gossip request now carries the same 10-second timeout the Noise gossip
  path already used, so a hung peer cannot tie up a gossip task and its
  concurrency permit. The Noise gossip path and both peer-forwarding paths were
  already bounded; these were the only unbounded outbound calls.

## [1.16.2] - 2026-06-14

### Fixed

- Per-model latency is no longer double-counted for locally-served requests.
  The local serving path added each request's duration to `total_latency_ms`
  twice — once directly and once inside `observe_latency` — so the counter grew
  at twice the true rate. This halved the derived `estimated_tokens_per_second`
  reported in `/v1/models` metadata and `/cluster/nodes` model stats, and
  inflated the `proxy_hash_total_latency_ms` Prometheus counter. Because the
  same throughput estimate feeds the cluster scheduler's performance bonus
  (`min(tps, 200) * 5`), the understated value also subtly skewed node
  selection, weighting throughput about half as much as intended and turning
  the 200 tok/s bonus clamp into an effective 400 tok/s clamp. Latency now
  accumulates exactly once on every path. Forwarded (proxied-to-peer) requests
  continue to accumulate `total_latency_ms` without feeding the local p95
  sample window, since their wall-clock time includes the peer round-trip and
  the peer's own queueing and generation. The bug had been present since the
  first release.

## [1.16.1] - 2026-06-14

### Fixed

- Config validation now rejects zero values for the `http` server limits and
  timeouts (`request_body_limit_bytes`, `idle_timeout_seconds`,
  `body_read_timeout_ms`, `protocol_detect_timeout_ms`) at startup, with a clear
  message for each. Previously a zero deserialized cleanly but broke request
  handling at runtime: a `request_body_limit_bytes` of 0 rejects every request
  carrying a body, a zero millisecond timeout elapses immediately (so body reads
  and protocol detection always time out — tokio treats a zero timeout as
  already expired, not disabled), and a zero `idle_timeout_seconds` tears down
  connections the moment they go idle. This matches the existing startup
  validation for zero gossip and port settings.

## [1.16.0] - 2026-06-14

### Added

- The `GET /version` endpoint now reports the llama.cpp commit the node is
  running alongside the proxy version:
  `{ "version": "1.16.0", "llama_cpp_version": "dd4623a74" }`. Previously the
  llama.cpp version was only available via the `proxy_build_info` Prometheus
  metric and `/cluster/nodes`; surfacing it on `/version` makes the obvious
  endpoint answer the obvious question, which matters because the llama.cpp
  version changes as the node auto-updates. Because that commit is an
  operational build detail otherwise exposed only by those authenticated
  endpoints, `llama_cpp_version` is included only when the caller is authorized
  (API key auth disabled, or an accepted key presented) and is omitted for
  unauthenticated callers; the always-public `version` field is unchanged.
  When present, `llama_cpp_version` is `"unknown"` until the startup build
  records the running commit.

## [1.15.6] - 2026-06-14

### Fixed

- A request whose `model` field is present but is not a non-empty string (for
  example `null`, a number, boolean, object, or empty string) is now rejected
  with a 400 `invalid_request_error` instead of silently falling back to
  `default_model`. Previously only an *omitted* `model` was meant to substitute
  the default, but any present-but-non-string value resolved to `None` through
  the same path and was served the default model — masking a client mistake by
  routing to a different model than was named. Only an omitted `model`
  substitutes `default_model`, as documented.

## [1.15.5] - 2026-06-14

### Fixed

- Scheduled (periodic) llama.cpp update checks now retry transient failures
  with backoff instead of giving up after a single attempt. A momentary
  `git fetch` network or DNS blip during the check previously skipped the whole
  cycle — waiting a full `auto_update_interval_seconds` (commonly a day) before
  trying again — and logged it at ERROR as a `llama_build_failure` event. The
  check now retries within the cycle, re-acquiring the rebuild lock per attempt
  (never held across a backoff sleep, so the manual rebuild endpoint stays
  responsive), and only escalates to an ERROR-level `llama_build_failure` event
  once the retry budget is exhausted. This brings the scheduled update path in
  line with the resilience the initial build already had.

## [1.15.4] - 2026-06-14

### Changed

- Circuit-breaker "now blocking" warnings now report the effective backoff
  duration (`backoff_ms`) and a uniform `consecutive_opens` count across all
  three open/reopen transitions (open after consecutive failures, reopen from
  half-open, reopen after a failed recovery probe). Previously the logs carried
  only the open count, so operators had to recompute the exponential backoff
  schedule by hand to know how long a peer would stay blocked before the next
  recovery probe. This is a logging-only change; circuit-breaker behavior is
  unchanged.

## [1.15.3] - 2026-06-14

### Fixed

- Streaming token counting now counts `reasoning_content` and `tool_calls`
  deltas in addition to `content`. Previously only visible `content` deltas were
  counted, so `tokens_generated` (and the derived `estimated_tokens_per_second`
  reported in `/v1/models`) undercounted responses from reasoning ("thinking")
  and tool-calling models whenever the upstream stream omitted the authoritative
  `usage` chunk. A `usage` chunk, when present, remains authoritative.

## [1.15.2] - 2026-06-14

### Fixed

- The llama.cpp build cleanup no longer deletes the build the live binary is
  currently using. Cleanup previously removed the oldest build directories by
  timestamp until only `keep_builds` remained, so a small value (`keep_builds:
  0` in particular) or an out-of-order build timestamp could delete the active
  build directory out from under the running server. The active build is now
  always retained, and `keep_builds` counts the previous (non-active) builds
  kept for rollback alongside it, so protecting the active build never reduces
  rollback capacity.

## [1.15.1] - 2026-06-14

### Fixed

- `/v1/models` and `/v1/models/{model}` now report a stable `created` timestamp
  for each model instead of the wall-clock time at request time, which changed
  on every call. OpenAI clients treat `created` as a fixed per-model creation
  timestamp, so the previous behavior could confuse tools that cache or compare
  it. Locally served models report this node's start time; models advertised by
  cluster peers report the owning peer's start time (propagated via gossip), so
  a peer-owned model's `created` is consistent across every front-end node that
  serves it. A peer running an older version that does not advertise its start
  time falls back to the querying node's start time.

## [1.15.0] - 2026-06-13

### Added

- `GET /v1/models/{model}` retrieves a single model by id, completing the
  OpenAI "retrieve model" endpoint (used by SDK calls such as
  `client.models.retrieve(...)`). It returns the same object shape as one entry
  of `GET /v1/models` — including models advertised only by cluster peers — and
  responds with `404 Not Found` and the standard error envelope (`type`
  `model_not_found`) when no local profile or peer advertises the id. The
  default profile is addressed by the bare model name, other profiles as
  `model:profile`.

## [1.14.0] - 2026-06-13

### Added

- The JSON metrics snapshot (`/metrics/json` and the persisted
  `node-metrics.json`) now mirrors five counters that were previously exposed
  only by the Prometheus `/metrics` endpoint: `queue_pending_tokens`,
  `queue_wait_total_ms`, `queue_wait_count`, `token_counting_disabled_total`,
  and `skipped_stream_cleanups_total`. Tooling and dashboards that read the
  JSON endpoint can now observe queue-wait latency (mean wait =
  `queue_wait_total_ms / queue_wait_count`), pending slot reservations, and the
  two operational health counters without scraping Prometheus. The values are
  mirrored for observability and — like `queue_length` and their Prometheus
  counterparts — start fresh on restart rather than being restored on load, so
  `/metrics/json` reports the same values as `/metrics` at every moment. Each
  new field is `#[serde(default)]`, so older snapshots remain loadable and the
  durable counters (`requests_total`, `errors_total`, `queue_drops_*`) are
  unaffected.

## [1.13.3] - 2026-06-13

### Documentation

- Documented configuration fields that existed in code but were missing from
  the annotated examples: `max_total_queue_entries`,
  `model_defaults.min_eviction_tenure_secs`,
  `cluster.version_mismatch_action`, and `http.body_read_timeout_ms` /
  `http.protocol_detect_timeout_ms` in `config.example.yaml`, the per-profile
  `min_eviction_tenure_secs` override in `cookbook.example.yaml`, and
  `min_eviction_tenure_secs` (config and eviction-tenure behavior) in
  `SPEC.md`, which previously described it nowhere. A new test loads both
  example files through the real loaders and validates them, so the examples
  cannot silently drift from the config structs again.

## [1.13.2] - 2026-06-13

### Fixed

- The Prometheus `/metrics` endpoint now emits each per-hash metric family
  (`proxy_hash_requests_total`, `proxy_hash_errors_total`, …) as one
  contiguous block. The per-hash section previously interleaved the seven
  families — printing all samples for one hash, then all for the next — which
  violates the exposition format's grouping requirement: strict parsers
  (OpenMetrics, `promtool check metrics`) split each family into one-sample
  fragments. Prometheus's own lenient parser was unaffected, so scraping
  continued to work. (#89)

## [1.13.1] - 2026-06-12

### Fixed

- Every integration-test node now writes its metrics snapshot to a dedicated
  `tests/metrics_<name>.json` file (removed before each run) instead of
  inheriting the default `./node-metrics.json`. Test nodes were persisting
  over each other's snapshots, over previous runs' state, and over the
  metrics file of any production node running from the same checkout —
  and loading that node's counters at startup. (#87)

## [1.13.0] - 2026-06-12

### Added

- `/v1/models` now reports `parsed_model_params` (effective context window,
  trained context length, slot count, architecture, …) even when no instance
  is running. The params are captured when an instance becomes ready and
  persisted per `args_hash` in the metrics snapshot — written immediately
  when captured, so the values survive instance eviction and proxy restarts
  — with a running instance's values taking precedence. The llama.cpp
  version, derived from the resolved managed-build path of the binary each
  instance was actually spawned from, is recorded alongside; params observed
  under a different binary than the one currently live are withheld (the
  same launch args can resolve to different effective values on another
  build). Previously the metadata reverted to `null` the moment
  an instance was evicted, which with short idle timeouts meant it was
  almost never visible. The persisted entry also appears in `/metrics/json`
  hash entries. (#85)

### Changed

- The mock llama-server used by integration tests now emits llama-server
  style startup log lines (`initializing slots, n_slots = …`,
  `new slot, n_ctx = …`), so tests exercise the real startup-log parsing
  path.

## [1.12.0] - 2026-06-12

### Added

- Cleartext HTTP/2 (h2c) support: connections beginning with the HTTP/2
  prior-knowledge preface are now served by the HTTP/2 stack. The protocol
  detector already classified them as HTTP/2, but the accept loop handed them
  to the HTTP/1 connection handler, so every h2c client failed with a framing
  error. In place of HTTP/1's header read timeout, h2c connection lifetime is
  bounded by ping-based keep-alive (reaps dead peers) plus an idle watchdog:
  a connection with no active streams for `http.idle_timeout_seconds` is shut
  down gracefully (GOAWAY), and one that cannot complete a graceful shutdown —
  such as a client that stalls mid-preface and never finishes the HTTP/2
  handshake — is dropped outright shortly after. HTTP/1.1, TLS, and Noise
  handling are unchanged. (#83)

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
