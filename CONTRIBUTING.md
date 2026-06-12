# Contributing to llamesh

Thanks for your interest in improving llamesh. This document covers the
practical conventions the project follows.

## Building and Testing

Requirements: Rust 1.88+, CMake, a C/C++ compiler, and git.

```bash
cargo build --release

# Unit tests (llamesh is a binary crate; there is no library target)
cargo test --bin llamesh

# Integration tests need the mock llama-server built first
cargo build --release --bin mock_llama_server
cargo test --test integration_test
```

## Quality Gates

The tree is kept clean against all three of these — please make sure your
change keeps them that way:

```bash
cargo fmt --check    # zero diffs
cargo clippy         # zero warnings
cargo test           # all tests pass
```

Any new warning or formatting diff is attributable to the change that
introduced it.

## Pull Requests

- **Open a tracking issue first** describing the problem or proposal, and
  close it from the PR (`Fixes #NNN`).
- **One logical change per PR.** Unrelated cleanups belong in their own PR.
- **Conventional commit titles**: `fix:`, `feat:`, `refactor:`, `docs:`,
  with an optional scope (e.g. `fix(security):`).
- **Version with the change**: bump `Cargo.toml` (SemVer — `fix:` is a
  patch, `feat:` is a minor), include the regenerated `Cargo.lock`, and add
  a dated release section to `CHANGELOG.md` (Keep a Changelog format) as
  part of the PR, not after it.
- **Tests accompany behavior changes.** Bug fixes include a test that fails
  without the fix where practical. Concurrency changes deserve particular
  scrutiny — the request path is lock-order sensitive (see the lock-ordering
  documentation at the top of `src/node_state/mod.rs`).
- Pull requests receive automated review feedback; please address findings
  (or explain why they do not apply) before merge.

## Persistence Compatibility

New fields on persisted state (`node-metrics.json`, written by
`src/metrics.rs`) must carry `#[serde(default)]` so snapshots written by
older versions still load — a parse failure silently resets all persisted
counters on upgrade.

## Documentation

`SPEC.md` is the technical specification; update it when behavior it
describes changes. `config.example.yaml` and `cookbook.example.yaml` are the
annotated configuration references; new configuration fields should appear
there with comments and defaults.
