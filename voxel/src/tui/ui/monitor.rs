// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Monitor view. Lays out the rack summary, topology and node details,
//! and derives health and traffic freshness from collected telemetry.

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
mod tests {
    use super::*;
    use crate::tui::reconcile::{ObservedDeploymentState, RouteEvidence};
    use crate::tui::test_support::{
        rate, render, router, sample, sled, switch, text,
    };
    use crate::tui::{
        app::TerminalSize,
        event::AppEvent,
        telemetry::{
            HealthDiagnostic, NtpDiagnostic, RackId, ServiceState,
            TrafficSample, TrafficSource, ZoneTraffic,
        },
    };
    use std::time::Instant;

    #[test]
    fn resource_health_stays_accurate_between_probes() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let tick = |seconds| AppEvent::Tick { now: at(seconds) };

        let switch_zone = switch(0, 0, "g0");
        let sled_g0 = sled(0, 0);
        // (case, resource, [(when, event, health)], final summary)
        let cases = [
            (
                "switch whose Oximeter samples stop while direct probes answer",
                switch_zone.clone(),
                vec![
                    ("before any sample", tick(0), HealthState::Checking),
                    (
                        "after an Oximeter sample",
                        AppEvent::OximeterTraffic {
                            id: switch_zone.id.clone(),
                            at: at(0),
                            samples: vec![(
                                at(0),
                                sample(0.0, TrafficSource::Oximeter),
                            )],
                        },
                        HealthState::Healthy,
                    ),
                    (
                        "after five quiet minutes",
                        tick(300),
                        HealthState::Unavailable,
                    ),
                    (
                        "after a direct probe success in the Oximeter gap",
                        AppEvent::Traffic {
                            id: switch_zone.id.clone(),
                            at: at(300),
                            sample: TrafficSample::default(),
                        },
                        HealthState::Healthy,
                    ),
                ],
                "Healthy (last success 0s ago)",
            ),
            (
                "sled whose health probe is slow to complete",
                sled_g0.clone(),
                vec![
                    ("before any sample", tick(0), HealthState::Checking),
                    (
                        "after a healthy sample",
                        AppEvent::Health {
                            id: sled_g0.id.clone(),
                            at: at(0),
                            diagnostic: HealthDiagnostic {
                                sled_agent: Some(ServiceState::Online),
                                ntp: NtpDiagnostic {
                                    synchronized: Some(true),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                        },
                        HealthState::Healthy,
                    ),
                    // Four serial probe calls with ten-second timeouts, plus
                    // scheduling delay, can leave this long between
                    // completions.
                    ("50 s after the sample", tick(50), HealthState::Healthy),
                ],
                "Healthy (last success 50s ago)",
            ),
        ];

        for (case, resource, steps, summary) in cases {
            let id = resource.id.clone();
            let mut app = App::new(vec![resource], 4, 4);
            app.deployment.observed = ObservedDeploymentState::Running;
            for (when, event, health) in steps {
                app.update(event);
                assert_eq!(
                    resource_health_state(&app, &id),
                    health,
                    "{case}, {when}"
                );
            }
            assert_eq!(resource_health_summary(&app, &id), summary, "{case}");
        }
    }

    fn rack_summary_text(app: &App) -> String {
        text(&render(120, 5, |frame| {
            super::super::rack_selector::draw(
                frame,
                frame.area(),
                app,
                true,
                true,
            )
        }))
    }

    fn node_details_text(app: &App) -> String {
        text(&render(100, 40, |frame| {
            super::super::node_detail::draw(frame, frame.area(), app, false)
        }))
    }

    fn app_selecting(resource: ResourceDescriptor) -> App {
        let mut app = App::new(vec![resource.clone()], 4, 4);
        app.session.selected_resource = Some(resource.id);
        app
    }

    /// Asserts that `screen` shows every phrase, printing the screen when one
    /// is missing.
    #[track_caller]
    fn assert_shows(screen: &str, phrases: &[&str]) {
        for phrase in phrases {
            assert!(
                screen.contains(phrase),
                "{phrase:?} missing from:\n{screen}"
            );
        }
    }

    #[test]
    fn operator_is_told_why_monitoring_is_missing() {
        let now = Instant::now();
        let log = "/work/voxel-tui.log";

        // Nexus outage: the rack summary names the outage, the log and the
        // likely cause until Nexus answers again.
        let rack = RackId(0);
        let mut app = App::new(vec![sled(0, 0)], 4, 4);
        app.update(AppEvent::NexusUnavailable {
            rack,
            at: now,
            message: "Nexus is unavailable".into(),
        });
        app.durable_log_path = log.into();
        assert_shows(
            &rack_summary_text(&app),
            &[
                "Nexus/control plane unavailable · direct-probe fallback \
                 enabled · log: /work/voxel-tui.log",
                "No host route to the rack's external network",
            ],
        );
        app.deployment.routes = RouteEvidence::Applied;
        assert_shows(
            &rack_summary_text(&app),
            &["traffic, CPU, ZFS, collector health may be stale"],
        );
        // The traffic history takes the banner's place once Nexus answers.
        app.update(AppEvent::NexusAvailable {
            rack,
            at: now + Duration::from_secs(1),
        });
        assert_shows(
            &rack_summary_text(&app),
            &["Rack traffic (this TUI session)"],
        );

        // Oximeter queries failing: a sled's details explain each missing
        // diagnostic and where the errors are logged.
        let mut app = app_selecting(sled(0, 3));
        app.update(AppEvent::ZoneCpuFailed {
            rack,
            at: now,
            message: "CPU query timed out".into(),
        });
        app.update(AppEvent::ZfsHeadroomFailed {
            rack,
            at: now,
            message: "ZFS response omitted a pool".into(),
        });
        app.durable_log_path = log.into();
        assert_shows(
            &node_details_text(&app),
            &[
                "no successful health sample",
                "Errors · logged to /work/voxel-tui.log",
                "Zone CPU unavailable: CPU query timed out",
                "ZFS unavailable: ZFS response omitted a pool",
            ],
        );

        // Router probes fail while the VM is still coming up, so errors from
        // before the running reconciliation are noise; later ones are real,
        // and a success clears them.
        let id = router("ce").id;
        let mut app = app_selecting(router("ce"));
        app.update(AppEvent::TrafficFailed {
            id: id.clone(),
            at: now,
            message: "propolis uuid for ce: No such file".into(),
        });
        app.deployment.observed = ObservedDeploymentState::Running;
        app.deployment.last_reconciliation_at =
            Some(now + Duration::from_secs(1));
        let details = node_details_text(&app);
        assert!(!details.contains("propolis uuid"), "{details}");

        let after_reconciliation = now + Duration::from_secs(2);
        app.update(AppEvent::TrafficFailed {
            id: id.clone(),
            at: after_reconciliation,
            message: "router command failed after readiness".into(),
        });
        assert_shows(
            &node_details_text(&app),
            &["router command failed after readiness"],
        );
        app.update(AppEvent::Traffic {
            id: id.clone(),
            at: after_reconciliation + Duration::from_secs(1),
            sample: TrafficSample::default(),
        });
        assert!(!resource_health_summary(&app, &id).contains("latest error"));
    }

    #[test]
    fn operator_can_page_through_top_zones() {
        // Sled g0 alone, with eight zones whose traffic falls with their
        // index, so zone-0 tops the list.
        let descriptor = sled(0, 0);
        let mut app = App::new(vec![descriptor.clone()], 4, 4);
        app.update(AppEvent::Traffic {
            id: descriptor.id,
            at: Instant::now(),
            sample: TrafficSample {
                zones: (0..8)
                    .map(|index| ZoneTraffic {
                        name: format!("zone-{index}"),
                        short_name: format!("zone-{index}"),
                        rate: rate((8 - index) as f64, 0.0),
                        errors: Default::default(),
                    })
                    .collect(),
                ..Default::default()
            },
        });

        // The topology starts focused and takes most rows, but the rack
        // summary keeps the rows its error banner needs.
        assert_eq!(app.session.monitoring_pane, MonitoringPane::Topology);
        let rows =
            monitor_rows(Rect::new(0, 0, 160, 30), &app, LayoutMode::Wide);
        assert_eq!(rows[0].height, 5);
        assert!(rows[1].height > rows[2].height);

        app.session.top_zones_scroll = 1;
        let page = text(&render(100, 7, |frame| {
            draw_top_zones(frame, frame.area(), &app, Some(RackId(0)))
        }));
        let visible = page
            .lines()
            .filter_map(|line| {
                line.split_whitespace().find(|word| word.starts_with("zone-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(visible, ["zone-1", "zone-2", "zone-3", "zone-4"], "{page}");
        assert_shows(&page, &["Top Zones by Traffic + CPU 2-5 of 8"]);

        // At this size Top Zones shows only its borders and header.
        app.session.terminal = TerminalSize { width: 48, height: 16 };
        assert_eq!(top_zones_page_capacity(&app), 0);
    }
}
