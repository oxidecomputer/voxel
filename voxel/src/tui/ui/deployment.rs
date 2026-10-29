// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Deployment view: operation progress, the phase checklist, status
//! lines and the operation log.

use super::{
    colors::*,
    logs,
    renderer::LayoutMode,
    widgets::{
        fit_terminal_width, section_block, section_heights, section_rects,
    },
};
use crate::{
    tui::operation::{
        CommandOutcome, DestroyPhase, LaunchPhase, OperationKind,
        OperationPhase, RoutePhase,
    },
    tui::{app::App, event::DeploymentPane},
};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Borders, Gauge, List, ListItem, Paragraph, Wrap},
};

pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    mode: LayoutMode,
) {
    let rows = deployment_rows(area, app, mode);

    draw_progress(frame, rows[0], app);
    draw_phases(frame, rows[1], app);
    draw_status(frame, rows[2], app);
    draw_subtasks(frame, rows[3], app);
    logs::draw(frame, rows[4], app);
}

pub(crate) fn deployment_rows(
    area: Rect,
    app: &App,
    mode: LayoutMode,
) -> Vec<Rect> {
    let preferred = match mode {
        LayoutMode::Wide => [3, 5, status_height(app, area.width, mode), 3, 7],
        LayoutMode::Compact => {
            [3, 4, status_height(app, area.width, mode), 3, 7]
        }
        LayoutMode::Minimum => [1; 5],
    };
    let expanded =
        DeploymentPane::ORDER.map(|pane| app.session.deployment_expanded(pane));
    let focused = DeploymentPane::ORDER
        .iter()
        .position(|pane| *pane == app.session.deployment_pane)
        .unwrap_or(1);
    let heights = section_heights(
        area.height,
        &expanded,
        focused,
        &preferred,
        &[2, 1, 0, 3, 4],
        1,
    );
    section_rects(area, &heights)
}

pub(crate) fn log_content_height(app: &App) -> usize {
    if !app.session.deployment_expanded(DeploymentPane::Logs) {
        return 0;
    }
    pane_content_height(app, 4)
}

fn pane_content_height(app: &App, pane: usize) -> usize {
    let (content, mode) = super::widgets::content_area(app);
    if mode == LayoutMode::Minimum {
        return 0;
    }
    deployment_rows(content, app, mode)[pane].height.saturating_sub(2) as usize
}

pub(crate) fn phase_content_height(app: &App) -> usize {
    pane_content_height(app, 1)
}

pub(crate) fn phase_order(kind: OperationKind) -> Vec<OperationPhase> {
    match kind {
        OperationKind::Launch => {
            LaunchPhase::ORDER.into_iter().map(OperationPhase::Launch).collect()
        }
        OperationKind::Destroy => DestroyPhase::ORDER
            .into_iter()
            .map(OperationPhase::Destroy)
            .collect(),
        OperationKind::Route => {
            RoutePhase::ORDER.into_iter().map(OperationPhase::Route).collect()
        }
    }
}

pub(crate) fn phase_name(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::Launch(LaunchPhase::Preflight) => "Preflight",
        OperationPhase::Launch(LaunchPhase::Stage) => "Stage",
        OperationPhase::Launch(LaunchPhase::Boot) => "Boot",
        OperationPhase::Launch(LaunchPhase::Initialize) => "Initialize",
        OperationPhase::Launch(LaunchPhase::RackSetup) => "Rack setup",
        OperationPhase::Launch(LaunchPhase::Route) => "Route",
        OperationPhase::Launch(LaunchPhase::Reconcile) => "Reconcile",
        OperationPhase::Destroy(DestroyPhase::OrphanCleanup) => {
            "Orphan cleanup"
        }
        OperationPhase::Destroy(DestroyPhase::FalconTeardown) => {
            "Falcon teardown"
        }
        OperationPhase::Destroy(DestroyPhase::StorageCleanup) => {
            "Storage cleanup"
        }
        OperationPhase::Destroy(DestroyPhase::Reconcile) => "Reconcile",
        OperationPhase::Route(RoutePhase::Validate) => "Validate",
        OperationPhase::Route(RoutePhase::Apply) => "Apply",
        OperationPhase::Route(RoutePhase::Reconcile) => "Reconcile",
    }
}

fn draw_progress(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let pane = DeploymentPane::OverallProgress;
    let expanded = app.session.deployment_expanded(pane);
    let focused = app.session.deployment_pane == pane;
    let block = section_block(" Overall Progress ", expanded, focused);
    if area.height <= 1 || !expanded {
        frame.render_widget(block, area);
        return;
    }
    let (completed, total, ratio, message) = progress_summary(app);
    if area.height == 2 {
        // The following section owns the separator below this compact region.
        // Omitting this block's bottom border leaves one real summary row.
        let compact_block =
            block.borders(Borders::TOP | Borders::LEFT | Borders::RIGHT);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{completed}/{total} {message}"),
                Style::default().fg(TUI_GREEN),
            )))
            .block(compact_block),
            area,
        );
    } else {
        frame.render_widget(
            Gauge::default()
                .block(block)
                .gauge_style(Style::default().fg(TUI_GREEN))
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!("{completed}/{total} {message}")),
            area,
        );
    }
}

fn progress_summary(app: &App) -> (usize, usize, f64, String) {
    app.operation.displayed().map_or(
        (0, 0, 0.0, "Idle".to_owned()),
        |operation| {
            let phases = phase_order(operation.kind);
            let phase_completed =
                operation.completed_phases.len().min(phases.len());
            let phase_ratio = if phases.is_empty() {
                0.0
            } else {
                phase_completed as f64 / phases.len() as f64
            };
            operation
                .progress
                .as_ref()
                .and_then(|progress| {
                    (progress.total > 0 && progress.completed <= progress.total)
                        .then(|| {
                            (
                                progress.completed,
                                progress.total,
                                progress.completed as f64
                                    / progress.total as f64,
                                progress.message.clone(),
                            )
                        })
                })
                .unwrap_or((
                    phase_completed,
                    phases.len(),
                    phase_ratio,
                    if phase_completed == phases.len() {
                        "Complete".to_owned()
                    } else {
                        phase_name(
                            operation.phase.unwrap_or(phases[phase_completed]),
                        )
                        .to_owned()
                    },
                ))
        },
    )
}

fn draw_phases(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let pane = DeploymentPane::Phases;
    let focused = app.session.deployment_pane == pane;
    let expanded = app.session.deployment_expanded(pane);
    let block = section_block(" Deployment Phases ", expanded, focused);
    if area.height <= 1 || !expanded {
        frame.render_widget(block, area);
        return;
    }
    let Some(operation) = app.operation.displayed() else {
        frame.render_widget(
            List::new([ListItem::new("○ No active deployment operation")])
                .block(block),
            area,
        );
        return;
    };
    let phases = phase_order(operation.kind);
    let visible = area.height.saturating_sub(2) as usize;
    let active_index = operation
        .phase
        .and_then(|active| phases.iter().position(|p| *p == active));
    let start = phase_window_start(
        app.session.phase_scroll,
        visible,
        phases.len(),
        active_index,
    );
    let failed_phase = None;
    let items = phases.into_iter().skip(start).take(visible).map(|phase| {
        let (icon, state, style) = if failed_phase == Some(phase) {
            (
                "✗",
                "Failed",
                Style::default().fg(OX_RED).add_modifier(Modifier::BOLD),
            )
        } else if operation.completed_phases.contains(&phase) {
            ("✓", "Complete", Style::default().fg(TUI_GREEN))
        } else if operation.phase == Some(phase) {
            (
                "◐",
                "Active",
                Style::default().fg(TUI_YELLOW).add_modifier(Modifier::BOLD),
            )
        } else {
            ("○", "Pending", Style::default().fg(TUI_GREY))
        };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{icon} {:<18}", phase_name(phase)), style),
            Span::styled(state, style),
        ]))
    });
    frame.render_widget(List::new(items).block(block), area);
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let pane = DeploymentPane::Status;
    let focused = app.session.deployment_pane == pane;
    let expanded = app.session.deployment_expanded(pane);
    let block = section_block(" Status ", expanded, focused);
    if area.height <= 1 || !expanded {
        frame.render_widget(block, area);
        return;
    }
    let lines = status_lines(app, area.width.saturating_sub(2) as usize)
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn status_lines(app: &App, width: usize) -> Vec<String> {
    let mut state = format!("Observed: {}", observed_name(app));
    if let Some(active) = app.operation.active.as_ref() {
        state.push_str(&format!(
            " · {}{}{}",
            operation_name(active.kind),
            active
                .phase
                .map(|p| format!(" · {}", phase_name(p)))
                .unwrap_or_default(),
            if active.cancelling { " · Cancelling" } else { "" }
        ));
        if let Some(progress) = active.progress.as_ref() {
            state.push_str(&format!(" · {}", progress.message));
        }
    } else if let Some(pending) = app.operation.pending.as_ref() {
        state.push_str(&format!(" · {} pending", operation_name(pending.kind)));
    }

    let mut failures = Vec::new();
    if let Some(failure) = app.operation.start_failure.as_ref() {
        failures.push(format!("Start: {failure}"));
    }
    if let Some(failure) = app.deployment.reconciliation_failure.as_ref() {
        failures.push(format!("Reconciliation: {failure}"));
    }
    let mut result = vec![fit_status_line(state, width)];

    let mut terminal =
        app.operation.outcome.as_ref().map(|outcome| match outcome {
            CommandOutcome::Exited { status, .. } if status.success() => {
                "Outcome: Command succeeded".into()
            }
            CommandOutcome::Exited { status, .. } => {
                format!("Outcome: Command exited {status}")
            }
            CommandOutcome::SpawnFailed { message } => {
                format!("Outcome: Failed · {message}")
            }
            CommandOutcome::ForceStopped { kill_error, .. } => {
                kill_error.as_ref().map_or_else(
                    || "Outcome: Force stopped".into(),
                    |error| format!("Outcome: Force stop failed · {error}"),
                )
            }
        });
    if !failures.is_empty() {
        let failure_summary =
            format!("Failures: {} · {}", failures.len(), failures.join(" · "));
        terminal = Some(match terminal {
            Some(outcome) => format!("{outcome} · {failure_summary}"),
            None => failure_summary,
        });
    }
    if let Some(terminal) = terminal {
        result.push(fit_status_line(terminal, width));
    }

    if let Some(first) = app.operation.retained_warnings.first() {
        let count = app.operation.retained_warnings.len();
        let remainder = count
            .checked_sub(1)
            .filter(|remaining| *remaining > 0)
            .map(|remaining| format!(" · {remaining} more"))
            .unwrap_or_default();
        result.push(fit_status_line(
            format!("Warnings: {count} · {}{remainder}", first.message),
            width,
        ));
    }
    result
}

fn fit_status_line(line: String, width: usize) -> String {
    fit_terminal_width(&line, width)
}

fn status_height(app: &App, width: u16, mode: LayoutMode) -> u16 {
    let content =
        status_lines(app, width.saturating_sub(2).max(1) as usize).len() as u16;
    content.saturating_add(2).max(match mode {
        LayoutMode::Wide => 5,
        LayoutMode::Compact => 4,
        LayoutMode::Minimum => 0,
    })
}

fn draw_subtasks(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let pane = DeploymentPane::CurrentPhase;
    let focused = app.session.deployment_pane == pane;
    let expanded = app.session.deployment_expanded(pane);
    let block = section_block(" Current Phase ", expanded, focused);
    if area.height <= 1 || !expanded {
        frame.render_widget(block, area);
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from("No subtasks yet")).block(block),
        area,
    );
}

pub(crate) fn phase_window_start(
    requested: usize,
    visible: usize,
    len: usize,
    important: Option<usize>,
) -> usize {
    let mut start = requested.min(len.saturating_sub(visible));
    if let Some(index) = important {
        if index < start {
            start = index;
        }
        if index >= start.saturating_add(visible) {
            start = index.saturating_add(1).saturating_sub(visible);
        }
    }
    start
}

fn operation_name(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Launch => "Launch",
        OperationKind::Destroy => "Destroy",
        OperationKind::Route => "Route",
    }
}
fn observed_name(app: &App) -> &'static str {
    match app.deployment.observed {
        crate::tui::reconcile::ObservedDeploymentState::Stopped => "Stopped",
        crate::tui::reconcile::ObservedDeploymentState::Starting => "Starting",
        crate::tui::reconcile::ObservedDeploymentState::Running => "Running",
        crate::tui::reconcile::ObservedDeploymentState::Degraded => "Degraded",
        crate::tui::reconcile::ObservedDeploymentState::Stopping => "Stopping",
        crate::tui::reconcile::ObservedDeploymentState::Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        app::ActiveOperation,
        event::OperationRequestId,
        operation::{LaunchPhase, OperationKind, OperationPhase},
        reconcile::ObservedDeploymentState,
    };

    /// The status section tells the operator what the deployment is doing and
    /// why anything failed.
    #[test]
    fn status_names_the_operation_and_every_failure() {
        let app = |setup: &dyn Fn(&mut App)| {
            let mut app = App::new(vec![], 8, 8);
            setup(&mut app);
            app
        };
        let cases = [
            (
                "stopped",
                app(&|app| {
                    app.deployment.observed = ObservedDeploymentState::Stopped
                }),
                vec!["Observed: Stopped"],
            ),
            (
                "cancelling",
                app(&|app| {
                    let mut active = ActiveOperation::new(
                        OperationRequestId::FIRST,
                        OperationKind::Launch,
                    );
                    active.phase =
                        Some(OperationPhase::Launch(LaunchPhase::Boot));
                    active.cancelling = true;
                    app.operation.active = Some(active);
                }),
                vec!["Observed: Unknown · Launch · Boot · Cancelling"],
            ),
            (
                "failed",
                app(&|app| {
                    app.operation.outcome = Some(CommandOutcome::SpawnFailed {
                        message: "boot failed".into(),
                    })
                }),
                vec!["Observed: Unknown", "Outcome: Failed · boot failed"],
            ),
            (
                "reconciliation and start failures",
                app(&|app| {
                    app.deployment.reconciliation_failure =
                        Some("ssh timed out".into());
                    app.operation.start_failure = Some("no request ID".into());
                }),
                vec![
                    "Observed: Unknown",
                    "Failures: 2 · Start: no request ID · Reconciliation: ssh \
                     timed out",
                ],
            ),
        ];
        for (case, app, expected) in cases {
            assert_eq!(status_lines(&app, 120), expected, "{case}");
        }
    }
}
