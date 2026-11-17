// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The help overlay, listing key bindings for each view and dialog.

use super::{
    colors::*,
    popup::{Button, Popup, PopupLayout},
    widgets::overlay_area,
};
use crate::tui::App;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
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
    ("← / →", "Select option"),
    ("Enter", "Confirm selection"),
    ("y", "Confirm; when leaving, copy the resume command"),
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

fn popup(header: String, scrolls: bool) -> Popup<'static> {
    let button = |label, key| Button { label, key: Some(key), selected: false };
    let scroll = scrolls.then(|| button("Scroll", "↑/↓"));
    Popup {
        header: Line::from(header),
        body: help_text(),
        buttons: scroll.into_iter().chain([button("Close", "Esc")]).collect(),
    }
}

/// The help pop-up's layout, which neither its header nor its scroll button
/// affects: the body is wider than either, and a button row is there anyway.
fn layout(terminal: Rect) -> PopupLayout {
    popup(String::new(), false).layout(overlay_area(terminal))
}

/// Body rows that fit, and how far the body can scroll.
fn scroll_bounds(layout: &PopupLayout) -> (usize, usize) {
    let capacity = usize::from(layout.body.height).max(1);
    let total = Paragraph::new(help_text())
        .wrap(Wrap { trim: false })
        .line_count(layout.body.width);
    (capacity, total.saturating_sub(capacity))
}

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let layout = layout(frame.area());
    let (capacity, max_scroll) = scroll_bounds(&layout);
    let total = capacity + max_scroll;
    let start = app.session.help_scroll.min(max_scroll);
    let header = if max_scroll == 0 {
        "Help".to_owned()
    } else {
        format!(
            "Help {}-{} of {total}",
            start + 1,
            (start + capacity).min(total)
        )
    };
    popup(header, max_scroll > 0).render(
        frame,
        overlay_area(frame.area()),
        start as u16,
    );
}

pub(crate) fn page_capacity(app: &App) -> usize {
    let terminal = Rect::new(
        0,
        0,
        app.session.terminal.width,
        app.session.terminal.height,
    );
    scroll_bounds(&layout(terminal)).0
}
