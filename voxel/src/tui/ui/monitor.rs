use super::{
    colors::{OX_RED, TUI_GREEN, TUI_GREY, TUI_GREY_DARK, TUI_YELLOW},
    renderer::LayoutMode,
    widgets::{
        format_rate, section_block, section_heights, section_rects,
        traffic_style,
    },
};
use crate::{
    tui::reconcile::ObservedDeploymentState,
    tui::{
        App,
        event::MonitoringPane,
        telemetry::{
            BidirectionalRate, Freshness, HealthContext, HealthState,
            LatestSample, ResourceDescriptor, ResourceId, ResourceKind,
            TrafficSeverity, derive_health_state,
        },
    },
};
use ratatui::{
    layout::{Alignment, Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table},
};
use std::time::Duration;

const TRAFFIC_STALE_AFTER: Duration = Duration::from_secs(15);
const TRAFFIC_UNAVAILABLE_AFTER: Duration = Duration::from_secs(60);
// A sled health probe performs four serial calls, each with a ten-second
// timeout. Keep a successful sample healthy through one slow probe cycle.
const HEALTH_STALE_AFTER: Duration = Duration::from_secs(60);
const HEALTH_UNAVAILABLE_AFTER: Duration = Duration::from_secs(120);

pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    mode: LayoutMode,
) {
    let rows = monitor_rows(area, app, mode);
    super::rack_selector::draw(
        frame,
        rows[0],
        app,
        app.session.monitoring_pane == MonitoringPane::RackSummary,
        app.session.monitoring_expanded(MonitoringPane::RackSummary),
    );
    draw_topology(frame, rows[1], app, mode);
    draw_top_zones(frame, rows[2], app, app.session.selected_rack);
}

pub(crate) fn monitor_rows(
    area: Rect,
    app: &App,
    mode: LayoutMode,
) -> Vec<Rect> {
    let mut preferred = match (mode, area.height) {
        (LayoutMode::Wide, 0..=17) => [4, 3, 3],
        (LayoutMode::Wide, _) => [5, 12, 7],
        (LayoutMode::Compact, 0..=11) => [3, 3, 2],
        (LayoutMode::Compact, _) => [4, 8, 3],
        (LayoutMode::Minimum, _) => [1, 1, 1],
    };
    // The rack elevation needs far more rows than a summary does, so the
    // Topology section claims them while it has focus, short of the rack
    // summary's rows: those carry the rack's control-plane error banner.
    if app.session.monitoring_pane == MonitoringPane::Topology {
        preferred[1] = (super::topology::PREFERRED_HEIGHT + 2)
            .min(area.height.saturating_sub(preferred[0] + 1));
    }
    let expanded =
        MonitoringPane::ORDER.map(|pane| app.session.monitoring_expanded(pane));
    let focused = MonitoringPane::ORDER
        .iter()
        .position(|pane| *pane == app.session.monitoring_pane)
        .unwrap_or(1);
    let heights = section_heights(
        area.height,
        &expanded,
        focused,
        &preferred,
        &[0, 2, 1],
        1,
    );
    section_rects(area, &heights)
}

#[derive(Clone, Copy)]
pub(crate) struct MiddleLayout {
    pub(crate) topology: Rect,
    pub(crate) detail: Rect,
    pub(crate) divider: Rect,
}

/// Wide layouts show the rack and the selected node's details side by
/// side; compact ones show one at a time, as wicket does.
pub(crate) fn middle_layout(area: Rect, mode: LayoutMode) -> MiddleLayout {
    let inner = Block::bordered().inner(area);
    if mode != LayoutMode::Wide {
        return MiddleLayout {
            topology: inner,
            detail: inner,
            divider: Rect::default(),
        };
    }
    let topology_width =
        super::topology::column_width(inner.height).min(inner.width / 2);
    let divider = Rect::new(
        inner.x.saturating_add(topology_width),
        inner.y,
        1,
        inner.height,
    );
    MiddleLayout {
        topology: Rect::new(inner.x, inner.y, topology_width, inner.height),
        detail: Rect::new(
            divider.x.saturating_add(1),
            inner.y,
            inner.right().saturating_sub(divider.x.saturating_add(1)),
            inner.height,
        ),
        divider,
    }
}

/// Where the details pane is drawn, if the Topology section shows it.
pub(crate) fn detail_area(app: &App) -> Option<Rect> {
    let (area, mode) = super::widgets::content_area(app);
    let rows = monitor_rows(area, app, mode);
    let visible = app.session.monitoring_expanded(MonitoringPane::Topology)
        && (mode == LayoutMode::Wide || app.session.detail_open);
    visible.then(|| middle_layout(rows[1], mode).detail)
}

pub(crate) fn resource_health_state(app: &App, id: &ResourceId) -> HealthState {
    let kind = resource_kind(app, id);
    if kind != Some(ResourceKind::Sled) {
        // A slow Oximeter cycle must not mark a node down while its direct
        // probe still answers, so the fresher of the two decides.
        let shown = app.observability.traffic_failures.get(id);
        let direct = app.observability.direct_traffic.get(id);
        // A source that has never reported says nothing, so it ranks below
        // one that has reported a failure.
        let rank = |state: HealthState| match state {
            HealthState::Healthy => 3,
            HealthState::Stale => 2,
            HealthState::Unavailable => 1,
            _ => 0,
        };
        return [
            shown.map(|sample| collection_health_state(app, sample)),
            direct.map(|sample| collection_health_state(app, sample)),
        ]
        .into_iter()
        .flatten()
        .max_by_key(|state| rank(*state))
        .unwrap_or(HealthState::Unavailable);
    }
    let Some(sample) = app.observability.health.get(id) else {
        return HealthState::Unavailable;
    };
    let context = if app.deployment.observed == ObservedDeploymentState::Stopped
    {
        HealthContext::Stopped
    } else if app.now.is_none()
        || (sample.good.is_none() && sample.last_attempt.is_none())
    {
        HealthContext::Checking
    } else {
        HealthContext::Active
    };
    let freshness = app
        .now
        .map(|now| {
            sample.freshness(now, HEALTH_STALE_AFTER, HEALTH_UNAVAILABLE_AFTER)
        })
        .unwrap_or(Freshness::Unavailable);
    derive_health_state(
        context,
        sample.good.as_ref().map(|good| &good.value),
        freshness,
    )
}

fn collection_health_state<T>(
    app: &App,
    sample: &LatestSample<T>,
) -> HealthState {
    if app.deployment.observed == ObservedDeploymentState::Stopped {
        return HealthState::Stopped;
    }
    let Some(now) = app.now else {
        return HealthState::Checking;
    };
    if sample.good.is_none() && sample.last_attempt.is_none() {
        return HealthState::Checking;
    }
    match sample.freshness(now, TRAFFIC_STALE_AFTER, TRAFFIC_UNAVAILABLE_AFTER)
    {
        Freshness::Fresh => HealthState::Healthy,
        Freshness::Stale => HealthState::Stale,
        Freshness::Unavailable => HealthState::Unavailable,
    }
}

fn resource_kind(app: &App, id: &ResourceId) -> Option<ResourceKind> {
    app.deployment
        .topology
        .iter()
        .find(|descriptor| &descriptor.id == id)
        .map(|descriptor| descriptor.kind)
}

fn resource_last_success(
    app: &App,
    id: &ResourceId,
) -> Option<std::time::Instant> {
    if resource_kind(app, id) == Some(ResourceKind::Sled) {
        app.observability.health.get(id).and_then(|sample| {
            sample.good.as_ref().map(|good| good.captured_at)
        })
    } else {
        last_good(app.observability.traffic_failures.get(id))
            .max(last_good(app.observability.direct_traffic.get(id)))
    }
}

fn last_good<T>(
    sample: Option<&LatestSample<T>>,
) -> Option<std::time::Instant> {
    sample.and_then(|sample| sample.good.as_ref().map(|good| good.captured_at))
}

pub(crate) fn health_status_label(state: HealthState) -> &'static str {
    match state {
        HealthState::Healthy => "Healthy",
        HealthState::Degraded => "Degraded",
        HealthState::Failed => "Failed",
        HealthState::Checking
        | HealthState::Unknown
        | HealthState::Stale
        | HealthState::Unavailable
        | HealthState::Stopped => "Checking Status",
    }
}

pub(crate) fn resource_health_summary(app: &App, id: &ResourceId) -> String {
    let age = resource_last_success(app, id)
        .and_then(|captured_at| {
            app.now
                .map(|now| now.saturating_duration_since(captured_at).as_secs())
        })
        .map(|seconds| format!("{seconds}s ago"))
        .unwrap_or_else(|| "never".into());
    let error = if resource_kind(app, id) == Some(ResourceKind::Sled) {
        app.observability
            .health
            .get(id)
            .and_then(|sample| sample.latest_error.as_ref())
    } else {
        visible_traffic_error(app, id)
    }
    .map(|error| format!("; latest error: {}", error.message))
    .unwrap_or_default();
    format!(
        "{} (last success {age}){error}",
        health_status_label(resource_health_state(app, id))
    )
}

pub(crate) fn visible_traffic_error<'a>(
    app: &'a App,
    id: &ResourceId,
) -> Option<&'a crate::tui::telemetry::CollectionError> {
    let error =
        app.observability.traffic_failures.get(id)?.latest_error.as_ref()?;
    collection_error_is_visible(app, id, error).then_some(error)
}

pub(crate) fn collection_error_is_visible(
    app: &App,
    id: &ResourceId,
    error: &crate::tui::telemetry::CollectionError,
) -> bool {
    resource_kind(app, id) != Some(ResourceKind::Router)
        || (matches!(
            app.deployment.observed,
            ObservedDeploymentState::Running
                | ObservedDeploymentState::Degraded
        ) && app
            .deployment
            .last_reconciliation_at
            .is_some_and(|ready_at| error.attempted_at >= ready_at))
}

pub(crate) fn health_style(state: HealthState) -> Style {
    Style::default().fg(match state {
        HealthState::Healthy => TUI_GREEN,
        HealthState::Degraded | HealthState::Failed => OX_RED,
        HealthState::Checking => TUI_YELLOW,
        HealthState::Stale => TUI_GREY,
        HealthState::Unknown
        | HealthState::Unavailable
        | HealthState::Stopped => TUI_GREY_DARK,
    })
}

pub(crate) fn sparkline_data(
    values: impl DoubleEndedIterator<Item = f64>,
    width: u16,
) -> Vec<u64> {
    values
        .rev()
        .take(width as usize)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|value| value.max(0.0).min(u64::MAX as f64) as u64)
        .collect()
}

fn draw_topology(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    mode: LayoutMode,
) {
    let rack = app.session.selected_rack;
    let expanded = app.session.monitoring_expanded(MonitoringPane::Topology);
    let focused = app.session.monitoring_pane == MonitoringPane::Topology;
    let outer = section_block("Topology", expanded, focused);
    frame.render_widget(outer.clone(), area);
    if !expanded || outer.inner(area).height == 0 {
        return;
    }
    let layout = middle_layout(area, mode);
    let detail_focused = app.session.detail_open;
    if mode == LayoutMode::Wide || !detail_focused {
        let scoped = scoped_descriptors(app, rack);
        super::topology::draw(frame, layout.topology, app, &scoped);
    }
    if mode == LayoutMode::Wide || detail_focused {
        super::node_detail::draw(frame, layout.detail, app, detail_focused);
    }
    if layout.divider.area() == 0 {
        return;
    }
    let edge = super::widgets::line_style(focused);
    for y in layout.divider.y..layout.divider.bottom() {
        frame.render_widget(
            Paragraph::new("│").style(edge),
            Rect::new(layout.divider.x, y, 1, 1),
        );
    }
    frame.render_widget(
        Paragraph::new("┬").style(edge),
        Rect::new(layout.divider.x, area.y, 1, 1),
    );
    frame.render_widget(
        Paragraph::new("┴").style(edge),
        Rect::new(layout.divider.x, area.bottom() - 1, 1, 1),
    );
}

pub(crate) fn scoped_descriptors(
    app: &App,
    rack: Option<crate::tui::telemetry::RackId>,
) -> Vec<ResourceDescriptor> {
    app.deployment
        .topology
        .iter()
        .filter(|descriptor| monitor_scope_contains(app, rack, descriptor))
        .cloned()
        .collect()
}

pub(crate) fn top_zones_len(
    app: &App,
    rack: Option<crate::tui::telemetry::RackId>,
) -> usize {
    app.observability
        .telemetry
        .resources
        .values()
        .filter(|state| monitor_scope_contains(app, rack, &state.descriptor))
        .map(|state| state.current_sample.zones.len())
        .sum()
}

pub(crate) fn top_zones_page_capacity(app: &App) -> usize {
    let (area, mode) = super::widgets::content_area(app);
    let height = monitor_rows(area, app, mode)[2].height;
    if height == 3 { 1 } else { usize::from(height.saturating_sub(3)) }
}

fn monitor_scope_contains(
    app: &App,
    rack: Option<crate::tui::telemetry::RackId>,
    descriptor: &ResourceDescriptor,
) -> bool {
    descriptor.kind == ResourceKind::Router
        || descriptor.rack == rack
        || app.session.selected_resource.as_ref() == Some(&descriptor.id)
}

pub(crate) fn rate_line(rate: BidirectionalRate) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("RX {} ", format_rate(rate.rx_bytes_sec)),
            traffic_style(TrafficSeverity::for_bytes_per_sec(
                rate.rx_bytes_sec,
            )),
        ),
        Span::styled(
            format!("TX {} ", format_rate(rate.tx_bytes_sec)),
            traffic_style(TrafficSeverity::for_bytes_per_sec(
                rate.tx_bytes_sec,
            )),
        ),
        Span::styled(
            format!("Total {}", format_rate(rate.total_bytes_sec())),
            traffic_style(TrafficSeverity::for_bytes_per_sec(
                rate.total_bytes_sec(),
            )),
        ),
    ])
}

fn draw_top_zones(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    rack: Option<crate::tui::telemetry::RackId>,
) {
    let expanded = app.session.monitoring_expanded(MonitoringPane::TopZones);
    let mut zones = app
        .observability
        .telemetry
        .resources
        .values()
        .filter(|state| monitor_scope_contains(app, rack, &state.descriptor))
        .flat_map(|state| {
            state.current_sample.zones.iter().map(move |zone| {
                (&state.descriptor.id, &state.descriptor.name, zone)
            })
        })
        .collect::<Vec<_>>();
    zones.sort_by(|(_, ra, a), (_, rb, b)| {
        b.rate
            .total_bytes_sec()
            .total_cmp(&a.rate.total_bytes_sec())
            .then_with(|| ra.cmp(rb))
            .then_with(|| a.name.cmp(&b.name))
    });
    let capacity = if area.height == 3 {
        1
    } else {
        usize::from(area.height.saturating_sub(3))
    };
    let start = app
        .session
        .top_zones_scroll
        .min(zones.len().saturating_sub(capacity.max(1)));
    let end = start.saturating_add(capacity).min(zones.len());
    let title = if capacity > 0 && zones.len() > capacity {
        format!(
            "Top Zones by Traffic + CPU {}-{end} of {}",
            start + 1,
            zones.len()
        )
    } else {
        "Top Zones by Traffic + CPU".into()
    };
    let block = section_block(
        title,
        expanded,
        app.session.monitoring_pane == MonitoringPane::TopZones,
    );
    frame.render_widget(block.clone(), area);
    if !expanded || block.inner(area).height == 0 {
        return;
    }
    if area.height == 3 {
        let sample = zones
            .get(start)
            .map(|(_, resource, zone)| {
                format!(
                    "{resource} · {} · {}",
                    zone.short_name,
                    format_rate(zone.rate.total_bytes_sec())
                )
            })
            .unwrap_or_else(|| "No zone samples".into());
        frame.render_widget(Paragraph::new(sample).block(block), area);
        return;
    }
    let header = Row::new(["Resource", "Zone", "Total", "CPU", "Wait"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = zones
        .iter()
        .skip(start)
        .take(capacity)
        .map(|(resource_id, resource, zone)| {
            let rates = zone_rate_cells(zone.rate);
            let cpu = rack
                .and_then(|rack| app.observability.zone_cpu.get(&rack))
                .and_then(|sample| sample.good.as_ref())
                .and_then(|sample| {
                    sample.value.iter().find(|cpu| {
                        cpu.id == **resource_id && cpu.name == zone.name
                    })
                });
            Row::new([
                Cell::from((*resource).clone()),
                Cell::from(zone.short_name.clone()),
                rates[2].clone(),
                Cell::from(
                    cpu.map(|cpu| format!("{:.1}%", cpu.total_percent()))
                        .unwrap_or_else(|| "—".into()),
                ),
                Cell::from(
                    cpu.map(|cpu| format!("{:.1}%", cpu.wait_percent))
                        .unwrap_or_else(|| "—".into()),
                ),
            ])
        })
        .collect::<Vec<_>>();
    if zones.is_empty() {
        frame.render_widget(
            Paragraph::new("No zone samples").block(block),
            area,
        );
    } else {
        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Percentage(22),
                    Constraint::Percentage(28),
                    Constraint::Percentage(20),
                    Constraint::Percentage(15),
                    Constraint::Percentage(15),
                ],
            )
            .header(header)
            .block(block),
            area,
        );
    }
}

pub(crate) fn zone_rate_cells(rate: BidirectionalRate) -> [Cell<'static>; 3] {
    let cell = |value| {
        Cell::from(Line::from(format_rate(value)).alignment(Alignment::Right))
            .style(traffic_style(TrafficSeverity::for_bytes_per_sec(value)))
    };
    [
        cell(rate.rx_bytes_sec),
        cell(rate.tx_bytes_sec),
        cell(rate.total_bytes_sec()),
    ]
}

#[cfg(test)]
mod height_tests {
    use super::*;
    use crate::tui::reconcile::ObservedDeploymentState;
    use crate::tui::{
        event::AppEvent,
        telemetry::{
            HealthDiagnostic, RackId, ResourceKind, ServiceState,
            TrafficSample, ZoneTraffic,
        },
    };
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    use std::time::Instant;

    fn app() -> App {
        let descriptor = ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "seeded"),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name: "seeded".into(),
            host: None,
            slot: None,
        };
        let mut app = App::new(
            vec![ResourceDescriptor {
                id: descriptor.id.clone(),
                ..descriptor.clone()
            }],
            4,
            4,
        );
        app.update(AppEvent::Traffic {
            id: descriptor.id,
            at: Instant::now(),
            sample: TrafficSample {
                zones: vec![ZoneTraffic {
                    name: "height-two-zone-traffic".into(),
                    short_name: "height-two-zone-traffic".into(),
                    rate: BidirectionalRate {
                        rx_bytes_sec: 987_654.0,
                        ..Default::default()
                    },
                    errors: Default::default(),
                }],
                ..Default::default()
            },
        });
        app
    }

    fn assert_height_two_chrome(
        title: &str,
        draw_section: impl FnOnce(&mut ratatui::Frame<'_>, Rect, &App),
    ) {
        let mut terminal = Terminal::new(TestBackend::new(48, 2)).unwrap();
        let app = app();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new("I".repeat(96)), area);
                draw_section(frame, area, &app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains(title), "missing {title:?}: {text}");
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(47, 0)].symbol(), "╮");
        assert_eq!(buffer[(0, 1)].symbol(), "╰");
        assert_eq!(buffer[(47, 1)].symbol(), "╯");
        assert!(!text.contains('I'), "seeded content survived: {text}");
    }

    #[test]
    fn height_two_monitoring_sections_render_only_intact_chrome() {
        assert_height_two_chrome("Rack Summary", |frame, area, app| {
            super::super::rack_selector::draw(frame, area, app, false, true);
        });
        assert_height_two_chrome("Topology", |frame, area, app| {
            draw_topology(frame, area, app, LayoutMode::Wide);
        });
        assert_height_two_chrome("Top Zones by Traffic", |frame, area, app| {
            draw_top_zones(frame, area, app, Some(RackId(0)));
        });

        let mut terminal = Terminal::new(TestBackend::new(48, 2)).unwrap();
        let app = app();
        terminal
            .draw(|frame| {
                draw_top_zones(frame, frame.area(), &app, Some(RackId(0)))
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Top Zones by Traffic"));
        assert!(!text.contains("height-two-zone-traffic"));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(47, 0)].symbol(), "╮");
        assert_eq!(buffer[(0, 1)].symbol(), "╰");
        assert_eq!(buffer[(47, 1)].symbol(), "╯");
    }

    #[test]
    fn wide_topology_shows_details_beside_a_focus_coloured_divider() {
        let area = Rect::new(0, 0, 160, 20);
        let layout = middle_layout(area, LayoutMode::Wide);
        let render = |app: &App, mode| {
            let mut terminal =
                Terminal::new(TestBackend::new(160, 20)).unwrap();
            terminal
                .draw(|frame| draw_topology(frame, area, app, mode))
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let text = |buffer: &ratatui::buffer::Buffer| {
            buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        let focused = render(&app(), LayoutMode::Wide);
        assert_eq!(
            focused[(layout.divider.x, area.y)].fg,
            super::super::colors::TUI_GREEN_DARK
        );
        assert_eq!(
            focused[(layout.divider.x, area.y + 1)].fg,
            super::super::colors::TUI_GREEN_DARK
        );
        assert!(text(&focused).contains("RACK 0 / SLED seeded"));

        let mut inactive_app = app();
        inactive_app.session.monitoring_pane = MonitoringPane::TopZones;
        let inactive = render(&inactive_app, LayoutMode::Wide);
        assert_eq!(inactive[(layout.divider.x, area.y)].fg, TUI_GREY);
        assert_eq!(inactive[(layout.divider.x, area.y + 1)].fg, TUI_GREY);

        // Compact layouts show the rack, or the details once focused.
        let mut compact = app();
        assert!(
            !text(&render(&compact, LayoutMode::Compact)).contains("RACK 0 /")
        );
        compact.session.detail_open = true;
        assert!(
            text(&render(&compact, LayoutMode::Compact))
                .contains("RACK 0 / SLED seeded")
        );
    }

    #[test]
    fn focused_topology_leaves_the_rack_summary_its_rows() {
        let app = app();
        assert_eq!(app.session.monitoring_pane, MonitoringPane::Topology);
        for height in [30, 51, 80] {
            let rows = monitor_rows(
                Rect::new(0, 0, 160, height),
                &app,
                LayoutMode::Wide,
            );
            assert_eq!(rows[0].height, 5, "height {height}");
            assert!(rows[1].height > rows[2].height, "height {height}");
        }
    }

    #[test]
    fn top_zones_capacity_is_zero_when_only_chrome_is_visible() {
        let mut app = app();
        app.session.terminal =
            crate::tui::app::TerminalSize { width: 48, height: 16 };

        assert_eq!(top_zones_page_capacity(&app), 0);
    }

    #[test]
    fn successful_traffic_probe_makes_non_sled_resources_healthy() {
        let descriptors = [
            (ResourceKind::SwitchZone, Some(RackId(0)), "switch0"),
            (ResourceKind::Router, None, "ce"),
        ]
        .into_iter()
        .map(|(kind, rack, name)| ResourceDescriptor {
            id: rack.map_or_else(
                || ResourceId::fleet(kind, name),
                |rack| ResourceId::rack(rack, kind, name),
            ),
            rack,
            kind,
            name: name.into(),
            host: None,
            slot: None,
        })
        .collect::<Vec<_>>();
        let mut app = App::new(descriptors.clone(), 4, 4);
        let now = Instant::now();
        app.deployment.observed = ObservedDeploymentState::Running;
        app.update(AppEvent::Tick { now });

        for descriptor in &descriptors {
            assert_eq!(
                resource_health_state(&app, &descriptor.id),
                HealthState::Checking
            );
            app.update(AppEvent::Traffic {
                id: descriptor.id.clone(),
                at: now,
                sample: TrafficSample::default(),
            });
            assert_eq!(
                resource_health_state(&app, &descriptor.id),
                HealthState::Healthy
            );
        }
    }

    #[test]
    fn direct_probes_keep_a_switch_healthy_through_oximeter_gaps() {
        let id = ResourceId::rack(RackId(0), ResourceKind::SwitchZone, "g0");
        let mut app = App::new(
            vec![ResourceDescriptor {
                id: id.clone(),
                rack: Some(RackId(0)),
                kind: ResourceKind::SwitchZone,
                name: "switch0".into(),
                host: Some("g0".into()),
                slot: Some(0),
            }],
            4,
            4,
        );
        let start = Instant::now();
        app.deployment.observed = ObservedDeploymentState::Running;
        app.update(AppEvent::OximeterTraffic {
            id: id.clone(),
            at: start,
            samples: vec![(
                start,
                TrafficSample {
                    source: crate::tui::telemetry::TrafficSource::Oximeter,
                    ..Default::default()
                },
            )],
        });
        // Oximeter goes quiet for five minutes; the direct probe does not.
        let later = start + Duration::from_secs(300);
        app.update(AppEvent::Tick { now: later });
        assert_eq!(resource_health_state(&app, &id), HealthState::Unavailable);
        app.update(AppEvent::Traffic {
            id: id.clone(),
            at: later,
            sample: TrafficSample::default(),
        });
        assert_eq!(resource_health_state(&app, &id), HealthState::Healthy);
        assert!(
            resource_health_summary(&app, &id).contains("last success 0s ago")
        );
    }

    #[test]
    fn sled_health_does_not_expire_between_slow_probe_completions() {
        let descriptor = ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "g0"),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name: "g0".into(),
            host: None,
            slot: None,
        };
        let mut app = App::new(vec![descriptor.clone()], 4, 4);
        let sampled_at = Instant::now();
        app.deployment.observed = ObservedDeploymentState::Running;
        app.update(AppEvent::Health {
            id: descriptor.id.clone(),
            at: sampled_at,
            diagnostic: HealthDiagnostic {
                sled_agent: Some(ServiceState::Online),
                ntp: crate::tui::telemetry::NtpDiagnostic {
                    synchronized: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        });
        app.update(AppEvent::Tick {
            now: sampled_at + Duration::from_secs(45),
        });

        assert_eq!(
            resource_health_state(&app, &descriptor.id),
            HealthState::Healthy
        );
    }

    #[test]
    fn rack_summary_names_session_traffic_history() {
        let app = app();
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).unwrap();

        terminal
            .draw(|frame| {
                super::super::rack_selector::draw(
                    frame,
                    frame.area(),
                    &app,
                    false,
                    true,
                )
            })
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Rack traffic (this TUI session)"), "{text}");
    }

    #[test]
    fn top_zones_renders_the_scrolled_range() {
        let descriptor = ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "g0"),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name: "g0".into(),
            host: None,
            slot: None,
        };
        let mut app = App::new(vec![descriptor.clone()], 4, 4);
        app.session.top_zones_scroll = 1;
        app.update(AppEvent::Traffic {
            id: descriptor.id,
            at: Instant::now(),
            sample: TrafficSample {
                zones: (0..8)
                    .map(|index| ZoneTraffic {
                        name: format!("zone-{index}"),
                        short_name: format!("zone-{index}"),
                        rate: BidirectionalRate {
                            rx_bytes_sec: (8 - index) as f64,
                            ..Default::default()
                        },
                        errors: Default::default(),
                    })
                    .collect(),
                ..Default::default()
            },
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 7)).unwrap();

        terminal
            .draw(|frame| {
                draw_top_zones(frame, frame.area(), &app, Some(RackId(0)))
            })
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Top Zones by Traffic + CPU 2-5 of 8"), "{text}");
        assert!(!text.contains("zone-0"), "{text}");
        for zone in 1..=4 {
            assert!(text.contains(&format!("zone-{zone}")), "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        event::AppEvent,
        telemetry::{BidirectionalRate, TrafficSample},
    };
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
    use std::time::Instant;

    #[test]
    fn sparkline_fits_width_and_clamps_values() {
        assert_eq!(
            sparkline_data([-1.0, 2.0, 3.0, 4.0].into_iter(), 2),
            vec![3, 4]
        );
        assert_eq!(sparkline_data([-1.0, 2.9].into_iter(), 8), vec![0, 2]);
    }

    #[test]
    fn zone_rate_cells_keep_independent_severity() {
        let cells = zone_rate_cells(BidirectionalRate {
            rx_bytes_sec: 100_000.0,
            tx_bytes_sec: 5_000_000.0,
            ..Default::default()
        });
        let row = Row::new(cells);
        let table = Table::new(
            [row],
            [
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(12),
            ],
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 36, 1));
        table.render(Rect::new(0, 0, 36, 1), &mut buffer);
        assert_eq!(buffer[(5, 0)].fg, super::super::colors::OX_OFF_WHITE);
        assert_eq!(buffer[(17, 0)].fg, super::super::colors::TUI_YELLOW);
        assert_eq!(buffer[(29, 0)].fg, super::super::colors::OX_RED);
    }

    #[test]
    fn router_summary_hides_only_errors_before_running_reconciliation() {
        let id = ResourceId::fleet(ResourceKind::Router, "ce");
        let descriptor = ResourceDescriptor {
            id: id.clone(),
            rack: None,
            kind: ResourceKind::Router,
            name: "ce".into(),
            host: None,
            slot: None,
        };
        let mut app = App::new(vec![descriptor], 4, 4);
        let before_reconciliation = Instant::now();
        app.update(AppEvent::TrafficFailed {
            id: id.clone(),
            at: before_reconciliation,
            message: "propolis uuid for ce: No such file".into(),
        });
        app.deployment.observed = ObservedDeploymentState::Running;
        app.deployment.last_reconciliation_at =
            Some(before_reconciliation + Duration::from_secs(1));

        assert!(!resource_health_summary(&app, &id).contains("propolis uuid"));

        let after_reconciliation =
            before_reconciliation + Duration::from_secs(2);
        app.update(AppEvent::TrafficFailed {
            id: id.clone(),
            at: after_reconciliation,
            message: "router command failed after readiness".into(),
        });
        assert!(
            resource_health_summary(&app, &id)
                .contains("router command failed after readiness")
        );

        app.update(AppEvent::Traffic {
            id: id.clone(),
            at: after_reconciliation + Duration::from_secs(1),
            sample: TrafficSample::default(),
        });
        assert!(!resource_health_summary(&app, &id).contains("latest error"));
    }
}
