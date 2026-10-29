// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The root layout and shared widgets: section sizing, the overlay area, the
//! action guidance bar, and width-aware text fitting.

use super::{
    colors::*,
    deployment,
    renderer::{LayoutMode, MIN_HEIGHT, MIN_WIDTH, layout_mode},
};
use crate::tui::{
    app::App,
    event::{Confirmation, View},
    telemetry::TrafficSeverity,
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Tabs},
};

pub(crate) fn terminal_width(text: &str) -> usize {
    Line::from(text).width()
}

pub(crate) fn fit_terminal_width(text: &str, width: usize) -> String {
    if terminal_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }

    let content_width = width - 1;
    let mut fitted = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = terminal_width(&character.to_string());
        if used + character_width > content_width {
            break;
        }
        fitted.push(character);
        used += character_width;
    }
    fitted.push('…');
    fitted
}

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    // Like wicket, paint the theme's background rather than the terminal's.
    frame.render_widget(Block::default().style(background()), frame.area());
    if let Some(splash) = &app.splash
        && super::splash::draw(frame, splash, app.now)
    {
        return;
    }
    let area = frame.area();
    let root = root_layout(area);
    if root.mode == LayoutMode::Minimum {
        minimum(frame, area);
        return;
    }
    let alerts = app.operation.retained_warnings.len()
        + usize::from(app.operation.start_failure.is_some())
        + usize::from(app.deployment.reconciliation_failure.is_some());
    let titles = ["Deployment", "Monitoring"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(usize::from(app.session.view == View::Monitor))
        .style(Style::default().fg(TUI_GREY))
        .highlight_style(header_style(true))
        .divider(Span::styled(" │ ", Style::default().fg(TUI_GREY_DARK)));
    let state = format!("{:?} │ alerts {alerts}", app.deployment.observed);
    if area.width < 80 {
        let header =
            Layout::vertical([Constraint::Length(1), Constraint::Length(2)])
                .split(root.header);
        frame.render_widget(
            Paragraph::new(format!(" VOXEL · {state}"))
                .style(Style::default().fg(OX_OFF_WHITE)),
            header[0],
        );
        frame.render_widget(
            tabs.block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(line_style(true)),
            ),
            header[1],
        );
    } else {
        let header =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(32)])
                .split(root.header);
        frame.render_widget(
            tabs.block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .title(" VOXEL ")
                    .border_style(line_style(true)),
            ),
            header[0],
        );
        frame.render_widget(
            Paragraph::new(state)
                .style(Style::default().fg(OX_OFF_WHITE))
                .right_aligned()
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM)
                        .border_style(line_style(true)),
                ),
            header[1],
        );
    }
    match app.session.view {
        View::Deployment => {
            deployment::draw(frame, root.content, app, root.mode)
        }
        View::Monitor => {
            super::monitor::draw(frame, root.content, app, root.mode)
        }
    }
    frame.render_widget(
        Paragraph::new(guidance_lines(app, root.footer.width)),
        root.footer,
    );
    if app.session.external_monitoring_open {
        super::external_monitoring::draw(frame);
    }
    if app.session.help_open {
        super::help::draw(frame, app);
    }
    if app.session.confirmation.is_some() {
        super::confirm_dialog::draw(frame, app);
    }
}

pub(crate) fn content_area(app: &App) -> (Rect, LayoutMode) {
    let terminal = Rect::new(
        0,
        0,
        app.session.terminal.width,
        app.session.terminal.height,
    );
    let root = root_layout(terminal);
    (root.content, root.mode)
}

pub(crate) fn overlay_area(area: Rect) -> Rect {
    root_layout(area).content
}

struct RootLayout {
    header: Rect,
    content: Rect,
    footer: Rect,
    mode: LayoutMode,
}

fn root_layout(area: Rect) -> RootLayout {
    let mode = layout_mode(area);
    if mode == LayoutMode::Minimum {
        return RootLayout {
            header: Rect::default(),
            content: Rect::default(),
            footer: Rect::default(),
            mode,
        };
    }
    let footer_height = if area.width >= 160 {
        1
    } else if area.width >= 100 {
        2
    } else {
        3
    };
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(footer_height),
    ])
    .split(area);
    RootLayout { header: rows[0], content: rows[1], footer: rows[2], mode }
}

fn grow_section(
    heights: &mut [u16],
    index: usize,
    preferred: &[u16],
    remaining: &mut u16,
) {
    if index >= heights.len() || index >= preferred.len() || *remaining == 0 {
        return;
    }
    let wanted = preferred[index].saturating_sub(heights[index]);
    let granted = wanted.min(*remaining);
    heights[index] = heights[index].saturating_add(granted);
    *remaining = remaining.saturating_sub(granted);
}

pub(crate) fn section_heights(
    total: u16,
    expanded: &[bool],
    focused: usize,
    preferred: &[u16],
    priority: &[usize],
    flexible: usize,
) -> Vec<u16> {
    let count = expanded.len().min(preferred.len());
    let mut heights = vec![0; count];
    for height in heights.iter_mut().take(total as usize) {
        *height = 1;
    }
    let mut remaining = total.saturating_sub(count as u16);
    if expanded.get(focused).copied().unwrap_or(false) {
        grow_section(&mut heights, focused, preferred, &mut remaining);
    }
    for &index in priority {
        if expanded.get(index).copied().unwrap_or(false) && index != focused {
            grow_section(&mut heights, index, preferred, &mut remaining);
        }
    }
    if expanded.get(flexible).copied().unwrap_or(false)
        && flexible < heights.len()
    {
        heights[flexible] = heights[flexible].saturating_add(remaining);
    }
    heights
}

pub(crate) fn section_rects(area: Rect, heights: &[u16]) -> Vec<Rect> {
    let mut y = area.y;
    heights
        .iter()
        .map(|height| {
            let available = area.bottom().saturating_sub(y);
            let height = (*height).min(available);
            let rect = Rect::new(area.x, y, area.width, height);
            y = y.saturating_add(height);
            rect
        })
        .collect()
}

#[derive(Clone, Copy)]
struct ActionGroup {
    key: &'static str,
    description: &'static str,
}

fn guidance_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let groups = action_groups(app, width < 80);
    let row_count = if width >= 160 {
        1
    } else if width >= 100 {
        2
    } else {
        3
    };
    let mut rows = vec![Vec::new(); row_count];
    let mut row = 0;
    for group in groups {
        let mut candidate = rows[row].clone();
        candidate.push(group);
        if row + 1 < row_count
            && styled_groups(candidate).width() > width as usize
        {
            row += 1;
        }
        rows[row].push(group);
    }
    rows.into_iter().map(styled_groups).collect()
}

fn styled_groups(groups: Vec<ActionGroup>) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, group) in groups.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        if !group.description.is_empty() {
            spans.push(Span::styled(
                format!("{} ", group.description),
                help_function_style(),
            ));
        }
        spans.push(Span::styled(format!("<{}>", group.key), help_key_style()));
    }
    Line::from(spans)
}

fn action_groups(app: &App, narrow: bool) -> Vec<ActionGroup> {
    let group = |key, description| ActionGroup { key, description };
    let lifecycle = || {
        vec![
            group("d", "detach"),
            group(
                "q",
                if app.resources_may_exist() { "destroy+quit" } else { "quit" },
            ),
        ]
    };
    let contextual_navigation = if narrow { "Tab/⇧" } else { "Tab/S-Tab" };
    if let Some(confirmation) = &app.session.confirmation {
        return vec![
            group("←/→", "move"),
            group("Enter", "select"),
            group(
                "y",
                if matches!(confirmation, Confirmation::Detach) {
                    "copy command"
                } else {
                    "confirm"
                },
            ),
            group("n/Esc", "back"),
        ];
    }
    if app.session.external_monitoring_open {
        return vec![
            group("s", "copy YAML"),
            group("a", "copy guide"),
            group("Esc", "close"),
        ];
    }
    if app.session.help_open {
        let mut groups = vec![group("↑/↓", "scroll"), group("?/Esc", "close")];
        groups.extend(lifecycle());
        return groups;
    }
    if app.session.detail_open {
        let mut groups = vec![
            group("↑/↓", "scroll"),
            group("←/→", "node"),
            group("Esc", "rack"),
            group("?", "help"),
        ];
        groups.extend(lifecycle());
        return groups;
    }
    if app.session.view == View::Monitor {
        let pane = app.session.monitoring_pane;
        let expanded = app.session.monitoring_expanded(pane);
        let mut groups =
            vec![group("Space", if expanded { "fold" } else { "expand" })];
        if expanded {
            match pane {
                crate::tui::event::MonitoringPane::RackSummary => {
                    groups.push(group("←/→", "rack"));
                }
                crate::tui::event::MonitoringPane::Topology => {
                    groups.push(group("↑↓←→", "node"));
                    if app.session.selected_resource.is_some() {
                        groups.push(group("Enter", "details"));
                    }
                }
                crate::tui::event::MonitoringPane::TopZones => {
                    groups.push(group("↑/↓", "zones"));
                    groups.push(group("Pg", if narrow { "" } else { "page" }));
                }
            }
        }
        if app.session.selected_resource.is_some() {
            groups.push(group("Esc", if narrow { "" } else { "clear" }));
        }
        groups.extend([
            group(contextual_navigation, "section"),
            group("v", "view"),
            group("x", if narrow { "" } else { "external monitoring" }),
        ]);
        groups.extend(lifecycle());
        groups.push(group("?", "help"));
        return groups;
    }
    let mut groups = Vec::new();
    if app.can_cancel() {
        // Leaving the deployment running is `d`, in the lifecycle group.
        groups.push(group("x", if narrow { "" } else { "destroy" }));
    } else {
        for (kind, key, description) in [
            (crate::tui::operation::OperationKind::Launch, "l", "launch"),
            (crate::tui::operation::OperationKind::Route, "r", "route"),
        ] {
            if app.can_start(kind) {
                groups.push(group(key, if narrow { "" } else { description }));
            }
        }
    }
    let pane = app.session.deployment_pane;
    let expanded = app.session.deployment_expanded(pane);
    groups.push(group("Space", if expanded { "fold" } else { "expand" }));
    if expanded {
        match pane {
            crate::tui::event::DeploymentPane::Phases => {
                groups.push(group("↑/↓", "item"));
                groups.push(group("Pg", if narrow { "" } else { "page" }));
            }
            crate::tui::event::DeploymentPane::Logs => {
                groups.push(group("f", "filter"));
                groups.push(group("↑/↓", "log"));
                groups.push(group("Pg", if narrow { "" } else { "page" }));
            }
            crate::tui::event::DeploymentPane::OverallProgress
            | crate::tui::event::DeploymentPane::Status
            | crate::tui::event::DeploymentPane::CurrentPhase => {}
        }
    }
    groups
        .extend([group(contextual_navigation, "section"), group("v", "view")]);
    groups.extend(lifecycle());
    groups.push(group("?", "help"));
    groups
}

fn minimum(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
    frame.render_widget(
        Paragraph::new(format!(
            "Terminal too small: {}x{}; required at least {}x{}",
            area.width, area.height, MIN_WIDTH, MIN_HEIGHT
        ))
        .block(primary_block(" VOXEL ")),
        area,
    );
}

pub fn format_rate(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.1} MB/s", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1} KB/s", value / 1_000.0)
    } else {
        format!("{value:.0} B/s")
    }
}

pub(crate) fn traffic_style(severity: TrafficSeverity) -> Style {
    Style::default().fg(match severity {
        TrafficSeverity::Normal => OX_OFF_WHITE,
        TrafficSeverity::Elevated => TUI_YELLOW,
        TrafficSeverity::High => OX_RED,
    })
}

// The styles below follow wicket's ui/defaults/style.rs.

/// The theme background painted under every frame.
pub(crate) fn background() -> Style {
    Style::default().bg(TUI_BLACK)
}

/// A highlighted item: the selected node, rack, or option.
pub(crate) fn selection_style() -> Style {
    Style::default().bg(TUI_PURPLE).fg(TUI_BLACK)
}

/// Border lines: dark green where focus is, grey elsewhere.
pub(crate) fn line_style(focused: bool) -> Style {
    Style::default().fg(if focused { TUI_GREEN_DARK } else { TUI_GREY })
}

/// Titles and active tabs: bright green where focus is, grey elsewhere.
pub(crate) fn header_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(TUI_GREEN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TUI_GREY)
    }
}

pub(crate) fn help_function_style() -> Style {
    Style::default().fg(TUI_GREEN)
}

pub(crate) fn help_key_style() -> Style {
    Style::default().fg(TUI_GREEN_DARK)
}

pub(crate) fn rounded_block<'a>() -> Block<'a> {
    Block::bordered().border_type(BorderType::Rounded)
}

/// Prepares a pop-up: fades the screen behind it, as wicket does, and
/// clears the pop-up's own area to the theme background.
pub(crate) fn popup_backdrop(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let screen = frame.area();
    frame
        .buffer_mut()
        .set_style(screen, Style::default().bg(TUI_BLACK).fg(TUI_GREY));
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(background()), area);
}

pub(crate) fn primary_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    rounded_block().title(title).border_style(line_style(true))
}

pub(crate) fn section_block<'a>(
    title: impl Into<Line<'a>>,
    expanded: bool,
    focused: bool,
) -> Block<'a> {
    let mut spans = vec![Span::raw(if expanded { " ▾ " } else { " ▸ " })];
    spans.extend(title.into().spans);
    rounded_block()
        .title(Line::from(spans))
        .border_style(line_style(focused))
        .title_style(header_style(focused))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        app::ActiveOperation,
        event::{Confirmation, DeploymentPane, OperationRequestId},
        operation::OperationKind,
        reconcile::ObservedDeploymentState,
        terminal::key_action,
        test_support::{key_codes, sled},
    };
    use crossterm::event::{KeyEvent, KeyModifiers};

    /// Every key a footer advertises, at either width, reaches an action, and
    /// each state keeps the group users find their way by. A retired or
    /// mistyped key fails the first check, so only bound keys that do nothing
    /// in a state need to be named as hidden.
    #[test]
    fn footer_advertises_only_keys_that_act() {
        let descriptor = sled(0, 0);
        let app = |setup: &dyn Fn(&mut App)| {
            let mut app = App::new(vec![descriptor.clone()], 8, 8);
            setup(&mut app);
            app
        };
        let monitor = |app: &mut App| app.session.view = View::Monitor;
        // (case, app, group shown, group prefix not shown)
        let cases: [(&str, App, &str, Option<&str>); 8] = [
            (
                "deployment idle",
                app(&|app| {
                    app.deployment.observed = ObservedDeploymentState::Stopped
                }),
                "l launch",
                Some("↑/↓ "),
            ),
            (
                "deployment logs",
                app(&|app| app.session.deployment_pane = DeploymentPane::Logs),
                "f filter",
                None,
            ),
            (
                "operation running",
                app(&|app| {
                    app.operation.active = Some(ActiveOperation::new(
                        OperationRequestId::FIRST,
                        OperationKind::Launch,
                    ))
                }),
                "x destroy",
                None,
            ),
            ("monitor", app(&monitor), "x external monitoring", None),
            (
                "node details",
                app(&|app| {
                    monitor(app);
                    app.session.selected_resource = Some(descriptor.id.clone());
                    app.session.detail_open = true;
                }),
                "Esc rack",
                None,
            ),
            (
                "help",
                app(&|app| app.session.help_open = true),
                "?/Esc close",
                None,
            ),
            (
                "confirmation",
                app(&|app| app.session.confirmation = Some(Confirmation::Quit)),
                "y confirm",
                None,
            ),
            (
                "external monitoring",
                app(&|app| {
                    monitor(app);
                    app.session.external_monitoring_open = true;
                }),
                "a copy guide",
                None,
            ),
        ];
        for (case, app, shown, hidden) in cases {
            for narrow in [false, true] {
                for group in action_groups(&app, narrow) {
                    for code in key_codes(group.key) {
                        assert!(
                            key_action(KeyEvent::new(code, KeyModifiers::NONE))
                                .is_some(),
                            "{case}: footer advertises {:?}, which no key binds",
                            group.key
                        );
                    }
                }
            }
            let groups = action_groups(&app, false)
                .into_iter()
                .map(|group| format!("{} {}", group.key, group.description))
                .collect::<Vec<_>>();
            assert!(
                groups.iter().any(|group| group == shown),
                "{case}: footer lacks {shown:?} in {groups:?}"
            );
            if let Some(prefix) = hidden {
                assert!(
                    !groups.iter().any(|group| group.starts_with(prefix)),
                    "{case}: footer shows {prefix:?} in {groups:?}"
                );
            }
        }
    }
}
