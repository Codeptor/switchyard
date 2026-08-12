# Switchyard project instructions

## Scope

Build a local gateway used by Claude Code. Keep the provider and model registry generic; Kimi, Muse, and Qwen are configuration examples, not special cases in core code.

## Ownership

- Codex: Claude Code-facing Anthropic Messages gateway, request lifecycle, model selection, WSL launch/service behavior, and gateway integration tests.
- Meta Muse: provider registry, provider configuration validation, upstream adapter contract, Anthropic-compatible adapters, and provider-level tests.
- Integration: Codex owns the merge/review path. Neither worktree edits the other worktree or rewrites the other branch.

## Boundaries

The gateway must expose one stable local endpoint to Claude Code. Provider code must be replaceable behind a small typed interface. Keep transport, routing, provider authentication, and model metadata separate.

## Security

Never put API keys in source, markdown, shell history, command arguments, test fixtures, or logs. Use environment variables for local development and redact request headers in diagnostics.

## Workflow

Use small imperative commits with scoped prefixes. Run the project formatter, linter, type checker, and tests after each logical change. Keep WSL paths Linux-native inside the worktrees.
