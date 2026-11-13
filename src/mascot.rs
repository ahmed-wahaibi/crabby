//! The mascot: turns `assets/crab.txt` (pixel rows, a blank line, then `letter rrggbb`
//! palette lines) into terminal lines, two pixels per cell. No pixel lives in this code.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

const DATA: &str = include_str!("../assets/crab.txt");
/// Smallest window that has room for the sprite plus the name and hint.
pub const MIN_WIDTH: u16 = 24;
pub const MIN_HEIGHT: u16 = 14;

/// One cell from its upper and lower pixel (`None` is transparent). Equal colours are a
/// space with a background, which fills the cell in any font; a half block only where
/// the pixels differ.
fn cell(top: Option<Color>, bottom: Option<Color>) -> Span<'static> {
    match (top, bottom) {
        (None, None) => Span::raw(" "),
        (Some(a), Some(b)) if a == b => Span::styled(" ", Style::new().bg(a)),
        (Some(a), Some(b)) => Span::styled("▀", Style::new().fg(a).bg(b)),
        (Some(a), None) => Span::styled("▀", Style::new().fg(a)),
        (None, Some(b)) => Span::styled("▄", Style::new().fg(b)),
    }
}

/// The sprite as centred lines, two pixel rows per line.
pub fn lines() -> Vec<Line<'static>> {
    // A Windows checkout can turn the file's line endings into CRLF.
    let data = DATA.replace('\r', "");
    let (rows, palette) = data.split_once("\n\n").unwrap_or((&data, ""));
    let colour = |ch: char| {
        let hex = palette
            .lines()
            .find(|l| l.starts_with(ch))?
            .split_whitespace()
            .nth(1)?;
        let n = u32::from_str_radix(hex, 16).ok()?;
        Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
    };
    let px: Vec<Vec<Option<Color>>> = rows
        .lines()
        .map(|r| r.chars().map(colour).collect())
        .collect();
    px.chunks(2)
        .map(|pair| {
            // `get`, not indexing: a ragged data file must not be able to crash the welcome screen.
            let at = |r: usize, x: usize| pair.get(r).and_then(|row| row.get(x).copied().flatten());
            let spans = (0..pair[0].len()).map(|x| cell(at(0, x), at(1, x)));
            Line::from(spans.collect::<Vec<_>>()).centered()
        })
        .collect()
}

/// Whether the terminal says it draws 24-bit colour.
pub fn truecolor() -> bool {
    // Windows Terminal draws 24-bit colour but does not always set COLORTERM; it does set WT_SESSION.
    std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit")
        || std::env::var_os("WT_SESSION").is_some()
}
