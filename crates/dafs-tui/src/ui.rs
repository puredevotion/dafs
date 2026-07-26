//! Rendering. Pure functions over a [`crate::client::Status`] snapshot — no
//! I/O and no daemon knowledge beyond what `Status` already carries.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::client::Status;

pub fn draw(frame: &mut Frame, url: &str, status: &Status) {
    let area = frame.area();
    let chunks =
        Layout::vertical([Constraint::Length(3), Constraint::Length(7), Constraint::Min(3)])
            .split(area);

    draw_header(frame, chunks[0], url, status);
    draw_stats(frame, chunks[1], status);
    draw_events(frame, chunks[2], status);
}

fn draw_header(frame: &mut Frame, area: Rect, url: &str, status: &Status) {
    let (label, color) = if !status.connected {
        ("disconnected", Color::Red)
    } else if status.ready {
        ("connected, ready", Color::Green)
    } else {
        ("connected, not ready", Color::Yellow)
    };
    let version = status.version.as_deref().unwrap_or("?");
    let uptime = status.uptime_seconds.map(format_duration).unwrap_or_else(|| "-".into());

    let text = format!("{url}   [{label}]   version {version}   uptime {uptime}   (q to quit)");
    let p = Paragraph::new(text)
        .style(Style::default().fg(color))
        .block(Block::default().borders(Borders::ALL).title("dafs-tui"));
    frame.render_widget(p, area);
}

fn draw_stats(frame: &mut Frame, area: Rect, status: &Status) {
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
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
    }

    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("stats"));
    frame.render_widget(p, area);
}

fn draw_events(frame: &mut Frame, area: Rect, status: &Status) {
    let items: Vec<ListItem> = status
        .events
        .iter()
        .map(|e| {
            let ts = format_utc_time(e.at_unix_ms);
            let size = e.size_bytes.map(|b| format_bytes(b.max(0) as u64)).unwrap_or_default();
            let suffix = if e.is_dir { "/" } else { "" };
            ListItem::new(format!("{ts}  #{:<6}{:<9}{}{}  {size}", e.id, e.kind, e.path, suffix))
        })
        .collect();

    let title = if status.events.is_empty() { "recent events (none yet)" } else { "recent events" };
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
}
