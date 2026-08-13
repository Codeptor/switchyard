# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Switchyard is a local, provider-agnostic Anthropic Messages gateway for Claude Code. Claude Code talks to one stable local endpoint (`127.0.0.1:3456`); Switchyard routes `provider/model` selections to configured upstream providers via an Anthropic-compatible adapter. v1 is Claude Code-only — do not add integrations for other coding agents.

## Commands

```bash
cargo build                          # debug build
cargo test                           # all tests (unit + integration)
cargo test --test providers          # provider-layer mock integration tests
cargo test --test gateway            # gateway HTTP tests (tower oneshot, no network)
cargo test <name>                    # single test by name substring
cargo clippy -- -D warnings          # lint (must stay clean)
cargo fmt                            # format
cargo install --path . && switchyard init && switchyard   # install + first-run setup + serve
cargo run -- --config ~/.config/switchyard/config.json    # run against a config file
```

## Architecture

Two layers split at a typed boundary, hexagonal/ports-and-adapters style:

**`src/gateway/` — Claude Code-facing side.**
- `backend.rs`: the `Backend` port — `models()` / `complete()` / `stream()` over `BackendRequest { model, body: Value }`. The request body stays opaque JSON at this layer so newer Claude Code fields pass through; `BackendError` maps to HTTP status + Anthropic error shape and redacts secret-looking tokens.
- `http.rs`: axum routes `/health`, `/v1/models`, `POST /v1/messages`. Streaming responses are re-encoded as SSE (`event: {type}\ndata: ...`); mid-stream errors become an `error` event, not a dropped connection.
- `provider_backend.rs`: the only `Backend` impl — splits `provider/model`, resolves via the registry, rewrites the body's `model` to the bare upstream model id, maps `ProviderError` → `BackendError`.
- `runtime.rs`: `ListenConfig` — non-loopback listeners are refused (no auth layer exists yet).

**`src/providers/` — provider layer.**
- `adapter.rs`: the `ProviderAdapter` port (`complete`/`stream` over typed `MessagesRequest` → `MessagesResponse` / `Stream<StreamEvent>`).
- `registry.rs`: `ProviderRegistry` — provider id → adapter + model list + default model. Works with manually configured model IDs only; upstream `/v1/models` is never called. If a provider's model list is non-empty, unknown models are rejected; an empty list accepts any model id.
- `anthropic.rs`: `AnthropicAdapter`, the only concrete adapter. POSTs to `{base_url}/v1/messages` (adapter owns the suffix; config base URLs are provider roots), adds `anthropic-version`, resolves the auth header from the environment at request time (secrets are never stored).
- `stream.rs`: SSE parser + normalization into `StreamEvent`s; unrecognized frames are dropped, not errors.
- `types.rs`: typed Anthropic Messages structures. Unknown content blocks and extra usage counters are preserved (`extra: BTreeMap`, `ContentBlock::Unknown`), which is what keeps Qwen thinking mode and provider extensions working.
- `config.rs` / `credentials.rs` / `error.rs`: config schema + validation, env-var credential loading + header redaction, normalized `ProviderError`.

**CLI / setup (`src/main.rs`, `src/setup.rs`, `src/config.rs`)**: `switchyard init` builds config from built-in presets (Kimi/Muse/Qwen — presets live only in `setup.rs`, they are configuration examples, not core special cases). Config goes to `~/.config/switchyard/config.json`; keys prompted with hidden input go to a separate `credentials.json` (0600), which `apply_credentials` loads into the environment at startup without overriding already-set variables.

### Model routing specifics

- Claude Code selects `provider/model` (e.g. `kimi/kimi-k3[1m]`).
- A trailing `[1m]` context-window suffix is routing metadata: it is stripped before the upstream call, and either form resolves to the same configured model (`model_without_context_suffix` in registry.rs).
- `default_model` per provider is used when no model is given.

## Ownership boundaries (from AGENTS.md / MUSE.md)

- Codex owns the gateway (`src/gateway/`, request lifecycle, WSL launch, gateway integration tests). Meta Muse owns the provider layer (`src/providers/`, provider tests). Coordinate through the two small typed ports (`Backend`, `ProviderAdapter`); don't restructure the other side's files.
- Three worktrees: `switchyard` (master, integration baseline), `switchyard-codex` (`codex/gateway`), `switchyard-muse` (`muse/providers`). Never edit another worktree or rewrite the other branch.

## Testing conventions

- Provider tests run against a local axum mock speaking the Anthropic wire protocol (`tests/providers/mock_tests.rs`, included via `#[path]` from `tests/providers.rs`) — never against real providers.
- Gateway tests use `tower::ServiceExt::oneshot` with a `MockBackend` — no sockets, no upstream.
- CLI tests drive the real binary via `env!("CARGO_BIN_EXE_switchyard")` against temp dirs.

## Security rules

Credentials come from environment variables or the 0600 `credentials.json`; they must never appear in source, commits, fixtures, command arguments, or logs. Redact `Authorization`/`x-api-key`/secret headers in any diagnostic output (`redact_headers`, `sanitize_message`, `sanitize_error_message` exist for this). Keep the listener loopback.

## Commit style

Small imperative commits with scoped prefixes matching history: `feat(provider):`, `fix(gateway):`, `test(provider):`, `docs:`.
