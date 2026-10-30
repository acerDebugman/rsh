# Use an inline viewport, not a fullscreen alternate buffer

rsh draws a bottom-anchored inline viewport and pushes finished transcript lines into the terminal's own scrollback, instead of taking over the alternate screen. This keeps native scrollback, selection, and copy working, and makes the terminal feel like a shell; the cost is no mouse handling and no persistent side panels.

## Considered Options

- **Fullscreen alternate buffer (CodeWhale-style)**: richer panels and mouse support, but takes over scrollback and selection, and needs a full terminal-compatibility matrix.
- **Line-based REPL (readline-style)**: cheapest, but cannot render streaming tool activity or a multi-line composer.

## Consequences

- No mouse capture; no side panels; overlays must be transient.
- Long transcripts rely on terminal scrollback rather than an in-app pager.
- The TUI must still own raw mode, bracketed paste, and terminal restoration.
