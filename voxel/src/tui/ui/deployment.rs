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
    let has_subtasks = false;
    let current_height = match mode {
        LayoutMode::Wide if has_subtasks => 5,
        LayoutMode::Wide => 3,
        LayoutMode::Compact if has_subtasks => 4,
        LayoutMode::Compact => 3,
        LayoutMode::Minimum => 1,
    };
    let preferred = match mode {
        LayoutMode::Wide => {
            [3, 5, status_height(app, area.width, mode), current_height, 7]
        }
        LayoutMode::Compact => {
            [3, 4, status_height(app, area.width, mode), current_height, 7]
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

pub(crate) fn subtask_content_height(app: &App) -> usize {
    pane_content_height(app, 3)
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
    let (completed, total, ratio, message) = app
        .operation
        .active
        .as_ref()
        .map_or((0, 0, 0.0, "Idle".to_owned()), |operation| {
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
                    phase_name(
                        operation.phase.unwrap_or(
                            phases[phase_completed
                                .min(phases.len().saturating_sub(1))],
                        ),
                    )
                    .to_owned(),
                ))
        });
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

fn draw_phases(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let pane = DeploymentPane::Phases;
    let focused = app.session.deployment_pane == pane;
    let expanded = app.session.deployment_expanded(pane);
    let block = section_block(" Deployment Phases ", expanded, focused);
    if area.height <= 1 || !expanded {
        frame.render_widget(block, area);
        return;
    }
    let Some(operation) = app.operation.active.as_ref() else {
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
            ("●", "Complete", Style::default().fg(TUI_GREEN))
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
    let rows: Vec<(Line<'_>, bool)> = Vec::new();
    let lines = if rows.is_empty() {
        vec![Line::from("No subtasks yet")]
    } else {
        let visible = area.height.saturating_sub(2) as usize;
        let important = rows
            .iter()
            .enumerate()
            .filter_map(|(index, (_, important))| important.then_some(index))
            .collect::<Vec<_>>();
        let start = subtask_window_start(
            app.session.subtask_scroll,
            visible,
            rows.len(),
            &important,
        );
        rows.into_iter()
            .skip(start)
            .take(visible)
            .map(|(line, _)| line)
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
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

pub(crate) fn subtask_window_start(
    requested: usize,
    visible: usize,
    len: usize,
    important: &[usize],
) -> usize {
    let maximum = len.saturating_sub(visible);
    if visible == 0 || important.is_empty() {
        return requested.min(maximum);
    }
    let first = important[0];
    let last = *important.last().unwrap_or(&first);
    if last.saturating_sub(first) < visible {
        return requested
            .min(first)
            .max(last.saturating_add(1).saturating_sub(visible))
            .min(maximum);
    }
    (0..=maximum)
        .max_by_key(|start| {
            let end = start.saturating_add(visible);
            (
                important
                    .iter()
                    .filter(|index| **index >= *start && **index < end)
                    .count(),
                std::cmp::Reverse(*start),
            )
        })
        .unwrap_or(0)
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

    #[test]
    fn status_lines_fit_terminal_columns() {
        for (line, width, expected) in [
            ("warning", 0, ""),
            ("warning", 1, "…"),
            ("警告ab", 4, "警…"),
            ("e\u{301}rror", 3, "e\u{301}r…"),
        ] {
            let fitted = fit_status_line(line.to_owned(), width);
            assert_eq!(fitted, expected);
            assert!(Line::from(fitted).width() <= width);
        }
    }

    #[test]
    fn compact_deployment_keeps_all_five_section_headers() {
        let mut app = App::new(vec![], 8, 8);
        app.session.terminal =
            crate::tui::app::TerminalSize { width: 48, height: 16 };
        let rows =
            deployment_rows(Rect::new(0, 0, 48, 11), &app, LayoutMode::Compact);
        assert_eq!(rows.len(), DeploymentPane::ORDER.len());
        assert!(rows.iter().all(|row| row.height >= 1));
        assert_eq!(rows.iter().map(|row| row.height).sum::<u16>(), 11);
    }

    #[test]
    fn important_window_contains_a_separated_range_when_it_fits() {
        assert_eq!(subtask_window_start(99, 6, 12, &[3, 8]), 3);
    }

    #[test]
    fn important_window_uses_deterministic_dense_coverage_when_range_cannot_fit()
     {
        assert_eq!(subtask_window_start(99, 3, 12, &[1, 4, 5, 9]), 3);
        assert_eq!(subtask_window_start(0, 0, 12, &[1, 4]), 0);
    }
}
