// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel builder`: persistent omicron build hosts, one per omicron commit.
//!
//! A builder is keyed by the first 7 hex digits of its commit. Its disk is
//! `<dataset>/img/builder-<key>`, its falcon deployment `vb_<key>` (short:
//! falcon's vnic names must fit illumos' 31 characters) and its host state
//! `<workdir>/builder/<key>`, so several can be up at once.
//!
//! The first `up` of a commit copies the most recently used builder's disk
//! (`zfs send | recv`, so neither depends on the other), which keeps cargo's
//! caches and target dir warm. With no builder to copy, or with `--fresh`,
//! the disk is cloned from the builder image instead. falcon clones a disk
//! at every launch and cannot reuse one.
//! `up` therefore has it clone the kept disk, and `down` promotes the running
//! clone back over it (`fold_disk`). The checkout, cargo caches and target
//! dir survive on that disk, and the second build is incremental because of
//! it.
//!
//! `build` compiles and packages omicron over ssh, pulls the packaged tree
//! into the image-build cargo-bay and bakes a `voxel-cp` image through the
//! same tail as the host path. It also builds the commit's own commtest and
//! pulls it to `commtest/`, for `heading` and `voxel commtest --builder`.
//! Host-side state lives under `<workdir>/builder`: the falcon dir, the
//! cargo-bay staged for the guest, and `state.toml`, a
//! cache of what the guest last reported. `status` reads it while the builder
//! is down.

use crate::commtest;
use crate::cpbuild::{self, CpBake, CpStamp};
use crate::imagebuild::{self, SLED_INIT, builder_network, stage_agent};
use crate::net;
use crate::util::confirm;
use crate::zfs::{self, Dataset};
use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use libfalcon::{NodeRef, Runner, unit::gb};
use std::time::Duration;
use voxel_config::VoxelConfig;

const NODE: &str = "builder";
/// No `voxel-` prefix. `image ls` lists only `voxel-` images and must not
/// offer a builder's live disk.
pub(crate) const DISK_PREFIX: &str = "builder-";
/// The builders' host directory under the anchored workdir, beside the
/// rack's `.falcon`. Each builder has `<DIR>/<key>`.
const DIR: &str = "builder";
const GUEST_SRC: &str = "/work/omicron";
const GUEST_RECORD: &str = "/opt/oxide/voxel/builder.toml";

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Absent,
    Down,
    Up,
}

pub(crate) struct Builder {
    cfg: VoxelConfig,
    dataset: String,
    /// The omicron rev as given; `up` checks it out.
    rev: String,
    /// `rev`'s first 7 hex digits, which name the disk and deployment.
    key: String,
    dir: Utf8PathBuf,
}

/// A builder's key: the first 7 digits of an omicron commit sha. Branch
/// names are refused, since the builder would keep the name after the
/// branch moved.
pub(crate) fn key(rev: &str) -> Result<String> {
    if rev.len() < 7
        || rev.len() > 40
        || !rev.chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!(
            "builders are per omicron commit sha (7-40 hex digits), not {rev:?}"
        );
    }
    Ok(rev[..7].to_ascii_lowercase())
}

/// The builders' host directory. Absolute, because the cp bake changes
/// directory into image-build/ and state is written afterwards.
fn builders_dir() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir().context("current dir")?;
    Ok(Utf8PathBuf::try_from(cwd)
        .context("non-utf8 working directory")?
        .join(DIR))
}

/// Every builder disk under the dataset as (key, disk), least recently
/// used first: `base` is re-taken at every `down`.
fn disks(dataset: &str) -> Result<Vec<(String, String)>> {
    let img = format!("{dataset}/img");
    Ok(zfs::children_by_snapshot_age(&img, "base")?
        .into_iter()
        .filter_map(|ds| {
            let key = ds.strip_prefix(&format!("{img}/{DISK_PREFIX}"))?;
            Some((key.to_string(), ds))
        })
        .collect())
}

impl Builder {
    pub(crate) fn new(cfg: VoxelConfig, rev: &str) -> Result<Self> {
        let key = key(rev)?;
        let dir = builders_dir()?.join(&key);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("mkdir {dir}"))?;
        Ok(Self {
            cfg,
            dataset: crate::image::falcon_dataset(),
            rev: rev.to_string(),
            key,
            dir,
        })
    }

    /// The builder for `rev`, or without one the only builder that is up,
    /// else the only one there is.
    pub(crate) fn pick(cfg: VoxelConfig, rev: Option<&str>) -> Result<Self> {
        if let Some(rev) = rev {
            return Self::new(cfg, rev);
        }
        let dataset = crate::image::falcon_dataset();
        let keys: Vec<String> = disks(&dataset)?
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| key(k).is_ok())
            .collect();
        let builders = keys
            .iter()
            .map(|k| Self::new(cfg.clone(), k))
            .collect::<Result<Vec<_>>>()?;
        let mut up = Vec::new();
        for b in builders {
            if b.state()? == State::Up {
                up.push(b);
            } else if keys.len() == 1 {
                return Ok(b);
            }
        }
        match (up.len(), keys.len()) {
            (1, _) => Ok(up.remove(0)),
            (_, 0) => {
                bail!("no builders; `voxel builder up <sha>` creates one")
            }
            _ => bail!(
                "several builders ({}); name one by its omicron sha",
                keys.join(", ")
            ),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    fn deploy(&self) -> String {
        format!("vb_{}", self.key)
    }

    fn falcon_dir(&self) -> Utf8PathBuf {
        self.dir.join(".falcon")
    }

    /// The commtest this builder compiled for its checkout: `commtest` and
    /// the `commtest.rs` it came from (which says whether it has multicast).
    pub(crate) fn commtest_dir(&self) -> Utf8PathBuf {
        self.dir.join("commtest")
    }

    fn cargo_bay(&self) -> Utf8PathBuf {
        self.dir.join("cargo-bay")
    }

    fn state_file(&self) -> Utf8PathBuf {
        self.dir.join("state.toml")
    }

    /// The kept disk.
    fn disk(&self) -> Result<Dataset> {
        Dataset::new(format!("{}/img/{DISK_PREFIX}{}", self.dataset, self.key))
    }

    /// falcon's clone of the kept disk while the builder is up.
    fn node_disk(&self) -> Result<Dataset> {
        Dataset::new(format!("{}/topo/{}/{NODE}", self.dataset, self.deploy()))
    }

    /// The most recently used other builder's disk, which a new disk is
    /// copied from.
    fn seed(&self) -> Result<Option<Dataset>> {
        let me = self.disk()?;
        disks(&self.dataset)?
            .into_iter()
            .rev()
            .map(|(_, ds)| ds)
            .find(|ds| ds != me.as_str())
            .map(Dataset::new)
            .transpose()
    }

    /// Whether `up` needs the builder image: there is no disk for this
    /// commit and none to copy.
    pub(crate) fn needs_image(&self) -> Result<bool> {
        Ok(!zfs::exists(&self.disk()?) && self.seed()?.is_none())
    }

    fn state(&self) -> Result<State> {
        if !zfs::exists(&self.disk()?) {
            return Ok(State::Absent);
        }
        Ok(if imagebuild::propolis_alive(&self.falcon_dir(), NODE) {
            State::Up
        } else {
            State::Down
        })
    }

    /// The falcon deployment, built identically on every call. A fresh
    /// process can then exec into or tear down the running builder.
    fn runner(&self) -> Result<(Runner, NodeRef)> {
        let b = &self.cfg.builder;
        let mut d = Runner::new(&self.deploy());
        d.persistent = true;
        d.set_falcon_dir(Some(self.falcon_dir().to_string()));
        if let Some(bin) = &self.cfg.falcon.propolis_binary {
            d.set_propolis_binary(Some(bin.clone()));
        }
        let disk = format!("{DISK_PREFIX}{}", self.key);
        let n = d.node(NODE, &disk, b.cores, gb(b.memory_gb));
        d.reserve(n, b.disk_gb as usize);
        match self.cfg.external.isolated() {
            true => d.ext_link(crate::isolated_external::STUB, n),
            false => d.default_ext_link(n).map_err(|e| {
                anyhow::anyhow!("find default external interface: {e}")
            })?,
        }
        d.mount(self.cargo_bay().as_str(), "/opt/cargo-bay", n)
            .map_err(|e| anyhow::anyhow!("mount cargo-bay: {e}"))?;
        Ok((d, n))
    }

    fn read_state(&self) -> toml::Table {
        std::fs::read_to_string(self.state_file())
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or_default()
    }

    fn write_state(&self, state: &toml::Table) -> Result<()> {
        let text = toml::to_string(state).context("render state.toml")?;
        std::fs::write(self.state_file(), text)
            .with_context(|| format!("write {}", self.state_file()))
    }

    fn state_str(&self, key: &str) -> Option<String> {
        self.read_state().get(key)?.as_str().map(str::to_string)
    }

    pub(crate) fn is_up(&self) -> Result<bool> {
        Ok(self.state()? == State::Up)
    }

    /// The builder's address. Fails unless the builder is up.
    fn require_up(&self) -> Result<String> {
        if self.state()? != State::Up {
            bail!(
                "builder {k} is not up; `voxel builder up {k}`",
                k = self.key
            );
        }
        self.state_str("ip").with_context(|| {
            format!(
                "builder {k} is up but its address is unknown; `voxel \
                 builder down {k}` then `up {k}`",
                k = self.key
            )
        })
    }

    /// Boot the builder and check out its rev. `force` discards uncommitted
    /// changes in the checkout; `fresh` starts the disk over from the
    /// builder image.
    pub(crate) async fn up(
        &self,
        force: bool,
        fresh: bool,
        yes: bool,
    ) -> Result<()> {
        let k = &self.key;
        if self.state()? == State::Up {
            bail!("builder {k} is already up; `voxel builder status {k}`");
        }
        if self.cfg.external.isolated()
            && let Some(other) = self.others_up()?.first()
        {
            // They would all take host_ip - 1.
            bail!(
                "builder {other} is up, and in isolated mode builders share \
                 one address; `voxel builder down {other}` first"
            );
        }
        let disk = self.disk()?;
        if fresh && zfs::exists(&disk) {
            if !yes
                && !confirm(&format!(
                    "discard the builder disk {} and its snapshots?",
                    disk.as_str()
                ))
            {
                bail!("aborted");
            }
            eprintln!("[voxel] discarding the builder disk");
            zfs::destroy_dataset_if_present(&disk)?;
        }
        let image = self.cfg.builder_image();
        // A copied disk carries its source's checkout, whose changes belong
        // to the source.
        let mut force = force;
        if !zfs::exists(&disk) {
            match self.seed()?.filter(|_| !fresh) {
                Some(seed) => {
                    eprintln!(
                        "[voxel] new builder disk for {k}: copying {} \
                         (warm caches)",
                        seed.as_str()
                    );
                    self.copy_disk(&seed)?;
                    force = true;
                }
                None => {
                    let base =
                        Dataset::new(format!("{}/img/{image}", self.dataset))?
                            .snapshot("base");
                    if !base.exists() {
                        bail!(
                            "no builder image {image}; build one with `voxel \
                             image create-builder` or name one in [builder] \
                             image"
                        );
                    }
                    eprintln!("[voxel] new builder disk for {k} from {image}");
                    zfs::clone(&base, &disk)?;
                    zfs::snapshot(&disk, "base")?;
                }
            }
        } else if zfs::exists(&self.node_disk()?) {
            // An interrupted `down` left the running clone in place.
            self.fold_disk()?;
        }

        let bay = imagebuild::fresh_dir(self.cargo_bay())?;
        stage_agent(&SLED_INIT, &bay)?;
        let network = builder_network(Some(&self.cfg.external))?;
        imagebuild::stage_builder_net(&bay, network.static_address.as_deref())?;
        std::fs::write(bay.join("builder-image"), format!("{image}\n"))
            .context("stage builder-image")?;
        std::fs::write(bay.join("omicron-rev"), format!("{}\n", self.rev))
            .context("stage omicron-rev")?;
        if force {
            std::fs::write(bay.join("omicron-force"), "")
                .context("stage omicron-force")?;
        }

        let (mut d, n) = self.runner()?;
        let b = &self.cfg.builder;
        eprintln!(
            "[voxel] booting the builder: {} cores, {} GB, {} GB disk",
            b.cores, b.memory_gb, b.disk_gb
        );
        d.launch().await.map_err(|e| anyhow::anyhow!("launch builder: {e}"))?;
        let out = exec(
            &d,
            n,
            "mkdir -p /opt/oxide && \
             cp /opt/cargo-bay/voxel-init /opt/oxide/voxel-init && \
             chmod +x /opt/oxide/voxel-init && \
             /opt/oxide/voxel-init builder 2>&1",
        )
        .await?;
        println!("{}", out.trim_end());
        if !out.contains("builder ready") {
            bail!("the builder's bring-up did not complete; see above");
        }
        let ip = match network.static_address.as_deref() {
            Some(cidr) => cidr.split('/').next().unwrap_or(cidr).to_string(),
            None => net::node_external_ip(&d, n, false, None).await?,
        };
        let record = exec(&d, n, &format!("cat {GUEST_RECORD}"))
            .await
            .unwrap_or_default();
        let mut state = toml::Table::new();
        state.insert("ip".into(), ip.clone().into());
        state.insert("image".into(), image.into());
        if let Some(rev) = record_value(&record, "omicron") {
            state.insert("omicron".into(), rev.into());
        }
        self.write_state(&state)?;
        println!(
            "builder {k} up at {ip}; `voxel builder login {k}` opens a shell"
        );
        Ok(())
    }

    pub(crate) async fn down(&self, destroy: bool, yes: bool) -> Result<()> {
        if destroy
            && self.state()? != State::Absent
            && !yes
            && !confirm(&format!(
                "destroy the builder disk {} and its snapshots?",
                self.disk()?.as_str()
            ))
        {
            bail!("aborted");
        }
        match self.state()? {
            State::Up => {
                let (d, n) = self.runner()?;
                eprintln!("[voxel] halting builder {}", self.key);
                let _ = exec(&d, n, "sync; sync; (sleep 1; halt) &").await;
                imagebuild::wait_for_propolis_exit(
                    &self.falcon_dir(),
                    NODE,
                    Duration::from_secs(60),
                );
                // The disk must be closed before promote and rename.
                imagebuild::hyperstop(&self.falcon_dir(), NODE);
                self.fold_disk()?;
                d.destroy().map_err(|e| {
                    anyhow::anyhow!("tear down the builder deployment: {e}")
                })?;
            }
            State::Down => {
                if zfs::exists(&self.node_disk()?) {
                    self.fold_disk()?;
                }
            }
            State::Absent if !destroy => bail!(
                "no builder {k}; `voxel builder up {k}` creates one",
                k = self.key
            ),
            State::Absent => {}
        }
        if destroy {
            zfs::destroy_dataset_if_present(&self.disk()?)?;
            let _ = std::fs::remove_dir_all(&self.dir);
            println!("builder {} destroyed", self.key);
        } else {
            let mut state = self.read_state();
            state.remove("ip");
            self.write_state(&state)?;
            println!(
                "builder {} down; the disk is kept for the next `up`",
                self.key
            );
        }
        Ok(())
    }

    /// The keys of the other builders that are up.
    fn others_up(&self) -> Result<Vec<String>> {
        let mut up = Vec::new();
        for (k, _) in disks(&self.dataset)? {
            if k == self.key || key(&k).is_err() {
                continue;
            }
            if imagebuild::propolis_alive(
                &builders_dir()?.join(&k).join(".falcon"),
                NODE,
            ) {
                up.push(k);
            }
        }
        Ok(up)
    }

    /// Make this builder's disk a full copy of `seed@base`. Received under a
    /// staging name and renamed, so a failed copy leaves no disk behind.
    fn copy_disk(&self, seed: &Dataset) -> Result<()> {
        let disk = self.disk()?;
        let staging = Dataset::new(format!(
            "{}/img/incoming-{DISK_PREFIX}{}",
            self.dataset, self.key
        ))?;
        zfs::destroy_dataset_if_present(&staging)?;
        zfs::send_recv(&seed.snapshot("base"), &staging)?;
        zfs::rename(&staging, &disk)?;
        // The stream carries `base` along, which the next clone needs.
        if !disk.snapshot("base").exists() {
            zfs::snapshot(&disk, "base")?;
        }
        Ok(())
    }

    /// Make the running clone the kept disk. Promote it over its origin, drop
    /// the origin, rename it, and re-snapshot `base` for the next clone.
    fn fold_disk(&self) -> Result<()> {
        let node = self.node_disk()?;
        let disk = self.disk()?;
        eprintln!("[voxel] keeping the builder disk as {}", disk.as_str());
        zfs::promote(&node)?;
        zfs::destroy_dataset_if_present(&disk)?;
        zfs::rename(&node, &disk)?;
        // falcon reserved the clone's full size. The kept disk does not need
        // the reservation.
        zfs::set(&disk, &[("reservation", "none")])?;
        zfs::destroy_snapshot_if_present(&disk.snapshot("base"))?;
        zfs::snapshot(&disk, "base")
    }

    /// One line per builder.
    pub(crate) fn list(cfg: &VoxelConfig) -> Result<()> {
        let all = disks(&crate::image::falcon_dataset())?;
        if all.is_empty() {
            println!("no builders; `voxel builder up <sha>` creates one");
        }
        for (k, ds) in all {
            if key(&k).is_err() {
                println!("{k}: {ds} (not a per-commit builder; unused)");
                continue;
            }
            let b = Self::new(cfg.clone(), &k)?;
            let state = match b.state()? {
                State::Up => format!(
                    "up at {}",
                    b.state_str("ip").unwrap_or_else(|| "?".into())
                ),
                _ => "down".into(),
            };
            let last = b
                .state_str("last_build")
                .map(|l| format!(", last build {l}"))
                .unwrap_or_default();
            println!("{k}: {state}{last}");
        }
        Ok(())
    }

    pub(crate) fn status(&self) -> Result<()> {
        let state = self.state()?;
        let disk = self.disk()?;
        println!(
            "builder {}: {}",
            self.key,
            match state {
                State::Absent => "absent (no disk)",
                State::Down => "down",
                State::Up => "up",
            }
        );
        println!("image: {}", self.cfg.builder_image());
        if state == State::Absent {
            return Ok(());
        }
        println!("disk: {}", disk.as_str());
        if let Some(origin) = zfs::origin(&disk) {
            println!("cloned from: {origin}");
        }
        match state {
            State::Up => {
                let ip = self.state_str("ip").unwrap_or_else(|| "?".into());
                println!("address: {ip}");
                let excludes: Vec<String> = cpbuild::dirty_excludes()
                    .iter()
                    .map(|e| format!("'{e}'"))
                    .collect();
                let probe = format!(
                    "cd {GUEST_SRC} && git rev-parse --short HEAD && \
                     git status --porcelain -- {} | wc -l",
                    excludes.join(" ")
                );
                match net::ssh_capture(&ip, &probe) {
                    Some(out) => {
                        let mut lines = out.lines();
                        let commit = lines.next().unwrap_or("?");
                        let changed: usize = lines
                            .next()
                            .and_then(|l| l.trim().parse().ok())
                            .unwrap_or(0);
                        println!(
                            "omicron: {commit}{}",
                            if changed > 0 {
                                format!(", {changed} files changed")
                            } else {
                                String::new()
                            }
                        );
                    }
                    None => println!("omicron: (builder not answering ssh)"),
                }
            }
            State::Down => {
                if let Some(rev) = self.state_str("omicron") {
                    println!("omicron: {rev} (as of the last up)");
                }
            }
            State::Absent => {}
        }
        if let Some(last) = self.state_str("last_build") {
            println!("last build: {last}");
        }
        let current = if state == State::Up { self.node_disk()? } else { disk };
        let snaps: Vec<(String, String)> = zfs::snapshots(&current)?
            .into_iter()
            .filter(|(name, _)| name.contains("@snap-"))
            .collect();
        for (name, created) in snaps {
            let label = name.split("@snap-").nth(1).unwrap_or(&name);
            println!("snap: {label} ({created})");
        }
        Ok(())
    }

    pub(crate) fn login(&self) -> Result<()> {
        let ip = self.require_up()?;
        eprintln!("[voxel] ssh root@{ip} (builder)");
        crate::access::ssh_exec(&ip, None)
    }

    /// Push a local omicron tree into the builder's checkout. target/ and
    /// out/ stay the builder's own.
    pub(crate) fn sync(&self, dir: &Utf8Path) -> Result<()> {
        let ip = self.require_up()?;
        if !dir.join("package-manifest.toml").exists() {
            bail!("{dir} does not look like an omicron checkout");
        }
        eprintln!("[voxel] syncing {dir} -> builder:{GUEST_SRC}");
        let mut rsync = net::rsync_over_ssh()?;
        rsync
            .args([
                "-a",
                "--delete",
                "--exclude",
                "/target",
                "--exclude",
                "/out",
            ])
            .arg(format!("{dir}/"))
            .arg(format!("root@{ip}:{GUEST_SRC}/"));
        let status = rsync.status().context("run rsync")?;
        if !status.success() {
            bail!("rsync to the builder failed ({status})");
        }
        Ok(())
    }

    /// Build and package omicron in the builder, then bake `voxel-cp-<name>`.
    /// With `halt`, the builder goes down once the packaged tree is pulled,
    /// so its memory is free for the bake VM.
    pub(crate) async fn build(&self, name: &str, halt: bool) -> Result<()> {
        let ip = self.require_up()?;
        // Read before the smf push renders into the checkout.
        let stamp = guest_stamp(&ip)?;
        if !cpbuild::PINNED_OMICRON_REV.starts_with(&stamp.commit) {
            eprintln!(
                "[voxel] building omicron {}; voxel's generated config-rss \
                 tracks {}, so a schema change between them shows up at \
                 commissioning",
                stamp.commit,
                cpbuild::PINNED_OMICRON_REV
            );
        }
        let root = imagebuild::build_root()?;
        // Staged in the builder's own dir: another builder may be pulling
        // its tree at the same time.
        let cargo_bay = imagebuild::fresh_dir(self.dir.join("bake-bay"))?;
        // voxel-config renders the smf configs on the host from the sled
        // schema sources, and the bake tail reads the firmware pins. Both
        // come through a checkout-shaped stub.
        let stub = imagebuild::fresh_dir(self.dir.join("schema-stub"))?;
        for (rel, file) in [
            ("sled-agent/src", "config.rs"),
            ("sled-hardware/src", "lib.rs"),
            ("tools", "permslip_staging"),
        ] {
            pull(&ip, &[&format!("{rel}/{file}")], &stub.join(rel), &[])?;
        }
        // The SP release pin, from omicron#10760 onwards. Older checkouts
        // have none and take their SP images from permslip_staging.
        let sp_release = crate::permslip::SP_RELEASE_FILE;
        if net::ssh_capture(&ip, &format!("test -f {GUEST_SRC}/{sp_release}"))
            .is_some()
        {
            let rel = Utf8Path::new(sp_release);
            pull(
                &ip,
                &[sp_release],
                &stub.join(rel.parent().expect("SP_RELEASE_FILE has a dir")),
                &[],
            )?;
        }
        crate::image::render_smf(&stub, cpbuild::num_gimlets())?;
        push(&stub.join("smf"), &ip, &format!("{GUEST_SRC}/smf"))?;

        eprintln!("[voxel] building omicron in the builder");
        net::ssh_run(&ip, &build_script())?;

        let stage = cargo_bay.join("omicron");
        eprintln!("[voxel] pulling the packaged tree -> {stage}");
        pull(&ip, cpbuild::STAGE_PATHS, &stage, &[])?;
        // While the caches are warm, so heading can run this commit's own
        // commtest (multicast included) on a host without a toolchain.
        self.pull_commtest(&ip)?;
        if halt {
            self.down(false, false).await?;
        }

        let image_name = format!("voxel-cp-{name}");
        let _lock = imagebuild::bake_lock()?;
        cpbuild::bake_cp(CpBake {
            root: &root,
            cargo_bay: &cargo_bay,
            image_name: &image_name,
            dataset: &self.dataset,
            external: Some(&self.cfg.external),
            checkout: &stub,
            stamp: &stamp,
            propolis_binary: self.cfg.falcon.propolis_binary.as_deref(),
            base_image: imagebuild::HELIOS_BASE,
        })
        .await?;
        let mut state = self.read_state();
        state.insert("last_build".into(), image_name.into());
        state.insert("omicron".into(), stamp.commit.into());
        self.write_state(&state)
    }

    /// Snapshot the builder disk, live or kept.
    /// `voxel builder commtest`: build this commit's commtest in a running
    /// builder and pull it to the host.
    pub(crate) fn commtest(&self) -> Result<()> {
        let ip = self.require_up()?;
        self.pull_commtest(&ip)
    }

    fn pull_commtest(&self, ip: &str) -> Result<()> {
        let dir = self.commtest_dir();
        // Never leave a binary from an earlier checkout behind.
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("remove {dir}"))?;
        }
        let src = commtest::COMMTEST_SRC;
        if net::ssh_capture(ip, &format!("test -f {GUEST_SRC}/{src}")).is_none()
        {
            eprintln!("[voxel] this omicron has no {src}; skipping commtest");
            return Ok(());
        }
        eprintln!("[voxel] building commtest in the builder");
        net::ssh_run(ip, &commtest_script())?;
        pull(ip, &["target/release/commtest", src], &dir, &[])?;
        eprintln!("[voxel] commtest -> {dir}");
        Ok(())
    }

    pub(crate) async fn snap(&self, label: &str) -> Result<()> {
        let (ds, up) = match self.state()? {
            State::Absent => bail!("no builder disk to snapshot"),
            State::Up => (self.node_disk()?, true),
            State::Down => (self.disk()?, false),
        };
        if up {
            let (d, n) = self.runner()?;
            let _ = exec(&d, n, "sync; sync").await;
        }
        zfs::snapshot(&ds, &snap_name(label)?)?;
        println!("snapshot {label} taken");
        Ok(())
    }

    /// Roll the kept disk back to a snapshot. The builder must be down
    /// because propolis holds the live clone open.
    pub(crate) fn restore(&self, label: &str) -> Result<()> {
        match self.state()? {
            State::Absent => bail!("no builder disk to restore"),
            State::Up => bail!("the builder is up; `voxel builder down` first"),
            State::Down => {}
        }
        let disk = self.disk()?;
        let snap = disk.snapshot(&snap_name(label)?);
        if !snap.exists() {
            bail!("no snapshot {label}; `voxel builder status` lists them");
        }
        zfs::rollback(&snap)?;
        // rollback -r drops every newer snapshot. The next clone needs a
        // fresh base.
        zfs::destroy_snapshot_if_present(&disk.snapshot("base"))?;
        zfs::snapshot(&disk, "base")?;
        println!("builder disk restored to {label}");
        Ok(())
    }
}

/// `snap-<label>`. The label is limited to a plain character set to keep
/// snapshot names predictable.
fn snap_name(label: &str) -> Result<String> {
    let ok = !label.is_empty()
        && label.len() <= 64
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "snapshot label {label:?}: use 1 to 64 letters, digits, '-', '_' or '.'"
        );
    }
    Ok(format!("snap-{label}"))
}

async fn exec(d: &Runner, n: NodeRef, cmd: &str) -> Result<String> {
    d.exec(n, cmd)
        .await
        .map_err(|e| anyhow::anyhow!("exec in the builder: {e}"))
}

/// A `key = "value"` line's value from the guest's builder.toml.
fn record_value(record: &str, key: &str) -> Option<String> {
    record.parse::<toml::Table>().ok()?.get(key)?.as_str().map(str::to_string)
}

/// The omicron build the host path runs in steps 2, 3 and 5 of
/// `cpbuild::create_cp`, as one script for the guest.
fn build_script() -> String {
    format!(
        "set -e\n\
         . /work/env.sh\n\
         cd {GUEST_SRC}\n\
         export RUSTFLAGS='{}'\n\
         ./tools/install_builder_prerequisites.sh -y\n\
         ./tools/ci_download_softnpu_machinery\n\
         cargo build --release -p omicron-package -p xtask -p xtask-downloader\n\
         ./target/release/omicron-package -t default target create -p a4x2\n\
         ./target/release/omicron-package package\n",
        cpbuild::OMICRON_RUSTFLAGS
    )
}

/// Release, with the omicron build's flags: it shares that build's deps.
fn commtest_script() -> String {
    format!(
        "set -e\n\
         . /work/env.sh\n\
         cd {GUEST_SRC}\n\
         export RUSTFLAGS='{}'\n\
         cargo build --release -p end-to-end-tests --bin commtest\n",
        cpbuild::OMICRON_RUSTFLAGS
    )
}

/// Copy `paths`, relative to the guest checkout, into `dest` with the host
/// stage's rsync flags and no -R. A file lands at `dest/<basename>`, the
/// layout the host stage produces.
fn pull(
    ip: &str,
    paths: &[&str],
    dest: &Utf8Path,
    excludes: &[&str],
) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("mkdir {dest}"))?;
    let mut rsync = net::rsync_over_ssh()?;
    cpbuild::stage_rsync_args(&mut rsync);
    for ex in excludes {
        rsync.arg("--exclude").arg(ex);
    }
    // After the first source, a bare `:path` means the same host.
    for (i, rel) in paths.iter().enumerate() {
        let host = if i == 0 { format!("root@{ip}") } else { String::new() };
        rsync.arg(format!("{host}:{GUEST_SRC}/{rel}"));
    }
    rsync.arg(format!("{dest}/"));
    let status = rsync.status().context("run rsync")?;
    if !status.success() {
        bail!("rsync from the builder failed ({status})");
    }
    Ok(())
}

/// Copy a local directory's contents into a guest directory.
fn push(src: &Utf8Path, ip: &str, dest: &str) -> Result<()> {
    let mut rsync = net::rsync_over_ssh()?;
    rsync
        .args(["-a", "--no-owner", "--no-group"])
        .arg(format!("{src}/"))
        .arg(format!("root@{ip}:{dest}/"));
    let status = rsync.status().context("run rsync")?;
    if !status.success() {
        bail!("rsync to the builder failed ({status})");
    }
    Ok(())
}

/// The stamp from the builder's checkout.
fn guest_stamp(ip: &str) -> Result<CpStamp> {
    let excludes: Vec<String> =
        cpbuild::dirty_excludes().iter().map(|e| format!("'{e}'")).collect();
    let out = net::ssh_capture(
        ip,
        &format!(
            "cd {GUEST_SRC} && git rev-parse HEAD && \
             git status --porcelain -- {} | wc -l",
            excludes.join(" ")
        ),
    )
    .context("read the builder's omicron commit")?;
    let mut lines = out.lines();
    let commit = lines.next().context("no commit in the stamp probe")?.trim();
    let changed: usize = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .context("no change count in the stamp probe")?;
    Ok(CpStamp { commit: commit.to_string(), dirty: changed > 0 })
}

#[cfg(test)]
mod tests {
    use super::key;

    #[test]
    fn keys_are_short_shas() {
        assert_eq!(key("5ce2b2c").unwrap(), "5ce2b2c");
        assert_eq!(
            key("5CE2B2C132de306765c8aad0df9d1cd0d55d4021").unwrap(),
            "5ce2b2c"
        );
        assert!(key("main").is_err());
        assert!(key("5ce2b2").is_err());
        assert!(key("disk").is_err());
        // falcon's vnic name for the builder must fit illumos' 31 chars.
        let vnic =
            format!("vb_{}_{}_vn_vnic0", key("5ce2b2c").unwrap(), super::NODE);
        assert!(vnic.len() <= 31, "{vnic}");
    }
}
