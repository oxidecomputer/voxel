// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The confirmation overlay for launch, route, detach, quit and destroy
//! requests, with the options and shortcuts each confirmation offers.

use super::{
    popup::{Button, Popup},
    widgets::overlay_area,
};
use crate::tui::{
    App,
    event::{Confirmation, ConfirmationOptionAction},
};
use ratatui::text::{Line, Text};

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
    let mut lines = Vec::new();
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

fn popup<'a>(app: &App, confirmation: &'a Confirmation) -> Popup<'a> {
    let selected = app.session.confirmation_selection;
    // Copying the resume command is a key, not an option to select.
    let copy = matches!(confirmation, Confirmation::Detach).then_some(Button {
        label: "Copy command",
        key: Some("y"),
        selected: false,
    });
    let buttons = copy
        .into_iter()
        .chain(confirmation.options().iter().enumerate().map(
            |(index, option)| Button {
                label: option.label,
                key: match option.action {
                    // Detach gives `y` to copying the resume command.
                    ConfirmationOptionAction::Confirm
                        if matches!(confirmation, Confirmation::Detach) =>
                    {
                        None
                    }
                    ConfirmationOptionAction::Confirm => Some("y"),
                    ConfirmationOptionAction::Reject => Some("n"),
                },
                selected: index == selected,
            },
        ))
        .collect();
    Popup {
        header: Line::from(prompt(confirmation)),
        body: body(app, confirmation),
        buttons,
    }
}

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let Some(confirmation) = app.session.confirmation.as_ref() else {
        return;
    };
    popup(app, confirmation).render(frame, overlay_area(frame.area()), 0);
}
