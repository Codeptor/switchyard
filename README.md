# Switchyard

Switchyard is a local, provider-agnostic gateway for Claude Code. Claude Code talks to one stable local Anthropic-compatible endpoint; Switchyard selects the configured upstream provider and model.

## Worktrees

- `switchyard` — integration baseline
- `switchyard-codex` — Claude Code gateway/runtime, branch `codex/gateway`
- `switchyard-muse` — provider registry and adapters, branch `muse/providers`

The project is Claude Code-only for v1. Do not add integrations for other coding agents until the core workflow is working.

## Security

Provider credentials stay in environment variables or a local secret store. Never commit API keys, `.env` files, request headers, or raw authenticated logs.
