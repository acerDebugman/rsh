# Persist sessions as append-only JSONL with compaction events

Each session is a JSONL file of typed events (user message, assistant message, tool result, compaction, turn end), appended as the turn runs. A `compaction` event records the summary and how many old messages it replaces, so replaying the log rebuilds the exact context the model saw. There is no database in v1.

## Considered Options

- **SQLite**: better querying and concurrent access, but a heavier dependency and schema migrations for a single-user CLI.
- **In-memory only**: simpler, but `/compact` and `--resume` both require durable history.

## Consequences

- Event schema is versioned with a top-level `v`; unknown event types are ignored with a warning.
- Replay must apply compaction boundaries exactly, or resumed sessions diverge from the original.
- Logs grow with tool output; large tool results are truncated before being written.
