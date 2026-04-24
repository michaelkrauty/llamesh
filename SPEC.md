# llamesh — Technical Specification

This document describes the actual behavior of llamesh as implemented. It serves as a detailed technical reference for the system's design, configuration, and behavior.

---

## High-Level Overview

`llamesh` ("the proxy") is a single binary that you run on one or more nodes. Each node:

* Listens on a single public HTTP(S) port.
* Accepts the same OpenAI-compatible HTTP endpoints that `llama-server` exposes (e.g. `/v1/completions`, `/v1/chat/completions`, `/v1/embeddings`) so a client can talk to the proxy exactly as it would talk to `llama-server`, plus the additional proxy/cluster/admin endpoints documented below ([reference](https://github.com/ggml-org/llama.cpp/tree/master/tools/server)).
* Resolves the requested `model` into a configured model/profile via a YAML "cookbook".
* Starts, stops, and routes to local `llama-server` instances, one per `(model, profile)` configuration.
* Optionally participates in a **mesh** of peers (multi-node). Any node can receive traffic and forward requests to a peer node based on capacity and performance.

The system is a drop-in replacement for a direct `llama-server` endpoint, but adds:

* Auto-spawn, auto-scale, and idle-evict model instances.
* Cluster-wide routing and load balancing.
* Support for the major `llama-server` serving modes that exist upstream (chat/completions, embeddings, grammar-constrained outputs via the appropriate grammar flags, e.g. `--grammar-file`, plus optional reranking when/if supported) by configuring **per-profile / per-instance** `llama-server` CLI flags (there are no separate node-level toggles for these modes).
* Centralized metrics and health endpoints.
* Safe build/update management for `llama.cpp` binaries.
* Tight control of memory usage via configurable guardrails.

---

## Core Concepts

### Models and Profiles

* A **model** is a logical name exposed to clients in the OpenAI `model` field (e.g. `"gpt-oss-20b"`).
* A **profile** is a specific configuration for a model (e.g. `fast`, `quality`, `balanced`) that controls:

  * `llama-server` launch arguments (context size, batch size, KV behavior, etc.).
  * Expected VRAM and system memory usage.

Each `(model, profile)` pair corresponds to a specific `llama-server` command line.

### Nodes

* Each running proxy process is a **node**.
* Nodes can run standalone or as part of a mesh (cluster).
* In mesh mode, nodes exchange metadata about capabilities and load, and can forward requests to each other.

### Instances

* An **instance** is a single `llama-server` child process managed by the proxy.
* Multiple instances of the same `(model, profile)` can be running simultaneously on one node.
* Instances are created on-demand, shared across concurrent requests, and evicted after an idle timeout.

---

## Rust Project Layout

```text
src/
├─ main.rs            — CLI, initialization, signal handling, background task spawning (gossip, build manager, cookbook watcher, binary swap watcher)
├─ config.rs          — YAML config loading with env var overrides, cookbook loading, validation
├─ api.rs             — Axum HTTP server, OpenAI endpoints, admin endpoints, protocol detection dispatch
├─ router.rs          — Per-request routing, model resolution, peer forwarding, streaming proxy
├─ node_state/
│  ├─ mod.rs          — NodeState facade: instances, queues, resource tracking, routing scores, eviction
│  ├─ model_index.rs  — Model/profile resolution with O(1) lookup, args hash computation
│  ├─ peer_state.rs   — Peer state types for cluster gossip
│  └─ port_pool.rs    — Port allocation for llama-server instances
├─ instance.rs        — llama-server process lifecycle, health polling, startup log parsing
├─ cluster.rs         — Mesh gossip loop, peer metadata exchange, mDNS integration
├─ build_manager.rs   — llama.cpp clone, build, smoke test, atomic symlink swap, version signaling
├─ circuit_breaker.rs — Per-peer circuit breaker (Closed/Open/HalfOpen with exponential backoff)
├─ metrics.rs         — Prometheus metrics, JSON snapshots, per-args-hash tracking
├─ memory_sampler.rs  — Real-time VRAM (NVML) and system memory (procfs) sampling
├─ protocol_detect.rs — Single-port multiplexing: TLS vs HTTP vs Noise protocol detection
├─ noise/
│  ├─ mod.rs          — Noise Protocol context and error types
│  ├─ handshake.rs    — Noise_XX handshake with HMAC token verification
│  ├─ transport.rs    — Encrypted HTTP-over-Noise request/response transport
│  ├─ keypair.rs      — Ed25519 keypair management
│  ├─ token.rs        — Cluster shared-secret token generation and storage
│  ├─ known_peers.rs  — TOFU known-peers store with file persistence
│  └─ permissions.rs  — File permission enforcement for secrets
├─ discovery/
│  ├─ mod.rs          — Peer discovery abstraction
│  └─ mdns.rs         — mDNS zero-config LAN peer discovery
├─ errors.rs          — AppError/ApiError types for structured HTTP error responses
├─ logging.rs         — Structured JSON logging with daily rotation
├─ health.rs          — /healthz, /readyz probes
├─ security.rs        — TLS/mTLS setup, API key auth
└─ util.rs            — Byte manipulation helpers
```

Additional files:

* `src/bin/mock_llama_server.rs` — Mock `llama-server` binary used by integration tests.
* `tests/` — Integration tests (`integration_test.rs`, `integration_test_features.rs`, `integration_test_admin_auth.rs`, `loop_prevention_test.rs`, `stress_test.rs`) with shared test helpers in `tests/common/mod.rs`.

---

## Configuration

Configuration is split into three main pieces:

1. **Node config**: Basic settings for this node.
2. **Cookbook**: Model/profile definitions and expected resource usage.
3. **Secrets / security config**: TLS and auth (typically another file or env vars).

### 1. Node Config (YAML)

Example `config.yaml`:

```yaml
node_id: "node-a"
listen_addr: "0.0.0.0:8080"
public_url: "https://node-a.example.com"  # optional, used in cluster metadata

# Resource guardrails (memory in MB)
max_vram_mb: 71680       # 71680 MB (~70 GiB)
max_sysmem_mb: 153600    # 153600 MB (~150 GiB)

# Global instance limit (optional, default: 100)
max_instances_per_node: 100

# Shutdown grace period (seconds)
shutdown_grace_period_seconds: 30

# Max forwarding hops for loop prevention (default: 10)
max_hops: 10

# Path for metrics JSON snapshot (default: "./node-metrics.json")
metrics_path: "./node-metrics.json"

# Global queue limit across all models (default: 0, disabled)
max_total_queue_entries: 0

# Default model if client omits `model`
default_model: "gpt-oss-20b:fast"

# Per-model defaults
model_defaults:
  max_concurrent_requests_per_instance: 0   # required; 0 = unlimited
  max_queue_size_per_model: 0               # required; 0 = unlimited
  max_instances_per_model: 4
  max_wait_in_queue_ms: 60000
  max_request_duration_ms: 0                # 0 = unlimited

# Optional static port configuration for llama.cpp instances
llama_cpp_ports:
  # You can mix explicit ports and one or more contiguous ranges (inclusive).
  ports: [8200, 8201, 8300]
  ranges:
    - start: 8200
      end: 8299
    - start: 8400
      end: 8499

# Llama.cpp build config
llama_cpp:
  repo_url: "https://github.com/ggml-org/llama.cpp.git"
  repo_path: "./llama.cpp"
  build_path: "./llama.cpp/build"
  binary_path: "./llama.cpp/build/bin/llama-server"
  branch: "master"                       # use release tag (e.g., "b6115") to pin version
  build_args:
    - "-DGGML_CUDA=ON"
    - "-DGGML_CUDA_FA_ALL_QUANTS=ON"
  build_command_args:
    - "-j32"
  auto_update_interval_seconds: 86400  # once per day; set to 0 when pinning to specific version
  enabled: true                        # set to false to disable auto-build
  keep_builds: 3                       # number of old builds to keep (default: 3)

# Cluster / mesh config
cluster:
  enabled: true

  # Peer discovery
  discovery:
    mdns: true                           # Zero-config LAN discovery (default: true)
    service_name: "_llama-mesh._tcp.local"

  # Explicit peers for WAN/cross-subnet (additive with mDNS)
  # Uses same port as listen_addr (protocol multiplexing)
  peers: []
    # - "node-b.example.com:8080"
    # - "node-c.example.com:8080"

  # Noise Protocol encryption (default: enabled)
  noise:
    enabled: true
    tofu: true                           # Trust-On-First-Use (default: true)
    config_dir: ~/.llama-mesh/           # Secrets directory (default)
    # private_key_path: null             # Override: config_dir/node.key
    # known_peers_path: null             # Override: config_dir/known_peers
    # previous_key_path: null            # For key rotation
    allowed_keys: []                     # Enterprise: pin specific node keys
    session_ttl_seconds: 3600            # Session cache lifetime (default: 1 hour)

  # Gossip settings
  gossip_interval_seconds: 5
  max_concurrent_gossip: 16

  # Circuit breaker for peer connections
  circuit_breaker:
    enabled: true
    failure_threshold: 5                 # Failures before opening circuit
    success_threshold: 2                 # Successes needed to close circuit
    open_duration_base_ms: 5000          # Base backoff (doubles each time circuit opens)
    open_duration_max_ms: 60000          # Maximum backoff cap

  # Action when peer llama.cpp version differs (default: "warn")
  version_mismatch_action: "warn"

# HTTP server options
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 120
  body_read_timeout_ms: 30000            # timeout for reading request body (default: 30s)
  protocol_detect_timeout_ms: 10000      # timeout for protocol detection on new connections (default: 10s)

# File logging (optional, disabled by default)
# logging:
#   enabled: true
#   directory: "./logs"
#   filename: "proxy.log"
#   max_keep_files: 7       # rotated files to keep
#   compression: true       # gzip compress rotated files

# API auth (optional)
auth:
  enabled: true
  required_header: "x-api-key"
  allowed_keys:
    - "my-secret-key-1"
    - "my-secret-key-2"

# TLS for client -> node (public API)
server_tls:
  enabled: true
  cert_path: "./tls/server.crt"
  key_path: "./tls/server.key"

# DEPRECATED: mTLS for node <-> node (use noise protocol instead)
# cluster_tls:
#   enabled: true
#   ca_cert_path: "./tls/ca.crt"
#   client_cert_path: "./tls/node-a.crt"
#   client_key_path: "./tls/node-a.key"
```

When NVIDIA NVML is available, `max_vram_mb` is enforced against device-wide
used VRAM, not only memory attributed to `llama-server` children. GPU memory
used by other processes is treated as non-evictable external usage and reduces
the node's spawn capacity.

#### Environment Variable Overrides

All config fields can be overridden via environment variables using the `LLAMESH_` prefix:

* Top-level fields use `_` as separator: `LLAMESH_NODE_ID`, `LLAMESH_MAX_VRAM_MB`, `LLAMESH_LISTEN_ADDR`
* Nested fields use `__` (double underscore): `LLAMESH_CLUSTER__ENABLED`, `LLAMESH_HTTP__IDLE_TIMEOUT_SECONDS`

Environment variables take precedence over file-based configuration. This is for config overrides, not for secrets (secrets use `secrets.yaml` or Noise token files).

#### Cluster Security Examples

**Hobbyist (zero-config):** Just enable cluster - mDNS discovers LAN peers, keys auto-generated:

```yaml
cluster:
  enabled: true
```

**Enterprise (WAN only):** Disable mDNS, explicit peers, pin allowed keys:

```yaml
cluster:
  enabled: true
  discovery:
    mdns: false
  peers: ["node-b.internal:8080", "node-c.internal:8080"]
  noise:
    tofu: false
    allowed_keys:
      - "noise25519:xAbC123...="
      - "noise25519:yDeF456...="
```

**Hybrid (LAN + WAN):** mDNS for local, explicit peers for remote:

```yaml
cluster:
  enabled: true
  discovery:
    mdns: true
  peers: ["remote-dc.example:8080"]
  noise:
    tofu: false
    allowed_keys:
      - "noise25519:xAbC123...="
```

**Secrets Location:** `~/.llama-mesh/` (mode 700)
- `cluster_token` (mode 600) - Auto-generated, copy to other nodes
- `node.key` (mode 600) - Auto-generated per node
- `known_peers` (mode 600) - TOFU-managed trusted peers

**Environment Variable Overrides:**
- `CLUSTER_TOKEN` - Overrides file-based token
- `NODE_PRIVATE_KEY` - Base64-encoded Noise static X25519 private key

`llama_cpp_ports` lets operators constrain which local TCP ports `llama-server` instances may bind to. If omitted, the proxy uses OS-assigned ephemeral ports; if provided, it will choose ports from the configured explicit `ports` list and/or one or more `ranges`, skipping any that are already in use.

### 2. Cookbook (Model & Profile Definitions)

**Hot Reloading:** The cookbook file is watched for changes using filesystem events. When you modify `cookbook.yaml`, the proxy automatically reloads the configuration without requiring a restart. This allows you to add new models, modify profiles, or disable models on-the-fly. Running instances are not affected by cookbook changes until they become idle and are restarted.

Example `cookbook.yaml`:

```yaml
models:
  - name: "gpt-oss-20b"
    description: "General-purpose 20B model"
    enabled: true                      # optional, default true
    profiles:
      - id: "fast"
        enabled: true                       # optional, default true
        description: "Lower context, higher throughput"
        model_path: "./models/gpt-oss-20b-q4_K_M.gguf"
        idle_timeout_seconds: 600         # optional, default 300
        startup_timeout_seconds: 60        # optional, default 60 (for local models)
        download_timeout_seconds: 3600     # optional, default 3600 (for HF downloads)
        max_instances: 4                   # 0 = disabled (won't run on this node), omit = use defaults
        max_wait_in_queue_ms: 45000
        max_request_duration_ms: 0         # optional, default 0 (unlimited)
        max_queue_size: 128                # optional, overrides global max_queue_size_per_model
        estimated_vram_mb: 12000           # optional, used until runtime memory is learned
        estimated_sysmem_mb: 8192          # optional, used until runtime memory is learned
        llama_server_args: "--alias gpt-oss-20b-fast -c 32768 -b 2048 -fa on --kv-unified"

      - id: "quality"
        description: "Larger context, more careful settings"
        model_path: "./models/gpt-oss-20b-q6_K.gguf"
        idle_timeout_seconds: 900
        max_instances: 2
        llama_server_args: "--alias gpt-oss-20b-quality -c 131072 -b 1024 -fa on --kv-unified"

  - name: "qwen3-vl-8b"
    description: "Vision-language model"
    profiles:
      - id: "default"
        description: "Balanced profile"
        model_path: "./models/qwen3-vl-8b-q4_K_M.gguf"
        idle_timeout_seconds: 600
        max_instances: 3
        llama_server_args: "--alias qwen3-vl-8b -c 262144 -b 1024 -fa on --kv-unified"

  # Example using Hugging Face to automatically download models
  - name: "qwen2.5-0.5b"
    description: "Small model downloaded from Hugging Face"
    profiles:
      - id: "default"
        description: "Default profile using Hugging Face model"
        hf_repo: "ggml-org/Qwen2.5-0.5B-Instruct-GGUF"
        hf_file: "qwen2.5-0.5b-instruct-q4_k_m.gguf"
        idle_timeout_seconds: 300
        download_timeout_seconds: 1800     # 30 minutes for download + model loading
        max_instances: 4
        llama_server_args: "--alias qwen2.5-0.5b -c 32768 -fa on"
```

**Model Source Options:**

Each profile must specify a model source using one of these approaches:

* **`model_path`**: Path to a local GGUF model file.
* **`hf_repo`** + **`hf_file`** (optional): Hugging Face repository and file. The model will be automatically downloaded when needed.
  * `hf_repo`: The Hugging Face repository (e.g., `"ggml-org/Qwen2.5-0.5B-Instruct-GGUF"`).
  * `hf_file`: (Optional) Specific file within the repository (e.g., `"qwen2.5-0.5b-instruct-q4_k_m.gguf"`). If omitted, `llama-server` will use the default file.
  * `download_timeout_seconds`: Maximum time for download + model loading. Default: 3600 seconds (1 hour).

You can also specify model source directly in `llama_server_args` using `-m`/`--model` or `-hfr`/`--hf-repo` flags.

**Startup Timeouts:**

* **`startup_timeout_seconds`**: Maximum time to wait for a local model to load and become ready. Default: 60 seconds. Only used when `model_path` is specified.
* **`download_timeout_seconds`**: Maximum time to wait for a Hugging Face model to download and become ready. Default: 3600 seconds (1 hour). Only used when `hf_repo` is specified. This timeout covers both the download time and the model loading time.

**Default Arguments:**

`llama_server_args` is merged with proxy-managed connection settings. By default the proxy appends `--host 127.0.0.1`, an auto-chosen `--port`, `-c 0` (max context size), and `--fit off` (disable auto-optimization) for each instance; if you explicitly include `--host`, `--port`, `-c`/`--ctx-size`, or `--fit` in `llama_server_args`, those values override the defaults. The `-c 0` default tells llama-server to use the model's maximum trained context length. The `--fit off` default ensures your hand-crafted cookbook settings are not overridden by llama-server's automatic optimization feature.

Model and profile `enabled` fields are independent. A profile is available only
when both its parent model and the profile are enabled. `enabled: false` on a
profile removes that profile from local routing, `/v1/models`, prewarm, and
cluster advertisements while leaving sibling profiles available. If a cookbook
reload disables a profile, existing instances for that profile are drained
gracefully and stopped once idle.

To mirror upstream `llama-server` behaviors ([docs](https://github.com/ggml-org/llama.cpp/tree/master/tools/server)), you typically configure different profiles with appropriate flags:

* **Speculative decoding**: Add an additional "draft" model/profile in the cookbook and reference it via `llama_server_args` (for example `-md /models/draft.gguf`); the proxy will manage such instances like any other.
* **Embeddings**: Create a profile whose `llama_server_args` include `--embedding` (and optionally `--pooling`, `-ub`, etc.), matching the upstream examples. That profile will then be addressable via the OpenAI `/v1/embeddings` endpoint on the proxy; there is no node-level `--embedding` toggle.
* **Reranking (optional / version-dependent)**: When the deployed `llama-server` version exposes reranking support, create a profile with the documented reranking flag(s) from `tools/server`; the proxy can then route dedicated reranking requests to those instances. If reranking is not available in your `llama-server` build, keep such profiles disabled with `enabled: false`; there is no node-level `--reranking` toggle.
* **Grammar-constrained outputs**: Configure profiles whose `llama_server_args` enable grammars (e.g. `--grammar-file` or other grammar-related flags supported by your `llama-server` build). The proxy does not have a node-level grammar setting; grammar is entirely per-instance via the underlying `llama-server` CLI.

**Endpoint-Type Enforcement:**

Profiles must match the request type. An embedding request (`/v1/embeddings`) is routed only to profiles configured with `--embedding`. A reranking request (`/v1/rerank`) is routed only to reranking-capable profiles. A text completion request is routed only to non-embedding, non-reranking profiles. This prevents misrouting requests to incompatible instances.

**Model name syntax:**

* Clients use `<model_name>[:<profile_id>]` in the OpenAI `model` field, e.g.:

  * `"gpt-oss-20b"` -> implicitly `gpt-oss-20b:default` (if `default` exists).
  * `"gpt-oss-20b:fast"` -> `gpt-oss-20b` with `fast` profile.

### 3. Persistent Metrics Store

Per node, metrics snapshots are stored as JSON files at the path specified by `metrics_path` (default: `./node-metrics.json`). Metrics are keyed by the SHA-256 hash of llama-server launch arguments, not by model name, since the same model can have different profiles with different resource characteristics. The `display_names` field maps each hash back to human-readable `model:profile` names.

```json
{
  "node_id": "node-a",
  "llama_cpp_version": "7099-d23355afc",
  "requests_total": 1234,
  "errors_total": 12,
  "current_requests": 3,
  "is_building": false,
  "last_build_error": null,
  "last_build_at": "2025-11-20T19:00:00Z",
  "node_resources": {
    "llamesh_vram_mb": 20480,
    "llamesh_sysmem_mb": 8192,
    "external_vram_mb": 4096,
    "effective_vram_mb": 24576,
    "effective_sysmem_mb": 8192,
    "device_vram_used_mb": 24576,
    "device_vram_total_mb": 81920,
    "gpu_telemetry_available": true
  },
  "hashes": {
    "<sha256-of-launch-args>": {
      "display_names": ["model-name:profile"],
      "requests_total": 500,
      "errors_total": 5,
      "tokens_generated_total": 50000,
      "avg_latency_ms": 430.0,
      "p95_latency_ms": 1200,
      "peak_vram_mb": 28672,
      "peak_sysmem_mb": 13312,
      "sample_count": 100
    }
  },
  "updated_at": "2025-11-20T19:00:00Z"
}
```

The format is internal; stability is not guaranteed, but it must be:

* Single file per node.
* Atomically updated (write to temp, then rename).
* Treated as a persistence/offline-analysis artifact, **not** the primary metrics API (use `/metrics` endpoints for live scraping).

---

## Building and Running

### Dependencies

* Rust (stable, e.g. 1.88+).
* CMake, a C/C++ compiler, and dependencies for `llama.cpp`.
* `git` available on PATH.
* `llama.cpp` repo will be cloned automatically based on config, or you can pre-clone it.

### Build

```bash
git clone https://github.com/michaelkrauty/llamesh.git
cd llamesh
cargo build --release
```

### Run

```bash
./target/release/llamesh \
  --config ./config.yaml \
  --cookbook ./cookbook.yaml
```

All file locations are configurable, but the defaults are self-contained:

* By default, the proxy looks for `config.yaml`, `cookbook.yaml`, and optional `secrets.yaml` in the same directory as the node entry binary (or current working directory when launched).
* You can override these with CLI flags such as `--config PATH`, `--cookbook PATH`, `--secrets PATH`, and with config options like `llama_cpp.repo_path`, `metrics_path`, and `log_dir` if implemented.
* Paths such as `llama_cpp.repo_path`, `llama_cpp.build_path`, `llama_cpp.binary_path`, TLS cert/key paths, metrics file path, and log directory are expected to be relative to the node entry directory by default, but may be set to absolute paths for more advanced deployments.

Recommended to run as a systemd service or under a supervisor.

---

## OpenAI-Compatible API

The proxy treats `llama-server` as the source of truth for OpenAI-style request/response behavior. For any supported `/v1/*` endpoint, it forwards the HTTP method, path, and JSON body to the selected `llama-server` instance and streams the response back verbatim, with no schema translation beyond model/profile resolution and optional auth handling.

### Completions

**Endpoint:** `POST /v1/completions`

Behavior:

* Same request/response semantics as upstream `llama-server` and OpenAI's `/v1/completions` (see the [llama.cpp tools/server docs](https://github.com/ggml-org/llama.cpp/tree/master/tools/server) and the [DeepWiki summary](https://deepwiki.com/ggml-org/llama.cpp/5.2-server?utm_source=openai)).
* Uses the same model resolution and routing steps as **Chat Completions** below (including default model behavior and error handling).
* Supports `stream: true` for SSE streaming, forwarded verbatim from the chosen `llama-server` instance.

### Chat Completions

**Endpoint:** `POST /v1/chat/completions`

Request (example):

```json
{
  "model": "gpt-oss-20b:fast",
  "messages": [
    { "role": "system", "content": "You are a helpful assistant." },
    { "role": "user", "content": "Explain speculative decoding." }
  ],
  "temperature": 0.7,
  "max_tokens": 512,
  "stream": true
}
```

Behavior:

* If `model` is omitted, substitute `default_model` from config.
* If `model` cannot be resolved to `(model_name, profile)` defined in the cookbook -> 404 JSON error (see **Error Model & HTTP Semantics**).
* Otherwise, route according to routing algorithm (see below), and stream the `llama-server` response converted to the OpenAI SSE format.

### Models Listing

**Endpoint:** `GET /v1/models`

Response:

```json
{
  "data": [
    {
      "id": "gpt-oss-20b:fast",
      "object": "model",
      "created": 1732113600,
      "owned_by": "node-a",
      "permission": [],
      "root": "gpt-oss-20b:fast",
      "parent": null,
      "metadata": {
        "model": "gpt-oss-20b",
        "profile_id": "fast",
        "profile_description": "Lower context, higher throughput",
        "idle_timeout_seconds": 600,
        "estimated_tokens_per_second": 42.5,
        "parsed_model_params": "20B"
      }
    },
    {
      "id": "gpt-oss-20b:quality",
      "object": "model",
      "created": 1732113600,
      "owned_by": "node-a",
      "permission": [],
      "root": "gpt-oss-20b:quality",
      "parent": null,
      "metadata": {
        "model": "gpt-oss-20b",
        "profile_id": "quality",
        "profile_description": "Larger context, more careful settings"
      }
    }
  ],
  "object": "list"
}
```

Implementation notes:

* `owned_by` is set to the `node_id` of the node that hosts the model.
* Metadata includes `model`, `profile_id`, `profile_description`, `idle_timeout_seconds`, `estimated_tokens_per_second` (from learned metrics), and `parsed_model_params` (extracted from startup logs, e.g. `"20B"`).
* For models advertised by cluster peers, metadata is `{ "source": "cluster", "advertised_by": "<peer_node_id>", "cluster_url": "<peer_url>" }`.
* When in cluster mode, returns the union of models supported by the cluster.

### Embeddings

**Endpoint:** `POST /v1/embeddings`

Request (example):

```json
{
  "model": "gpt-oss-20b:fast",
  "input": [
    "Llama.cpp is a lightweight C/C++ inference engine.",
    "Mesh proxy nodes can route requests across a cluster."
  ]
}
```

Behavior:

* Resolves `model` to a cookbook profile whose `llama_server_args` include `--embedding`.
* Forwards the request to a matching `llama-server` instance (or starts one) and returns an OpenAI-compatible embeddings response.
* If no embedding-capable profile exists for the requested model, returns a 404 `model_not_found` error (the endpoint itself is always present; capability is per-profile / per-instance).

### Reranking

**Endpoints:** `POST /v1/rerank`, `POST /v1/reranking`, `POST /rerank`

All three paths are aliases and produce the same behavior. The `/v1/reranking` and `/rerank` paths exist for compatibility with different client libraries.

This endpoint is always exposed by the proxy; whether a given `model` can be used for reranking depends on the configured reranking-capable profiles whose `llama_server_args` include the appropriate reranking flags.

Request (example):

```json
{
  "model": "qwen3-vl-8b:default",
  "query": "what does speculative decoding do?",
  "documents": [
    "Speculative decoding speeds up sampling by using a smaller draft model.",
    "Mesh routing picks nodes based on capacity and latency."
  ]
}
```

Behavior:

* Resolves `model` to a reranking-capable profile in the cookbook.
* Forwards the request to a reranking-capable `llama-server` instance using whatever reranking HTTP endpoint and JSON schema are defined by the deployed `llama-server` version (see `tools/server` docs) and maps the response into a JSON structure with scores per document.
* If reranking is not configured or not supported by the underlying `llama-server` build, returns a 404 or 400 depending on implementation choice.

### Endpoints Summary

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/completions` | POST | Text completions |
| `/v1/chat/completions` | POST | Chat completions |
| `/v1/embeddings` | POST | Embeddings |
| `/v1/rerank` | POST | Reranking |
| `/v1/reranking` | POST | Reranking (alias) |
| `/rerank` | POST | Reranking (bare path alias) |
| `/v1/models` | GET | Model listing |
| `/healthz` | GET | Liveness probe |
| `/health` | GET | Liveness probe (llama-server compatibility alias) |
| `/readyz` | GET | Readiness probe |
| `/version` | GET | Proxy version |
| `/metrics` | GET | Prometheus metrics |
| `/metrics/json` | GET | JSON metrics |
| `/cluster/nodes` | GET | Cluster node view |
| `/cluster/gossip` | POST | Internal peer gossip (node-to-node) |
| `/admin/prewarm` | POST | Pre-warm a model/profile |
| `/admin/rebuild-llama` | POST | Trigger llama.cpp rebuild |

### Error Model & HTTP Semantics

The proxy follows the same general error envelope and HTTP semantics as OpenAI's API and the `llama.cpp` server (`tools/server`) so that standard OpenAI clients behave as expected ([reference](https://github.com/ggml-org/llama.cpp/tree/master/tools/server)).

#### Error JSON Envelope

All non-2xx responses use a JSON body of the form:

```json
{
  "error": {
    "message": "human-readable description",
    "type": "short_machine_identifier",
    "param": "optional_parameter_name_or_null",
    "code": "optional_error_code_or_null"
  }
}
```

The fields are:

* `message`: Human-readable explanation of the error, suitable for logs or user-facing messages.
* `type`: Short, machine-readable identifier (e.g. `"invalid_request_error"`, `"model_not_found"`, `"no_capacity"`, `"queue_full"`, `"queue_timeout"`, `"authentication_error"`).
* `param`: Optional name of the offending parameter (e.g. `"model"`, `"max_tokens"`), or `null` when not applicable.
* `code`: Optional, implementation-defined code (string or `null`), which operators can use for finer-grained diagnostics.

This is intentionally compatible with the error envelope used by OpenAI and the upstream `llama.cpp` server, so existing client libraries can parse it without modification.

#### Status Codes & Canonical Error Types

The proxy uses standard HTTP status codes with the following canonical mappings:

* `400 Bad Request`:
  * `type`: `"invalid_request_error"`.
  * Conditions:
    * Malformed JSON.
    * Unsupported or out-of-range parameter values (e.g. negative `max_tokens`).
    * Missing required fields.
    * `X-Llama-Mesh-Hops` exceeds `max_hops`.
* `401 Unauthorized`:
  * `type`: `"authentication_error"`.
  * Conditions:
    * API auth enabled and required header missing or invalid.
* `403 Forbidden`:
  * `type`: `"forbidden"`.
  * Conditions:
    * API key is valid but not allowed to access requested resource/model (if such policies are implemented).
* `404 Not Found`:
  * `type`: `"model_not_found"` for unknown models/profiles.
  * May be used for other missing resources if additional endpoints are added.
* `408 Request Timeout`:
  * `type`: `"request_timeout"`.
  * Conditions:
    * Request body read timed out (`body_read_timeout_ms`).
* `409 Conflict`:
  * `type`: `"conflict"`.
  * Conditions:
    * Administrative operations where a concurrent action conflicts (e.g., triggering a rebuild while one is already in progress).
* `500 Internal Server Error`:
  * `type`: `"internal_error"`.
  * Conditions:
    * Unexpected, non-recoverable internal failures not covered by other codes.
* `502 Bad Gateway`:
  * `type`: `"peer_forward_failed"` or `"upstream_error"`.
  * Conditions:
    * `peer_forward_failed`: Peer forwarding failed (connection error, peer returned error).
    * `upstream_error`: The local `llama-server` instance returned an error response.
* `503 Service Unavailable`:
  * `type`: `"no_capacity"`, `"queue_full"`, `"queue_timeout"`, `"spawn_failures_exhausted"`, `"draining"`, or `"peer_unavailable_retry"`.
  * Conditions:
    * `no_capacity`: No node or instance has capacity to serve the request.
    * `queue_full`: Per-model or global queue is full (always 503, not configurable).
    * `queue_timeout`: Request waited in queue longer than `max_wait_in_queue_ms`.
    * `spawn_failures_exhausted`: Local instance spawn failed repeatedly and no peer could serve the request.
    * `draining`: Node is shutting down and not accepting new requests.
    * `peer_unavailable_retry`: Peer is temporarily unavailable, client should retry.

For `503` responses, the proxy may include a `Retry-After` header (e.g., `Retry-After: 30` for `spawn_failures_exhausted`).

### Health and Readiness

The proxy exposes standard Kubernetes-style probes:

* `GET /healthz` (also available as `GET /health` for llama-server compatibility):

  * 200 if process up, config parsed, and:

    * At least one model is defined in cookbook, AND
    * `llama.cpp` binary exists.
  * Non-200 (503) otherwise.

* `GET /readyz`:

  * 200 only if all of:

    * Not draining or shutting down.
    * `llama.cpp` binary exists.
    * At least one model is defined in cookbook.
  * Response body includes a `can_spawn` boolean field indicating whether the node has resources to spawn new instances.
  * Returns 503 if draining, no binary, or no models configured.

For full semantics (including drain/shutdown behavior), see **Health, Readiness, and Shutdown** below.

### Metrics

* `GET /metrics` -> Prometheus-style metrics (canonical source for real-time scraping).
* `GET /metrics/json` -> JSON-encoded metrics for ad-hoc inspection or tooling.

### Cluster / Admin

* `GET /cluster/nodes` -> current view of nodes and their capacities.
* `POST /cluster/gossip` -> internal peer gossip endpoint (used by nodes to exchange state). When Noise is enabled without cluster mTLS, plain HTTP gossip is rejected.
* `POST /admin/prewarm` -> request pre-warming of a model/profile.
* `POST /admin/rebuild-llama` -> trigger on-demand `llama.cpp` rebuild.

---

## Request Lifecycle

### 1. Accept Request

* Parse HTTP request.
* Enforce auth (if enabled).
* Parse JSON and extract `model`, `stream`, and other settings.
* Attach or generate a `session_id`:

  * Prefer header `x-session-id` if present.
  * Otherwise, generate ULID/UUID.
* Log an entry with `request_id`, `session_id`, `model`, `node_id`.

### 2. Model Resolution

* Parse `model` string as `<model>[:<profile>]`.
* Look up in cookbook:

  * If `<profile>` omitted, use profile with `id == "default"` if present.
* If not found locally:

  * If cluster is enabled, check whether any cluster peer supports the model. If a peer supports it, forward the request to that peer.
* If not found locally and no cluster peer supports it:

  * Return JSON error: `{ "error": { "type": "model_not_found", "message": ... } }` with 404.

### 3. Routing (Cluster vs Local)

If `cluster.enabled == false`:

* Route locally (see Local Node Routing below).

If `cluster.enabled == true`:

* Pull latest cluster snapshot (gossip data):

  * For each node: models supported, resource usage (including external VRAM when device telemetry is available), tokens/sec, queue lengths, loaded models, ready state, etc.
* Filter candidate nodes:

  * Node supports requested `(model, profile)`.
  * Node not in drain/shutdown state (`ready` flag is true).
  * Node's circuit breaker is not open.
  * Node has been seen within 3x `gossip_interval_seconds` (staleness check).
* `prefer_local`: Forwarded requests (hops > 0) are always served locally to prevent ping-pong routing loops.
* Score each candidate using the following formula:

```
score = load_score + cold_start_cost - performance_bonus

load_score = (queue_length * 50) + (effective_current_requests * 50)

cold_start_cost:
  Model already loaded:               0
  Model fits without eviction:       +30
  Model needs eviction:             +500 + (eviction_count * 200)
  Model cannot fit:               +10000

performance_bonus = min(tps, 200) * 5 + min(remaining_slots, 4) * 50
```

`effective_current_requests` adjusts the gossip-based in-flight count to close the staleness window between gossip ticks:
  * For peers: `peer.current_requests + pending_forwards_to_peer`, where `pending_forwards_to_peer` is the number of requests this node has forwarded to that peer that have not yet completed. This makes bursts visible to the scorer immediately instead of after the next gossip tick.
  * For self: `self.current_requests - sum(pending_forwards_to_all_peers)`. The edge counter includes forwarded-out requests that don't consume local GPU, so we subtract them to reflect true local processing load.

Lowest score wins.

* Peers with the model already loaded are preferred (reflected in cold_start_cost of 0).
* If no candidate is available, the router waits on a capacity notification rather than returning an immediate 503. This wait is bounded by `max_wait_in_queue_ms`.

* If chosen node == self:

  * Route locally.
  * If local spawning fails repeatedly (3 consecutive failures), fall back to forwarding to a peer that supports the model before returning an error.
* Else:

  * Forward request to chosen node (with Noise encryption if enabled, falling back to HTTPS with mTLS), fully streaming response back to client.

### 4. Local Node Routing

On chosen node (or standalone):

1. Look up or create a **model context** structure that tracks:

   * Existing instances for this `(model, profile)`.
   * Per-model queue.
2. Prune any crashed instances before attempting to find or spawn an instance.
3. For each instance, check:

   * `in_flight_requests < max_concurrent_requests_per_instance` (where 0 means unlimited).
   * Instance is not in `draining` state (from binary swap drain).
4. If at least one instance has capacity:

   * Scan all matching instances under read lock, track the one with fewest in-flight requests.
   * Upgrade to write lock on the winner, re-check capacity (handles races).
   * Dispatch the request to that instance.
5. If no instance has capacity and total instances < `max_instances` for that profile and resource guardrails allow:

   * Start a new instance with spawn retry and port rotation (up to 3 retries on OS-level failure).
   * After spawn, perform post-spawn race detection: if another request raced and consumed capacity, kill the just-spawned process to avoid waste.
   * Queue the request until the instance is ready.
6. If no new instance can be started (due to guardrails or `max_instances` reached):

   * If per-model queue length < `max_queue_size_per_model` (or unlimited when 0) and global queue < `max_total_queue_entries` (or unlimited when 0):

     * Enqueue the request.
   * Else:

     * Return 503 with `queue_full` error.

**Queue wait behavior:**

* Each request has a `max_wait_in_queue_ms` configured globally or per model.
* A `max_wait_in_queue_ms` of 0 means infinite wait (no timeout).
* If the request stays in queue longer than that (and the timeout is non-zero):

  * Remove it from the queue and return 503 `queue_timeout`.

**Priority token fairness:**

A priority token system prevents queue bypass. When a request is dequeued, it receives a priority token that guarantees it will be served next, preventing newly-arriving requests from stealing capacity ahead of queued requests.

### 5. Instance Management

Each instance is a `llama-server` process spawned by the proxy:

* On spawn:

  * Allocate a local TCP port (e.g. via OS or preconfigured port range).
  * Build command line from cookbook `model_path` + `llama_server_args` plus auto-assigned `--host`/`--port` and defaults (`-c 0`, `--fit off`):

    * Example:

      * `/path/to/llama-server -m /models/gpt-oss-20b-q4_K_M.gguf --alias gpt-oss-20b-fast -c 32768 -b 2048 --host 127.0.0.1 --port 8200 ...` (host is always `127.0.0.1`, port chosen by the proxy)
  * Inject `LD_LIBRARY_PATH` (prepends the binary's parent directory) to ensure shared libraries are found.
  * Record `start_time`, assigned `instance_id`, and estimated usage.

* Health checking:

  * Uses the `/health` endpoint on the `llama-server` instance exclusively.
  * If spawn/health check fails:

    * Mark instance as failed, emit error logs.
    * Track consecutive spawn failures per request. After 3 consecutive failures (which covers the cold-start OOM recovery stages plus one additional attempt), the node gives up on local spawning.
    * If the node is part of a cluster, the router attempts to forward the request to a peer that supports the model before returning an error to the client.

* Startup log parsing:

  * The proxy parses `llama-server` startup logs to extract model parameters (`n_ctx_train`, `n_layer`, architecture). These are exposed in `/v1/models` metadata as `parsed_model_params`.

* Idle eviction:

  * Background eviction loop runs every 10 seconds.
  * Each instance tracks `last_activity_time`.
  * If `now - last_activity_time > idle_timeout_seconds` and `in_flight_requests == 0`:

    * Terminate instance gracefully.
  * Fallback timeout of 600 seconds is applied if no profile-specific timeout is set.

* Background monitoring:

  * Background task detects and removes crashed instances.
  * Background task samples memory (VRAM and system memory) every 10 seconds continuously, not just during cold-start.

* Draining:

  * Instances have a `draining` flag. Draining instances do not receive new requests but continue serving in-flight requests until completion.
  * Used during binary swap (see Build Manager) and graceful shutdown.

### 6. Streaming & Backpressure

Once an instance is selected and ready:

* Proxy takes the OpenAI-style request, resolves `model` to a backend profile, and forwards it to the corresponding `llama-server` OpenAI endpoint (`/v1/chat/completions`, `/v1/embeddings`, etc.) without changing field names or semantics beyond model/profile mapping.
* For streaming:

  * Establish an HTTP connection to `llama-server` via reqwest.
  * Pipe response body to client using `Body::from_stream` via a streaming pipeline.
  * A `CleanupStream` wrapper parses SSE chunks to count tokens for metrics.
  * A `RequestGuard` RAII pattern ensures metric cleanup on client disconnect.

If the request is proxied cross-node:

* The same streaming behavior applies:

  * Client <-> Node A <-> Node B <-> `llama-server`.
* Each hop runs in backpressure-aware mode.

### 7. Completion & Metrics

* When a request finishes or errors:

  * Update per-instance and per-model metrics:

    * Latency, tokens/sec, token counts, memory usage snapshots.
  * Write relevant info to logs (with `session_id` and `request_id`).

---

## Memory & Resource Guardrails

Before starting a new instance, the node must verify resources:

1. Compute VRAM/sysmem attributed to running llamesh-managed instances using live process samples or learned values.
2. Sample device-wide GPU memory when telemetry is available.
3. Compute non-evictable external VRAM:

   ```text
   external_vram = device_used_vram - llamesh_tracked_vram, saturating at 0
   effective_vram = llamesh_tracked_vram + external_vram
   ```

   If device telemetry is unavailable, `external_vram` is 0 and the node falls back to llamesh-tracked memory only.
4. Estimate additional usage for the new instance using learned memory values (keyed by llama-server launch args hash), falling back to optional cookbook estimates.
5. If starting instance would exceed `max_vram_mb` or `max_sysmem_mb`:

   * Try to free space by:

     * Stopping least-recently used **idle** instances.
   * Recompute usage.
   * If still cannot start:

     * Either queue request and wait for capacity (bounded by timeout), or immediately return `no_capacity` error depending on configuration.

External VRAM is never counted as evictable capacity. Eviction simulation only
subtracts memory attributed to llamesh-managed instances.

### Cookbook Estimates

Profiles may define optional `estimated_vram_mb` and `estimated_sysmem_mb`
values. These are cold-start admission hints only. Once runtime sampling has
observed a profile's launch-args hash, learned peak memory replaces the static
estimate for future scheduling.

### Learned Memory

Memory usage is automatically learned at runtime:

* **VRAM**: Sampled via NVIDIA NVML (if available)
* **Device VRAM**: Device-wide NVML memory telemetry is sampled to account for non-llamesh GPU consumers.
* **System memory**: Sampled via `/proc/[pid]/status` (VmRSS)
* **Continuous sampling**: Memory is sampled every 10 seconds throughout the instance lifetime, not just during cold-start.
* **Peak tracking**: Uses atomic `fetch_max` so values can only grow over the instance lifetime, capturing the true peak usage.

Learned values are persisted in the metrics JSON file (at `metrics_path`) and keyed by a SHA-256 hash of the llama-server launch arguments. This means:

* Changing context size, batch size, or other args creates a new memory estimate.
* The system learns once and remembers across restarts.

### Cold-Start OOM Recovery

On cold start (unknown args hash), the system cannot predict memory requirements. The trigger is any spawn failure on cold-start instances (not specifically OOM detection). The following multi-stage recovery process handles this:

1. **Attempt spawn**: The instance is attempted with no pre-eviction (since memory estimate is unknown).
2. **Stage 1 (idle eviction)**: If spawn fails, evict all idle instances and retry.
3. **Stage 2 (graceful eviction)**: If spawn still fails, drain all remaining instances (bounded by `shutdown_grace_period_seconds`), wait for in-flight requests to complete, evict all, and retry.
4. **Stage 3 (wait for capacity)**: If spawn still fails after evicting everything, wait on a capacity notification for resources to become available.
5. **Peer fallback**: If `consecutive_spawn_failures >= 3`, forward the request to a cluster peer that supports the model.
6. **Failure**: If no peer can serve the request (or standalone mode), return `spawn_failures_exhausted` error with `Retry-After: 30`.

Once a cold-start instance successfully starts, its peak memory is sampled and learned for future scheduling.

---

## `llama.cpp` Build & Upgrade Manager

Each node is responsible for its local `llama.cpp`.

### Responsibilities

* Maintain a git clone at `repo_path`.
* Periodically check for updates:

  * `git fetch` + `git status` to detect new commits on `branch`.
* On update:

  * Configure and build `llama.cpp` with configured `build_args` in per-commit build directories (`build-<hash>/`).
  * `keep_builds` config controls retention (default 3 old builds).
  * Run smoke test:

    * `llama-server --help` or simple trivial prompt.
  * If smoke test passes:

    * Atomically swap symlink at `binary_path` to the new binary.

### Non-Disruptive Binary Swap

When a new binary is ready:

* The symlink swap signals `binary_swap_notify`, which triggers instance draining.
* **Version-aware drain**: All existing instances are marked as draining (they stop accepting new requests but continue serving in-flight requests).
* New requests spawn fresh instances using the new binary.
* Drained instances are terminated when their in-flight request count hits 0.

On build failure:

* Log error and keep using old binary.
* Expose status in metrics and admin endpoints.
* Build status (`is_building`, `last_build_error`, `last_build_at`) is exposed in the metrics JSON and admin endpoints.

Manual control:

* `/admin/rebuild-llama` endpoint triggers immediate rebuild.
* Returns `409 Conflict` if a rebuild is already in progress.
* Optional config to pin specific commit or version.

---

## Cluster / Mesh Design

### Node Discovery

Peer discovery uses two complementary methods, both additive:

* **mDNS (primary, zero-config)**: Enabled by default. Nodes advertise and discover peers on the LAN using the `_llama-mesh._tcp.local` service name. No configuration required for single-subnet deployments.
* **Explicit peer URLs (secondary)**: For WAN or cross-subnet connections, peers can be listed explicitly in `cluster.peers`. These are merged with mDNS-discovered peers.

First gossip tick fires immediately on startup (no initial delay).

### Gossip Metadata

Every `gossip_interval_seconds`, nodes exchange metadata including:

* `node_id`, `version`, supported models.
* `loaded_models`: Which models are currently loaded and running.
* `model_stats`: Per-model metrics including tokens per second, queue length, and learned VRAM/sysmem usage.
* `ready` flag: Whether the node is ready to serve (false when draining or binary is unavailable).
* `known_peers`: List of known peers for transitive discovery (peers learn about peers-of-peers).
* Current capacity and resource usage.

### Loop Prevention

The proxy attaches a `X-Llama-Mesh-Hops` header to forwarded requests. This is incremented at each hop. If the count exceeds `max_hops` (configurable, default 10), the request is rejected with a 400 Bad Request error.

### Security

**Noise Protocol (Primary)**

The proxy uses the Noise Protocol Framework (`Noise_XX_25519_ChaChaPoly_SHA256`) for encrypted inter-node communication:

* **Zero-config encryption**: Noise static X25519 keys are auto-generated on first run.
* **Perfect forward secrecy**: Each session uses ephemeral keys.
* **HMAC-SHA256 cluster token**: Shared secret proves cluster membership during handshake.
* **TOFU (Trust-On-First-Use)**: SSH-style peer trust model. New peer keys are automatically accepted on first connection.
* **Key pinning**: Enterprise deployments can explicitly pin allowed node public keys via `allowed_keys`, effectively disabling TOFU.
* **Single-port multiplexing**: Protocol detection (`protocol_detect.rs`) distinguishes TLS, HTTP, and Noise connections on the same port. No separate port needed for inter-node traffic.

Secrets are stored in `~/.llama-mesh/` with strict permissions (mode 600 for files, mode 700 for directory):
- `cluster_token` — Auto-generated shared secret, must be copied to other nodes.
- `node.key` — Auto-generated Noise static X25519 private key, unique per node.
- `known_peers` — TOFU-managed list of trusted peer public keys.

When enabled, Noise is used for cluster gossip and peer request forwarding. mTLS remains available as the legacy fallback when Noise is disabled.

**mTLS (Legacy, Deprecated)**

mTLS is still supported but deprecated in favor of Noise Protocol:

* TLS 1.3 with mutual TLS.
* Nodes trust a common CA.
* Node identity is derived from certificate subject (e.g. `CN=node-a`) and strictly enforced against the sender's claimed node ID.
* When both `noise` and `cluster_tls` are enabled, Noise is preferred for cluster gossip and peer request forwarding.

---

## Queueing & Concurrency Control

### Per-Instance Concurrency

* Configurable `max_concurrent_requests_per_instance` (required; 0 means unlimited).
* If instance has reached this limit (and the limit is non-zero):

  * Do not send more requests to it.

### Per-Model Queue

* Each model/profile has a FIFO queue.
* Max queue length `max_queue_size_per_model` is configurable (required; 0 means unlimited).
* Per-profile `max_queue_size` in the cookbook overrides the global setting.
* If queue is full:

  * New requests are rejected with 503 `queue_full`.
* Global queue limit `max_total_queue_entries` (default 0, disabled) caps total queued requests across all models.

### Timeouts

* `max_wait_in_queue_ms` per request (global default + per model override). 0 means infinite wait.
* `max_request_duration_ms` per request (default 0, unlimited). 0 means no timeout.

If either timeout is exceeded, proxy returns error and cancels the request.

Configuration tips:

* Set the node-wide default via `model_defaults.max_wait_in_queue_ms` in `config.yaml`.
* Override on a per-profile basis with `max_wait_in_queue_ms` inside the cookbook entry when certain models should wait longer or shorter.

---

## Tuning Guide

### Queue Management

The proxy provides multiple layers of queue control:

| Setting | Location | Default | Description |
|---------|----------|---------|-------------|
| `max_queue_size_per_model` | `config.yaml` (model_defaults) | required (0 = unlimited) | Global per-model queue limit |
| `max_queue_size` | `cookbook.yaml` (per profile) | inherits global | Per-profile queue limit override |
| `max_total_queue_entries` | `config.yaml` | 0 (disabled) | Global limit across all models |
| `max_wait_in_queue_ms` | both | 60000 | Request timeout while queued |

**Tuning tips:**

* Set `max_queue_size` per-profile for models with different request characteristics (e.g., small fast models can handle larger queues).
* Use `max_total_queue_entries` to cap overall memory pressure from queued requests.
* Default of 0 for `max_queue_size_per_model` means unlimited per-model queues (still bounded by `max_total_queue_entries` if set).

### Circuit Breaker

Prevents cascading failures when peers become unhealthy. Only affects cross-node requests.

| Setting | Default | Description |
|---------|---------|-------------|
| `failure_threshold` | 5 | Failures before opening circuit |
| `success_threshold` | 2 | Successes needed to close circuit |
| `open_duration_base_ms` | 5000 | Initial backoff (doubles each time the circuit opens) |
| `open_duration_max_ms` | 60000 | Maximum backoff cap |

Configuration in `config.yaml`:

```yaml
cluster:
  circuit_breaker:
    failure_threshold: 5
    success_threshold: 2
    open_duration_base_ms: 5000
    open_duration_max_ms: 60000
```

**Tuning tips:**

* Lower `failure_threshold` for faster failure detection (trades off more false positives).
* Increase `open_duration_base_ms` if peer recovery is slow.
* Circuit breaker is per-peer; a failing peer doesn't affect routing to healthy peers.

---

## Health, Readiness, and Shutdown

### Health Probes

* `/healthz` (also `/health`):

  * 200 if process up, config parsed, and:

    * At least one model is defined in cookbook, AND
    * `llama.cpp` binary exists.
  * Non-200 (503) otherwise.

* `/readyz`:

  * 200 only if all of:

    * Not draining or shutting down.
    * `llama.cpp` binary exists.
    * At least one model is defined in cookbook.
  * Response includes `can_spawn` boolean field.
  * Returns 503 if any of these conditions are not met.

### Version Information

* `/version`:

  * Returns the current version of the proxy in a JSON object: `{ "version": "0.1.0" }`.

### Graceful Shutdown

* On first SIGTERM:

  * Mark node as **draining**:

    * `/readyz` returns non-200.
    * Stop accepting new requests (client and peer).
    * Wake all requests blocked waiting for capacity so they can exit promptly.
  * Wait for in-flight requests and queues to drain up to `shutdown_grace_period_seconds`.

* On second SIGTERM, or after grace period:

  * Call `process::exit(0)` for immediate hard exit.
  * Child `llama-server` processes are cleaned up by the OS.

---

## Logging & Tracing

All logs are structured (JSON), one event per line.

Minimum fields for each log event:

* `timestamp`
* `level`
* `node_id`
* `session_id` (if available)
* `request_id` (generated per request)
* `event` (short string)

Examples of log events:

* Instance lifecycle:

  * `event="instance_spawn"`, `model`, `profile`, `instance_id`, `port`, `reason` (`"demand"`, `"prewarm"`).
  * `event="instance_terminate"`, `reason` (`"idle"`, `"manual"`, `"error"`, `"cold_oom_stage1"`, `"cold_oom_stage2"`), memory stats (`freed_vram_mb`, `freed_sysmem_mb`).
  * `event="cold_oom_evict_stage1"`, `count` - Evicting idle instances for cold-start OOM recovery.
  * `event="cold_oom_evict_stage2_start"`, `count` - Beginning graceful eviction of all instances.
  * `event="cold_oom_evict_stage2_drain_complete"` - All instances drained, proceeding with eviction.

* Routing:

  * `event="route_decision"`, `chosen_node`, `candidate_nodes`, `scores`.

* Build manager:

  * `event="llama_build_start"`, `branch`, `commit`.
  * `event="llama_build_success"`, `commit`, `binary_path`.
  * `event="llama_build_failure"`, `error`.

* Queueing:

  * `event="queue_enqueue"`, `model`, `queue_length`.
  * `event="queue_dequeue"`, `token_id`.
  * `event="queue_drop"`, `reason="full"` or `"timeout"`.

---

## Security Model

### Client Authentication

* Optional API key via header (e.g. `x-api-key`).
* If auth enabled and header missing/invalid -> 401.

### Inter-Node Authentication

See **Cluster / Mesh Design > Security** above for Noise Protocol and mTLS details.

### Configuration Safety

* No API keys or secrets stored in cookbook.
* Sensitive values provided via:

  * Separate `secrets.yaml` with restricted permissions.
  * Noise token files in `~/.llama-mesh/` with mode 600.

---

## Example Client Usage

Python (OpenAI client pointed to proxy):

```python
from openai import OpenAI

client = OpenAI(
    base_url="https://node-a.example.com/v1",
    api_key="my-secret-key-1",
)

resp = client.chat.completions.create(
    model="gpt-oss-20b:fast",
    messages=[
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "Explain KV cache in llama.cpp."},
    ],
    stream=True,
)

for chunk in resp:
    print(chunk.choices[0].delta.content or "", end="", flush=True)
```

---

## Troubleshooting

### GPT-OSS Models: Empty `content` with Populated `reasoning_content`

**Symptoms:**
When using `gpt-oss` models, responses may have empty `content` fields but populated `reasoning_content` fields.

**Root Cause:**
This is a **known issue in llama.cpp** itself ([GitHub Issue #15789](https://github.com/ggml-org/llama.cpp/issues/15789)), not llamesh. The `gpt-oss` models use the "Harmony" response format ([OpenAI Cookbook](https://cookbook.openai.com/articles/openai-harmony)) which includes multiple channels:

* **`analysis` channel**: Chain-of-thought reasoning (mapped to `reasoning_content`)
* **`final` channel**: User-facing response (mapped to `content`)
* **`commentary` channel**: Function/tool calls

Certain versions of llama.cpp have a bug where the model generates content in the `analysis` channel but fails to produce output in the `final` channel, resulting in empty responses.

**The proxy is not causing this issue.** llamesh:
1. Passes through whatever `llama_server_args` are configured in the cookbook (including `--jinja` and `--reasoning-format auto` if specified)
2. Forwards responses verbatim without any modification
3. Does not filter or transform `content` or `reasoning_content` fields

**Solutions:**

1. **Update llama.cpp**: Build the latest version of llama.cpp, as the bug may be fixed in newer releases.

2. **Downgrade llama.cpp**: If you're on an affected version (e.g., build 6360/`3de0082`), try reverting to an earlier version (e.g., build 6115/`50aa938`).

3. **Verify cookbook configuration**: Ensure your gpt-oss model profiles include these flags:
   ```yaml
   llama_server_args: "--jinja --reasoning-format auto --chat-template-kwargs '{\"reasoning_effort\": \"high\"}'"
   ```

4. **Check llama.cpp logs**: The proxy logs llama-server output. Look for `Chat format: GPT-OSS` to confirm the Harmony format is being recognized correctly.

5. **Monitor upstream**: Track [llama.cpp GitHub issues](https://github.com/ggml-org/llama.cpp/issues) for fixes related to gpt-oss and Harmony format handling.
