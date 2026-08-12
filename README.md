# Switchyard

Switchyard is a local, provider-agnostic gateway for Claude Code. Claude Code talks to one stable local Anthropic-compatible endpoint; Switchyard selects the configured upstream provider and model.

## Local development

Install/build the binary, then use the first-run CLI. It asks which built-in
providers to enable and reads API keys with hidden input:

```bash
cargo install --path .
switchyard init
switchyard
```

`init` writes `~/.config/switchyard/config.json` and, when keys are entered,
stores them separately in `~/.config/switchyard/credentials.json` with `0600`
permissions. Existing environment variables take precedence, and keys are
never written into the provider config or printed. Use `--force` to replace an
existing setup. For a config template without key prompts:

```bash
switchyard init --all --no-credentials
```

Choose presets non-interactively with `--provider kimi`, `--provider muse`, or
`--provider qwen`; repeat the flag or use a comma-separated list.

Manual setup remains supported:

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

## CLI commands

### `switchyard doctor`

Pre-flight checks for config, credentials, provider reachability, and gateway
health. Prints a table with PASS/WARN/FAIL markers. Exits 1 when the config is
invalid or a required credential is missing; reachability failures are WARN
and do not affect the exit code:

```bash
switchyard doctor
switchyard doctor --config /path/to/config.json
```

### `switchyard models`

Lists the model routes exposed by a running gateway:

```bash
switchyard models
switchyard models --host 127.0.0.1 --port 3456 --token my-token
```

### `switchyard status`

Shows gateway health, version, git SHA, and usage summary from a running
instance:

```bash
switchyard status
switchyard status --host 127.0.0.1 --port 3456
```

### `switchyard usage`

Tabular per-provider, per-model, per-day usage counters from a running gateway:

```bash
switchyard usage --host 127.0.0.1 --port 3456
```

### `switchyard service install|uninstall`

Manage a systemd user service unit for switchyard. `install` writes
`~/.config/systemd/user/switchyard.service` and prints the follow-up
`systemctl` commands; `uninstall` removes the unit file:

```bash
switchyard service install --config ~/.config/switchyard/config.json
switchyard service uninstall
```

### `switchyard --version`

Prints the version and embedded git short SHA: `0.1.0 (abc1234)`.

## Model aliases

Map short names to `provider/model` routes in the root config:

```json
{
  "providers": [ ... ],
  "aliases": {
    "fast": "qwen/qwen3.6-flash"
  }
}
```

Aliases resolve before routing, appear in `/v1/models`, and work anywhere a
route does (`claude --model fast`, `/model fast`, `ANTHROPIC_MODEL=fast`).
Substitution is single-step: an alias target is not re-resolved as an alias.

Note on Claude Code model discovery: with
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`, Claude Code only shows
gateway models whose id contains `claude` or `anthropic` in its `/model`
picker. To surface a provider model there, add an alias with a matching
name, e.g. `"claude-k3": "kimi/kimi-k3[1m]"`. Env-var model selection
(`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL`) is unaffected by this
filter.

## Fallback routing

Configure ordered fallback routes for transient upstream failures. When a
request returns 429, 500, 502, 503, 529, or the provider is unreachable, the
gateway retries each fallback target in order:

```json
{
  "providers": [ ... ],
  "fallbacks": {
    "kimi/kimi-k3[1m]": ["qwen/qwen3.8-max", "muse/muse-spark-1.2-contributor"]
  }
}
```

Client errors (400, 401, 403, 404) and model-not-found responses pass through
without fallback. For streaming requests, fallback only applies to stream
creation failures — mid-stream errors are not retried.

## Authentication

Pass `--token <secret>` (or set `SWITCHYARD_TOKEN`) to require
`Authorization: Bearer <token>` on `/v1/*` routes. The `/health` endpoint
stays open. When a token is configured, non-loopback bind addresses are
permitted:

```bash
switchyard --host 0.0.0.0 --port 3456 --token my-secret-token
```

Without `--token`, the gateway refuses non-loopback binds to keep provider
credentials local.

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
