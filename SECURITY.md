# Security Policy

## Supported Versions

Only the latest release receives security fixes. llamesh moves quickly and
upgrades are designed to be drop-in: counters and learned state persist
across in-place upgrades, and configuration is backwards compatible within a
major version.

## Reporting a Vulnerability

Please report vulnerabilities privately via GitHub's
[private vulnerability reporting](https://github.com/michaelkrauty/llamesh/security/advisories/new)
("Report a vulnerability" under the repository's **Security** tab). Do not
open a public issue for security reports.

Reports are triaged on a best-effort basis. A fix, advisory, or explanation
of why the report is out of scope will follow as quickly as practical.

## Security Model

llamesh's threat model and security posture are described in detail in
[SPEC.md](SPEC.md) (see *Security Model*). The short version:

- **Client authentication is optional.** When `auth` is configured, API keys
  are required on client-facing endpoints and compared in constant time.
  Without it, anyone who can reach the listen address can use the API and
  the admin endpoints — deploy on a trusted network or behind your own
  authenticating proxy if you leave auth off.
- **Inter-node traffic** is encrypted with the Noise Protocol by default
  (Trust-On-First-Use key exchange; key files are created with mode 600),
  and mTLS is available as an alternative.
- **TLS** for client traffic is optional and configured per node.
- llamesh spawns and supervises `llama-server` processes and builds
  llama.cpp from source when `llama_cpp.enabled` is set; the build pipeline
  fetches from the configured git remote. Pin `llama_cpp.branch` to a tag or
  commit if you need reproducible, reviewable builds.

## Out of Scope

- Vulnerabilities in [llama.cpp](https://github.com/ggml-org/llama.cpp)
  itself — report those upstream.
- Deployments that expose an unauthenticated llamesh directly to untrusted
  networks; authentication exists for that purpose.
