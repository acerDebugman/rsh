# Drive dsh subagents over ACP, not by scraping its TUI

The dsh TUI exposes no machine interface, but dsh ships an `acp` profile that speaks ACP v1 over newline-delimited JSON-RPC on stdio, and the dsh-tui package ships `dst run --output-format stream-json` for one-shot automation. rsh drives `dsh --profile acp` as the primary subagent backend and falls back to `dst run` only when spawning or handshaking fails; it never drives the interactive TUI through a pty.

## Considered Options

- **PTY + screen scraping of `dsh-tui`**: no structured events, no session id, breaks on every TUI change, and approval modals block unattended runs.
- **`dsh --profile headless`**: simpler, but no persistent session and weaker structured events than ACP.
- **ACP only**: clean, but a single spawn/version failure would leave no backend.

## Consequences

- Node cold start adds roughly two seconds per subagent; sessions are per task in v1.
- ACP request ids from the agent start at 0 and collide with client ids; dispatch must be by message shape.
- The model must be set with `session/set_config_option` using the JSON-stringified `["provider","model"]` tuple.
- Fallback is recorded in the subagent result so failures are diagnosable.
