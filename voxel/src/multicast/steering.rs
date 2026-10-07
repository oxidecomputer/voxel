// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Multicast group parsing and delivery-path selection for steering
//! traffic toward the voxel rack's emulated customer network.
//!
//! A [`PathSelection`] is a switch slot with an optional router. An unqualified
//! slot would apply to every forwarding router.
//!
//! The `--steer` flag, which this module supplies, accepts switch names, LLDP
//! labels, and router interface names. [`Steering::parse`] resolves these
//! against the voxel rack topology before any FRR configuration is applied.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{Context, bail, ensure};
use itertools::Itertools;
use voxel_config::{MulticastDelivery, VoxelConfig};

use super::record::{MulticastState, PathSelection};

/// A group as passed to commtest.
///
/// This consists of just the address, or `GROUP@SRC,...` for a
/// source-filtered join. The suffix is a property of the join, and a caller
/// may name a source that never appears anywhere on the wire
/// (e.g., commtest's deny group example).
///
/// Processing-wise, this input is stripped here and the sender is
/// resolved separately.
///
/// TODO: IPv4 only, matching commtest's `validate_mcast`. Once that accepts v6
/// groups, this file needs v6 throughout, which for the router side means the
/// MLD spelling of a static group too.
fn group_addr(group: &str) -> anyhow::Result<Ipv4Addr> {
    let addr = group.split_once('@').map_or(group, |(addr, _)| addr);
    let addr: Ipv4Addr = addr
        .parse()
        .with_context(|| format!("multicast group '{group}' must be IPv4"))?;
    if !addr.is_multicast() {
        bail!("'{addr}' is not a multicast address (224.0.0.0/4)");
    }
    if addr.octets()[..3] == [224, 0, 0] {
        bail!(
            "'{addr}' is in the local network control block (224.0.0.0/24), \
             which routers never forward"
        );
    }
    Ok(addr)
}

/// A multicast group as commtest lays out, reduced to just a IPv4 address.
#[derive(Clone, Debug)]
pub(crate) struct GroupSpec {
    pub(super) addr: Ipv4Addr,
}

impl std::str::FromStr for GroupSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let addr = group_addr(s)?;
        if let Some((_, sources)) = s.split_once('@') {
            for source in sources.split(',') {
                let source = source.trim();
                source.parse::<Ipv4Addr>().with_context(|| {
                    format!("source '{source}' in '{s}' must be IPv4")
                })?;
            }
        }
        Ok(Self { addr })
    }
}

impl std::fmt::Display for GroupSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

/// The spec determining which ingress paths a group uses, allowing us to
/// drive duplicate copies into the rack (on purpose) or pin a group to one
/// (router, switch) pairing path.
///
/// LLDP spellings (`uplink0`) are accepted too, resolving through [`Aliases`]
/// and reading back as `switchN` in the ownership record.
#[derive(Clone, Debug)]
pub(crate) struct SteerSpec {
    group: Ipv4Addr,
    selection: String,
}

const ALL_SELECTION: &str = "all";
const NONE_SELECTION: &str = "none";

impl std::str::FromStr for SteerSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (group, selection) = s.split_once('=').with_context(|| {
            format!(
                "'{s}' must be GROUP={ALL_SELECTION}|{NONE_SELECTION}\
                 |switch0|uplink0|cr1:switch0"
            )
        })?;
        Ok(Self { group: group_addr(group)?, selection: selection.to_string() })
    }
}

/// A group's requested delivery paths.
///
/// Each path pairs an optional router with the switch its rack-facing link
/// reaches:
///
/// - A path naming no router whatsoever applies to every configured router.
/// - Naming one restricts the group to it.
///
/// Empty here means deliver to no one at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Paths(pub(super) Vec<PathSelection>);

impl Paths {
    /// The paths `[network.multicast] delivery` asks for.
    pub(super) fn from_config(
        cfg: &VoxelConfig,
        routers: &[String],
    ) -> Option<Self> {
        match cfg.network.multicast.delivery {
            MulticastDelivery::All => Some(Self(
                (0..cfg.scrimlet_count())
                    .map(|switch| PathSelection { router: None, switch })
                    .collect(),
            )),
            MulticastDelivery::Single => routers.first().map(|r| {
                Self(vec![PathSelection { router: Some(r.clone()), switch: 0 }])
            }),
        }
    }

    /// Reject selections with more than one outgoing path per router.
    ///
    /// TODO: lift this once the frr image carries FRR's leaf-list `oif`
    /// (outgoing interface) model, which is on their mainline branch only.
    /// FRR 8.4 through 10.x withdraw by just `(S,G)` and can strand the other
    /// outgoing interface.
    ///
    /// # Errors
    ///
    /// This fails when a router is assigned several switch slots.
    pub(super) fn ensure_single_path_per_router(
        &self,
        addr: &Ipv4Addr,
        routers: &[String],
    ) -> anyhow::Result<()> {
        for router in routers {
            ensure!(
                self.switches_for(router).len() <= 1,
                "{addr} selects multiple outgoing paths on {router}; \
                 FRR cannot reliably withdraw them. Select at most one \
                 switch per router"
            );
        }
        Ok(())
    }

    /// The switches `router` is asked to deliver to. A group whose paths all
    /// name other routers gives this one nothing.
    pub(super) fn switches_for(&self, router: &str) -> Vec<usize> {
        Self::switches_in(&self.0, router)
    }

    /// The switch slots that `paths` select on a given `router`.
    pub(super) fn switches_in(
        paths: &[PathSelection],
        router: &str,
    ) -> Vec<usize> {
        paths
            .iter()
            .filter(|path| path.router.as_deref().is_none_or(|n| n == router))
            .map(|path| path.switch)
            .unique()
            .collect()
    }

    /// Expand each unqualified path into one qualified path per forwarding
    /// router.
    ///
    /// The result of this is what the ownership record persists, pinning paths
    /// to the routers in order to keep a recorded selection fixed if/when the
    /// forwarding set changes between commands.
    pub(super) fn materialize(&mut self, routers: &[String]) {
        self.0 = self
            .0
            .iter()
            .flat_map(|path| match &path.router {
                Some(_) => vec![path.clone()],
                None => routers
                    .iter()
                    .map(|router| PathSelection {
                        router: Some(router.clone()),
                        switch: path.switch,
                    })
                    .collect(),
            })
            .collect();
    }
}

/// The set of multicast group addresses mapped to its steering `Paths`.
#[derive(Debug, Default)]
pub(super) struct Steering {
    paths: HashMap<Ipv4Addr, Paths>,
}

impl Steering {
    /// Parse `--steer GROUP=SPEC`.
    ///
    /// SPEC is `all`, `none`, or a comma-separated list of paths. A path is a
    /// rack-facing name on its own, which selects that switch on every
    /// forwarding router, or a `<router>:<name>` pair, which selects one
    /// specific router's delivery path.
    ///
    /// The rack-facing name is either a `switch<N>`, an LLDP base label such as
    /// `uplink0`, or an emitted label such as `uplink0-cr1` (already a complete
    /// path).
    ///
    /// For example, `239.100.0.1=cr1:uplink0` becomes the path for switch 0 on
    /// `cr1`, while `239.100.0.1=switch0` becomes an unqualified path on every
    /// forwarding router.
    ///
    /// # Errors
    ///
    /// This fails when a group appears more than once, or when a rack's uplinks
    /// share an LLDP description. Unknown names, routers that do not forward
    /// multicast, and emitted labels whose router conflicts with the
    /// `<router>:` prefix are rejected as well.
    pub(super) fn parse(
        specs: &[SteerSpec],
        cfg: &VoxelConfig,
    ) -> anyhow::Result<Self> {
        if specs.is_empty() {
            return Ok(Self::default());
        }
        let aliases = Aliases::try_from(cfg)?;
        let mut out = Self::default();
        for spec in specs {
            let addr = spec.group;
            let paths: Vec<PathSelection> = match spec.selection.as_str() {
                ALL_SELECTION => (0..aliases.switches)
                    .map(|s| PathSelection { router: None, switch: s })
                    .collect(),
                NONE_SELECTION => Vec::new(),
                list => list
                    .split(',')
                    .map(|one| aliases.parse_path(one.trim()))
                    .process_results(|paths| {
                        paths.flatten().unique().collect()
                    })?,
            };

            ensure!(
                out.paths.insert(addr, Paths(paths)).is_none(),
                "--steer names {addr} more than once"
            );
        }
        Ok(out)
    }

    /// The groups with a selected delivery path set.
    pub(super) fn groups(&self) -> impl Iterator<Item = &Ipv4Addr> + '_ {
        self.paths.keys()
    }

    /// The delivery recorded for each of `addrs`, as switch indices.
    ///
    /// A group without a record contributes nothing, falling back to the
    /// configured delivery.
    pub(super) fn from_recorded(
        state: Option<&MulticastState>,
        addrs: &[Ipv4Addr],
    ) -> Self {
        let Some(state) = state else { return Self::default() };
        Self {
            paths: addrs
                .iter()
                .filter_map(|addr| {
                    state
                        .selected_paths(addr)
                        .map(|items| (*addr, Paths(items.to_vec())))
                })
                .collect(),
        }
    }

    /// Reuse recorded paths for groups without an explicit selection.
    pub(super) fn reuse_recorded(
        &mut self,
        state: Option<&MulticastState>,
        addrs: &[Ipv4Addr],
    ) {
        for (addr, paths) in Self::from_recorded(state, addrs).paths {
            self.paths.entry(addr).or_insert(paths);
        }
    }

    /// Fill missing selections from configured delivery and qualify the paths.
    pub(super) fn finalize(
        &mut self,
        cfg: &VoxelConfig,
        addrs: &[Ipv4Addr],
        routers: &[String],
    ) {
        if let Some(default) = Paths::from_config(cfg, routers) {
            for addr in addrs {
                self.paths.entry(*addr).or_insert_with(|| default.clone());
            }
        }
        self.paths.values_mut().for_each(|paths| paths.materialize(routers));
    }

    /// The switch slots a group selects on a router.
    pub(super) fn selected_links(
        &self,
        addr: &Ipv4Addr,
        router: &str,
    ) -> Option<Vec<usize>> {
        self.paths.get(addr).map(|paths| paths.switches_for(router))
    }

    /// The complete paths selected for a group.
    pub(super) fn selected_paths(
        &self,
        addr: &Ipv4Addr,
    ) -> Option<&[PathSelection]> {
        self.paths.get(addr).map(|paths| paths.0.as_slice())
    }

    /// The groups without selected delivery paths into the rack.
    pub(super) fn unreached_groups(
        &self,
        cfg: &VoxelConfig,
        addrs: &[Ipv4Addr],
        routers: &[String],
        rack: usize,
    ) -> Vec<Ipv4Addr> {
        let rack_switches: Vec<usize> = cfg
            .sleds()
            .into_iter()
            .filter(|s| s.scrimlet)
            .enumerate()
            .filter_map(|(idx, s)| (s.rack == rack).then_some(idx))
            .collect();
        addrs
            .iter()
            .copied()
            .filter(|addr| match self.paths.get(addr) {
                None => rack_switches.is_empty(),
                Some(paths) => !routers.iter().any(|router| {
                    paths
                        .switches_for(router)
                        .iter()
                        .any(|switch| rack_switches.contains(switch))
                }),
            })
            .collect()
    }

    /// Reject groups with more than one outgoing path per router.
    ///
    /// # Errors
    ///
    /// This fails when a group's selection assigns several slots to a router.
    pub(super) fn ensure_single_path_per_router(
        &self,
        routers: &[String],
    ) -> anyhow::Result<()> {
        for (addr, paths) in &self.paths {
            paths.ensure_single_path_per_router(addr, routers)?;
        }
        Ok(())
    }
}

/// A single uplink port for alias resolution.
struct AliasPort {
    rack: usize,
    slot: usize,
    router: String,
    lldp: String,
}

/// A label to the entries it names.
type AliasTable<T> = Vec<(String, Vec<T>)>;

/// The rack-facing paths a caller can name/assign.
///
/// Resolved once. A spelling names one or more (router, switch) pairs
/// (an unqualified switch name selects that switch on every router).
///
/// A path has three possible spellings:
///
/// - `switch<N>`: the operator's, and the one `check` prints.
/// - LLDP labels: the customer's, since they are what the rack advertises to
///   the device on the other end of the cable.
/// - Interface names: the router's, what the mroutes and `show ip mroute`
///   name on the FRR side.
struct Aliases {
    /// Fabric routers in topology order, which is the order `uplink_ports`
    /// indexes them by.
    routers: Vec<String>,
    /// LLDP base label (`uplink0`) to switch slot (the label as written
    /// in config, not emitted).
    base: AliasTable<(usize, usize)>,
    /// Emitted LLDP label (`uplink0-cr1`) to the full path it names.
    emitted: AliasTable<(String, usize)>,
    interfaces: AliasTable<(String, usize)>,
    switches: usize,
}

impl TryFrom<&VoxelConfig> for Aliases {
    type Error = anyhow::Error;

    fn try_from(cfg: &VoxelConfig) -> Result<Self, Self::Error> {
        let routers = cfg.multicast_routers();
        let ports = alias_ports(cfg);
        let switches = cfg.scrimlet_count();
        let emitted = emitted_aliases(&ports, switches);
        let base = base_aliases(&ports, switches)?;
        let interfaces =
            routers.iter().fold(Vec::new(), |mut interfaces, router| {
                cfg.router_scrimlet_ifaces(router)
                    .into_iter()
                    .enumerate()
                    .map(|(switch, interface)| (interface, switch))
                    .for_each(|(interface, switch)| {
                        bucket(&mut interfaces, &interface)
                            .push((router.clone(), switch));
                    });
                interfaces
            });

        Ok(Self { routers, base, emitted, interfaces, switches })
    }
}

impl Aliases {
    /// One element of a steering selection.
    ///
    /// A selection names more than one path. For example, `switch0` selects
    /// slot 0 on every router, while `cr1:uplink0` selects slot 0 on `cr1`.
    ///
    /// # Errors
    ///
    /// See [`Steering::parse`].
    fn parse_path(&self, one: &str) -> anyhow::Result<Vec<PathSelection>> {
        let (router, name) = match one.split_once(':') {
            Some((r, n)) => {
                ensure!(
                    self.routers.iter().any(|known| known == r),
                    "'{r}' does not forward multicast in this topology ({})",
                    self.routers.join(", ")
                );
                (Some(r.to_string()), n)
            }
            None => (None, one),
        };

        // An emitted LLDP label already names a router.
        if let Some(paths) = self.emitted_path(name) {
            let selected: Vec<PathSelection> = paths
                .into_iter()
                .filter(|(labelled, _)| {
                    router.as_ref().is_none_or(|r| labelled == r)
                })
                .map(|(labelled, slot)| PathSelection {
                    router: Some(labelled),
                    switch: slot,
                })
                .collect();

            if let Some(r) = &router {
                ensure!(!selected.is_empty(), "'{name}' is not {r}'s uplink");
            }

            ensure!(
                selected.len() == 1,
                "'{one}' names more than one rack path; use <router>:switch<N>"
            );

            return Ok(selected);
        }

        if let Some(slots) = self.switch(name) {
            ensure!(
                slots.len() == 1,
                "'{one}' names switches in more than one rack; use switch<N> \
                 or <router>:switch<N>"
            );
            return Ok(slots
                .into_iter()
                .map(|slot| PathSelection {
                    router: router.clone(),
                    switch: slot,
                })
                .collect());
        }

        let selected = self
            .interface_paths(name)
            .into_iter()
            .filter(|(owner, _)| {
                router.as_ref().is_none_or(|r| owner.as_str() == r)
            })
            .map(|(owner, slot)| PathSelection {
                router: Some(owner),
                switch: slot,
            })
            .collect::<Vec<_>>();
        if !selected.is_empty() {
            return Ok(selected);
        }

        bail!("'{one}' names no switch, interface, or LLDP label")
    }

    /// The switch a rack-facing name selects, whichever spelling it is in.
    /// `None` when the name is not a switch at all.
    fn switch(&self, name: &str) -> Option<Vec<usize>> {
        if let Some(idx) =
            name.strip_prefix("switch").and_then(|n| n.parse().ok())
        {
            return (idx < self.switches).then_some(vec![idx]);
        }
        self.base
            .iter()
            .find(|(label, _)| label == name)
            .map(|(_, slots)| slots.iter().map(|(_, slot)| *slot).collect())
    }

    /// The full path an emitted LLDP label names.
    fn emitted_path(&self, name: &str) -> Option<Vec<(String, usize)>> {
        self.emitted
            .iter()
            .find(|(label, _)| label == name)
            .map(|(_, paths)| paths.clone())
    }

    /// The paths named by a router interface.
    fn interface_paths(&self, name: &str) -> Vec<(String, usize)> {
        self.interfaces
            .iter()
            .find(|(interface, _)| interface == name)
            .map(|(_, paths)| paths.clone())
            .unwrap_or_default()
    }
}

/// The entries record under a `label`, inserting an empty row on init.
fn bucket<'t, T>(table: &'t mut AliasTable<T>, label: &str) -> &'t mut Vec<T> {
    let at = match table.iter().position(|(known, _)| known == label) {
        Some(at) => at,
        None => {
            table.push((label.to_string(), Vec::new()));
            table.len() - 1
        }
    };
    &mut table[at].1
}

fn alias_ports(cfg: &VoxelConfig) -> Vec<AliasPort> {
    let racks = cfg.topology.racks();
    let scrimlets_per_rack = cfg
        .sleds()
        .into_iter()
        .filter(|sled| sled.scrimlet)
        .fold(vec![0; racks], |mut counts, sled| {
            counts[sled.rack] += 1;
            counts
        });
    let offsets: Vec<usize> = scrimlets_per_rack
        .iter()
        .copied()
        .scan(0, |offset, count| {
            let current = *offset;
            *offset += count;
            Some(current)
        })
        .collect();

    (0..racks)
        .flat_map(|rack| {
            let offset = offsets[rack];
            let switches = scrimlets_per_rack[rack];
            cfg.uplink_ports(rack)
                .into_iter()
                .filter(move |port| port.switch_slot < switches)
                .map(move |port| AliasPort {
                    rack,
                    slot: offset + port.switch_slot,
                    router: port.router,
                    lldp: port.lldp,
                })
        })
        .collect()
}

fn emitted_aliases(
    ports: &[AliasPort],
    switches: usize,
) -> AliasTable<(String, usize)> {
    ports
        .iter()
        // Same bound as `base_aliases`. `[network.uplinks]` can hold more
        // entries than the topology has switches, and a label resolving past
        // the last one would be recorded and then silently dropped by
        // `Transit::selected`.
        .filter(|port| port.slot < switches)
        .fold(Vec::new(), |mut emitted, port| {
            bucket(&mut emitted, &port.lldp)
                .push((port.router.clone(), port.slot));
            emitted
        })
}

fn base_aliases(
    ports: &[AliasPort],
    switches: usize,
) -> anyhow::Result<AliasTable<(usize, usize)>> {
    ports.iter().filter(|port| port.slot < switches).try_fold(
        Vec::new(),
        |mut base, port| {
            // The emitted label is `<description>-<router>`.
            let suffix = format!("-{}", port.router);
            let stem = port
                .lldp
                .strip_suffix(suffix.as_str())
                .unwrap_or(port.lldp.as_str());
            let slots = bucket(&mut base, stem);

            ensure!(
                !slots.iter().any(|(known_rack, known_slot)| {
                    *known_rack == port.rack && *known_slot != port.slot
                }),
                "uplinks in rack {} share the LLDP description '{stem}', \
                 so it names no single switch",
                port.rack
            );

            if !slots.iter().any(|(_, known_slot)| *known_slot == port.slot) {
                slots.push((port.rack, port.slot));
            }
            Ok(base)
        },
    )
}

/// The distinct addresses in `groups`. A repeated `--group` configures one
/// assignment.
pub(super) fn group_addrs(groups: &[GroupSpec]) -> Vec<Ipv4Addr> {
    groups.iter().map(|g| g.addr).unique().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(router: Option<&str>, switch: usize) -> PathSelection {
        PathSelection { router: router.map(str::to_string), switch }
    }

    #[test]
    fn group_addr_strips_source_and_rejects_unicast() {
        assert_eq!(
            group_addr("232.100.0.1@192.168.1.199").unwrap(),
            Ipv4Addr::new(232, 100, 0, 1)
        );
        assert_eq!(
            group_addr("239.1.1.1").unwrap(),
            Ipv4Addr::new(239, 1, 1, 1)
        );
        assert!(group_addr("198.51.100.1").is_err());
        assert!(group_addr("ff05::1").is_err());
        assert!(group_addr("224.0.0.5").is_err());
        assert!(group_addr("224.0.1.1").is_ok());
    }

    #[test]
    fn group_addrs_dedupes_repeats_in_first_occurrence_order() {
        let groups: Vec<GroupSpec> = [
            "239.1.1.2",
            "239.1.1.1@192.168.1.199",
            "239.1.1.2@10.0.0.1",
            "239.1.1.1",
        ]
        .iter()
        .map(|g| g.parse().unwrap())
        .collect();
        assert_eq!(
            group_addrs(&groups),
            vec![Ipv4Addr::new(239, 1, 1, 2), Ipv4Addr::new(239, 1, 1, 1)]
        );
        assert!("239.1.1.1@garbage".parse::<GroupSpec>().is_err());
    }

    #[test]
    fn steering_accepts_supported_names_and_rejects_invalid_ones() {
        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            sleds = 4
        "})
        .unwrap();
        let steer = |spec: &str| {
            Steering::parse(
                &[format!("239.1.1.1={spec}").parse().unwrap()],
                &cfg,
            )
            .map(|s| s.paths[&Ipv4Addr::new(239, 1, 1, 1)].clone())
        };

        assert_eq!(steer("switch0").unwrap().switches_for("cr1"), vec![0]);
        assert_eq!(steer("switch0").unwrap().switches_for("cr2"), vec![0]);
        assert_eq!(steer("uplink0").unwrap().switches_for("cr2"), vec![0]);
        assert_eq!(steer("uplink1").unwrap().switches_for("cr1"), vec![1]);

        let emitted = steer("uplink0-cr1").unwrap();
        assert_eq!(emitted.switches_for("cr1"), vec![0]);
        assert!(emitted.switches_for("cr2").is_empty());

        let qualified = steer("cr1:uplink1").unwrap();
        assert_eq!(qualified.switches_for("cr1"), vec![1]);
        assert!(qualified.switches_for("cr2").is_empty());
        assert_eq!(steer("all").unwrap().switches_for("cr2"), vec![0, 1]);
        assert!(steer("none").unwrap().switches_for("cr1").is_empty());
        assert_eq!(
            steer("cr1:switch0,cr2:uplink1").unwrap().switches_for("cr2"),
            vec![1]
        );

        assert_eq!(steer("enp0s9").unwrap().switches_for("cr1"), vec![0]);
        assert_eq!(steer("cr1:enp0s9").unwrap().switches_for("cr1"), vec![0]);
        assert!(steer("cr2:enp0s9").unwrap().switches_for("cr1").is_empty());
        assert!(steer("cr1:enp0s11").is_err(), "the external NIC is no path");
        assert!(steer("cr2:uplink0-cr1").is_err());
        assert!(steer("switch7").is_err(), "a switch the topology lacks");
        assert!(steer("cr9:switch0").is_err(), "an unknown router");
        assert!(
            "239.1.1.1".parse::<SteerSpec>().is_err(),
            "missing the selection"
        );
        assert!(
            "198.51.100.1=all".parse::<SteerSpec>().is_err(),
            "not a multicast group"
        );
    }

    #[test]
    fn steering_end_to_end() {
        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            sleds = 4
        "})
        .unwrap();
        let spec: SteerSpec = "239.100.0.1=cr1:uplink0".parse().unwrap();
        let steering = Steering::parse(&[spec], &cfg).unwrap();
        let paths = &steering.paths[&Ipv4Addr::new(239, 100, 0, 1)];
        assert_eq!(paths.switches_for("cr1"), vec![0]);
        assert!(paths.switches_for("cr2").is_empty());
    }

    #[test]
    fn paths_follow_the_router_qualifier() {
        let paths = Paths(vec![ps(None, 0), ps(Some("cr1"), 1)]);

        // cr2 sees only the unqualified path, cr1 sees both.
        assert_eq!(paths.switches_for("cr2"), vec![0]);
        assert_eq!(paths.switches_for("cr1"), vec![0, 1]);
    }

    #[test]
    fn a_one_switch_topology_still_parses_steering() {
        // A label that resolves past the last switch is dropped from the
        // alias tables rather than being rejected.

        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            sleds = 1
        "})
        .unwrap();
        assert!(Steering::parse(&[], &cfg).is_ok(), "an unsteered run");
        assert!(
            Steering::parse(&["239.1.1.1=uplink1".parse().unwrap()], &cfg)
                .is_err()
        );
        assert!(
            Steering::parse(&["239.1.1.1=uplink1-cr1".parse().unwrap()], &cfg)
                .is_err(),
            "an emitted label past the last switch"
        );
    }

    #[test]
    fn the_default_delivery_uses_one_path() {
        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            sleds = 4
        "})
        .unwrap();
        let routers = cfg.multicast_routers();
        let paths =
            Paths::from_config(&cfg, &routers).expect("single delivery");
        assert_eq!(paths.switches_for(&routers[0]), vec![0]);
        for other in &routers[1..] {
            assert!(paths.switches_for(other).is_empty(), "{other} carries it");
        }

        let cfg = VoxelConfig::from_toml(indoc::indoc! {r#"
            [topology]
            sleds = 4
            [network.multicast]
            delivery = "all"
        "#})
        .unwrap();
        let paths = Paths::from_config(&cfg, &cfg.multicast_routers())
            .expect("all delivery");
        for router in cfg.multicast_routers() {
            assert_eq!(paths.switches_for(&router), vec![0, 1]);
        }
    }

    #[test]
    fn rack_reachability_uses_global_switch_indices() {
        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            racks = 2
            sleds = 3
        "})
        .unwrap();
        let group = Ipv4Addr::new(239, 1, 1, 1);
        let groups = [group];
        let routers = cfg.multicast_routers();
        let into_rack_1 = Steering {
            paths: HashMap::from([(
                group,
                Paths(vec![ps(Some(&routers[0]), 0)]),
            )]),
        };
        assert!(
            into_rack_1.unreached_groups(&cfg, &groups, &routers, 0).is_empty()
        );
        assert_eq!(
            into_rack_1.unreached_groups(&cfg, &groups, &routers, 1),
            groups
        );

        let into_rack_2 = Steering {
            paths: HashMap::from([(
                group,
                Paths(vec![ps(Some(&routers[0]), 2)]),
            )]),
        };
        assert_eq!(
            into_rack_2.unreached_groups(&cfg, &groups, &routers, 0),
            groups
        );
        assert!(
            into_rack_2.unreached_groups(&cfg, &groups, &routers, 1).is_empty()
        );

        assert!(
            Steering::default()
                .unreached_groups(&cfg, &groups, &routers, 1)
                .is_empty()
        );
    }

    #[test]
    fn repeated_lldp_labels_require_switch_names_across_racks() {
        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            racks = 2
            sleds = 3
        "})
        .unwrap();

        for selection in ["uplink1", "cr1:uplink1", "uplink1-cr1"] {
            assert!(
                Steering::parse(
                    &[format!("239.1.1.1={selection}").parse().unwrap()],
                    &cfg
                )
                .is_err()
            );
        }
        let parsed =
            Steering::parse(&["239.1.1.1=switch2".parse().unwrap()], &cfg)
                .unwrap();
        assert_eq!(
            parsed.paths[&Ipv4Addr::new(239, 1, 1, 1)].switches_for("cr1"),
            vec![2]
        );
        assert!(Steering::parse(&[], &cfg).unwrap().paths.is_empty());
    }

    #[test]
    fn lldp_aliases_skip_slots_missing_from_every_rack() {
        let cfg = VoxelConfig::from_toml(indoc::indoc! {"
            [topology]
            racks = 2
            sleds = 1
        "})
        .unwrap();

        for selection in ["uplink1", "cr1:uplink1", "uplink1-cr1"] {
            assert!(
                Steering::parse(
                    &[format!("239.1.1.1={selection}").parse().unwrap()],
                    &cfg
                )
                .is_err()
            );
        }
    }
}
