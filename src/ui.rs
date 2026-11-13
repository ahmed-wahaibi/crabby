//! Drawing only: turns a [`State`] into a frame. It changes nothing in the state.

use crate::{
    app::{Entry, State},
    commands, display, mascot,
};
use ratatui::{
    layout::{Constraint, Layout, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
/// Most rows the input box grows to.
const MAX_INPUT_ROWS: usize = 6;
const MAX_MENU_ROWS: usize = 9;

/// Tabs become spaces and other control characters go: they would corrupt the screen.
fn clean(s: &str) -> String {
    s.replace('\t', "    ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Splits one line into rows no wider than `width` columns. With `words`, a row
/// breaks at its last space when it has one; otherwise it breaks at the column, which
/// keeps cursor arithmetic exact for the input box.
pub fn wrap(line: &str, width: usize, words: bool) -> Vec<String> {
    let width = width.max(1);
    let (mut rows, mut cur, mut used) = (vec![], String::new(), 0);
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width && !cur.is_empty() {
            let space = cur
                .rfind(' ')
                .filter(|&i| words && ch != ' ' && !cur[..i].trim().is_empty());
            match space {
                Some(i) => {
                    let tail = cur.split_off(i + 1);
                    rows.push(cur.trim_end().to_string());
                    used = tail.width();
                    cur = tail;
                }
                None => {
                    rows.push(std::mem::take(&mut cur));
                    used = 0;
                }
            }
        }
        if words && ch == ' ' && cur.is_empty() && !rows.is_empty() {
            continue; // no space at the start of a continuation row
        }
        cur.push(ch);
        used += w;
    }
    rows.push(cur);
    rows
}

fn style(color: bool, fg: Color) -> Style {
    if color {
        Style::default().fg(fg)
    } else {
        Style::default()
    }
}

/// How one transcript entry looks: wrapped to `width`, styled.
pub fn entry_lines(entry: &Entry, width: usize, color: bool) -> Vec<Line<'static>> {
    let (prefix, indent, text, st) = match entry {
        Entry::Banner(t) => (
            "",
            "",
            t,
            style(color, Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Entry::User(t) => (
            "> ",
            "  ",
            t,
            style(color, Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Entry::Assistant(t) => ("", "", t, Style::default()),
        Entry::Tool(t) => (
            "  · ",
            "    ",
            t,
            Style::default().add_modifier(Modifier::DIM),
        ),
        Entry::Info(t) => ("", "", t, Style::default().add_modifier(Modifier::DIM)),
        Entry::Error(t) => (
            "error: ",
            "       ",
            t,
            style(color, Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let room = width.saturating_sub(prefix.len()).max(1);
    let mut lines: Vec<Line> = vec![];
    if matches!(entry, Entry::User(_)) {
        lines.push(Line::default());
    }
    let mut first = true;
    for logical in clean_lines(text) {
        for row in wrap(&logical, room, true) {
            let lead = if first { prefix } else { indent };
            first = false;
            lines.push(Line::from(Span::styled(format!("{lead}{row}"), st)));
        }
        if matches!(entry, Entry::Tool(_)) {
            break; // one line per tool call
        }
    }
    lines
}

/// The text split at line breaks, each line cleaned.
fn clean_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| clean(l.trim_end_matches('\r')))
        .collect()
}

pub fn transcript(st: &State, width: usize) -> Vec<Line<'static>> {
    // The crab sits above the banner, so it scrolls away with the transcript.
    let crab = st.mascot
        && st.width >= mascot::MIN_WIDTH
        && st.height >= mascot::MIN_HEIGHT
        && matches!(st.entries.first(), Some(Entry::Banner(_)));
    let top = if crab { mascot::lines() } else { vec![] };
    top.into_iter()
        .chain(
            st.entries
                .iter()
                .flat_map(|e| entry_lines(e, width, st.color)),
        )
        .collect()
}

/// How many lines the whole transcript takes at the current width.
pub fn total_lines(st: &State) -> usize {
    transcript(st, st.width as usize).len()
}

/// The input box's rows and where the cursor is, for a text area `width` wide.
fn input_rows(st: &State, width: usize) -> (Vec<String>, usize, usize) {
    let text = clean_keep_newlines(&st.input);
    let rows: Vec<String> = text
        .split('\n')
        .flat_map(|l| wrap(l, width, false))
        .collect();
    let before = clean_keep_newlines(&st.input[..st.cursor]);
    let before_rows: Vec<String> = before
        .split('\n')
        .flat_map(|l| wrap(l, width, false))
        .collect();
    let (mut row, mut col) = (
        before_rows.len() - 1,
        before_rows.last().map_or(0, |r| r.width()),
    );
    if col >= width {
        (row, col) = (row + 1, 0); // a full row: the cursor sits at the start of the next
    }
    (rows, row, col)
}

fn clean_keep_newlines(s: &str) -> String {
    s.split('\n').map(clean).collect::<Vec<_>>().join("\n")
}

fn status_text(st: &State) -> String {
    let model = st
        .model
        .clone()
        .or_else(|| st.default_model.clone())
        .unwrap_or_else(|| "default".into());
    let mut parts = vec![];
    if st.armed {
        parts.push("press Ctrl+C again to quit".to_string());
    } else if let Some(w) = st.working() {
        let spin = ['|', '/', '-', '\\'][st.tick % 4];
        parts.push(format!("{spin} {w}, Esc stops"));
    }
    if st.scroll > 0 {
        parts.push("scrolled, PageDown returns".into());
    }
    parts.push(model);
    parts.push(
        st.effort
            .map_or("effort default".into(), |e| format!("effort {}", e.name())),
    );
    parts.push(format!("mode {}", st.mode.name()));
    parts.push(format!(
        "{} in / {} out · ${:.4}",
        display::commas(st.totals.input),
        display::commas(st.totals.output),
        st.totals.cost
    ));
    parts.push(st.cwd.clone());
    parts.join(" · ")
}

pub fn draw(f: &mut Frame, st: &State) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "crabby needs a window of at least {MIN_WIDTH}x{MIN_HEIGHT} (now {}x{})",
            area.width, area.height
        );
        f.render_widget(
            Paragraph::new(msg).wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
        return;
    }
    let inner = area.width.saturating_sub(2) as usize;
    let (rows, cursor_row, cursor_col) = input_rows(st, inner);
    let max_rows = (area.height as usize)
        .saturating_sub(6)
        .clamp(1, MAX_INPUT_ROWS);
    let shown_rows = rows.len().clamp(1, max_rows);
    // Keep the cursor's row inside the box.
    let top = (cursor_row + 1).saturating_sub(shown_rows);

    let menu = if !st.asking() && !st.input.contains('\n') {
        commands::matching(&st.input)
    } else {
        vec![]
    };
    let menu_rows = menu
        .len()
        .min(MAX_MENU_ROWS)
        .min((area.height as usize).saturating_sub(shown_rows + 4));

    let [log, menu_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(menu_rows as u16),
        Constraint::Length(shown_rows as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(area);

    // Transcript, pinned to the bottom unless scrolled.
    let lines = transcript(st, log.width as usize);
    let h = log.height as usize;
    let max_scroll = lines.len().saturating_sub(h);
    let scroll = st.scroll.min(max_scroll);
    let start = lines.len().saturating_sub(h + scroll);
    let end = (start + h).min(lines.len());
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), log);

    if menu_rows > 0 {
        let pad = menu.iter().map(|c| c.1.chars().count()).max().unwrap_or(0);
        let text: Vec<Line> = menu
            .iter()
            .take(menu_rows)
            .map(|(_, usage, what)| {
                Line::from(vec![
                    Span::styled(format!("  {usage:<pad$}"), style(st.color, Color::Cyan)),
                    Span::styled(
                        format!("  {what}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(text), menu_area);
    }

    let block = Block::bordered().border_style(if st.busy.is_some() {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    });
    let body: Vec<Line> = if st.input.is_empty() {
        let hint = if st.confirm.is_some() {
            "type yes to confirm"
        } else if st.asking() {
            "type your answer to the question above (Esc skips)"
        } else {
            "Message or /command"
        };
        vec![Line::from(Span::styled(
            hint,
            Style::default().add_modifier(Modifier::DIM),
        ))]
    } else {
        rows.iter()
            .skip(top)
            .take(shown_rows)
            .map(|r| Line::from(r.clone()))
            .collect()
    };
    f.render_widget(Paragraph::new(body).block(block), input_area);
    f.set_cursor_position(Position::new(
        input_area.x + 1 + (cursor_col as u16).min(inner as u16),
        input_area.y + 1 + (cursor_row - top).min(shown_rows - 1) as u16,
    ));

    f.render_widget(
        Paragraph::new(status_text(st)).style(Style::default().add_modifier(Modifier::DIM)),
        status_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{update, Msg};
    use crate::catalog::Agent;
    use ratatui::{backend::TestBackend, Terminal};
    use std::sync::Arc;

    fn state(w: u16, h: u16) -> State {
        let agents = Arc::new(Vec::<Agent>::new());
        State::new(
            agents,
            Some("opus".into()),
            "C:\\proj".into(),
            false,
            (w, h),
            &[],
        )
    }

    fn screen(st: &State) -> Vec<String> {
        let (w, h) = (st.width, st.height);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, st)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn type_text(st: &mut State, s: &str) {
        for c in s.chars() {
            let k = ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            );
            update(st, Msg::Key(k));
        }
    }

    #[test]
    fn wrapping_breaks_at_words_then_at_columns_and_counts_wide_characters() {
        assert_eq!(
            wrap("hello brave new world", 11, true),
            ["hello brave", "new world"]
        );
        assert_eq!(wrap("abcdefghij", 4, true), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("hello brave", 8, false), ["hello br", "ave"]);
        assert_eq!(wrap("  indented", 20, true), ["  indented"]);
        assert_eq!(wrap("", 5, true), [""]);
        // Each wide character is two columns.
        assert_eq!(wrap("日本語日本語", 6, true), ["日本語", "日本語"]);
        // A character wider than the box still makes progress.
        assert_eq!(wrap("日日", 1, true), ["日", "日"]);
    }

    #[test]
    fn an_empty_chat_shows_the_banner_input_and_status() {
        let st = state(100, 12);
        let s = screen(&st);
        assert_eq!(s.len(), 12);
        assert!(s[0].starts_with("crabby "), "{s:?}");
        assert!(s[1].contains("/help"), "{s:?}");
        assert!(s[8].starts_with('┌'), "{s:?}");
        assert!(s[9].contains("Message or /command"), "{s:?}");
        assert!(s[10].starts_with('└'), "{s:?}");
        assert!(
            s[11].contains("opus · effort default · mode read · 0 in / 0 out"),
            "{s:?}"
        );
        assert!(s[11].ends_with("C:\\proj"), "{s:?}");
    }

    #[test]
    fn the_welcome_screen_draws_the_crab_or_only_the_name() {
        let draw_buf = |st: &State| {
            let mut term = Terminal::new(TestBackend::new(st.width, st.height)).unwrap();
            term.draw(|f| draw(f, st)).unwrap();
            term.backend().buffer().clone()
        };
        let rgb = |n: u32| Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8);
        let (red, dark, white) = (rgb(0xe02b2b), rgb(0xa31515), rgb(0xffffff));
        let mut st = state(60, 16);
        st.mascot = true;
        let buf = draw_buf(&st);
        let at = |x: u16, y: u16| &buf[(20 + x, y)]; // 60 wide: the 20-wide sprite starts at 20
                                                     // First row: transparent-over-red, red-over-dark, red-over-transparent, empty.
        assert_eq!((at(0, 0).symbol(), at(0, 0).fg), ("▄", red));
        assert_eq!(
            (at(1, 0).symbol(), at(1, 0).fg, at(1, 0).bg),
            ("▀", red, dark)
        );
        assert_eq!((at(2, 0).symbol(), at(2, 0).fg), ("▀", red));
        assert_eq!((at(3, 0).symbol(), at(3, 0).bg), (" ", Color::Reset));
        // Last row (sprite pixel rows 10 and 11).
        assert_eq!((at(0, 5).symbol(), at(0, 5).bg), (" ", Color::Reset));
        assert_eq!(
            (at(1, 5).symbol(), at(1, 5).fg, at(1, 5).bg),
            ("▀", red, dark)
        );
        // Two equal pixels are a space with a background: no glyph, no seam.
        assert_eq!((at(6, 1).symbol(), at(6, 1).bg), (" ", white));
        // The name follows the sprite, and the input box and status line are intact.
        let s = screen(&st);
        assert!(s[6].starts_with("crabby "), "{s:?}");
        assert!(
            s[12].starts_with('┌') && s[15].contains("mode read"),
            "{s:?}"
        );
        // Too short for the crab, or the mascot off: the name only, no half blocks.
        st.height = 13;
        st.mascot = true;
        let small = screen(&st).join("\n");
        st.height = 16;
        st.mascot = false;
        let off = screen(&st).join("\n");
        for text in [small, off] {
            assert!(
                text.contains("crabby ") && !text.contains(['▀', '▄']),
                "{text}"
            );
        }
    }

    #[test]
    fn a_streaming_reply_shows_dim_tool_lines_and_the_working_status() {
        use crate::app::{Effect, JobEvent};
        let mut st = state(60, 14);
        type_text(&mut st, "hi");
        let k = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        );
        let id = match update(&mut st, Msg::Key(k)).remove(0) {
            Effect::Start { id, .. } => id,
            _ => unreachable!(),
        };
        let ev = |event| Msg::Job { id, event };
        update(
            &mut st,
            ev(JobEvent::Tool {
                name: "Read".into(),
                arg: "src/main.rs".into(),
            }),
        );
        update(
            &mut st,
            ev(JobEvent::Text(
                "The file starts with a long sentence that has to wrap onto another line.".into(),
            )),
        );
        let s = screen(&st);
        let all = s.join("\n");
        assert!(all.contains("> hi"), "{all}");
        assert!(all.contains("  · Read src/main.rs"), "{all}");
        assert!(all.contains("another line."), "{all}");
        assert!(
            s.last().unwrap().starts_with("| replying, Esc stops"),
            "{all}"
        );
    }

    #[test]
    fn typing_a_slash_opens_the_command_menu_above_the_input() {
        let mut st = state(60, 16);
        type_text(&mut st, "/m");
        let s = screen(&st);
        let at = s
            .iter()
            .position(|l| l.contains("/model [name]"))
            .expect("menu");
        assert!(s[at + 1].contains("/mode [read|edit]"), "{s:?}");
        assert!(s[at + 2].starts_with('┌'), "{s:?}");
        assert!(s[at + 3].starts_with("│/m"), "{s:?}");
        assert!(!s.iter().any(|l| l.contains("/clear")));
    }

    #[test]
    fn a_narrow_or_short_window_gets_a_message_not_a_broken_layout() {
        for (w, h) in [(39, 24), (80, 9), (10, 3)] {
            let st = state(w, h);
            let s = screen(&st);
            assert!(s.join(" ").contains("crabby needs"), "{w}x{h}: {s:?}");
        }
        // Exactly the minimum still draws the chat.
        assert!(screen(&state(40, 10))
            .iter()
            .any(|l| l.contains("Message or /command")));
    }

    #[test]
    fn long_lines_wide_characters_and_pasted_text_stay_inside_the_box() {
        let mut st = state(40, 14);
        update(
            &mut st,
            Msg::Paste("日本語".repeat(20) + "\nsecond\tline\nthird"),
        );
        let s = screen(&st);
        assert!(s.iter().all(|l| l.chars().count() <= 40), "{s:?}");
        assert!(s.iter().any(|l| l.contains("third")), "{s:?}");
        // The box grows but never past its cap, so the status line is still there.
        assert!(s.last().unwrap().contains("mode read"), "{s:?}");
        let mut st = state(40, 14);
        update(&mut st, Msg::Paste("x\n".repeat(30)));
        assert!(screen(&st).last().unwrap().contains("mode read"));
    }

    #[test]
    fn a_long_transcript_shows_the_end_and_scrolls() {
        use crate::app::{Effect, JobEvent};
        let mut st = state(50, 12);
        type_text(&mut st, "go");
        let k = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        );
        let Effect::Start { id, .. } = update(&mut st, Msg::Key(k)).remove(0) else {
            unreachable!()
        };
        for i in 0..40 {
            update(
                &mut st,
                Msg::Job {
                    id,
                    event: JobEvent::Text(format!("line {i}")),
                },
            );
        }
        assert!(screen(&st).join("\n").contains("line 39"));
        assert!(!screen(&st).join("\n").contains("line 0\n"));
        let up = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::PageUp,
            ratatui::crossterm::event::KeyModifiers::NONE,
        );
        for _ in 0..30 {
            update(&mut st, Msg::Key(up));
        }
        let s = screen(&st);
        assert!(s[0].starts_with("crabby "), "{s:?}");
        assert!(s.last().unwrap().contains("scrolled"), "{s:?}");
    }
}
