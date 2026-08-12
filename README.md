# Switchyard

Switchyard is a local, provider-agnostic gateway for Claude Code. Claude Code talks to one stable local Anthropic-compatible endpoint; Switchyard selects the configured upstream provider and model.

## Local development

Copy `config.example.json` to `~/.config/switchyard/config.json`, set the credential environment variables named by each provider, and start the gateway:

```bash
mkdir -p ~/.config/switchyard
cp config.example.json ~/.config/switchyard/config.json
export MOONSHOT_API_KEY=...
export MODEL_API_KEY=...
export QWEN_API_KEY=...
cargo run -- --config ~/.config/switchyard/config.json
```

The default listener is `127.0.0.1:3456`, which keeps the gateway local inside WSL. Claude Code model selections use `provider/model`, for example `kimi/kimi-k3[1m]`, `muse/muse-spark-1.2-contributor`, or `qwen/qwen3.8-max`. The gateway exposes `/v1/models` so a client can discover manually configured models without an upstream model-list request.

## Worktrees

- `switchyard` — integration baseline
- `switchyard-codex` — Claude Code gateway/runtime, branch `codex/gateway`
- `switchyard-muse` — provider registry and adapters, branch `muse/providers`

The project is Claude Code-only for v1. Do not add integrations for other coding agents until the core workflow is working.

## Security

Provider credentials stay in environment variables or a local secret store. Never commit API keys, `.env` files, request headers, or raw authenticated logs.
