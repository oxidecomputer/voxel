use super::{
    monitor::{resource_health_state, sparkline_data},
    widgets::{format_rate, section_block, selection_style, traffic_style},
};
use crate::{
    tui::reconcile::ObservedDeploymentState,
    tui::reconcile::RssObservation,
    tui::{
        App,
        telemetry::{Freshness, HealthState, LatestSample, TrafficSeverity},
    },
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Paragraph, Sparkline},
};
use std::{collections::BTreeSet, time::Duration};

pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    focused: bool,
    expanded: bool,
) {
    let block = section_block("Rack Summary", expanded, focused);
    frame.render_widget(block.clone(), area);
    let inner = block.inner(area);
    if !expanded || inner.height == 0 {
        return;
    }
    let racks = app
        .deployment
        .topology
        .iter()
        .filter_map(|descriptor| descriptor.rack)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let selected_index = app
        .session
        .selected_rack
        .and_then(|rack| racks.iter().position(|candidate| *candidate == rack))
        .unwrap_or(0);
    let Some(rack) = racks.get(selected_index).copied() else {
        frame.render_widget(
            Paragraph::new(
                "No racks configured · RSS unavailable · no traffic samples",
            )
            .block(block),
            area,
        );
        return;
    };
    let rate = app
        .observability
        .telemetry
        .rack_rates
        .get(&rack)
        .copied()
        .unwrap_or_default();
    let (healthy, unhealthy, checking) = app
        .deployment
        .topology
        .iter()
        .filter(|descriptor| descriptor.rack == Some(rack))
        .map(|descriptor| resource_health_state(app, &descriptor.id))
        .fold((0, 0, 0), |(healthy, unhealthy, checking), state| match state {
            HealthState::Healthy => (healthy + 1, unhealthy, checking),
            HealthState::Degraded
            | HealthState::Failed
            | HealthState::Stale => (healthy, unhealthy + 1, checking),
            _ => (healthy, unhealthy, checking + 1),
        });
    let rss = app
        .deployment
        .rss
        .get(&rack)
        .map(|sample| rss_summary(app, sample))
        .unwrap_or_else(|| "RSS unavailable (no sample)".into());
    let zfs = app
        .observability
        .zfs_headroom
        .get(&rack)
        .and_then(|sample| sample.good.as_ref())
        .map(|sample| {
            let available: u64 =
                sample.value.iter().map(|pool| pool.available_bytes()).sum();
            let total: u64 =
                sample.value.iter().map(|pool| pool.total_bytes).sum();
            format!(
                "ZFS {:.1}/{:.1} GiB free",
                available as f64 / 1024.0_f64.powi(3),
                total as f64 / 1024.0_f64.powi(3)
            )
        })
        .unwrap_or_else(|| "ZFS —".into());
    let exceptions = app
        .observability
        .oximeter_exceptions
        .get(&rack)
        .and_then(|sample| sample.good.as_ref())
        .map(|sample| {
            format!(
                "Oximeter collection health: {} failures / {} dropped",
                sample.value.failed_collections, sample.value.dropped_samples
            )
        })
        .unwrap_or_else(|| "Oximeter collection health unavailable".into());
    let compact = area.width < 100;
    let title = if compact {
        format!(" Rack {} ◀ {}/{} ▶ ", rack.0, selected_index + 1, racks.len())
    } else {
        format!(
            " Rack {} ◀ {}/{} ▶ · selected rack summary ",
            rack.0,
            selected_index + 1,
            racks.len()
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            ratatui::text::Span::styled(title, selection_style()),
            ratatui::text::Span::raw(format!(" · {rss}")),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
        .split(Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        ));
    let rate_text = if compact {
        format!(
            "RX {} TX {} Σ{} · {healthy}/{unhealthy}/{checking} · {zfs} · {exceptions}",
            format_rate(rate.rx_bytes_sec),
            format_rate(rate.tx_bytes_sec),
            format_rate(rate.total_bytes_sec())
        )
    } else {
        format!(
            "RX {} · TX {} · Total {} · health {healthy} ok / {unhealthy} bad / {checking} checking · {zfs} · {exceptions}",
            format_rate(rate.rx_bytes_sec),
            format_rate(rate.tx_bytes_sec),
            format_rate(rate.total_bytes_sec())
        )
    };
    let history_area = if rows[1].height > 0 {
        frame.render_widget(
            Paragraph::new(rate_text).style(traffic_style(
                TrafficSeverity::for_bytes_per_sec(rate.total_bytes_sec()),
            )),
            rows[0],
        );
        rows[1]
    } else {
        let columns = Layout::horizontal([
            Constraint::Percentage(68),
            Constraint::Percentage(32),
        ])
        .split(rows[0]);
        frame.render_widget(
            Paragraph::new(rate_text).style(traffic_style(
                TrafficSeverity::for_bytes_per_sec(rate.total_bytes_sec()),
            )),
            columns[0],
        );
        columns[1]
    };
    if history_area.height > 0 {
        let data = app
            .observability
            .telemetry
            .rack_histories
            .get(&rack)
            .map(|history| {
                sparkline_data(
                    history
                        .points()
                        .iter()
                        .map(|point| point.rate.total_bytes_sec()),
                    history_area.width,
                )
            })
            .unwrap_or_default();
        if data.is_empty() {
            frame.render_widget(
                Paragraph::new("Rack traffic (this TUI session): collecting"),
                history_area,
            );
        } else {
            let label = "Rack traffic (this TUI session) ";
            let label_width = label.len().min(history_area.width.into()) as u16;
            frame.render_widget(Paragraph::new(label), history_area);
            frame.render_widget(
                Sparkline::default()
                    .data(&data)
                    .style(Style::default().fg(Color::Cyan)),
                Rect::new(
                    history_area.x.saturating_add(label_width),
                    history_area.y,
                    history_area.width.saturating_sub(label_width),
                    history_area.height,
                ),
            );
        }
    }
}

fn rss_summary(app: &App, sample: &LatestSample<RssObservation>) -> String {
    if app.deployment.observed == ObservedDeploymentState::Stopped {
        return "RSS Stopped".into();
    }
    let freshness = app
        .now
        .map(|now| {
            sample.freshness(
                now,
                Duration::from_secs(15),
                Duration::from_secs(60),
            )
        })
        .unwrap_or(Freshness::Unavailable);
    let state = match freshness {
        Freshness::Stale => "Stale",
        Freshness::Unavailable => "Unavailable",
        Freshness::Fresh => {
            match sample.good.as_ref().map(|good| &good.value) {
                Some(RssObservation::Initialized { .. }) => {
                    "Healthy (initialized)"
                }
                Some(RssObservation::Initializing { .. }) => {
                    "Checking (initializing)"
                }
                Some(RssObservation::StaleInitialized) => {
                    "Degraded (stale initialized)"
                }
                Some(RssObservation::Failed { .. }) => "Failed",
                Some(RssObservation::Unavailable) => "Unavailable",
                Some(RssObservation::UnknownResponse) | None => "Unknown",
            }
        }
    };
    let age = sample
        .good
        .as_ref()
        .and_then(|good| {
            app.now.map(|now| {
                now.saturating_duration_since(good.captured_at).as_secs()
            })
        })
        .map(|seconds| format!("{seconds}s ago"))
        .unwrap_or_else(|| "never".into());
    let error = sample
        .latest_error
        .as_ref()
        .map(|error| error.message.as_str())
        .unwrap_or("none");
    format!("RSS {state} · success {age} · error {error}")
}
