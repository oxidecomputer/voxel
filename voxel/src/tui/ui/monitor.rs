use super::{
    colors::{
        OX_GREEN_LIGHT, OX_RED, TUI_GREEN, TUI_GREY, TUI_GREY_DARK, TUI_PURPLE,
        TUI_YELLOW,
    },
    renderer::LayoutMode,
    widgets::{
        fit_terminal_width, format_rate, section_block, section_heights,
        section_rects, terminal_width, traffic_style,
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
    widgets::{Block, Cell, Paragraph, Row, Sparkline, Table},
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
    let preferred = match (mode, area.height) {
        (LayoutMode::Wide, 0..=17) => [4, 3, 3],
        (LayoutMode::Wide, _) => [5, 12, 7],
        (LayoutMode::Compact, 0..=11) => [3, 3, 2],
        (LayoutMode::Compact, _) => [4, 8, 3],
        (LayoutMode::Minimum, _) => [1, 1, 1],
    };
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
    pub(crate) inspector: Rect,
    pub(crate) divider: Rect,
}

pub(crate) fn middle_layout(area: Rect, mode: LayoutMode) -> MiddleLayout {
    let inner = Block::bordered().inner(area);
    if mode == LayoutMode::Wide {
        let topology_width = ((u32::from(inner.width) * 3 / 5) as u16)
            .max(1)
            .min(inner.width.saturating_sub(1));
        let divider = Rect::new(
            inner.x.saturating_add(topology_width),
            inner.y,
            1,
            inner.height,
        );
        return MiddleLayout {
            topology: Rect::new(inner.x, inner.y, topology_width, inner.height),
            inspector: Rect::new(
                divider.x.saturating_add(1),
                inner.y,
                inner.right().saturating_sub(divider.x.saturating_add(1)),
                inner.height,
            ),
            divider,
        };
    }
    let inspector_height: u16 = if inner.height <= 4 { 2 } else { 3 };
    let topology_height =
        inner.height.saturating_sub(inspector_height.saturating_add(1)).min(9);
    let divider_y = inner.y.saturating_add(topology_height);
    MiddleLayout {
        topology: Rect::new(inner.x, inner.y, inner.width, topology_height),
        divider: Rect::new(
            inner.x,
            divider_y,
            inner.width,
            u16::from(inner.height > topology_height),
        ),
        inspector: Rect::new(
            inner.x,
            divider_y.saturating_add(1),
            inner.width,
            inner.bottom().saturating_sub(divider_y.saturating_add(1)),
        ),
    }
}

pub(crate) fn page_capacity(app: &App) -> usize {
    let (area, mode) = super::widgets::content_area(app);
    if !app.session.monitoring_expanded(MonitoringPane::Topology) {
        return 1;
    }
    let rows = monitor_rows(area, app, mode);
    if rows[1].height <= 2 {
        return 1;
    }
    let scoped = scoped_descriptors(app, app.session.selected_rack);
    let scene = super::topology::layout_scene(
        middle_layout(rows[1], mode).topology,
        mode,
        &scoped,
        app.session.selected_resource.as_ref(),
        app.session.monitor_scroll,
    );
    scene
        .tiers
        .iter()
        .find(|tier| {
            tier.visible_ids
                .iter()
                .any(|id| Some(id) == app.session.selected_resource.as_ref())
        })
        .map_or(1, |tier| tier.visible_ids.len())
        .max(1)
}

pub(crate) fn resource_health_state(app: &App, id: &ResourceId) -> HealthState {
    let kind = resource_kind(app, id);
    if kind != Some(ResourceKind::Sled) {
        return app
            .observability
            .traffic_failures
            .get(id)
            .map_or(HealthState::Unavailable, |sample| {
                collection_health_state(app, sample)
            });
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
        app.observability.traffic_failures.get(id).and_then(|sample| {
            sample.good.as_ref().map(|good| good.captured_at)
        })
    }
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
        app.observability
            .traffic_failures
            .get(id)
            .and_then(|sample| sample.latest_error.as_ref())
    }
    .map(|error| format!("; latest error: {}", error.message))
    .unwrap_or_default();
    format!(
        "{} (last success {age}){error}",
        health_status_label(resource_health_state(app, id))
    )
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
    let scoped = scoped_descriptors(app, rack);
    let scene = super::topology::layout_scene(
        layout.topology,
        mode,
        &scoped,
        app.session.selected_resource.as_ref(),
        app.session.monitor_scroll,
    );
    super::topology::draw(frame, &scene, app);
    let edge =
        Style::default().fg(if focused { TUI_YELLOW } else { OX_GREEN_LIGHT });
    for y in layout.divider.y..layout.divider.bottom() {
        let line = if mode == LayoutMode::Wide {
            "│".to_string()
        } else {
            "─".repeat(layout.divider.width.into())
        };
        frame.render_widget(
            Paragraph::new(line).style(edge),
            Rect::new(layout.divider.x, y, layout.divider.width, 1),
        );
    }
    if mode == LayoutMode::Wide && area.height >= 2 {
        frame.render_widget(
            Paragraph::new("┬").style(edge),
            Rect::new(layout.divider.x, area.y, 1, 1),
        );
        frame.render_widget(
            Paragraph::new("┴").style(edge),
            Rect::new(layout.divider.x, area.bottom() - 1, 1, 1),
        );
    } else if layout.divider.height > 0 {
        frame.render_widget(
            Paragraph::new("├").style(edge),
            Rect::new(area.x, layout.divider.y, 1, 1),
        );
        frame.render_widget(
            Paragraph::new("┤").style(edge),
            Rect::new(area.right() - 1, layout.divider.y, 1, 1),
        );
    }
    if layout.inspector.width > 0 {
        let title = fit_terminal_width(
            " Selected Resource ",
            layout.inspector.width.into(),
        );
        frame.render_widget(
            Paragraph::new(title.clone()).style(
                Style::default().fg(TUI_PURPLE).add_modifier(Modifier::BOLD),
            ),
            Rect::new(
                layout.inspector.x,
                if mode == LayoutMode::Wide {
                    area.y
                } else {
                    layout.divider.y
                },
                terminal_width(&title) as u16,
                1,
            ),
        );
        draw_selected_resource_inspector(frame, layout.inspector, app);
    }
}

fn draw_selected_resource_inspector(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
) {
    let Some(id) = app.session.selected_resource.as_ref() else {
        frame.render_widget(Paragraph::new("No resource selected"), area);
        return;
    };
    let Some(descriptor) =
        app.deployment.topology.iter().find(|descriptor| &descriptor.id == id)
    else {
        frame.render_widget(Paragraph::new("No resource selected"), area);
        return;
    };
    if area.height <= 7 {
        let traffic = app.observability.telemetry.resources.get(id);
        let sampled = traffic.and_then(|value| value.current_at).is_some();
        let mut lines = Vec::new();
        if area.height >= 3 {
            lines.push(Line::from(vec![
                Span::raw(format!(
                    "{:?} {} · ",
                    descriptor.kind, descriptor.name
                )),
                Span::styled(
                    health_status_label(resource_health_state(app, id)),
                    health_style(resource_health_state(app, id)),
                ),
            ]));
        }
        lines.push(if sampled {
            let traffic = traffic.expect("sampled traffic exists");
            Line::from(format!(
                "{} · source: {}",
                rate_line(traffic.current_rate),
                traffic.current_sample.source.label()
            ))
        } else {
            Line::from("RX — TX — Total — · collecting")
        });
        let data = sparkline_data(
            traffic.into_iter().flat_map(|value| {
                value
                    .history
                    .points()
                    .iter()
                    .map(|point| point.rate.total_bytes_sec())
            }),
            area.width.saturating_sub(9),
        );
        if data.is_empty() {
            lines.push(Line::from("History: collecting (no samples)"));
            frame.render_widget(Paragraph::new(lines), area);
        } else {
            lines.push(Line::from("History "));
            let history_y =
                area.y.saturating_add(lines.len().saturating_sub(1) as u16);
            frame.render_widget(Paragraph::new(lines), area);
            let label_width = terminal_width("History ") as u16;
            frame.render_widget(
                Sparkline::default()
                    .data(&data)
                    .style(health_style(resource_health_state(app, id))),
                Rect::new(
                    area.x.saturating_add(label_width),
                    history_y,
                    area.width.saturating_sub(label_width),
                    1,
                ),
            );
        }
        return;
    }
    let age = resource_last_success(app, id)
        .and_then(|captured_at| {
            app.now
                .map(|now| now.saturating_duration_since(captured_at).as_secs())
        })
        .map(|age| format!("{age}s ago"))
        .unwrap_or_else(|| "never".into());
    let rack = descriptor
        .rack
        .map(|rack| format!("Rack {}", rack.0))
        .unwrap_or_else(|| "Fleet".into());
    let host = descriptor
        .host
        .as_ref()
        .map(|host| format!(" · host {host}"))
        .unwrap_or_default();
    let mut lines = vec![
        Line::from(format!("{:?} {}", descriptor.kind, descriptor.name)),
        Line::from(format!("{rack}{host}")),
        Line::from(Span::styled(
            health_status_label(resource_health_state(app, id)),
            health_style(resource_health_state(app, id)),
        )),
        Line::from(format!("Last success {age}")),
    ];
    let traffic = app.observability.telemetry.resources.get(id);
    let sampled = traffic.and_then(|traffic| traffic.current_at).is_some();
    if let Some(traffic) = traffic.filter(|_| sampled) {
        lines.push(rate_line(traffic.current_rate));
        lines.push(Line::from(format!(
            "Packets RX {:.0}/s · TX {:.0}/s",
            traffic.current_rate.rx_packets_sec,
            traffic.current_rate.tx_packets_sec
        )));
        lines.push(Line::from(format!(
            "Link errors RX {:.2}/s · TX {:.2}/s · source: {}",
            traffic.current_sample.errors.rx_sec,
            traffic.current_sample.errors.tx_sec,
            traffic.current_sample.source.label()
        )));
    } else {
        lines.push(Line::from("Traffic: collecting/unavailable"));
    }
    if let Some(error) = latest_collection_error(app, id) {
        lines.push(Line::from(format!("Latest error: {}", error.message)));
    }
    if let Some(rack) = descriptor.rack {
        if let Some(zfs) = app
            .observability
            .zfs_headroom
            .get(&rack)
            .and_then(|sample| sample.good.as_ref())
        {
            let pools = zfs.value.iter().filter(|pool| pool.id == *id);
            let (available, total, count) = pools.fold(
                (0_u64, 0_u64, 0_usize),
                |(available, total, count), pool| {
                    (
                        available.saturating_add(pool.available_bytes()),
                        total.saturating_add(pool.total_bytes),
                        count + 1,
                    )
                },
            );
            if count > 0 {
                lines.push(Line::from(format!(
                    "ZFS {:.1}/{:.1} GiB free · {count} pools",
                    available as f64 / 1024.0_f64.powi(3),
                    total as f64 / 1024.0_f64.powi(3)
                )));
            }
        }
        if let Some(error) = app
            .observability
            .zone_cpu
            .get(&rack)
            .and_then(|sample| sample.latest_error.as_ref())
        {
            lines.push(Line::from(format!(
                "Zone CPU unavailable: {}",
                error.message
            )));
        }
        if let Some(error) = app
            .observability
            .zfs_headroom
            .get(&rack)
            .and_then(|sample| sample.latest_error.as_ref())
        {
            lines.push(Line::from(format!(
                "ZFS unavailable: {}",
                error.message
            )));
        }
    }
    lines.push(Line::from("History (60s)"));
    let guidance_y = area.bottom().saturating_sub(1);
    let text_height =
        lines.len().min(area.height.saturating_sub(1) as usize) as u16;
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, text_height),
    );
    let mut y = area.y.saturating_add(text_height);
    if y < guidance_y {
        let data = sparkline_data(
            traffic.into_iter().flat_map(|traffic| {
                traffic
                    .history
                    .points()
                    .iter()
                    .map(|point| point.rate.total_bytes_sec())
            }),
            area.width,
        );
        if data.is_empty() {
            frame.render_widget(
                Paragraph::new("History: collecting (no samples)"),
                Rect::new(area.x, y, area.width, 1),
            );
        } else {
            frame.render_widget(
                Sparkline::default()
                    .data(&data)
                    .style(health_style(resource_health_state(app, id))),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        y += 1;
    }
    let mut zones = traffic
        .into_iter()
        .flat_map(|traffic| traffic.current_sample.zones.iter())
        .collect::<Vec<_>>();
    zones.sort_by(|a, b| {
        b.rate
            .total_bytes_sec()
            .total_cmp(&a.rate.total_bytes_sec())
            .then_with(|| a.name.cmp(&b.name))
    });
    if y < guidance_y {
        frame.render_widget(
            Paragraph::new("Zones for selected resource")
                .style(Style::default().add_modifier(Modifier::BOLD)),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }
    if let Some(rack) = descriptor.rack
        && let Some(cpu) = app
            .observability
            .zone_cpu
            .get(&rack)
            .and_then(|sample| sample.good.as_ref())
    {
        for zone in cpu
            .value
            .iter()
            .filter(|zone| zone.id == *id)
            .take(guidance_y.saturating_sub(y) as usize)
        {
            frame.render_widget(
                Paragraph::new(format!(
                    "CPU {} {:.1}% wait {:.1}%",
                    zone.name,
                    zone.total_percent(),
                    zone.wait_percent
                )),
                Rect::new(area.x, y, area.width, 1),
            );
            y += 1;
        }
    }
    if zones.is_empty() && y < guidance_y {
        frame.render_widget(
            Paragraph::new("No zone samples"),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }
    let capacity = guidance_y.saturating_sub(y) as usize;
    let shown = if zones.len() > capacity {
        capacity.saturating_sub(1)
    } else {
        capacity
    };
    for zone in zones.iter().take(shown) {
        frame.render_widget(
            Paragraph::new(format!(
                "{} {}",
                zone.short_name,
                format_rate(zone.rate.total_bytes_sec())
            ))
            .style(traffic_style(
                TrafficSeverity::for_bytes_per_sec(zone.rate.total_bytes_sec()),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }
    if zones.len() > shown && y < guidance_y {
        frame.render_widget(
            Paragraph::new(format!("+{} more", zones.len() - shown)),
            Rect::new(area.x, y, area.width, 1),
        );
    }
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new("Enter opens full detail")
                .style(Style::default().fg(TUI_GREY)),
            Rect::new(area.x, guidance_y, area.width, 1),
        );
    }
}

fn latest_collection_error<'a>(
    app: &'a App,
    id: &ResourceId,
) -> Option<&'a crate::tui::telemetry::CollectionError> {
    [
        app.observability
            .health
            .get(id)
            .and_then(|sample| sample.latest_error.as_ref()),
        app.observability
            .addresses
            .get(id)
            .and_then(|sample| sample.latest_error.as_ref()),
        app.observability
            .traffic_failures
            .get(id)
            .and_then(|sample| sample.latest_error.as_ref()),
    ]
    .into_iter()
    .flatten()
    .max_by_key(|error| error.attempted_at)
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

fn rate_line(rate: BidirectionalRate) -> Line<'static> {
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
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert_eq!(buffer[(47, 0)].symbol(), "┐");
        assert_eq!(buffer[(0, 1)].symbol(), "└");
        assert_eq!(buffer[(47, 1)].symbol(), "┘");
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
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert_eq!(buffer[(47, 0)].symbol(), "┐");
        assert_eq!(buffer[(0, 1)].symbol(), "└");
        assert_eq!(buffer[(47, 1)].symbol(), "┘");
    }

    #[test]
    fn topology_divider_follows_focus_without_recoloring_selection() {
        let area = Rect::new(0, 0, 160, 20);
        let layout = middle_layout(area, LayoutMode::Wide);
        let render = |app: &App| {
            let mut terminal =
                Terminal::new(TestBackend::new(160, 20)).unwrap();
            terminal
                .draw(|frame| draw_topology(frame, area, app, LayoutMode::Wide))
                .unwrap();
            terminal.backend().buffer().clone()
        };

        let focused = render(&app());
        assert_eq!(focused[(layout.divider.x, area.y)].fg, TUI_YELLOW);
        assert_eq!(focused[(layout.divider.x, area.y + 1)].fg, TUI_YELLOW);
        assert_eq!(focused[(layout.inspector.x + 1, area.y)].fg, TUI_PURPLE);

        let mut inactive_app = app();
        inactive_app.session.monitoring_pane = MonitoringPane::TopZones;
        let inactive = render(&inactive_app);
        assert_eq!(inactive[(layout.divider.x, area.y)].fg, OX_GREEN_LIGHT);
        assert_eq!(inactive[(layout.divider.x, area.y + 1)].fg, OX_GREEN_LIGHT);
        assert_eq!(inactive[(layout.inspector.x + 1, area.y)].fg, TUI_PURPLE);
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
    fn sled_health_does_not_expire_between_slow_probe_completions() {
        let descriptor = ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "g0"),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name: "g0".into(),
            host: None,
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
    fn non_sled_inspector_uses_simple_health_status() {
        let descriptor = ResourceDescriptor {
            id: ResourceId::fleet(ResourceKind::Router, "ce"),
            rack: None,
            kind: ResourceKind::Router,
            name: "ce".into(),
            host: None,
        };
        let mut app = App::new(vec![descriptor.clone()], 4, 4);
        let now = Instant::now();
        app.deployment.observed = ObservedDeploymentState::Running;
        app.session.selected_resource = Some(descriptor.id.clone());
        app.update(AppEvent::Tick { now });
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| {
                draw_selected_resource_inspector(frame, frame.area(), &app)
            })
            .unwrap();
        let checking = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(checking.contains("Checking Status"), "{checking}");

        app.update(AppEvent::Traffic {
            id: descriptor.id,
            at: now,
            sample: TrafficSample::default(),
        });

        terminal
            .draw(|frame| {
                draw_selected_resource_inspector(frame, frame.area(), &app)
            })
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Healthy"), "{text}");
        assert!(!text.contains("Freshness"), "{text}");
        assert!(text.contains("Last success 0s ago"), "{text}");
        assert!(text.contains("source: direct probe"), "{text}");
    }

    #[test]
    fn sled_inspector_explains_unavailable_oximeter_diagnostics() {
        let mut app = app();
        let id = app.deployment.topology[0].id.clone();
        app.session.selected_resource = Some(id);
        let now = Instant::now();
        app.update(AppEvent::ZoneCpuFailed {
            rack: RackId(0),
            at: now,
            message: "CPU query timed out".into(),
        });
        app.update(AppEvent::ZfsHeadroomFailed {
            rack: RackId(0),
            at: now,
            message: "ZFS response omitted a pool".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();

        terminal
            .draw(|frame| {
                draw_selected_resource_inspector(frame, frame.area(), &app)
            })
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            text.contains("Zone CPU unavailable: CPU query timed out"),
            "{text}"
        );
        assert!(
            text.contains("ZFS unavailable: ZFS response omitted a pool"),
            "{text}"
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
    use crate::tui::telemetry::BidirectionalRate;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

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
}
