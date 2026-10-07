// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Persistent state handling for multicast host routes.
//!
//! This module stores the gateway, source, and rack-facing path selection for
//! each multicast group within a falcon environment. When a gateway or source
//! is replaced, i.e., becomes stale, it remains alongside the current ones
//! until that update commits.
//!
//! Multicast commands run while sharing a working-directory lock; state-file
//! writes replace previous records atomically.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use itertools::Itertools;

/// A selection of rack-facing paths or every path by default.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize,
)]
#[serde(untagged)]
pub(super) enum Selection {
    Paths(Vec<PathSelection>),
    #[default]
    All,
}

impl Selection {
    /// Borrow the explicit paths selected or return `None` when the selection
    /// is every path.
    pub(super) fn as_paths(&self) -> Option<&[PathSelection]> {
        match self {
            Selection::All => None,
            Selection::Paths(paths) => Some(paths),
        }
    }
    /// Whether this selects every path or not.
    pub(super) fn is_all(&self) -> bool {
        *self == Selection::All
    }
}

impl From<Option<Vec<PathSelection>>> for Selection {
    fn from(paths: Option<Vec<PathSelection>>) -> Self {
        paths.map_or(Selection::All, Selection::Paths)
    }
}

impl From<Option<&[PathSelection]>> for Selection {
    fn from(paths: Option<&[PathSelection]>) -> Self {
        paths.map(|paths| paths.to_vec()).into()
    }
}

/// A single rack-facing path consisting of a switch, optionally pinned to
/// a router.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize,
)]
pub(super) struct PathSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) router: Option<String>,
    pub(super) switch: usize,
}

/// A host route installed for one falcon environment.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(super) struct MulticastRoute {
    group: Ipv4Addr,
    gateway: String,
    /// Source address used by this environment's router assignments.
    #[serde(default)]
    pub(super) source: Option<Ipv4Addr>,
    /// The rack-facing paths selected for this group.
    ///
    /// This can be [`Selection::All`] or an empty `Paths` when steering
    /// nowhere explicitly.
    #[serde(default, skip_serializing_if = "Selection::is_all")]
    selection: Selection,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stale_gateways: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stale_sources: Vec<Ipv4Addr>,
}

/// Host-side multicast state persisted per falcon environment.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(super) struct MulticastState {
    pub(super) environment: String,
    pub(super) routes: Vec<MulticastRoute>,
    #[serde(default)]
    pub(super) routers: Vec<String>,
}

impl MulticastState {
    /// Create the empty multicast state for an environment, recording
    /// the incoming router as the first in the list.
    pub(super) fn new(environment: &str, router: &str) -> Self {
        Self {
            environment: environment.to_string(),
            routes: Vec::new(),
            routers: vec![router.to_owned()],
        }
    }

    /// The route recorded for `group`, if any.
    pub(super) fn route(&self, group: &Ipv4Addr) -> Option<&MulticastRoute> {
        self.routes.iter().find(|route| route.group == *group)
    }

    fn route_mut(&mut self, group: &Ipv4Addr) -> Option<&mut MulticastRoute> {
        self.routes.iter_mut().find(|route| route.group == *group)
    }

    pub(super) fn ensure_topology_exists(
        &self,
        addrs: &[Ipv4Addr],
        routers: &[String],
        switches: usize,
    ) -> anyhow::Result<()> {
        if addrs.iter().any(|addr| self.route(addr).is_some()) {
            ensure!(
                !self.routers.is_empty(),
                "multicast state has routes but no recorded forwarding routers"
            );

            for router in &self.routers {
                ensure!(
                    router == "ce" || routers.contains(router),
                    "multicast state records router {router}, but it is not \
                     in the current topology"
                );
            }
        }
        for addr in addrs {
            let Some(route) = self.route(addr) else { continue };
            for path in route.selected_paths().into_iter().flatten() {
                if let Some(router) = &path.router {
                    ensure!(
                        routers.contains(router),
                        "{addr}'s record names router {router}, but it is not \
                         in the current topology"
                    );
                }
                ensure!(
                    path.switch < switches,
                    "{addr}'s record names switch{}, but the current \
                     topology has {switches} switch(es). Run with the \
                     topology that recorded it",
                    path.switch
                );
            }
        }
        Ok(())
    }

    /// The groups with recorded routes.
    pub(super) fn groups(&self) -> impl Iterator<Item = Ipv4Addr> + '_ {
        self.routes.iter().map(|route| route.group)
    }

    /// Create or update a group's host-route gateway.
    ///
    /// This keeps replaced gateways in the stale list until the update
    /// is committed.
    pub(super) fn prepare_gateway(&mut self, group: Ipv4Addr, gateway: &str) {
        if let Some(route) = self.route_mut(&group) {
            if route.gateway != gateway
                && !route.stale_gateways.contains(&route.gateway)
            {
                route.stale_gateways.push(route.gateway.clone());
            }
            route.gateway = gateway.to_string();
        } else {
            self.routes.push(MulticastRoute {
                group,
                gateway: gateway.to_string(),
                source: None,
                selection: Selection::All,
                stale_gateways: Vec::new(),
                stale_sources: Vec::new(),
            });
        }
    }

    /// Prepare source and steering changes for an existing route, if
    /// it exists.
    ///
    /// This keeps replaced sources in the stale list until the update
    /// is committed.
    pub(super) fn prepare_route(
        &mut self,
        group: &Ipv4Addr,
        source: Ipv4Addr,
        selection: Selection,
    ) {
        let Some(route) = self.route_mut(group) else { return };

        if route.source != Some(source) {
            if let Some(prev) = route.source
                && !route.stale_sources.contains(&prev)
            {
                route.stale_sources.push(prev);
            }
            route.source = Some(source);
        }

        route.selection = selection;
    }

    /// The current selection's paths or `None` when it selects
    /// every single path.
    pub(super) fn selected_paths(
        &self,
        group: &Ipv4Addr,
    ) -> Option<&[PathSelection]> {
        self.route(group)?.selection.as_paths()
    }

    /// Commit updates for `groups`, while discarding their stale values.
    pub(super) fn commit_routes(&mut self, groups: &[Ipv4Addr]) {
        for group in groups {
            let Some(route) = self.route_mut(group) else { continue };
            route.stale_gateways.clear();
            route.stale_sources.clear();
        }
    }

    /// Record a router.
    pub(super) fn add_router(&mut self, router: &str) {
        if !self.routers.iter().any(|current| current == router) {
            self.routers.push(router.to_owned());
        }
    }

    /// Remove `groups` routes.
    pub(super) fn remove_groups(&mut self, groups: &[Ipv4Addr]) {
        self.routes.retain(|route| !groups.contains(&route.group));
    }
}

impl MulticastRoute {
    /// Both current and stale gateways (deduped), leading with the
    /// current gateway.
    pub(super) fn gateways(&self) -> Vec<String> {
        std::iter::once(&self.gateway)
            .chain(&self.stale_gateways)
            .cloned()
            .unique()
            .collect()
    }

    /// Both current and stale sources (deduped), leading with the
    /// current source.
    pub(super) fn sources(&self) -> Vec<Ipv4Addr> {
        self.source
            .iter()
            .chain(&self.stale_sources)
            .copied()
            .unique()
            .collect()
    }

    /// This route's explicit paths, or `None` when it selects every path.
    pub(super) fn selected_paths(&self) -> Option<&[PathSelection]> {
        self.selection.as_paths()
    }
}

/// The local state file is keyed by the falcon environment name, and hexed
/// to retain one file per environment.
pub(super) fn multicast_state_path(name: &str) -> PathBuf {
    let key = hex::encode(name);
    PathBuf::from(".falcon").join(format!("multicast-{key}.json"))
}

/// This holds an advisory lock on the working directory for the life of a
/// multicast command.
pub(crate) struct MulticastLock {
    _lock: File,
}

impl MulticastLock {
    /// Drop the environment's state record once a destroy command has completed.
    ///
    /// # Errors
    ///
    /// This fails if the state record cannot be removed.
    pub(crate) fn remove_state_record(&self, name: &str) -> anyhow::Result<()> {
        remove_multicast_state_at(&multicast_state_path(name))
    }
}

fn remove_multicast_state_at(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Lock multicast state in the current working directory.
///
/// The scope of this is the working directory, not an environment. The host
/// route table and the fabric routers are shared by every falcon environment
/// recorded through here; their updates must serialize with each other.
///
/// Returns a guard that holds the lock until it's dropped.
///
/// # Errors
///
/// This fails if the working directory cannot be opened or locked.
pub(super) fn lock_multicast_routes() -> anyhow::Result<MulticastLock> {
    let lock = File::open(".")
        .context("open the working directory for multicast locking")?;
    lock.lock().context("lock the working directory for multicast")?;
    Ok(MulticastLock { _lock: lock })
}

/// Read the multicast state for a falcon environment `name`.
///
/// Returns `None` when no record exists.
///
/// # Errors
///
/// This fails if the record cannot be read, is malformed, or belongs to another
/// environment.
pub(super) fn read_multicast_state(
    name: &str,
) -> anyhow::Result<Option<MulticastState>> {
    read_multicast_state_at(&multicast_state_path(name), name)
}

fn read_multicast_state_at(
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<MulticastState>> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("read {}", path.display()));
        }
    };

    let state: MulticastState = serde_json::from_str(&body)
        .with_context(|| format!("parse {}", path.display()))?;

    if state.environment != name {
        anyhow::bail!(
            "{} belongs to falcon environment '{}', not '{name}'",
            path.display(),
            state.environment,
        );
    }
    Ok(Some(state))
}

/// Write the multicast state for a falcon environment `name`.
///
/// An empty route set removes the record; other updates replace it atomically.
///
/// # Errors
///
/// This fails if the environment name does not match, or if the record cannot be
/// serialized, written, replaced, or removed.
pub(super) fn write_multicast_state(
    name: &str,
    state: &MulticastState,
) -> anyhow::Result<()> {
    write_multicast_state_at(&multicast_state_path(name), name, state)
}

fn write_multicast_state_at(
    path: &Path,
    name: &str,
    state: &MulticastState,
) -> anyhow::Result<()> {
    ensure!(
        state.environment == name,
        "multicast state environment '{}' does not match '{name}'",
        state.environment,
    );
    if state.routes.is_empty() {
        return remove_multicast_state_at(path);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(state)
        .with_context(|| format!("serialize {}", path.display()))?;
    fs::write(&tmp, body)
        .with_context(|| format!("write {}", tmp.display()))?;

    // A failed rename leaves the old record intact, making the update atomic.
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| {
            format!("rename {} to {}", tmp.display(), path.display())
        });
    }
    Ok(())
}

/// Every environment's multicast state under `.falcon/`.
///
/// # Errors
///
/// This fails if the directory cannot be read or a record is malformed.
pub(super) fn read_multicast_states() -> anyhow::Result<Vec<MulticastState>> {
    let entries = match fs::read_dir(".falcon") {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).context("read .falcon multicast state directory");
        }
    };

    let entries = entries
        .collect::<Result<Vec<_>, _>>()
        .context("read .falcon multicast state directory entries")?;
    entries
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (name.starts_with("multicast-") && name.ends_with(".json"))
                .then_some(entry.path())
        })
        .map(|path| {
            let body = fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            serde_json::from_str(&body)
                .with_context(|| format!("parse {}", path.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn path_selector(router: &str, switch: usize) -> PathSelection {
        PathSelection { router: Some(router.to_string()), switch }
    }

    #[test]
    fn recorded_topology_must_remain() {
        let group = Ipv4Addr::new(239, 1, 1, 1);
        let source = Ipv4Addr::new(192, 0, 2, 1);
        let mut state = MulticastState::new("env", "cr1");

        state.add_router("cr2");
        state.prepare_gateway(group, "192.0.2.10");
        state.prepare_route(
            &group,
            source,
            Selection::Paths(vec![path_selector("cr1", 1)]),
        );
        state.commit_routes(&[group]);
        let routers = ["cr1".to_string(), "cr2".to_string()];

        assert!(state.ensure_topology_exists(&[group], &routers, 2).is_ok());
        assert!(state.ensure_topology_exists(&[group], &routers, 1).is_err());
        assert!(
            state
                .ensure_topology_exists(&[group], &["cr1".to_string()], 2,)
                .is_err()
        );

        state.prepare_route(
            &group,
            source,
            Selection::Paths(vec![path_selector("cr3", 0)]),
        );
        state.commit_routes(&[group]);
        assert!(state.ensure_topology_exists(&[group], &routers, 2).is_err());

        state.routers.clear();
        assert!(state.ensure_topology_exists(&[group], &routers, 2).is_err());
    }

    #[test]
    fn steered_to_none_survives_the_record() {
        let mut state = MulticastState::new("env", "cr1");
        let gateway = "192.168.1.54";
        let none = Ipv4Addr::new(239, 1, 1, 1);
        let unsteered = Ipv4Addr::new(239, 1, 1, 2);
        let source = Ipv4Addr::new(192, 168, 1, 10);

        state.prepare_gateway(none, gateway);
        state.prepare_route(&none, source, Selection::Paths(Vec::new()));
        state.prepare_gateway(unsteered, gateway);
        state.prepare_route(&unsteered, source, Selection::All);
        state.commit_routes(&[none, unsteered]);

        assert_eq!(state.selected_paths(&none), Some(&[][..]));
        assert_eq!(state.selected_paths(&unsteered), None);
    }

    #[test]
    fn state_round_trip_preserves_committed_routes() {
        let groups = [
            Ipv4Addr::new(239, 1, 1, 1),
            Ipv4Addr::new(239, 1, 1, 2),
            Ipv4Addr::new(239, 1, 1, 3),
        ];
        let selections = [
            Selection::All,
            Selection::Paths(Vec::new()),
            Selection::Paths(vec![
                path_selector("cr1", 0),
                path_selector("cr2", 1),
            ]),
        ];

        let source = Ipv4Addr::new(192, 0, 2, 1);
        let mut state = MulticastState::new("env", "cr1");
        state.add_router("cr2");

        for (group, selection) in groups.iter().zip(&selections) {
            state.prepare_gateway(*group, "192.0.2.10");
            state.prepare_route(group, source, selection.clone());
        }

        state.commit_routes(&groups);

        let body = serde_json::to_vec_pretty(&state).unwrap();
        let restored: MulticastState = serde_json::from_slice(&body).unwrap();

        assert_eq!(restored.environment, "env");
        assert_eq!(restored.routers, ["cr1", "cr2"]);
        assert_eq!(restored.groups().collect::<Vec<_>>(), groups);

        for (group, selection) in groups.iter().zip(&selections) {
            let route = restored.route(group).unwrap();
            assert_eq!(route.gateway, "192.0.2.10");
            assert_eq!(route.source, Some(source));
            assert_eq!(&route.selection, selection);
            assert_eq!(restored.selected_paths(group), selection.as_paths());
            assert!(route.stale_gateways.is_empty());
            assert!(route.stale_sources.is_empty());
        }
    }

    #[test]
    fn state_round_trip_preserves_in_progress_routes() {
        let group = Ipv4Addr::new(239, 1, 1, 1);
        let source1 = Ipv4Addr::new(192, 0, 2, 1);
        let source2 = Ipv4Addr::new(192, 0, 2, 2);
        let source3 = Ipv4Addr::new(192, 0, 2, 3);
        let first = Selection::Paths(vec![path_selector("cr1", 0)]);
        let second = Selection::Paths(vec![path_selector("cr1", 1)]);

        for selection in [
            Selection::All,
            Selection::Paths(Vec::new()),
            Selection::Paths(vec![path_selector("cr2", 1)]),
        ] {
            let mut state = MulticastState::new("env", "cr1");
            state.add_router("cr2");
            state.prepare_gateway(group, "192.0.2.10");
            state.prepare_route(&group, source1, first.clone());
            state.commit_routes(&[group]);
            state.prepare_gateway(group, "192.0.2.11");
            state.prepare_route(&group, source2, second.clone());
            state.prepare_gateway(group, "192.0.2.12");
            state.prepare_route(&group, source3, selection.clone());

            let route = state.route(&group).unwrap();
            assert_eq!(
                route.gateways(),
                ["192.0.2.12", "192.0.2.10", "192.0.2.11"]
            );
            assert_eq!(route.sources(), [source3, source1, source2]);
            assert_eq!(route.selected_paths(), selection.as_paths());

            let body = serde_json::to_vec_pretty(&state).unwrap();
            let mut restored: MulticastState =
                serde_json::from_slice(&body).unwrap();

            let route = restored.route(&group).unwrap();
            assert_eq!(
                route.gateways(),
                ["192.0.2.12", "192.0.2.10", "192.0.2.11"]
            );
            assert_eq!(route.sources(), [source3, source1, source2]);
            assert_eq!(route.selection, selection);
            assert_eq!(restored.selected_paths(&group), selection.as_paths());

            restored.commit_routes(&[group]);
            let route = restored.route(&group).unwrap();
            assert_eq!(route.selection, selection);
            assert!(route.stale_gateways.is_empty());
            assert!(route.stale_sources.is_empty());
        }
    }

    #[test]
    fn state_file_round_trip_and_empty_state_removal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".falcon/multicast.json");
        let group = Ipv4Addr::new(239, 1, 1, 1);
        let source = Ipv4Addr::new(192, 0, 2, 1);
        let mut state = MulticastState::new("env", "cr1");
        state.prepare_gateway(group, "192.0.2.10");
        state.prepare_route(
            &group,
            source,
            Selection::Paths(vec![path_selector("cr1", 0)]),
        );

        write_multicast_state_at(&path, "env", &state).unwrap();
        let restored = read_multicast_state_at(&path, "env").unwrap().unwrap();
        let route = restored.route(&group).unwrap();
        assert_eq!(route.gateway, "192.0.2.10");
        assert_eq!(route.source, Some(source));
        assert_eq!(
            route.selection,
            Selection::Paths(vec![path_selector("cr1", 0)])
        );

        state.remove_groups(&[group]);
        write_multicast_state_at(&path, "env", &state).unwrap();
        assert!(!path.exists());
    }
}
