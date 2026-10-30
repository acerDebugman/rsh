# Default to full access for the main agent and subagents

rsh defaults to `danger-full-access`: the main agent runs tool calls without asking, and subagents run with full filesystem and network access, including ACP permission requests auto-answered with allow-once. The `ask` and `read-only` modes remain available in config, but they are not the default.

## Considered Options

- **Ask before every shell command**: safer, but turns an autonomous coding loop into an approval dialog and breaks unattended tests.
- **Workspace-write by default**: a middle ground, but out-of-workspace reads and network calls still block common tasks.

## Consequences

- A bad command can delete files or leak data without confirmation; the README and startup banner must state this.
- E2E tests can run unattended, which is required for the project and grill-me tests.
- Users who want guardrails must opt into `ask` or `read-only` explicitly.
