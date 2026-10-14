// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The confirmation overlay for launch, route, detach, quit and destroy
//! requests, with the options and shortcuts each confirmation offers.

use super::{
    colors::TUI_PURPLE,
    widgets::{overlay_area, popup_backdrop, popup_block, terminal_width},
};
use crate::tui::{App, event::Confirmation};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Text},
    widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
};

fn prompt(confirmation: &Confirmation) -> &'static str {
    match confirmation {
        Confirmation::Launch => "Launch deployment?",
        Confirmation::Route => "Apply routes?",
        Confirmation::Detach => "Leave the TUI, keep the deployment?",
        Confirmation::Quit => "Quit Voxel TUI?",
        Confirmation::QuitAndDestroy => "Quit and destroy the deployment?",
        Confirmation::CancelAndDestroy => {
            "Stop the operation and destroy the deployment?"
        }
        Confirmation::ForceStop => "Force stop the running command?",
    }
}

fn body(app: &App, confirmation: &Confirmation) -> Text<'static> {
    let mut lines = vec![Line::from(prompt(confirmation))];
    if matches!(confirmation, Confirmation::Detach) {
        if app.operation.active.is_some() || app.operation.pending.is_some() {
            lines.push(Line::from("The running operation stops."));
            lines.push(Line::from("The deployment is left in place."));
        }
        lines.push(Line::from("Resume later with:"));
        lines.push(Line::from("voxel tui resume"));
        if app.clipboard_copied {
            lines.push(Line::from("Full fallback command copied."));
        }
    }
    if matches!(confirmation, Confirmation::CancelAndDestroy) {
        lines.push(Line::from(
            "The running command is allowed to finish safely first, so this can take a moment.",
        ));
    }
    if matches!(confirmation, Confirmation::ForceStop) {
        lines.push(Line::from(
            "This can leave the deployment half-built. Use it only when the command will not stop on its own.",
        ));
    }
    Text::from(lines)
}

fn visible_shortcuts(
    confirmation: &Confirmation,
    available_width: usize,
) -> &'static str {
    let wide = if matches!(confirmation, Confirmation::Detach) {
        "↑/↓ move · Enter select · y copy fallback · n/Esc back"
    } else {
        "↑/↓ move · Enter select · n/Esc back"
    };
    if terminal_width(wide) <= available_width {
        wide
    } else if matches!(confirmation, Confirmation::Detach) {
        "↑/↓ Enter y copy n/Esc"
    } else {
        "↑/↓ Enter n/Esc"
    }
}

pub(crate) fn dialog_area(app: &App, overlay: Rect) -> Rect {
    let confirmation =
        app.session.confirmation.as_ref().expect("open confirmation");
    let options = confirmation.options(app.can_cancel());
    let dialog_body = body(app, confirmation);
    let content_width = dialog_body
        .lines
        .iter()
        .map(Line::width)
        .chain(options.iter().map(|option| terminal_width(option.label)))
        .chain(std::iter::once(terminal_width(
            "↑/↓ move · Enter select · y copy fallback · n/Esc back",
        )))
        .max()
        .unwrap_or_default() as u16;
    let side_margin = if overlay.width >= 4 { 4 } else { 0 };
    let width = content_width
        .saturating_add(6)
        .min(if matches!(confirmation, Confirmation::Detach) { 96 } else { 64 })
        .min(overlay.width.saturating_sub(side_margin).max(1));
    let inner_width = width.saturating_sub(2).max(1);
    let body_height = Paragraph::new(dialog_body)
        .wrap(Wrap { trim: false })
        .line_count(inner_width) as u16;
    let height = body_height
        .saturating_add(options.len() as u16)
        .saturating_add(3)
        .min(overlay.height);
    Rect::new(
        overlay.x + overlay.width.saturating_sub(width) / 2,
        overlay.y + overlay.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let Some(confirmation) = app.session.confirmation.as_ref() else {
        return;
    };
    let options = confirmation.options(app.can_cancel());
    let dialog_body = body(app, confirmation);
    let area = dialog_area(app, overlay_area(frame.area()));
    popup_backdrop(frame, area);
    let block = popup_block(" Confirmation ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let body_height = Paragraph::new(dialog_body.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width) as u16;
    let rows = Layout::vertical([
        Constraint::Length(body_height),
        Constraint::Length(options.len() as u16),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(dialog_body).wrap(Wrap { trim: false }),
        rows[0],
    );
    let list =
        List::new(options.iter().map(|option| ListItem::new(option.label)))
            .highlight_symbol("▶ ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(
                Style::default().fg(TUI_PURPLE).add_modifier(Modifier::BOLD),
            );
    let selected =
        app.session.confirmation_selection.min(options.len().saturating_sub(1));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, rows[1], &mut state);
    frame.render_widget(
        Paragraph::new(visible_shortcuts(confirmation, rows[2].width as usize)),
        rows[2],
    );
}
