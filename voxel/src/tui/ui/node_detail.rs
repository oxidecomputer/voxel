//! Per-node details pane, after wicket's inventory view: a title bar naming
//! the node, scrollable labelled sections, and a bar of the keys that apply.

use super::{
    colors::{
        OX_GREEN_LIGHT, OX_OFF_WHITE, OX_RED, OX_YELLOW, TUI_GREY, TUI_YELLOW,
    },
    monitor::{
        collection_error_is_visible, detail_area, health_status_label,
        health_style, rate_line, resource_health_state,
        resource_health_summary, sparkline_data, visible_traffic_error,
    },
    topology::health_glyph,
    widgets::{active_tab_style, format_rate, terminal_width},
};
use crate::tui::{
    App,
    telemetry::{ResourceDescriptor, ResourceKind, TrafficSeverity},
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};

/// Below this height the title and key bars would leave no room for content.
const CHROME_MIN_HEIGHT: u16 = 9;
const BULLET: &str = "  • ";

struct DetailLayout {
    title: Option<Rect>,
    content: Rect,
    keys: Option<Rect>,
}

fn detail_layout(area: Rect) -> DetailLayout {
    if area.height < CHROME_MIN_HEIGHT {
        return DetailLayout { title: None, content: area, keys: None };
    }
    let [title, content, keys] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(area);
    DetailLayout { title: Some(title), content, keys: Some(keys) }
}

fn frame_block(focused: bool) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded).border_style(
        if focused {
            active_tab_style()
        } else {
            Style::default().fg(TUI_GREY)
        },
    )
}

/// The content block joins the key bar's top border, as in wicket.
fn content_block(layout: &DetailLayout, focused: bool) -> Block<'static> {
    if layout.keys.is_some() {
        frame_block(focused)
            .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
    } else {
        Block::default()
    }
}

fn selected_descriptor(app: &App) -> Option<&ResourceDescriptor> {
    let id = app.session.selected_resource.as_ref()?;
    app.deployment.topology.iter().find(|descriptor| &descriptor.id == id)
}

/// Content viewport height and the furthest the content can scroll.
pub(crate) fn scroll_limits(app: &App) -> (usize, usize) {
    let (Some(area), Some(descriptor)) =
        (detail_area(app), selected_descriptor(app))
    else {
        return (0, 0);
    };
    let layout = detail_layout(area);
    let inner = content_block(&layout, true).inner(layout.content);
    let lines = Paragraph::new(detail_text(app, descriptor, inner.width))
        .wrap(Wrap { trim: false })
        .line_count(inner.width);
    let viewport = usize::from(inner.height);
    (viewport, lines.saturating_sub(viewport))
}

pub(crate) fn draw(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    focused: bool,
) {
    let Some(descriptor) = selected_descriptor(app) else {
        frame.render_widget(
            Paragraph::new("No node selected")
                .style(Style::default().fg(TUI_GREY)),
            area,
        );
        return;
    };
    let layout = detail_layout(area);
    if let Some(title) = layout.title {
        frame.render_widget(
            Paragraph::new(title_line(descriptor)).block(frame_block(focused)),
            title,
        );
    }
    let block = content_block(&layout, focused);
    let inner = block.inner(layout.content);
    frame.render_widget(block, layout.content);
    let paragraph = Paragraph::new(detail_text(app, descriptor, inner.width))
        .wrap(Wrap { trim: false });
    let limit = paragraph
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height));
    let scroll = if focused { app.session.detail_scroll.min(limit) } else { 0 };
    frame.render_widget(
        paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        inner,
    );
    if let Some(keys) = layout.keys {
        let border = frame_block(focused);
        frame.render_widget(
            Paragraph::new(key_line(focused)).block(border),
            keys,
        );
        let style = if focused {
            active_tab_style()
        } else {
            Style::default().fg(TUI_GREY)
        };
        let buffer = frame.buffer_mut();
        buffer[(keys.x, keys.y)].set_symbol("├").set_style(style);
        buffer[(keys.right() - 1, keys.y)].set_symbol("┤").set_style(style);
    }
}

fn title_line(descriptor: &ResourceDescriptor) -> Line<'static> {
    let place = descriptor
        .rack
        .map_or_else(|| "UPLINK".to_owned(), |rack| format!("RACK {}", rack.0));
    let kind = match descriptor.kind {
        ResourceKind::Sled => "SLED",
        ResourceKind::SwitchZone => "SWITCH ZONE",
        ResourceKind::Router => "ROUTER",
    };
    Line::from(vec![
        Span::styled(format!("{place} / "), Style::default().fg(TUI_GREY)),
        Span::styled(
            format!("{kind} {}", descriptor.name),
            Style::default().fg(OX_OFF_WHITE).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn key_line(focused: bool) -> Line<'static> {
    let pairs: &[(&str, &str)] = if focused {
        &[("Rack", "Esc"), ("Node", "←/→"), ("Scroll", "↑/↓")]
    } else {
        &[("Details", "Enter")]
    };
    let mut spans = Vec::new();
    for (function, key) in pairs {
        spans.push(Span::styled(
            format!("{function} "),
            Style::default().fg(OX_OFF_WHITE),
        ));
        spans.push(Span::styled(
            format!("<{key}>  "),
            Style::default().fg(TUI_YELLOW).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn heading(text: &str) -> Line<'static> {
    Line::styled(
        text.to_owned(),
        Style::default().fg(OX_OFF_WHITE).add_modifier(Modifier::BOLD),
    )
}

fn item(label: &str, value: impl Into<Span<'static>>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{BULLET}{label}: "),
            Style::default().fg(OX_OFF_WHITE),
        ),
        value.into(),
    ])
}

fn ok(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(OX_GREEN_LIGHT))
}

fn warn(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(OX_YELLOW))
}

fn bad(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(OX_RED))
}

fn listed(values: &[String]) -> Span<'static> {
    if values.is_empty() { warn("none") } else { ok(values.join(", ")) }
}

/// A one-line sparkline drawn with block elements so it scrolls with text.
fn sparkline(values: &[u64]) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().max().unwrap_or(0).max(1);
    values.iter().map(|value| LEVELS[(value * 7 / max) as usize]).collect()
}

fn detail_text(
    app: &App,
    descriptor: &ResourceDescriptor,
    width: u16,
) -> Text<'static> {
    let id = &descriptor.id;
    let mut lines = vec![heading("Identity")];
    lines.push(item("Kind", ok(format!("{:?}", descriptor.kind))));
    let location = match (descriptor.rack, descriptor.kind, descriptor.slot) {
        (None, _, _) => "fleet uplink".to_owned(),
        (Some(rack), ResourceKind::Sled, Some(cubby)) => {
            format!("rack {} · cubby {cubby}", rack.0)
        }
        (Some(rack), ResourceKind::SwitchZone, Some(slot)) => {
            format!("rack {} · switch {slot}", rack.0)
        }
        (Some(rack), _, _) => format!("rack {} · no rack bay", rack.0),
    };
    lines.push(item("Location", ok(location)));
    if let Some(host) = &descriptor.host {
        lines.push(item("Host sled", ok(host.clone())));
    }

    let state = resource_health_state(app, id);
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(
            "Health: ",
            Style::default().fg(OX_OFF_WHITE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} {}", health_glyph(state), health_status_label(state)),
            health_style(state),
        ),
    ]));
    lines.push(Line::styled(
        format!("{BULLET}{}", resource_health_summary(app, id)),
        Style::default().fg(OX_OFF_WHITE),
    ));

    lines.push(Line::default());
    lines.push(heading("Traffic"));
    let traffic = app
        .observability
        .telemetry
        .resources
        .get(id)
        .filter(|traffic| traffic.current_at.is_some());
    match traffic {
        Some(traffic) => {
            let rate = traffic.current_rate;
            let mut rates = vec![Span::styled(
                format!("{BULLET}Rate: "),
                Style::default().fg(OX_OFF_WHITE),
            )];
            rates.extend(rate_line(rate).spans);
            lines.push(Line::from(rates));
            lines.push(item(
                "Packets",
                ok(format!(
                    "RX {:.0}/s · TX {:.0}/s",
                    rate.rx_packets_sec, rate.tx_packets_sec
                )),
            ));
            let errors = &traffic.current_sample.errors;
            let error_span = format!(
                "RX {:.2}/s · TX {:.2}/s",
                errors.rx_sec, errors.tx_sec
            );
            lines.push(item(
                "Link errors",
                if errors.rx_sec > 0.0 || errors.tx_sec > 0.0 {
                    warn(error_span)
                } else {
                    ok(error_span)
                },
            ));
            lines.push(item(
                "Source",
                ok(traffic.current_sample.source.label()),
            ));
            let label = format!("{BULLET}History: ");
            let data = sparkline_data(
                traffic
                    .history
                    .points()
                    .iter()
                    .map(|point| point.rate.total_bytes_sec()),
                width.saturating_sub(terminal_width(&label) as u16),
            );
            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(OX_OFF_WHITE)),
                Span::styled(sparkline(&data), health_style(state)),
            ]));
        }
        None => lines.push(item("Rate", warn("collecting"))),
    }

    if descriptor.kind == ResourceKind::Sled {
        lines.push(Line::default());
        lines.push(heading("Diagnostics"));
        match app
            .observability
            .health
            .get(id)
            .and_then(|sample| sample.good.as_ref())
        {
            Some(good) => {
                let diagnostic = &good.value;
                lines.push(item(
                    "Sled agent",
                    match diagnostic.sled_agent {
                        Some(crate::tui::telemetry::ServiceState::Online) => {
                            ok("online")
                        }
                        Some(other) => bad(format!("{other:?}").to_lowercase()),
                        None => warn("unknown"),
                    },
                ));
                let ntp =
                    match (diagnostic.ntp.synchronized, diagnostic.ntp.stratum)
                    {
                        (Some(true), Some(stratum)) => {
                            ok(format!("synchronized, stratum {stratum}"))
                        }
                        (Some(true), None) => ok("synchronized"),
                        (Some(false), _) => warn("not synchronized"),
                        (None, _) => warn("unknown"),
                    };
                lines.push(item("NTP", ntp));
                lines.push(item(
                    "Failed services",
                    if diagnostic.failed_services.is_empty() {
                        ok("none")
                    } else {
                        bad(diagnostic.failed_services.join(", "))
                    },
                ));
                lines.push(item("Zones", listed(&diagnostic.zones.zones)));
                if !diagnostic.notes.is_empty() {
                    lines
                        .push(item("Notes", warn(diagnostic.notes.join("; "))));
                }
            }
            None => {
                lines.push(item("Status", warn("no successful health sample")))
            }
        }
    }

    lines.push(Line::default());
    lines.push(heading("Addresses"));
    match app
        .observability
        .addresses
        .get(id)
        .and_then(|sample| sample.good.as_ref())
    {
        Some(addresses) => {
            lines.push(item("IPv4", listed(&addresses.value.ipv4)));
            lines.push(item("IPv6", listed(&addresses.value.ipv6)));
        }
        None => lines.push(item("Status", warn("no successful sample"))),
    }

    let mut errors = Vec::new();
    if let Some(rack) = descriptor.rack {
        if let Some(pools) = app
            .observability
            .zfs_headroom
            .get(&rack)
            .and_then(|sample| sample.good.as_ref())
        {
            let pools = pools
                .value
                .iter()
                .filter(|pool| pool.id == *id)
                .collect::<Vec<_>>();
            if !pools.is_empty() {
                lines.push(Line::default());
                lines.push(heading("Storage"));
                for pool in pools {
                    lines.push(item(
                        &pool.pool,
                        ok(format!(
                            "{:.1} of {:.1} GiB free",
                            pool.available_bytes() as f64 / 1024.0_f64.powi(3),
                            pool.total_bytes as f64 / 1024.0_f64.powi(3)
                        )),
                    ));
                }
            }
        }
        if let Some(cpu) = app
            .observability
            .zone_cpu
            .get(&rack)
            .and_then(|sample| sample.good.as_ref())
        {
            let zones = cpu
                .value
                .iter()
                .filter(|zone| zone.id == *id)
                .collect::<Vec<_>>();
            if !zones.is_empty() {
                lines.push(Line::default());
                lines.push(heading("Zone CPU"));
                for zone in zones {
                    lines.push(item(
                        &zone.name,
                        ok(format!(
                            "{:.1}% · wait {:.1}%",
                            zone.total_percent(),
                            zone.wait_percent
                        )),
                    ));
                }
            }
        }
        for (label, error) in [
            (
                "Zone CPU unavailable",
                app.observability
                    .zone_cpu
                    .get(&rack)
                    .and_then(|sample| sample.latest_error.as_ref()),
            ),
            (
                "ZFS unavailable",
                app.observability
                    .zfs_headroom
                    .get(&rack)
                    .and_then(|sample| sample.latest_error.as_ref()),
            ),
        ] {
            if let Some(error) = error {
                errors.push((label, error.message.clone()));
            }
        }
    }

    let mut zones = traffic
        .into_iter()
        .flat_map(|traffic| traffic.current_sample.zones.iter())
        .collect::<Vec<_>>();
    if !zones.is_empty() {
        zones.sort_by(|left, right| {
            right
                .rate
                .total_bytes_sec()
                .total_cmp(&left.rate.total_bytes_sec())
                .then_with(|| left.name.cmp(&right.name))
        });
        lines.push(Line::default());
        lines.push(heading("Zone traffic"));
        for zone in zones {
            let total = zone.rate.total_bytes_sec();
            let style = super::widgets::traffic_style(
                TrafficSeverity::for_bytes_per_sec(total),
            );
            lines.push(item(
                &zone.short_name,
                Span::styled(
                    format!(
                        "RX {} · TX {} · Total {}",
                        format_rate(zone.rate.rx_bytes_sec),
                        format_rate(zone.rate.tx_bytes_sec),
                        format_rate(total)
                    ),
                    style,
                ),
            ));
        }
    }

    if let Some(error) = app
        .observability
        .health
        .get(id)
        .and_then(|sample| sample.latest_error.as_ref())
    {
        errors.push(("Health", error.message.clone()));
    }
    if let Some(error) = app
        .observability
        .addresses
        .get(id)
        .and_then(|sample| sample.latest_error.as_ref())
        .filter(|error| collection_error_is_visible(app, id, error))
    {
        errors.push(("Addresses", error.message.clone()));
    }
    if let Some(error) = visible_traffic_error(app, id) {
        errors.push(("Traffic", error.message.clone()));
    }
    if !errors.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(
                "Errors",
                Style::default().fg(OX_OFF_WHITE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · logged to {}", app.durable_log_path),
                Style::default().fg(TUI_GREY),
            ),
        ]));
        for (label, message) in errors {
            lines.push(item(label, bad(message)));
        }
    }
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        event::{AppEvent, MonitoringPane, View},
        reconcile::ObservedDeploymentState,
        telemetry::{
            RackId, ResourceDescriptor, ResourceId, ResourceKind, TrafficSample,
        },
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::{Duration, Instant};

    fn router() -> ResourceDescriptor {
        ResourceDescriptor {
            id: ResourceId::fleet(ResourceKind::Router, "ce"),
            rack: None,
            kind: ResourceKind::Router,
            name: "ce".into(),
            host: None,
            slot: None,
        }
    }

    fn sled() -> ResourceDescriptor {
        ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "g3"),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name: "g3".into(),
            host: None,
            slot: Some(3),
        }
    }

    fn render(app: &App, focused: bool) -> String {
        render_sized(app, focused, 100, 40)
    }

    fn render_sized(
        app: &App,
        focused: bool,
        width: u16,
        height: u16,
    ) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, frame.area(), app, focused)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn app_with(descriptor: ResourceDescriptor) -> App {
        let mut app = App::new(vec![descriptor.clone()], 4, 4);
        app.session.selected_resource = Some(descriptor.id);
        app
    }

    #[test]
    fn title_and_key_bars_follow_wicket() {
        let app = app_with(sled());
        let unfocused = render(&app, false);
        assert!(unfocused.contains("RACK 0 / SLED g3"), "{unfocused}");
        assert!(unfocused.contains("cubby 3"), "{unfocused}");
        assert!(unfocused.contains("Details <Enter>"), "{unfocused}");
        let focused = render(&app, true);
        assert!(focused.contains("Rack <Esc>"), "{focused}");
        assert!(focused.contains("Node <←/→>"), "{focused}");
        assert!(focused.contains('├') && focused.contains('┤'), "{focused}");
        let router = render(&app_with(router()), false);
        assert!(router.contains("UPLINK / ROUTER ce"), "{router}");
    }

    #[test]
    fn router_health_reads_checking_then_healthy() {
        let mut app = app_with(router());
        let now = Instant::now();
        app.deployment.observed = ObservedDeploymentState::Running;
        app.update(AppEvent::Tick { now });
        let checking = render(&app, false);
        assert!(checking.contains("Checking Status"), "{checking}");

        app.update(AppEvent::Traffic {
            id: router().id,
            at: now,
            sample: TrafficSample::default(),
        });
        let text = render(&app, false);
        assert!(text.contains("Healthy"), "{text}");
        assert!(text.contains("last success 0s ago"), "{text}");
        assert!(text.contains("Source: direct probe"), "{text}");
    }

    #[test]
    fn router_errors_before_reconciliation_stay_hidden() {
        let mut app = app_with(router());
        let before_reconciliation = Instant::now();
        app.update(AppEvent::TrafficFailed {
            id: router().id,
            at: before_reconciliation,
            message: "propolis uuid for ce: No such file".into(),
        });
        app.deployment.observed = ObservedDeploymentState::Running;
        app.deployment.last_reconciliation_at =
            Some(before_reconciliation + Duration::from_secs(1));
        let text = render(&app, false);
        assert!(!text.contains("propolis uuid"), "{text}");
        assert!(!text.contains("Errors"), "{text}");

        app.update(AppEvent::TrafficFailed {
            id: router().id,
            at: before_reconciliation + Duration::from_secs(2),
            message: "router command failed after readiness".into(),
        });
        let text = render(&app, false);
        assert!(text.contains("router command failed after readiness"));
    }

    #[test]
    fn sled_details_explain_unavailable_oximeter_diagnostics() {
        let mut app = app_with(sled());
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
        app.durable_log_path = "/work/voxel-tui.log".into();
        let text = render(&app, false);
        assert!(text.contains("no successful health sample"), "{text}");
        assert!(
            text.contains("Errors · logged to /work/voxel-tui.log"),
            "{text}"
        );
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
    fn scrolling_is_bounded_by_the_wrapped_content() {
        let mut app = app_with(sled());
        app.session.view = View::Monitor;
        app.session.monitoring_pane = MonitoringPane::Topology;
        app.session.terminal.width = 120;
        app.session.terminal.height = 24;
        let (viewport, limit) = scroll_limits(&app);
        assert!(viewport > 0);
        assert!(limit > 0, "a short pane should need scrolling");
        // Drawing clamps a stale offset to what the drawn pane can scroll.
        app.session.detail_scroll = usize::MAX;
        let bottom = render_sized(&app, true, 60, 14);
        assert!(!bottom.contains("Identity"), "{bottom}");
        assert!(bottom.contains("Addresses"), "{bottom}");
        assert!(render_sized(&app, false, 60, 14).contains("Identity"));
    }

    #[test]
    fn sparkline_scales_to_its_peak() {
        assert_eq!(sparkline(&[0, 7, 14]), "▁▄█");
        assert_eq!(sparkline(&[0, 0]), "▁▁");
        assert_eq!(sparkline(&[]), "");
    }
}
