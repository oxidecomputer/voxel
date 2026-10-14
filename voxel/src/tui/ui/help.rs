// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The help overlay, listing key bindings for each view and dialog.

use super::{
    colors::*,
    widgets::{overlay_area, popup_backdrop, popup_block},
};
use crate::tui::App;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};

const GLOBAL: &[(&str, &str)] = &[
    ("v", "Toggle Deployment / Monitoring"),
    ("Tab / Shift-Tab", "Next / previous section"),
    ("Space", "Fold / expand section"),
    ("?", "Toggle help"),
    ("Esc", "Close pane or selection"),
    ("d", "Leave the TUI, keep the deployment running"),
    ("q", "Quit and destroy the deployment"),
];
const DEPLOYMENT: &[(&str, &str)] = &[
    ("↑ / ↓", "Move within section"),
    ("PgUp / PgDn", "Page content"),
    ("f", "Cycle log filter"),
    ("l / r", "Launch / route"),
    ("x", "Stop the operation and destroy the deployment"),
];
const MONITORING: &[(&str, &str)] = &[
    ("← / →", "Previous / next rack; sibling node in the rack"),
    ("↑ / ↓", "Move within section"),
    ("PgUp / PgDn", "Page top zones"),
    ("Enter / Esc", "Focus node details / back to the rack"),
];
const DIALOGS: &[(&str, &str)] = &[
    ("↑ / ↓", "Select option"),
    ("Enter", "Confirm selection"),
    ("y", "Copy fallback command"),
    ("n", "Reject"),
    ("Esc", "Close dialog"),
];
const HELP: &[(&str, &str)] = &[("↑ / ↓", "Scroll")];

fn key_line(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<18}"),
            Style::default().fg(TUI_GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(description, Style::default().fg(OX_OFF_WHITE)),
    ])
}

fn help_text() -> Text<'static> {
    let mut lines = Vec::new();
    for (title, rows) in [
        ("Global", GLOBAL),
        ("Deployment", DEPLOYMENT),
        ("Monitoring", MONITORING),
        ("Dialogs", DIALOGS),
        ("Help", HELP),
    ] {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            title,
            Style::default().fg(TUI_PURPLE).add_modifier(Modifier::BOLD),
        ));
        lines.extend(
            rows.iter().map(|(key, description)| key_line(key, description)),
        );
    }
    Text::from(lines)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HelpMetrics {
    total: usize,
    capacity: usize,
    max_scroll: usize,
}

fn text_metrics(text: &Text<'_>, area: Rect) -> HelpMetrics {
    let inner = Block::bordered().inner(area);
    let capacity = usize::from(inner.height).max(1);
    let total = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width);
    HelpMetrics { total, capacity, max_scroll: total.saturating_sub(capacity) }
}

fn help_metrics(area: Rect) -> HelpMetrics {
    text_metrics(&help_text(), area)
}

pub(crate) fn help_area(root: Rect) -> Rect {
    let horizontal_margin = if root.width >= 52 { 4 } else { 0 };
    let vertical_margin = if root.height >= 12 { 2 } else { 0 };
    let width = root.width.saturating_sub(horizontal_margin).min(76);
    let height = root.height.saturating_sub(vertical_margin).min(18);
    Rect::new(
        root.x + root.width.saturating_sub(width) / 2,
        root.y + root.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    _mode: super::renderer::LayoutMode,
) {
    let area = help_area(overlay_area(frame.area()));
    // The application renderer returns its minimum-size fallback before Help.
    // Keep direct helper draws for degenerate test/backend areas empty as well.
    if area.width <= 2 || area.height <= 2 {
        return;
    }
    popup_backdrop(frame, area);

    let body = help_text();
    let metrics = text_metrics(&body, area);
    let start = app.session.help_scroll.min(metrics.max_scroll);
    let end = (start + metrics.capacity).min(metrics.total);
    let title = format!(
        " Help {}-{} of {} ",
        start.saturating_add(1).min(metrics.total),
        end,
        metrics.total
    );
    let block = popup_block(title);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((start as u16, 0))
            .block(block),
        area,
    );
}

pub(crate) fn page_capacity(app: &App) -> usize {
    let terminal = Rect::new(
        0,
        0,
        app.session.terminal.width,
        app.session.terminal.height,
    );
    help_metrics(help_area(overlay_area(terminal))).capacity
}
