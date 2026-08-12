# Instructions for Meta Muse

You are working as the provider-layer contributor for Switchyard.

## Worktree and branch

Work only in:

```text
/home/esoteric/switchyard-muse
branch: muse/providers
```

Do not edit `/home/esoteric/switchyard`, `/home/esoteric/switchyard-codex`, or the `codex/gateway` branch. Do not reset, rebase, force-push, or rewrite shared history.

## Product goal

Switchyard is a local gateway used only by Claude Code in v1. Claude Code will call one stable local Anthropic-compatible endpoint. Switchyard then forwards the request to whichever provider and model the user selected.

The implementation must be provider/model agnostic. Kimi, Meta Muse, and Qwen are merely initial configurations. Do not hardcode their names, URLs, authentication schemes, or model behavior into the core provider layer.

## Your ownership

Own the provider side of the boundary:

- provider and model configuration schema
- validation and safe normalization of endpoint/auth settings
- typed provider and adapter interfaces
- Anthropic Messages-compatible upstream adapter
- streaming, tool-use, stop-reason, usage, and upstream-error normalization
- provider-level unit tests and protocol fixtures

Keep your changes under provider-specific paths where possible, such as `src/providers/` and `tests/providers/`. If the repository chooses a different layout, preserve the same ownership boundary and document it in the handoff.

## Coordination contract

Codex owns the Claude Code-facing gateway, request lifecycle, routing/session behavior, WSL launch behavior, and gateway integration tests. Coordinate through small stable interfaces rather than editing Codex-owned files.

Before changing a shared interface:

1. Inspect the current code and tests.
2. Write the proposed contract in your response or a focused design note.
3. Keep the interface minimal: provider identity, model identity, request forwarding, streaming events, and normalized errors.

If a provider does not implement model discovery, the registry must still work with manually configured model IDs. Do not require `/v1/models`.

## Security rules

- Never write API keys or bearer tokens to files, commits, shell arguments, fixtures, or logs.
- Read credentials from environment variables or the project secret mechanism.
- Redact `Authorization`, `x-api-key`, and provider-specific secret headers in errors and test output.
- Do not send real provider requests during tests unless a test is explicitly opt-in.

## Quality bar

- Preserve Claude Code semantics for streaming text, tool calls, stop reasons, usage, and errors.
- Prefer typed data structures over provider-specific string maps at the core boundary.
- Keep provider quirks inside adapters.
- Add tests for malformed config, missing credentials, streaming chunks, tool calls, upstream non-2xx responses, and timeouts.
- Run the repository formatter, linter, type checker, and test suite before handoff.

## Commit and handoff

Use small commits with imperative scoped messages, for example:

```text
feat(provider): add anthropic upstream adapter
test(provider): cover streamed tool calls
```

Every handoff must include:

- summary of behavior
- changed files
- tests and commands run
- interface changes Codex must consume
- known limitations or follow-up work
- commit SHA
