use super::{
    monitor::{
        health_style, resource_health_state, resource_health_summary,
        sparkline_data, zone_rate_cells,
    },
    widgets::{centered_rect, format_rate, overlay_area, selection_style},
};
use crate::tui::{App, telemetry::TrafficSeverity};
use ratatui::{
    layout::{Constraint, Layout},
    style::Modifier,
    text::Line,
    widgets::{Block, Cell, Clear, Paragraph, Row, Sparkline, Table},
};

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let Some(id) = app.session.selected_resource.as_ref() else {
        return;
    };
    let Some(d) = app.deployment.topology.iter().find(|d| &d.id == id) else {
        return;
    };
    let Some(t) = app.observability.telemetry.resources.get(id) else {
        return;
    };
    let area = centered_rect(85, 80, overlay_area(frame.area()));
    let rate = t.current_rate;
    let health = &app.observability.health[id];
    let addresses = &app.observability.addresses[id];
    let mut lines = vec![
        Line::from(format!(
            "Scope {:?} | Rack {:?} | {:?} {} | host {}",
            id.scope,
            d.rack,
            d.kind,
            d.name,
            d.host.as_deref().unwrap_or("n/a")
        )),
        Line::from(format!(
            "RX {} ({:.0} pkt/s) TX {} ({:.0} pkt/s) Total {} [{:?}]",
            format_rate(rate.rx_bytes_sec),
            rate.rx_packets_sec,
            format_rate(rate.tx_bytes_sec),
            rate.tx_packets_sec,
            format_rate(rate.total_bytes_sec()),
            TrafficSeverity::for_bytes_per_sec(rate.total_bytes_sec())
        )),
        Line::from(format!("Health: {}", resource_health_summary(app, id))),
    ];
    if let Some(g) = &health.good {
        lines.push(Line::from(format!(
            "Diagnostics: agent {:?}; NTP sync {:?} stratum {:?}; failed {}; zones {}; notes {}",
            g.value.sled_agent,
            g.value.ntp.synchronized,
            g.value.ntp.stratum,
            summarize(&g.value.failed_services, 3),
            summarize(&g.value.zones.zones, 3),
            summarize(&g.value.notes, 3)
        )));
    } else {
        lines.push(Line::from("Diagnostics: no successful health sample"));
    }
    lines.push(Line::from(format!(
        "Health latest error: {}",
        health
            .latest_error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("none")
    )));
    if let Some(a) = &addresses.good {
        lines.push(Line::from(format!(
            "IPv4: {} | IPv6: {}",
            summarize(&a.value.ipv4, 3),
            summarize(&a.value.ipv6, 3)
        )));
    } else {
        lines.push(Line::from("Addresses: no successful sample"));
    }
    lines.push(Line::from(format!(
        "Address latest error: {}",
        addresses
            .latest_error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("none")
    )));
    lines.push(Line::from(format!(
        "Traffic latest error: {}",
        app.observability.traffic_failures[id]
            .latest_error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("none")
    )));

    let mut zones = t.current_sample.zones.iter().collect::<Vec<_>>();
    zones.sort_by(|left, right| {
        right
            .rate
            .total_bytes_sec()
            .total_cmp(&left.rate.total_bytes_sec())
            .then_with(|| left.name.cmp(&right.name))
    });
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(Line::styled(
            " ▶ Resource detail — Enter/Esc closes ",
            selection_style().add_modifier(Modifier::REVERSED),
        ))
        .border_style(health_style(resource_health_state(app, id)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let diagnostic_height =
        (lines.len() as u16).min(inner.height.saturating_sub(4));
    let rows = Layout::vertical([
        Constraint::Length(diagnostic_height),
        Constraint::Length(2),
        Constraint::Min(2),
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(lines), rows[0]);
    let data = sparkline_data(
        t.history.points().iter().map(|point| point.rate.total_bytes_sec()),
        rows[1].width,
    );
    if data.is_empty() {
        frame.render_widget(
            Paragraph::new("History: collecting (no samples)"),
            rows[1],
        );
    } else {
        frame.render_widget(
            Sparkline::default()
                .data(&data)
                .style(health_style(resource_health_state(app, id)))
                .block(Block::default().title(" History ")),
            rows[1],
        );
    }
    if zones.is_empty() {
        frame.render_widget(
            Paragraph::new("No zone samples")
                .block(Block::default().title(" Zones ")),
            rows[2],
        );
        return;
    }
    let shown = rows[2].height.saturating_sub(2) as usize;
    let count = zones.len();
    let table_rows = zones.into_iter().take(shown).map(|zone| {
        let rates = zone_rate_cells(zone.rate);
        Row::new([
            Cell::from(zone.short_name.clone()),
            rates[0].clone(),
            rates[1].clone(),
            rates[2].clone(),
        ])
    });
    frame.render_widget(
        Table::new(
            table_rows,
            [
                Constraint::Percentage(40),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ],
        )
        .header(Row::new(["Zone", "RX", "TX", "Total"]))
        .block(Block::default().title(format!(
            " Zones · showing {} of {count} ",
            shown.min(count)
        ))),
        rows[2],
    );
}

fn summarize(values: &[String], limit: usize) -> String {
    if values.is_empty() {
        return "none".into();
    }
    let shown =
        values.iter().take(limit).cloned().collect::<Vec<_>>().join(", ");
    if values.len() > limit {
        format!("{shown} (+{})", values.len() - limit)
    } else {
        shown
    }
}
