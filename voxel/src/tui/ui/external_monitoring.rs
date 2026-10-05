use crate::tui::{App, telemetry::RackId};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::widgets::{
    header_style, help_function_style, help_key_style, overlay_area,
    popup_backdrop, popup_block,
};

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

pub(crate) fn receiver_yaml(app: &App) -> String {
    yaml_for(
        app.external_monitoring_endpoints
            .iter()
            .map(|(rack, host)| (*rack, host.as_str())),
    )
}

pub fn draw(frame: &mut ratatui::Frame<'_>) {
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
    popup_backdrop(frame, area);
    let body = vec![
        Line::styled("Setting up External Monitoring", header_style(true)),
        Line::default(),
        Line::from(
            "To monitor your Voxel deployment, copy the OpenTelemetry receiver",
        ),
        Line::from("YAML and follow the integration guide:"),
        Line::from(GUIDE_URL),
        Line::default(),
        Line::from(vec![
            Span::styled("Copy receiver YAML ", help_function_style()),
            Span::styled("<s>", help_key_style()),
            Span::raw("  "),
            Span::styled("Copy guide URL ", help_function_style()),
            Span::styled("<a>", help_key_style()),
            Span::raw("  "),
            Span::styled("Close ", help_function_style()),
            Span::styled("<Esc>", help_key_style()),
        ]),
    ];
    let block = popup_block(" External monitoring ");
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
            slot: None,
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
        // One fragment covers every rack; it just grows with the rack count.
        assert!(matches!(
            app.update(Action::CopyExternalMonitoringYaml.into()).as_slice(),
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
