//! Rendering. Pure functions over a [`crate::client::Status`] snapshot — no
//! I/O and no daemon knowledge beyond what `Status` already carries.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline};

use crate::capabilities::Capabilities;
use crate::client::Status;

#[derive(Clone, Copy)]
enum Semantic {
    Ok,
    Warn,
    Err,
}

/// Truecolor gets a softer, deliberately chosen palette; everything else
/// falls back to the basic ANSI colors every terminal supports.
fn color(caps: Capabilities, semantic: Semantic) -> Color {
    if caps.truecolor {
        match semantic {
            Semantic::Ok => Color::Rgb(87, 199, 133),
            Semantic::Warn => Color::Rgb(224, 175, 104),
            Semantic::Err => Color::Rgb(224, 108, 117),
        }
    } else {
        match semantic {
            Semantic::Ok => Color::Green,
            Semantic::Warn => Color::Yellow,
            Semantic::Err => Color::Red,
        }
    }
}

/// An emoji glyph when the terminal can render one, else a plain ASCII tag —
/// same information either way, never emoji-only.
fn glyph(caps: Capabilities, semantic: Semantic) -> &'static str {
    match (caps.unicode, semantic) {
        (true, Semantic::Ok) => "🟢",
        (true, Semantic::Warn) => "🟡",
        (true, Semantic::Err) => "🔴",
        (false, Semantic::Ok) => "[UP]",
        (false, Semantic::Warn) => "[..]",
        (false, Semantic::Err) => "[XX]",
    }
}

/// A panel title with an optional leading emoji — dropped entirely rather
/// than replaced with a placeholder when Unicode isn't supported.
fn titled(caps: Capabilities, emoji: &str, text: &str) -> String {
    if caps.unicode { format!("{emoji} {text}") } else { text.to_string() }
}

pub fn draw(
    frame: &mut Frame,
    url: &str,
    status: &Status,
    rss_history: &std::collections::VecDeque<u64>,
    caps: Capabilities,
) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(6),
        Constraint::Length(3),
        Constraint::Min(3),
    ])
    .split(area);

    draw_header(frame, chunks[0], url, status, caps);
    draw_stats(frame, chunks[1], status, caps);
    draw_sparkline(frame, chunks[2], rss_history, caps);
    draw_events(frame, chunks[3], status, caps);
}

fn draw_header(frame: &mut Frame, area: Rect, url: &str, status: &Status, caps: Capabilities) {
    let (label, semantic) = if !status.connected {
        ("disconnected", Semantic::Err)
    } else if status.ready {
        ("connected, ready", Semantic::Ok)
    } else {
        ("connected, not ready", Semantic::Warn)
    };
    let version = status.version.as_deref().unwrap_or("?");
    let uptime = status.uptime_seconds.map(format_duration).unwrap_or_else(|| "-".into());

    let text = format!(
        "{} {url}   {label}   version {version}   uptime {uptime}   (q to quit)",
        glyph(caps, semantic)
    );
    let p = Paragraph::new(text)
        .style(Style::default().fg(color(caps, semantic)))
        .block(Block::default().borders(Borders::ALL).title(titled(caps, "🛰", "dafs-tui")));
    frame.render_widget(p, area);
}

fn draw_stats(frame: &mut Frame, area: Rect, status: &Status, caps: Capabilities) {
    let rss = status.resident_bytes.map(format_bytes).unwrap_or_else(|| "-".into());
    let events_total = status.events_total.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
    let files_known = status.files_known.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
    let schema = status.schema_version.map(|n| n.to_string()).unwrap_or_else(|| "-".into());

    let mut lines = vec![
        Line::from(format!("RSS:          {rss}")),
        Line::from(format!("Events total: {events_total}")),
        Line::from(format!("Files known:  {files_known}")),
        Line::from(format!("Schema ver.:  {schema}")),
    ];
    if let Some(error) = &status.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(color(caps, Semantic::Err)),
        )));
    }

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(titled(caps, "📊", "stats")));
    frame.render_widget(p, area);
}

fn draw_sparkline(
    frame: &mut Frame,
    area: Rect,
    rss_history: &std::collections::VecDeque<u64>,
    caps: Capabilities,
) {
    let data: Vec<u64> = rss_history.iter().copied().collect();
    let title = if data.is_empty() {
        titled(caps, "📈", "rss trend (waiting for samples)")
    } else {
        titled(caps, "📈", "rss trend")
    };
    let sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(title))
        .data(&data)
        .style(Style::default().fg(color(caps, Semantic::Ok)));
    frame.render_widget(sparkline, area);
}

fn draw_events(frame: &mut Frame, area: Rect, status: &Status, caps: Capabilities) {
    let items: Vec<ListItem> = status
        .events
        .iter()
        .map(|e| {
            let ts = format_utc_time(e.at_unix_ms);
            let size = e.size_bytes.map(|b| format_bytes(b.max(0) as u64)).unwrap_or_default();
            let kind_glyph = if caps.unicode {
                match e.kind.as_str() {
                    "created" => "✨",
                    "modified" => "✏️ ",
                    "deleted" => "🗑️ ",
                    "renamed" => "↪️ ",
                    _ => "  ",
                }
            } else {
                ""
            };
            let suffix = if e.is_dir { "/" } else { "" };
            ListItem::new(format!(
                "{ts}  #{:<6}{kind_glyph}{:<9}{}{}  {size}",
                e.id, e.kind, e.path, suffix
            ))
        })
        .collect();

    let title = if status.events.is_empty() {
        titled(caps, "🗒", "recent events (none yet)")
    } else {
        titled(caps, "🗒", "recent events")
    };
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h}h{m:02}m{s:02}s")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// No timezone dependency for one HH:MM:SS label — this is UTC, deliberately
/// unlabelled-local, to avoid pulling a time/chrono crate into a status tool
/// for a single formatting need.
fn format_utc_time(unix_ms: i64) -> String {
    let secs_of_day = unix_ms.div_euclid(1000).rem_euclid(86_400);
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_scale_to_the_right_unit() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(9_805_824), "9.4 MiB");
    }

    #[test]
    fn duration_renders_hours_minutes_seconds() {
        assert_eq!(format_duration(5), "0h00m05s");
        assert_eq!(format_duration(3_661), "1h01m01s");
    }

    #[test]
    fn utc_time_wraps_at_midnight() {
        assert_eq!(format_utc_time(0), "00:00:00");
        assert_eq!(format_utc_time(86_400_000 + 3_661_000), "01:01:01");
    }

    #[test]
    fn ascii_fallback_never_contains_emoji_glyphs() {
        let caps = Capabilities { truecolor: false, unicode: false };
        for s in [Semantic::Ok, Semantic::Warn, Semantic::Err] {
            assert!(glyph(caps, s).is_ascii(), "glyph leaked non-ASCII with unicode disabled");
        }
        assert_eq!(titled(caps, "📈", "rss trend"), "rss trend");
    }

    #[test]
    fn unicode_titles_keep_the_emoji_prefix() {
        let caps = Capabilities { truecolor: false, unicode: true };
        assert_eq!(titled(caps, "📈", "rss trend"), "📈 rss trend");
    }
}
