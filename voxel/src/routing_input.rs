// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One named-routing input for typed commissioning and direct RSS.

use anyhow::{Context, Result, bail};
use rack_init_config::{
    InitialRouterConfiguration, RouterConfigurationSwitch, RoutingConfig,
};
use serde::Deserialize;
use voxel_config::VoxelConfig;
use wicketd_commission_types_versions::latest::rack_setup::UserSpecifiedRoutingConfig;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingDocument {
    routing: UserSpecifiedRoutingConfig,
}

pub(crate) fn load(
    cfg: &VoxelConfig,
    rack: usize,
) -> Result<Option<UserSpecifiedRoutingConfig>> {
    let Some(file) = cfg.routing_files.get(&rack) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("read routing file {file} for rack {rack}"))?;
    let document: RoutingDocument =
        toml::from_str(&text).with_context(|| {
            format!("parse routing file {file} for rack {rack}")
        })?;
    Ok(Some(document.routing))
}

pub(crate) fn for_direct_rss(
    input: UserSpecifiedRoutingConfig,
) -> Result<RoutingConfig> {
    let mut router_configurations = Vec::new();
    for rc in input.router_configurations {
        let mut switches = Vec::new();
        for block in rc.switches {
            let mut bgp_peers = Vec::new();
            for peer in block.bgp_peers {
                if peer.auth_key_id.is_some() {
                    bail!(
                        "direct RSS cannot resolve a BGP key ID; use commissioning with the separate key upload"
                    );
                }
                bgp_peers.push(peer.with_auth_key(None));
            }
            switches.push(RouterConfigurationSwitch {
                switch: block.switch,
                bgp_config: block.bgp_config,
                bgp_peers,
                routes: block.routes,
                bfd_peers: block.bfd_peers,
            });
        }
        router_configurations.push(InitialRouterConfiguration {
            name: rc.name,
            description: rc.description,
            switches,
        });
    }
    Ok(RoutingConfig {
        announce_sets: input.announce_sets,
        router_configurations,
        control_plane_router_configurations: input
            .control_plane_router_configurations,
        recovery_silo_router_configurations: input
            .recovery_silo_router_configurations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_routing_reaches_both_init_paths_and_only_selected_rack() {
        let mut cfg = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 3\n[network]\nrouter_mode = \"static\"\n",
        )
        .unwrap();
        cfg.routing_files.insert(
            0,
            format!(
                "{}/tests/fixtures/662-routing.toml",
                env!("CARGO_MANIFEST_DIR")
            ),
        );

        let commission =
            crate::commission::rss_config(&cfg, 0).unwrap().routing.unwrap();
        let direct = crate::rss_request::request_from_config(&cfg, 0)
            .unwrap()
            .routing
            .unwrap();
        assert_eq!(commission.router_configurations.len(), 1);
        assert_eq!(direct.router_configurations.len(), 1);
        assert_eq!(
            commission.router_configurations[0].name,
            direct.router_configurations[0].name
        );
        assert_eq!(commission.router_configurations[0].switches.len(), 2);
        assert_eq!(direct.router_configurations[0].switches.len(), 2);
        for (commission_switch, direct_switch) in commission
            .router_configurations[0]
            .switches
            .iter()
            .zip(&direct.router_configurations[0].switches)
        {
            assert_eq!(commission_switch.switch, direct_switch.switch);
            assert_eq!(commission_switch.routes, direct_switch.routes);
            assert_eq!(
                commission_switch.bgp_peers.len(),
                direct_switch.bgp_peers.len()
            );
            assert_eq!(
                commission_switch.bgp_peers[0].name,
                direct_switch.bgp_peers[0].name
            );
            assert_eq!(
                commission_switch.bgp_peers[0].peer,
                direct_switch.bgp_peers[0].peer
            );
        }
        assert_eq!(
            commission.control_plane_router_configurations,
            direct.control_plane_router_configurations,
        );
        assert_eq!(
            commission.recovery_silo_router_configurations,
            direct.recovery_silo_router_configurations,
        );
        assert!(
            crate::commission::rss_config(&cfg, 1).unwrap().routing.is_none()
        );
        assert!(
            crate::rss_request::request_from_config(&cfg, 1)
                .unwrap()
                .routing
                .is_none()
        );
    }
}
