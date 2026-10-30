use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenMode {
    /// Bottom-anchored viewport; finished transcript lines are inserted into
    /// the terminal's scrollback.
    Inline,
    /// Fallback when the terminal does not answer cursor-position queries
    /// (e.g. a detached multiplexer). The transcript lives in memory.
    Fullscreen,
}

/// Enter raw mode, enable bracketed paste and key disambiguation, and create
/// an inline viewport. Falls back to a fullscreen (non-alternate) viewport when
/// the inline viewport cannot obtain a cursor-position report.
pub fn init() -> Result<(AppTerminal, ScreenMode)> {
    enable_raw_mode().context("enable raw mode")?;

    let backend = CrosstermBackend::new(io::stdout());
    match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(4),
        },
    ) {
        Ok(terminal) => {
            enable_modes();
            Ok((terminal, ScreenMode::Inline))
        }
        Err(_) => {
            let terminal = init_fullscreen()?;
            Ok((terminal, ScreenMode::Fullscreen))
        }
    }
}

pub fn init_fullscreen() -> Result<AppTerminal> {
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend).context("create fullscreen terminal")?;
    enable_modes();
    Ok(terminal)
}

fn enable_modes() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, EnableBracketedPaste, EnableFocusChange);
    // Best effort: legacy terminals ignore the kitty protocol escape.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
}

pub fn restore() {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableBracketedPaste,
        Show
    );
    let _ = disable_raw_mode();
}

/// Restores the terminal on every exit path, including unwinding panics.
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}
