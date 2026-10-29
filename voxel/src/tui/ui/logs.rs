// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The log pane: filtered, scrollable application and child log entries,
//! styled by level, with a summary title when collapsed.

use super::{
    colors::*,
    widgets::{fit_terminal_width, section_block, terminal_width},
};
use crate::{
    tui::App,
    tui::app::{LogEntry, LogFilter, LogSource},
    tui::event::DeploymentPane,
    tui::operation::LogLevel,
};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::path::Path;

pub fn draw(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let visible = area.height.saturating_sub(2) as usize;
    let title = collapsed_title(app, area.width, visible);
    let expanded = app.session.deployment_expanded(DeploymentPane::Logs);
    let focused = app.session.deployment_pane == DeploymentPane::Logs;
    let block = section_block(title, expanded, focused);
    if !expanded || area.height <= 1 {
        frame.render_widget(block, area);
        return;
    }
    let entries = filtered_entries(app);
    let scroll = effective_scroll(app.logs.scroll, entries.len(), visible);
    let end = entries.len().saturating_sub(scroll);
    let start = end.saturating_sub(visible);
    let lines = if entries.is_empty() {
        vec![Line::from("No log records match this filter")]
    } else {
        entries[start..end].iter().map(log_line).collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn collapsed_title(app: &App, width: u16, visible: usize) -> String {
    // The bordered section consumes two columns and `section_block` owns the
    // three-column fold marker. The returned title must fit what remains.
    let available = width.saturating_sub(5) as usize;
    if available == 0 {
        return String::new();
    }
    let entries = filtered_entries(app);
    let scroll = effective_scroll(app.logs.scroll, entries.len(), visible);
    let position = if scroll == 0 {
        "Bottom".to_owned()
    } else if scroll >= entries.len().saturating_sub(visible) {
        "Top".to_owned()
    } else {
        format!(
            "{} above / {} below",
            entries.len().saturating_sub(visible + scroll),
            scroll
        )
    };
    let filter = filter_name(app.logs_filter);
    let (counts, compact_counts) =
        if app.session.deployment_expanded(DeploymentPane::Logs) {
            (String::new(), String::new())
        } else {
            let warnings = app
                .logs
                .entries
                .iter()
                .filter(|e| e.level == LogLevel::Warning)
                .count();
            let errors = app
                .logs
                .entries
                .iter()
                .filter(|e| e.level == LogLevel::Error)
                .count();
            (
                format!(" · {warnings} warning / {errors} error"),
                format!(" · {warnings}W/{errors}E"),
            )
        };
    let suffixes = [
        format!(" · {filter} · {position}{counts}"),
        format!(" · {filter} · {position}{compact_counts}"),
    ];
    let filename = Path::new(&app.durable_log_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&app.durable_log_path);
    for path in [app.durable_log_path.as_str(), filename, "…"] {
        for suffix in &suffixes {
            let candidate = format!("Logs: {path}{suffix}");
            if terminal_width(&candidate) <= available {
                return candidate;
            }
        }
    }
    // Widths below the supported renderer minimum may not fit even the
    // required state. Elide safely rather than indexing or subtracting past 0.
    let required = format!("{filter} · {position}{compact_counts}");
    fit_terminal_width(&required, available)
}

fn filtered_entries(app: &App) -> Vec<&LogEntry> {
    app.logs
        .entries
        .iter()
        .filter(|entry| app.logs_filter.accepts(entry.level))
        .collect()
}

pub(crate) fn filtered_len(app: &App) -> usize {
    filtered_entries(app).len()
}

pub(crate) fn effective_scroll(
    requested: usize,
    len: usize,
    visible: usize,
) -> usize {
    requested.min(len.saturating_sub(visible))
}

fn log_line(entry: &&LogEntry) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            source_label(entry.source),
            Style::default().fg(TUI_PURPLE),
        ),
        Span::raw(" "),
        Span::styled(
            level_label(entry.level),
            level_style(entry.level).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(entry.message.clone(), Style::default().fg(OX_OFF_WHITE)),
    ])
}

fn source_label(source: LogSource) -> &'static str {
    match source {
        LogSource::Application => "APP",
        LogSource::Operation => "OP",
    }
}
fn level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "INFO",
        LogLevel::Warning => "WARN",
        LogLevel::Error => "ERROR",
    }
}
fn filter_name(filter: LogFilter) -> &'static str {
    match filter {
        LogFilter::All => "All",
        LogFilter::Info => "Info",
        LogFilter::Warning => "Warning",
        LogFilter::Error => "Error",
    }
}
fn level_style(level: LogLevel) -> Style {
    Style::default().fg(match level {
        LogLevel::Info => TUI_GREEN,
        LogLevel::Warning => TUI_YELLOW,
        LogLevel::Error => OX_RED,
    })
}
