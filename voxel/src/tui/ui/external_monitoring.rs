// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The external monitoring overlay: points at the OpenTelemetry guide and
//! renders the per-rack receiver YAML an operator can copy into a collector.

use crate::tui::{App, telemetry::RackId};
use ratatui::text::{Line, Text};

use super::{
    popup::{Button, Popup},
    widgets::overlay_area,
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
    let button = |label, key| Button { label, key: Some(key), selected: false };
    Popup {
        header: Line::from("Setting up External Monitoring"),
        body: Text::from(vec![
            Line::from(
                "To monitor your Voxel deployment, copy the OpenTelemetry receiver",
            ),
            Line::from("YAML and follow the integration guide:"),
            Line::from(GUIDE_URL),
        ]),
        buttons: vec![
            button("Copy receiver YAML", "s"),
            button("Copy guide URL", "a"),
            button("Close", "Esc"),
        ],
    }
    .render(frame, overlay_area(frame.area()), 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The YAML an operator copies names a receiver per rack and takes each
    /// rack's token from the environment, never embedding a credential.
    #[test]
    fn receiver_yaml_covers_every_rack_without_embedding_credentials() {
        let mut app = App::new(vec![], 8, 8);
        app.external_monitoring_endpoints = BTreeMap::from([
            (RackId(0), "https://recovery.sys.oxide.test/".into()),
            (RackId(1), "https://recovery.sys.rack1.oxide.test/".into()),
        ]);

        let yaml = receiver_yaml(&app);

        let tokens: Vec<_> = yaml
            .lines()
            .filter(|line| line.contains("token:"))
            .map(str::trim)
            .collect();
        assert_eq!(
            tokens,
            [
                "token: ${env:OXIDE_TOKEN_RACK0}",
                "token: ${env:OXIDE_TOKEN_RACK1}"
            ]
        );
        for wanted in [
            "oxide/rack0:",
            "\"https://recovery.sys.oxide.test/\"",
            "oxide/rack1:",
            "\"https://recovery.sys.rack1.oxide.test/\"",
        ] {
            assert!(yaml.contains(wanted), "missing {wanted}: {yaml}");
        }
    }
}
