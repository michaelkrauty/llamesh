# llamesh

An OpenAI-compatible mesh proxy for [llama.cpp](https://github.com/ggml-org/llama.cpp). It manages `llama-server` instances across one or more machines, handling spawn/evict lifecycle, load balancing, and cluster routing — while exposing a standard OpenAI API to clients.

Point any OpenAI-compatible client at llamesh and it handles the rest: spinning up the right model, routing to the best available instance, and tearing it down when idle.

## Features

- **OpenAI-compatible API** — `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/models`
- **Automatic instance management** — on-demand spawn, idle eviction, health monitoring
- **Multi-node mesh** — zero-config LAN discovery (mDNS) or explicit WAN peers, encrypted with Noise Protocol
- **Model profiles** — configure multiple profiles per model (e.g. `fast` vs `quality`) with different llama-server args
- **Resource guardrails** — VRAM and system memory tracking prevents OOM
- **Hot-reload cookbook** — add/modify models without restarting
- **Auto-build llama.cpp** — clones, builds, smoke tests, and atomically swaps binaries
- **Hugging Face integration** — download models automatically via `hf_repo`/`hf_file`
- **Streaming** — SSE streaming with backpressure, forwarded verbatim
- **Metrics & health** — Prometheus metrics, JSON snapshots, `/healthz` and `/readyz` probes
- **Security** — TLS, API key auth, Noise Protocol encryption for inter-node traffic

## Quick Start

### Build

```bash
git clone https://github.com/michaelkrauty/llamesh.git
cd llamesh
cargo build --release
```

**Requirements:** Rust 1.80+, CMake, C/C++ compiler, git

### Configure

Create a minimal `config.yaml`:

```yaml
node_id: "my-node"
listen_addr: "0.0.0.0:8080"
max_vram_mb: 24000
max_sysmem_mb: 64000

llama_cpp:
  repo_url: "https://github.com/ggml-org/llama.cpp.git"
  build_args:
    - "-DGGML_CUDA=ON"
  enabled: true
```

Create a `cookbook.yaml` with your models:

```yaml
models:
  - name: "my-model"
    profiles:
      - id: "default"
        model_path: "./models/my-model.gguf"
        llama_server_args: "-c 32768 -fa on"
```

### Run

```bash
./target/release/llamesh --config ./config.yaml --cookbook ./cookbook.yaml
```

### Use

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "my-model",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
  }'
```

Or with the OpenAI Python client:

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="unused")

response = client.chat.completions.create(
    model="my-model",
    messages=[{"role": "user", "content": "Hello!"}],
    stream=True,
)

for chunk in response:
    print(chunk.choices[0].delta.content or "", end="", flush=True)
```

## Multi-Node Mesh

Enable clustering to spread load across machines. Nodes discover each other and route requests to wherever capacity is available.

**Zero-config LAN** — just enable it:

```yaml
cluster:
  enabled: true
```

**Explicit WAN peers:**

```yaml
cluster:
  enabled: true
  peers: ["other-node.example.com:8080"]
```

Inter-node traffic is encrypted with Noise Protocol (keys auto-generated, Trust-On-First-Use by default).

## Model Profiles

Request a specific profile with `model:profile` syntax:

```json
{ "model": "my-model:fast", ... }
```

Define profiles in the cookbook to trade off speed vs quality, context size, quantization, etc. — each profile maps to a distinct set of `llama-server` args.

## Hugging Face Models

Download models automatically instead of managing files manually:

```yaml
models:
  - name: "qwen2.5-0.5b"
    profiles:
      - id: "default"
        hf_repo: "ggml-org/Qwen2.5-0.5B-Instruct-GGUF"
        hf_file: "qwen2.5-0.5b-instruct-q4_k_m.gguf"
        llama_server_args: "-c 32768 -fa on"
```

## Environment Variable Overrides

Config values can be overridden with environment variables using the `LLAMESH_` prefix:

```bash
LLAMESH_NODE_ID=my-node LLAMESH_MAX_VRAM_MB=48000 ./target/release/llamesh --config ./config.yaml --cookbook ./cookbook.yaml
```

Use `__` (double underscore) for nested fields: `LLAMESH_CLUSTER__ENABLED=true`.

## API Endpoints

| Endpoint | Description |
|---|---|
| `POST /v1/chat/completions` | Chat completions (streaming supported) |
| `POST /v1/completions` | Text completions (streaming supported) |
| `POST /v1/embeddings` | Embeddings (requires `--embedding` profile) |
| `POST /v1/rerank` | Reranking (requires `--reranking` profile) |
| `GET /v1/models` | List available models |
| `GET /healthz` | Health check |
| `GET /readyz` | Readiness check |
| `GET /metrics` | Prometheus metrics |
| `GET /metrics/json` | JSON metrics snapshot |
| `GET /cluster/nodes` | Cluster state |
| `POST /admin/prewarm` | Pre-warm a model/profile |
| `POST /admin/rebuild-llama` | Trigger llama.cpp rebuild |

## Documentation

- **[SPEC.md](SPEC.md)** — Full technical specification: configuration reference, request lifecycle, routing algorithms, resource management, cluster design, security model, and implementation details.
- **[config.example.yaml](config.example.yaml)** — Annotated example configuration.
- **[cookbook.example.yaml](cookbook.example.yaml)** — Annotated example cookbook with model definitions.

## License

This project is licensed under the [GNU Affero General Public License v3.0](LICENSE).

Commercial licensing is available for organizations that need to use llamesh without AGPL obligations. [Open an issue](https://github.com/michaelkrauty/llamesh/issues) to inquire.
