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

Point Claude Code at Switchyard once per shell (or put these exports in your WSL shell profile):

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3456
export ANTHROPIC_AUTH_TOKEN=switchyard-local
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

Then start Claude Code with a model or switch inside the TUI without editing Claude Code settings:

```bash
claude --model 'kimi/kimi-k3[1m]'
```

```text
/model qwen/qwen3.8-max
/model muse/muse-spark-1.2-contributor
```

For Qwen's 1M-context plan, use the `[1m]` model ID when that model supports it and follow [QwenCloud's Claude Code guide](https://docs.qwencloud.com/developer-guides/clients-and-developer-tools/claude-code) for the matching context setting.

For the Qwen Token Plan example in that guide, the matching WSL export is:

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=983616
```

## Provider compatibility

The adapter sends `POST {base_url}/v1/messages`, adds the Anthropic version
header, and uses the auth header declared in `config.json`. The example bases
are deliberately the provider roots so the adapter owns the `/v1/messages`
suffix:

- Kimi K3: `https://api.moonshot.ai/anthropic`
- Muse Spark: `https://api.meta.ai`
- QwenCloud Token Plan: `https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic`

The wire boundary forwards provider-specific request fields, unknown content
blocks, thinking/signature stream deltas, and provider usage counters. That is
needed for Qwen thinking mode and keeps the same adapter usable for Muse and
Kimi extensions. Qwen documents both `Authorization: Bearer ...` and
`x-api-key`; if an account requires the latter, change only that provider's
`auth.header` to `x-api-key` and set `prefix` to `null`.

Useful upstream references:

- [QwenCloud Claude Code integration](https://docs.qwencloud.com/developer-guides/clients-and-developer-tools/claude-code)
- [Kimi Claude Code integration](https://platform.kimi.ai/docs/guide/claude-code-kimi)
- [Meta Muse Spark API announcement](https://ai.meta.com/blog/introducing-muse-spark-meta-model-api/)

## Worktrees

- `switchyard` — integration baseline
- `switchyard-codex` — Claude Code gateway/runtime, branch `codex/gateway`
- `switchyard-muse` — provider registry and adapters, branch `muse/providers`

The project is Claude Code-only for v1. Do not add integrations for other coding agents until the core workflow is working.

## Security

Provider credentials stay in environment variables or a local secret store. Never commit API keys, `.env` files, request headers, or raw authenticated logs.
