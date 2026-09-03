use crate::tui::{App, telemetry::RackId};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};

use super::{colors::*, widgets::overlay_area};

pub const GUIDE_URL: &str =
    "https://docs.oxide.computer/guides/integrations/opentelemetry";

const PATTERNS: &[&str] = &[
    "sled_data_link:.*",
    "zone:cpu_nsec",
    "zfs_pool:.*",
    "oximeter_collector:.*",
    ".*http.*latency.*",
];

fn yaml_for<'a>(racks: impl IntoIterator<Item = (RackId, &'a str)>) -> String {
    let mut yaml = String::from(
        "# Receiver-only fragment. Add these receiver IDs to\n\
         # service.pipelines.metrics.receivers and choose your own processors/exporters.\n\
         # Create one least-privilege fleet-viewer token per rack.\n\
         receivers:\n",
    );
    for (rack, host) in racks {
        yaml.push_str(&format!(
            "  oxide/rack{}:\n    host: {:?}\n    token: ${{env:OXIDE_TOKEN_RACK{}}}\n    # Voxel virtual racks only; do not copy this TLS setting to production racks.\n    insecure_skip_verify: true\n    metric_patterns:\n",
            rack.0, host, rack.0
        ));
        for pattern in PATTERNS {
            yaml.push_str(&format!("      - {pattern:?}\n"));
        }
    }
    yaml
}

pub(crate) fn selected_yaml(app: &App) -> String {
    let selected = app
        .session
        .selected_rack
        .or_else(|| app.external_monitoring_endpoints.keys().next().copied());
    yaml_for(selected.into_iter().filter_map(|rack| {
        app.external_monitoring_endpoints
            .get(&rack)
            .map(|host| (rack, host.as_str()))
    }))
}

pub(crate) fn all_yaml(app: &App) -> String {
    yaml_for(
        app.external_monitoring_endpoints
            .iter()
            .map(|(rack, host)| (*rack, host.as_str())),
    )
}

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let root = overlay_area(frame.area());
    let width = root.width.min(92);
    let height = root.height.min(26);
    let area = Rect::new(
        root.x + root.width.saturating_sub(width) / 2,
        root.y + root.height.saturating_sub(height) / 2,
        width,
        height,
    );
    if area.width <= 2 || area.height <= 2 {
        return;
    }
    frame.render_widget(Clear, area);
    let selected = app.session.selected_rack.map_or_else(
        || "first configured rack".into(),
        |rack| format!("rack {}", rack.0),
    );
    let body = vec![
        Line::styled(
            "The Voxel TUI is a deployment assistant, not a monitoring platform",
            Style::default().fg(TUI_YELLOW).add_modifier(Modifier::BOLD),
        ),
        Line::default(),
        Line::from(
            "Use the official Oxide receiver to ingest Oximeter metrics,",
        ),
        Line::from("then connect your own observability stack."),
        Line::default(),
        Line::from(
            "1. Create a least-privilege fleet-viewer token for each virtual rack.",
        ),
        Line::from(
            "2. Copy the receiver fragment and set OXIDE_TOKEN_RACKN in the Collector environment.",
        ),
        Line::from(
            "3. Add oxide/rack<N> to your list of receivers, and choose your exporters.",
        ),
        Line::default(),
        Line::from(vec![
            Span::styled("s", Style::default().fg(TUI_YELLOW)),
            Span::raw(format!(" copy {selected} YAML   ")),
            Span::styled("a", Style::default().fg(TUI_YELLOW)),
            Span::raw(" copy all-rack YAML   "),
            Span::styled("u", Style::default().fg(TUI_YELLOW)),
            Span::raw(" copy official guide URL"),
        ]),
        Line::from("Esc close this window"),
        Line::default(),
        Line::from("Documentation:"),
        Line::from(format!("1. {GUIDE_URL}")),
        Line::from(
            "2. https://docs.oxide.computer/guides/metrics/oxql-tutorial",
        ),
        Line::from(
            "3. https://docs.oxide.computer/guides/metrics/timeseries-schemas",
        ),
        Line::default(),
        Line::styled(
            "NOTE: insecure_skip_verify is included only because Voxel uses self-signed virtual-rack certificates.",
            Style::default().fg(OX_RED),
        ),
    ];
    let block = Block::bordered()
        .title(" External monitoring ")
        .border_style(Style::default().fg(TUI_PURPLE))
        .style(Style::default().bg(OX_GREEN_DARKEST));
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(block),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        Action, Effect,
        event::View,
        telemetry::{ResourceDescriptor, ResourceId, ResourceKind},
    };
    use std::collections::BTreeMap;

    #[test]
    fn generated_yaml_is_deterministic_and_contains_no_session_secret() {
        let yaml = yaml_for([
            (RackId(0), "https://198.51.100.22"),
            (RackId(1), "https://198.51.100.86"),
        ]);
        assert!(yaml.contains("oxide/rack0:"));
        assert!(yaml.contains("${env:OXIDE_TOKEN_RACK1}"));
        assert!(yaml.contains("sled_data_link:.*"));
        assert!(yaml.contains("Voxel virtual racks only"));
        assert!(!yaml.contains("password"));
        assert!(!yaml.contains("oxide\n"));
    }

    #[test]
    fn overlay_actions_copy_selected_all_and_guide_then_close() {
        let descriptor = ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "g0"),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name: "g0".into(),
            host: None,
        };
        let mut app = App::new(vec![descriptor], 4, 4);
        app.session.view = View::Monitor;
        app.external_monitoring_endpoints = BTreeMap::from([
            (RackId(0), "https://198.51.100.22/".into()),
            (RackId(1), "https://198.51.100.86/".into()),
        ]);

        app.update(Action::RequestCancelAndDestroy.into());
        assert!(app.session.external_monitoring_open);
        app.update(Action::RequestCancelAndDestroy.into());
        assert!(app.session.external_monitoring_open);
        assert!(matches!(
            app.update(Action::CopyExternalMonitoringSelected.into()).as_slice(),
            [Effect::CopyToClipboard(yaml)] if yaml.contains("oxide/rack0:") && !yaml.contains("oxide/rack1:")
        ));
        assert!(matches!(
            app.update(Action::CopyExternalMonitoringAll.into()).as_slice(),
            [Effect::CopyToClipboard(yaml)] if yaml.contains("oxide/rack0:") && yaml.contains("oxide/rack1:")
        ));
        assert_eq!(
            app.update(Action::CopyExternalMonitoringGuide.into()),
            vec![Effect::CopyToClipboard(GUIDE_URL.into())]
        );
        app.update(Action::Close.into());
        assert!(!app.session.external_monitoring_open);
    }
}
