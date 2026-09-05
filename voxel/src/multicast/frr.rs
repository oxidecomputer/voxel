// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FRR-specific pieces for multicast plumbing. The `up` command sends rendered
//! [vtysh] configuration to a transit router, while the `check` command reads
//! back FRR interface and mroute state.
//!
//! See FRR's [vtysh] and [PIM] documentation for the command grammar.
//!
//! [vtysh]: https://docs.frrouting.org/en/stable-10.3/vtysh.html
//! [PIM]: https://docs.frrouting.org/en/stable-10.3/pim.html

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use anyhow::{Context, bail, ensure};
use itertools::Itertools;
use voxel_config::VoxelConfig;

use crate::util::shell_quote;

/// The forwarding path's shape, derived from configuration.
pub(super) struct Transit {
    /// The router receiving forwarding setup.
    pub(super) router: String,
    /// The host-facing NIC the groups arrive on, i.e., PIM's incoming interface.
    pub(super) iif: String,
    /// The scrimlet-facing NICs each group is assigned to.
    pub(super) ifaces: Vec<String>,
}

/// One `interface <iif>` block carrying an `ip mroute` (or `no ip mroute`)
/// command per outgoing interface.
///
/// This is empty when nothing is selected.
fn mroute_block(
    iif: &str,
    oifs: Vec<String>,
    group: &Ipv4Addr,
    source: &Ipv4Addr,
    withdraw: bool,
) -> Vec<String> {
    let no = if withdraw { "no " } else { "" };
    let cmds: Vec<String> = oifs
        .into_iter()
        .map(|oif| format!("{no}ip mroute {oif} {group} {source}"))
        .collect();
    if cmds.is_empty() {
        return cmds;
    }

    std::iter::once(format!("interface {iif}"))
        .chain(cmds)
        .chain(std::iter::once("exit".to_string()))
        .collect()
}

impl Transit {
    /// The router and the interfaces that FRR configuration names.
    ///
    /// This is derived from configuration. The router's address is resolved
    /// separately (via `node_addr`), which is the only step that needs a
    /// running rack.
    pub(super) fn new(cfg: &VoxelConfig, router: &str) -> anyhow::Result<Self> {
        let router = router.to_owned();
        let fabric = cfg.multicast_routers();
        ensure!(
            fabric.iter().any(|r| r == &router),
            "'{router}' is not a fabric router in this topology ({})",
            fabric.join(", ")
        );
        let iif = cfg.router_ext_iface(&router);
        let ifaces = cfg.router_scrimlet_ifaces(&router);
        if ifaces.is_empty() {
            bail!("{router} has no multicast forwarding NIC");
        }
        Ok(Self { router, iif, ifaces })
    }

    /// Static PIM VIFs on the rack-facing and host-facing links.
    pub(super) fn vif_cmds(&self) -> Vec<String> {
        self.ifaces
            .iter()
            .chain(std::iter::once(&self.iif))
            .flat_map(|dev| {
                [
                    format!("interface {dev}"),
                    String::from("ip pim passive"),
                    String::from("exit"),
                ]
            })
            .collect()
    }

    /// The rack-facing links a selection names.
    ///
    /// `None` means every link.
    pub(super) fn selected(&self, links: Option<&[usize]>) -> Vec<String> {
        match links {
            None => self.ifaces.clone(),
            Some(sel) => sel
                .iter()
                .filter_map(|idx| self.ifaces.get(*idx).cloned())
                .collect(),
        }
    }

    /// The static mroute for `group` on each selected link.
    pub(super) fn mroute_cmds(
        &self,
        group: &Ipv4Addr,
        source: &Ipv4Addr,
        links: Option<&[usize]>,
    ) -> Vec<String> {
        mroute_block(&self.iif, self.selected(links), group, source, false)
    }

    /// Withdraw the static mroute for `group` on each selected link.
    pub(super) fn no_mroute_cmds(
        &self,
        group: &Ipv4Addr,
        source: &Ipv4Addr,
        links: Option<&[usize]>,
    ) -> Vec<String> {
        mroute_block(&self.iif, self.selected(links), group, source, true)
    }
}

/// A `vtysh` invocation, one `-c` per line, quoted so FRR sees each line whole.
fn vtysh(lines: &[String]) -> String {
    std::iter::once("vtysh".to_string())
        .chain(lines.iter().map(|l| format!("-c {}", shell_quote(l))))
        .join(" ")
}

/// A `vtysh` read of one show command.
pub(super) fn vtysh_show(cmd: &str) -> String {
    vtysh(&[cmd.to_string()])
}

/// A parsed `show ip mroute` result for one `(S,G)` entry.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Mroute {
    pub(super) incoming: Option<String>,
    pub(super) outgoing: Vec<String>,
    pub(super) installed: bool,
}

impl Mroute {
    /// Match an entry against its expected forwarding interfaces.
    ///
    /// A missing entry matches an empty outgoing set.
    pub(super) fn matches(
        route: Option<&Self>,
        iif: &str,
        expected: &[String],
    ) -> bool {
        match expected {
            [] => route.is_none_or(|route| {
                !route.installed || route.outgoing.is_empty()
            }),
            _ => route.is_some_and(|route| {
                route.installed
                    && route.incoming.as_deref() == Some(iif)
                    && route.outgoing == expected
            }),
        }
    }

    /// A parsed `show ip mroute` result for a `(S,G)` forwarding entry.
    ///
    /// `None` when the table holds no entry for that pair.
    ///
    /// # Errors
    ///
    /// This fails when FRR's JSON output cannot be parsed.
    pub(super) fn parse(
        json: &str,
        group: &Ipv4Addr,
        source: &Ipv4Addr,
    ) -> anyhow::Result<Option<Self>> {
        let routes: BTreeMap<String, BTreeMap<String, MrouteRecord>> =
            serde_json::from_str(json)
                .context("parse `show ip mroute json` output")?;
        let Some(route) = routes
            .get(&group.to_string())
            .and_then(|group| group.get(&source.to_string()))
        else {
            return Ok(None);
        };
        Ok(Some(route.into()))
    }
}

impl From<&MrouteRecord> for Mroute {
    fn from(route: &MrouteRecord) -> Self {
        Self {
            incoming: route.iif.clone(),
            outgoing: route.oil.keys().cloned().collect(),
            installed: route.installed > 0
                || route.oil.values().any(|entry| entry.protocol_static),
        }
    }
}

/// A raw record from `show ip mroute json` referring to one `(S,G)`
/// entry under a group.
#[derive(serde::Deserialize)]
struct MrouteRecord {
    #[serde(default)]
    iif: Option<String>,
    #[serde(default)]
    oil: BTreeMap<String, OilEntry>,
    #[serde(default)]
    installed: u64,
}

/// A single outgoing-interface entry from `show ip mroute json`.
///
/// Static mroutes carry no top-level `installed`; `protocolStatic` on the oil
/// entry is what marks them as programmed in.
#[derive(serde::Deserialize, Default)]
struct OilEntry {
    #[serde(default, rename = "protocolStatic")]
    protocol_static: bool,
}

/// Scan for errors in the vtysh output.
pub(super) fn config_errors(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('%') || line.contains("Failed"))
        .map(str::to_string)
        .collect()
}

/// A `vtysh` configuration session wrapping `lines`.
pub(super) fn vtysh_config(lines: &[String]) -> String {
    let session: Vec<String> =
        std::iter::once("configure terminal".to_string())
            .chain(lines.iter().cloned())
            .collect();
    vtysh(&session)
}

/// The interfaces PIM holds a VIF on.
///
/// FRR [keys on the interface name] with no wrapper around it, making the
/// top-level keys the interface list.
///
/// [keys on the interface name]: https://github.com/FRRouting/frr/blob/85cf1ed576deed121751e16a64970f8a652a9e1e/pimd/pim_cmd_common.c#L2265-L2317
pub(super) fn parse_pim_ifaces(json: &str) -> anyhow::Result<Vec<String>> {
    let map: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(json)
            .context("parse `show ip pim interface json` output")?;
    Ok(map.into_keys().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transit() -> Transit {
        Transit {
            router: "cr1".to_string(),
            iif: "enp0s11".to_string(),
            ifaces: vec!["enp0s9".to_string(), "enp0s10".to_string()],
        }
    }

    #[test]
    fn live_mroute_requires_kernel_installation() {
        let expected = vec!["enp0s9".to_string()];
        let uninstalled = Mroute {
            incoming: Some("enp0s11".to_string()),
            outgoing: expected.clone(),
            installed: false,
        };
        assert!(!Mroute::matches(Some(&uninstalled), "enp0s11", &expected));

        let installed = Mroute { installed: true, ..uninstalled };
        assert!(Mroute::matches(Some(&installed), "enp0s11", &expected));
        assert!(!Mroute::matches(Some(&installed), "wrong", &expected));
        assert!(Mroute::matches(Some(&Mroute::default()), "enp0s11", &[],));
        assert!(!Mroute::matches(Some(&installed), "enp0s11", &[],));
        assert!(Mroute::matches(None, "enp0s11", &[],));

        let withdrawn = Mroute { outgoing: Vec::new(), ..installed };
        assert!(Mroute::matches(Some(&withdrawn), "enp0s11", &[]));
        assert!(!Mroute::matches(Some(&withdrawn), "enp0s11", &expected));
    }

    #[test]
    fn fabric_transit_uses_external_iif() {
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 4").unwrap();
        let transit = Transit::new(&cfg, "cr1").unwrap();
        assert_eq!(transit.iif, "enp0s11");
    }

    #[test]
    fn route_cmds_follow_the_selection() {
        let t = transit();
        let group = Ipv4Addr::new(239, 1, 1, 1);
        let source = Ipv4Addr::new(192, 168, 1, 199);
        assert_eq!(
            t.mroute_cmds(&group, &source, Some(&[1])),
            vec![
                "interface enp0s11",
                "ip mroute enp0s10 239.1.1.1 192.168.1.199",
                "exit",
            ]
        );
        assert_eq!(
            t.no_mroute_cmds(&group, &source, Some(&[0])),
            vec![
                "interface enp0s11",
                "no ip mroute enp0s9 239.1.1.1 192.168.1.199",
                "exit",
            ]
        );
        assert!(t.mroute_cmds(&group, &source, Some(&[])).is_empty());
    }

    #[test]
    fn vif_cmds_cover_rack_links_and_the_iif() {
        // All links are passive because the rack side does not send PIM
        // hello messages. The VIFs exist only for FRR to hold
        // static mroutes.

        assert_eq!(
            transit().vif_cmds(),
            vec![
                "interface enp0s9",
                "ip pim passive",
                "exit",
                "interface enp0s10",
                "ip pim passive",
                "exit",
                "interface enp0s11",
                "ip pim passive",
                "exit",
            ]
        );
    }

    #[test]
    fn vtysh_quotes_each_line_whole() {
        assert_eq!(
            vtysh_config(&["interface enp0s9".to_string()]),
            "vtysh -c 'configure terminal' -c 'interface enp0s9'"
        );
    }

    #[test]
    fn pim_ifaces_are_the_top_level_keys() {
        let json = r#"{
          "enp0s9":{"name":"enp0s9","state":"up","address":"192.0.2.1",
            "index":2,"flagMulticast":true,"pimNeighbors":0,
            "pimIfChannels":0,"firstHopRouterCount":0,
            "pimDesignatedRouter":"192.0.2.1","pimDesignatedRouterLocal":true},
          "pimreg":{"name":"pimreg","state":"up","address":"0.0.0.0",
            "index":5,"flagMulticast":true,"pimNeighbors":0,
            "pimIfChannels":0,"firstHopRouterCount":0,
            "pimDesignatedRouter":"0.0.0.0"}}"#;
        let mut ifaces = parse_pim_ifaces(json).unwrap();
        ifaces.sort();
        assert_eq!(ifaces, vec!["enp0s9", "pimreg"]);
    }

    #[test]
    fn mroute_parser_selects_the_requested_entry() {
        let output = r#"{
          "239.1.1.2": {
            "192.168.1.199": {
              "iif": "wrong",
              "oil": {"wrong": {}},
              "installed": 0
            }
          },
          "239.1.1.1": {
            "192.168.1.199": {
              "iif": "enp0s11",
              "oil": {"enp0s9": {}, "enp0s10": {}},
              "installed": 1
            }
          },
          "239.1.1.3": {
            "192.168.1.199": {
              "iif": "enp0s11",
              "oil": {"enp0s9": {"protocolStatic": true}},
              "installed": 0
            }
          }
        }"#;
        let group = Ipv4Addr::new(239, 1, 1, 1);
        let source = Ipv4Addr::new(192, 168, 1, 199);

        assert_eq!(
            Mroute::parse(output, &group, &source).unwrap(),
            Some(Mroute {
                incoming: Some("enp0s11".to_string()),
                outgoing: vec!["enp0s10".to_string(), "enp0s9".to_string()],
                installed: true,
            })
        );
        assert!(
            !Mroute::parse(output, &Ipv4Addr::new(239, 1, 1, 2), &source)
                .unwrap()
                .unwrap()
                .installed
        );
        assert_eq!(
            Mroute::parse(output, &group, &Ipv4Addr::new(192, 168, 1, 200))
                .unwrap(),
            None
        );
        assert_eq!(
            Mroute::parse(output, &Ipv4Addr::new(239, 1, 1, 3), &source)
                .unwrap(),
            Some(Mroute {
                incoming: Some("enp0s11".to_string()),
                outgoing: vec!["enp0s9".to_string()],
                installed: true,
            })
        );
        assert!(Mroute::parse("{", &group, &source).is_err());
    }
}
