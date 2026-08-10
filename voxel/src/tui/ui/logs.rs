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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::LogEntry;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, frame.area(), app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn chronological_window_follows_bottom_and_pauses_above_it() {
        let mut app = App::new(vec![], 8, 8);
        app.durable_log_path = "/work/voxel-tui.log".into();
        app.session.terminal.height = 5;
        app.logs.push(LogEntry::application(LogLevel::Info, "first"));
        app.logs.push(LogEntry::operation(LogLevel::Warning, "second"));
        app.logs.push(LogEntry::application(LogLevel::Error, "third"));
        let bottom = render(&app, 100, 4);
        assert!(bottom.find("second").unwrap() < bottom.find("third").unwrap());
        assert!(!bottom.contains("first"));
        assert!(bottom.contains("Bottom"));

        app.logs.scroll = 1;
        let paused = render(&app, 100, 4);
        assert!(paused.find("first").unwrap() < paused.find("second").unwrap());
        assert!(!paused.contains("third"));
        app.logs.push(LogEntry::operation(LogLevel::Info, "fourth"));
        let still_paused = render(&app, 100, 4);
        assert!(
            still_paused.contains("first") && still_paused.contains("second")
        );

        app.logs.scroll = 0;
        assert!(render(&app, 100, 4).contains("fourth"));
    }

    #[test]
    fn collapsed_title_keeps_path_and_counts_and_is_width_aware() {
        let mut app = App::new(vec![], 8, 8);
        app.durable_log_path = "/work/voxel-tui.log".into();
        assert!(collapsed_title(&app, 100, 5).contains("/work/voxel-tui.log"));
        let unsupported_narrow = collapsed_title(&app, 32, 5);
        assert!(unsupported_narrow.contains("All"));
        assert!(unsupported_narrow.contains("Bottom"));
        app.session.collapsed_deployment.insert(DeploymentPane::Logs);
        app.logs.push(LogEntry::application(LogLevel::Warning, "warn"));
        app.logs.push(LogEntry::operation(LogLevel::Error, "error"));
        let title = collapsed_title(&app, 100, 0);
        assert!(title.contains("1 warning / 1 error"));
    }

    #[test]
    fn compact_titles_keep_filter_position_and_collapsed_counts_exactly() {
        let mut app = App::new(vec![], 8, 8);
        app.durable_log_path =
            "/very/long/durable/path/to/voxel-tui.log".into();
        app.logs_filter = LogFilter::Error;
        for level in
            [LogLevel::Error, LogLevel::Error, LogLevel::Error, LogLevel::Error]
        {
            app.logs.push(LogEntry::application(level, "record"));
        }
        app.logs.push(LogEntry::application(LogLevel::Warning, "warning"));
        app.logs.scroll = 1;

        assert_eq!(
            collapsed_title(&app, 48, 1),
            "Logs: … · Error · 2 above / 1 below"
        );

        app.session.collapsed_deployment.insert(DeploymentPane::Logs);
        app.logs.scroll = 0;
        assert_eq!(
            collapsed_title(&app, 48, 0),
            "Logs: … · Error · Bottom · 1W/4E"
        );
        assert_eq!(collapsed_title(&app, 1, 0), "");
    }

    #[test]
    fn rendered_48_column_titles_keep_durable_suffixes() {
        let mut app = App::new(vec![], 8, 8);
        app.durable_log_path =
            "/very/long/durable/path/to/voxel-tui.log".into();
        app.logs_filter = LogFilter::Error;
        for level in [
            LogLevel::Error,
            LogLevel::Error,
            LogLevel::Error,
            LogLevel::Error,
            LogLevel::Warning,
        ] {
            app.logs.push(LogEntry::application(level, "record"));
        }
        app.logs.scroll = 1;
        let expanded = render(&app, 48, 3);
        assert!(expanded.contains("Error · 2 above / 1 below"), "{expanded}");

        app.session.collapsed_deployment.insert(DeploymentPane::Logs);
        app.logs.scroll = 0;
        let collapsed = render(&app, 48, 1);
        assert!(collapsed.contains("Error · Bottom · 1W/4E"), "{collapsed}");
    }

    #[test]
    fn six_entries_can_scroll_in_a_five_row_viewport_and_title_tracks_position()
    {
        let mut app = App::new(vec![], 8, 8);
        for index in 1..=6 {
            app.logs.push(LogEntry::application(
                LogLevel::Info,
                format!("entry-{index}"),
            ));
        }
        assert!(collapsed_title(&app, 100, 5).contains("Bottom"));
        app.logs.scroll = 1;
        assert!(collapsed_title(&app, 100, 5).contains("Top"));
    }

    #[test]
    fn filtering_preserves_arrival_order_and_range_uses_filtered_entries() {
        let mut app = App::new(vec![], 8, 8);
        app.session.terminal.height = 5;
        app.logs.push(LogEntry::application(LogLevel::Error, "accepted-one"));
        app.logs.push(LogEntry::operation(LogLevel::Info, "filtered"));
        app.logs.push(LogEntry::operation(LogLevel::Error, "accepted-two"));
        app.logs_filter = LogFilter::Error;
        let text = render(&app, 100, 4);
        assert!(
            text.find("accepted-one").unwrap()
                < text.find("accepted-two").unwrap()
        );
        assert!(!text.contains("filtered"));
        assert!(text.contains("Bottom"));
    }
}
