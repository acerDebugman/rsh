# v1 fence: scope, permission floor, and test gate

rsh v1 is the existing M5 tree (nine-tool closed set, one agent loop, skill loader, native/dsh subagents, JSONL sessions, inline viewport) **converged and hardened**, not rewritten. This ADR records the three fences that make v1 finishable.

## Scope fence

- **Baseline**: the current repository. v1 is "one developer's daily coding turn works on Linux/macOS", single provider (OpenAI-compatible, `deepseek-flash`).
- **Out of v1**: MCP, multi-provider abstraction, Windows (`bash.rs` Job Object TODO stays), subagent depth > 1, image input, plugin marketplace, built-in web search.
- Adding a tenth tool, a second provider, or a second orchestration layer is a v2 discussion, not a v1 task.

## Permission floor

`danger-full-access` is the default and never prompts. Below it sits an **unremovable floor** — these are invariants, not approvals, and have no off switch:

1. Every tool call (name, args, outcome) is appended to `events.jsonl`; the session is replayable from it.
2. `bash` has a wall-clock timeout and kills the whole process group on expiry/cancel.
3. Privilege escalation (`sudo`, `su`, `doas`) is refused in every mode.

`ask` and `read-only` stay first-class and tested. The floor is deliberately *not* "confirm before touching paths outside the workspace": that would silently turn full access back into `ask`.

## Test gate

A release is green when:

- `cargo test` passes fully offline against a fake model (contracts, tool semantics, permission modes, subagent handshake with a fake ACP peer).
- One PTY-driven TUI test passes: `Enter`, `Ctrl+J`, `Esc`, `!cmd`, and bracketed paste keep their documented meanings.
- The behavioural e2e ("build a Python todo CLI with tests") runs on the **native** backend only. `dsh` is covered by contract tests of the handshake and the `acp → dst stream-json → headless` downgrade chain.
- Live tests requiring a real key are `#[ignore]`d and run manually before a release; a normal test run must never report them as passing.

## Considered Options

- **Rewrite on a smaller core**: cleaner, but throws away contract tests that already encode the nine-tool fence.
- **Zero constraints under full access**: simplest, but forfeits the only payoff of the JSONL design — auditability — and no coding task needs root.
- **Real-model e2e in the gate**: makes release depend on an external CLI's availability.

## Consequences

- The floor costs a small, constant amount of code and is cheap to test.
- `dsh` regressions show up in contract tests, not in behaviour e2e; the downgrade chain must therefore be asserted explicitly.
- Live coverage is manual, so provider drift is caught late — accepted, because a paid, networked gate is not worth the flakiness.
