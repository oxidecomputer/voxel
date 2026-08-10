use super::{
    colors::{OX_GREEN_DARKEST, OX_OFF_WHITE, OX_YELLOW, TUI_PURPLE},
    widgets::{overlay_area, terminal_width},
};
use crate::tui::{App, event::Confirmation};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Text},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState,
        Paragraph, Wrap,
    },
};

fn prompt(confirmation: &Confirmation) -> &'static str {
    match confirmation {
        Confirmation::Launch => "Launch deployment?",
        Confirmation::Route => "Apply routes?",
        Confirmation::Detach => "Detach from Voxel TUI?",
        Confirmation::Quit => "Quit Voxel TUI?",
        Confirmation::QuitAndDestroy => {
            "Quit TUI and destroy Voxel deployment?"
        }
        Confirmation::CancelAndLeave => "Cancel operation and leave resources?",
        Confirmation::CancelAndDestroy => {
            "Cancel operation and DESTROY resources?"
        }
        Confirmation::ForceStop => "Force stop the direct Voxel child?",
    }
}

#[cfg(test)]
pub(crate) fn prompt_text(confirmation: Confirmation) -> &'static str {
    prompt(&confirmation)
}

fn body(app: &App, confirmation: &Confirmation) -> Text<'static> {
    let mut lines = vec![Line::from(prompt(confirmation))];
    if matches!(confirmation, Confirmation::Detach) {
        if app.operation.active.is_some() || app.operation.pending.is_some() {
            lines.push(Line::from(
                "The active operation will be cancelled; resources will be left in place.",
            ));
        }
        lines.push(Line::from("Resume later with:"));
        lines.push(Line::from("voxel tui resume"));
        if app.clipboard_copied {
            lines.push(Line::from("Full fallback command copied."));
        }
    }
    if matches!(
        confirmation,
        Confirmation::CancelAndLeave | Confirmation::CancelAndDestroy
    ) {
        lines.push(Line::from(
            "The Voxel command is still running; waiting for the Voxel command to settle without interrupting an opaque Falcon boundary.",
        ));
    }
    if matches!(confirmation, Confirmation::ForceStop) {
        lines.push(Line::from(
            "Force stop can leave partial deployment state. Only the direct child is terminated; reconciliation still follows.",
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
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirmation ")
        .border_style(Style::default().fg(OX_YELLOW))
        .style(Style::default().bg(OX_GREEN_DARKEST).fg(OX_OFF_WHITE));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tui::operation::OperationKind,
        tui::{app::ActiveOperation, event::OperationRequestId},
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn app_for(confirmation: Confirmation) -> App {
        let mut app = App::new(vec![], 2, 2);
        app.reattach_command = Some("pfexec '/opt/voxel bin/voxel' --config '/cfg/voxel.toml' --workdir '/work dir' --name 'rack' --dataset 'pool/falcon' --build-root '/build root' tui".into());
        app.session.confirmation_selection =
            confirmation.options(app.can_cancel()).len() - 1;
        app.session.confirmation = Some(confirmation);
        app
    }
    fn render(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }
    fn rendered_dialog_text(app: &App, width: u16, height: u16) -> String {
        let buffer = render(app, width, height);
        let area =
            dialog_area(app, overlay_area(Rect::new(0, 0, width, height)));
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    #[test]
    fn lifecycle_prompts_match_exactly() {
        for (confirmation, expected) in [
            (Confirmation::Detach, "Detach from Voxel TUI?"),
            (Confirmation::Quit, "Quit Voxel TUI?"),
            (
                Confirmation::QuitAndDestroy,
                "Quit TUI and destroy Voxel deployment?",
            ),
        ] {
            let text = rendered_dialog_text(&app_for(confirmation), 100, 22);
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
    }
    #[test]
    fn detach_renders_short_resume_selected_back_copy_and_warning() {
        for (width, height) in [(100, 22), (80, 20)] {
            let app = app_for(Confirmation::Detach);
            let text = rendered_dialog_text(&app, width, height);
            assert!(
                text.contains("Resume later with:")
                    && text.contains("voxel tui resume"),
                "{text}"
            );
            assert!(!text.contains("--config"), "{text}");
            assert!(
                text.contains("▶ Back")
                    && text.contains("y copy")
                    && text.contains("n/Esc back"),
                "{text}"
            );
        }
        let mut copied = app_for(Confirmation::Detach);
        copied.clipboard_copied = true;
        assert!(
            rendered_dialog_text(&copied, 80, 20)
                .contains("Full fallback command copied.")
        );
        let mut app = app_for(Confirmation::Detach);
        app.reattach_command = Some("voxel --name rack tui".into());
        app.operation.active = Some(ActiveOperation::new(
            OperationRequestId::FIRST,
            OperationKind::Launch,
        ));
        let text = rendered_dialog_text(&app, 48, 16);
        assert!(
            text.contains("voxel tui resume") && text.contains("▶ Back"),
            "{text}"
        );
        assert!(
            text.contains("The active operation will be cancelled;")
                && text.contains("resources will be left in place."),
            "{text}"
        );
    }
    #[test]
    fn dialog_geometry_is_bounded_and_uses_lifecycle_caps() {
        for (confirmation, affirmative, selected_reject) in [
            (Confirmation::Launch, "Launch deployment", "▶ Cancel"),
            (Confirmation::Route, "Apply routes", "▶ Cancel"),
            (Confirmation::Detach, "Detach and leave resources", "▶ Back"),
            (Confirmation::Quit, "Quit Voxel TUI", "▶ Back"),
            (
                Confirmation::QuitAndDestroy,
                "Destroy deployment and quit",
                "▶ Back",
            ),
            (
                Confirmation::CancelAndLeave,
                "Cancel and leave resources",
                "▶ Back",
            ),
            (
                Confirmation::CancelAndDestroy,
                "Cancel and destroy resources",
                "▶ Back",
            ),
        ] {
            for (width, height) in [(160, 38), (100, 22), (80, 20), (48, 16)] {
                let mut app = app_for(confirmation.clone());
                if width == 48 && matches!(confirmation, Confirmation::Detach) {
                    app.reattach_command = Some("voxel tui".into());
                }
                let overlay = overlay_area(Rect::new(0, 0, width, height));
                let area = dialog_area(&app, overlay);
                assert!(
                    area.x >= overlay.x
                        && area.y >= overlay.y
                        && area.right() <= overlay.right()
                        && area.bottom() <= overlay.bottom()
                );
                if overlay.width >= 4 {
                    assert!(area.width <= overlay.width - 4);
                }
                assert!(
                    area.width
                        <= if matches!(confirmation, Confirmation::Detach) {
                            96
                        } else {
                            64
                        }
                );
                let text = rendered_dialog_text(&app, width, height);
                assert!(
                    text.contains(affirmative),
                    "missing affirmative {affirmative:?} for {confirmation:?} {width}x{height}: {text}"
                );
                assert!(
                    text.contains(selected_reject),
                    "missing selected reject {selected_reject:?} for {confirmation:?} {width}x{height}: {text}"
                );
                assert!(
                    text.contains("n/Esc"),
                    "{confirmation:?} {width}x{height}: {text}"
                );
                assert_eq!(
                    text.contains("y copy"),
                    matches!(confirmation, Confirmation::Detach),
                    "{confirmation:?} {width}x{height}: {text}"
                );
            }
        }
    }
    #[test]
    fn selected_back_row_keeps_existing_list_style() {
        let app = app_for(Confirmation::Quit);
        let buffer = render(&app, 80, 20);
        let area = dialog_area(&app, overlay_area(Rect::new(0, 0, 80, 20)));
        let back = (area.y..area.bottom())
            .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
            .find(|&(x, y)| buffer[(x, y)].symbol() == "▶")
            .unwrap();
        assert_eq!(buffer[back].fg, TUI_PURPLE);
        assert!(buffer[back].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(area.x, area.y)].fg, OX_YELLOW);
    }
}
