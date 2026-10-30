use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::UiState;
use super::terminal::{AppTerminal, ScreenMode};

pub fn draw(frame: &mut Frame, state: &UiState, mode: ScreenMode) {
    match mode {
        ScreenMode::Inline => draw_composer(frame, state, frame.area()),
        ScreenMode::Fullscreen => {
            let area = frame.area();
            let composer_height = area.height.min(4);
            let transcript_height = area.height.saturating_sub(composer_height);
            draw_transcript(frame, state, area, transcript_height);
            let composer_area = Rect {
                x: area.x,
                y: area.y + transcript_height,
                width: area.width,
                height: composer_height,
            };
            draw_composer(frame, state, composer_area);
        }
    }
}

fn draw_transcript(frame: &mut Frame, state: &UiState, area: Rect, height: u16) {
    if height == 0 {
        return;
    }
    let width = area.width.max(1) as usize;
    let mut rows: Vec<String> = Vec::new();
    for line in &state.lines {
        rows.extend(wrap(line, width));
    }
    let start = rows.len().saturating_sub(height as usize);
    let visible = &rows[start..];
    let lines: Vec<Line> = visible.iter().map(|row| Line::from(row.clone())).collect();
    let transcript_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height,
    };
    frame.render_widget(Paragraph::new(Text::from(lines)), transcript_area);
}

/// Draw the 3-row composer plus one status row.
fn draw_composer(frame: &mut Frame, state: &UiState, area: Rect) {
    let input_height = area.height.saturating_sub(1).max(1);
    let width = area.width.saturating_sub(2).max(1) as usize;
    let rows = wrap(&state.input, width);
    let start = rows.len().saturating_sub(input_height as usize);
    let visible = &rows[start..];

    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let prefix = if index == 0 { "› " } else { "  " };
            Line::from(format!("{prefix}{row}"))
        })
        .collect();
    let input_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: input_height,
    };
    frame.render_widget(Paragraph::new(Text::from(lines)), input_area);

    let last = visible.last().map_or("", String::as_str);
    let column = (2 + UnicodeWidthStr::width(last)) as u16;
    frame.set_cursor_position(Position::new(
        area.x + column.min(area.width.saturating_sub(1)),
        area.y + input_height - 1,
    ));

    let status = if let Some(approval) = &state.approval {
        format!(
            "⚠ {}: {} · y allow · a allow for session · n deny",
            approval.request.tool,
            truncate(&approval.request.detail, 60)
        )
    } else if state.question.is_some() {
        format!("{} · answer the question · Enter send", state.model)
    } else if state.busy {
        let queued = if state.queued.is_some() {
            " · 1 queued"
        } else {
            ""
        };
        format!("{} · working…  Esc cancel{queued}", state.model)
    } else {
        format!(
            "{} · {} · Enter send · Ctrl+J newline · !shell · /help",
            state.model, state.cwd
        )
    };
    let status_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::new().fg(Color::DarkGray)),
        status_area,
    );
}

/// In inline mode, print queued transcript lines above the viewport. In
/// fullscreen mode, append them to the in-memory transcript.
pub fn flush_pending(
    terminal: &mut AppTerminal,
    state: &mut UiState,
    mode: ScreenMode,
) -> Result<()> {
    if state.pending.is_empty() {
        return Ok(());
    }
    if mode == ScreenMode::Fullscreen {
        state.lines.append(&mut state.pending);
        return Ok(());
    }

    let width = terminal.size()?.width.max(1) as usize;
    let pending = std::mem::take(&mut state.pending);
    let mut rows: Vec<String> = Vec::new();
    for line in &pending {
        rows.extend(wrap(line, width));
    }
    if rows.is_empty() {
        return Ok(());
    }
    let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
    if let Err(error) = terminal.insert_before(height, |buffer| {
        let text = Text::from(
            rows.iter()
                .map(|row| Line::from(row.clone()))
                .collect::<Vec<_>>(),
        );
        Paragraph::new(text).render(buffer.area, buffer);
    }) {
        // Put the lines back so a caller that falls back to fullscreen does
        // not lose the transcript.
        state.pending = pending;
        return Err(error.into());
    }
    Ok(())
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(limit).collect();
    shortened.push('…');
    shortened
}

/// Hard-wrap by display width so CJK and emoji never exceed the terminal.
fn wrap(input: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in input.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for character in line.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if current_width + character_width > width && !current.is_empty() {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(character);
            current_width += character_width;
        }
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_ascii_at_width() {
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
    }

    #[test]
    fn wraps_wide_characters() {
        assert_eq!(wrap("中文", 2), vec!["中", "文"]);
    }

    #[test]
    fn preserves_blank_lines() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }
}
