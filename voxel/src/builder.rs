// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel builder`: a persistent omicron build host with its own disk.
//!
//! The disk is `<dataset>/img/builder-disk`, cloned from the builder image on
//! the first `up`. falcon clones a disk at every launch and cannot reuse one.
//! `up` therefore has it clone the kept disk, and `down` promotes the running
//! clone back over it (`fold_disk`). The checkout, cargo caches and target
//! dir survive on that disk, and the second build is incremental because of
//! it.
//!
//! `build` compiles and packages omicron over ssh, pulls the packaged tree
//! into the image-build cargo-bay and bakes a `voxel-cp` image through the
//! same tail as the host path. Host-side state lives under `<workdir>/builder`:
//! the falcon dir, the cargo-bay staged for the guest, and `state.toml`, a
//! cache of what the guest last reported. `status` reads it while the builder
//! is down.

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

const DEPLOY: &str = "voxel_builder";
const NODE: &str = "builder";
/// No `voxel-` prefix. `image ls` lists only `voxel-` images and must not
/// offer the builder's live disk.
const DISK: &str = "builder-disk";
/// The builder's host directory under the anchored workdir, beside the
/// rack's `.falcon`.
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
    dir: Utf8PathBuf,
}

impl Builder {
    pub(crate) fn new(cfg: VoxelConfig) -> Result<Self> {
        // The path is made absolute because the cp bake changes directory
        // into image-build/ and state is written afterwards.
        let cwd = std::env::current_dir().context("current dir")?;
        let dir = Utf8PathBuf::try_from(cwd)
            .context("non-utf8 working directory")?
            .join(DIR);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("mkdir {dir}"))?;
        Ok(Self { cfg, dataset: crate::image::falcon_dataset(), dir })
    }

    fn falcon_dir(&self) -> Utf8PathBuf {
        self.dir.join(".falcon")
    }

    fn cargo_bay(&self) -> Utf8PathBuf {
        self.dir.join("cargo-bay")
    }

    fn state_file(&self) -> Utf8PathBuf {
        self.dir.join("state.toml")
    }

    /// The kept disk.
    fn disk(&self) -> Result<Dataset> {
        Dataset::new(format!("{}/img/{DISK}", self.dataset))
    }

    /// falcon's clone of the kept disk while the builder is up.
    fn node_disk(&self) -> Result<Dataset> {
        Dataset::new(format!("{}/topo/{DEPLOY}/{NODE}", self.dataset))
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
        let mut d = Runner::new(DEPLOY);
        d.persistent = true;
        d.set_falcon_dir(Some(self.falcon_dir().to_string()));
        if let Some(bin) = &self.cfg.falcon.propolis_binary {
            d.set_propolis_binary(Some(bin.clone()));
        }
        let n = d.node(NODE, DISK, b.cores, gb(b.memory_gb));
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

    /// The builder's address. Fails unless the builder is up.
    fn require_up(&self) -> Result<String> {
        if self.state()? != State::Up {
            bail!("the builder is not up; `voxel builder up`");
        }
        self.state_str("ip").context(
            "the builder is up but its address is unknown; `voxel builder \
             down` then `up`",
        )
    }

    pub(crate) async fn up(
        &self,
        omicron: Option<&str>,
        force: bool,
        fresh: bool,
        yes: bool,
    ) -> Result<()> {
        if self.state()? == State::Up {
            bail!("the builder is already up; `voxel builder status`");
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
        if !zfs::exists(&disk) {
            let base = Dataset::new(format!("{}/img/{image}", self.dataset))?
                .snapshot("base");
            if !base.exists() {
                bail!(
                    "no builder image {image}; build one with `voxel image \
                     create-builder` or name one in [builder] image"
                );
            }
            eprintln!("[voxel] new builder disk from {image}");
            zfs::clone(&base, &disk)?;
            zfs::snapshot(&disk, "base")?;
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
        if let Some(rev) = omicron {
            std::fs::write(bay.join("omicron-rev"), format!("{rev}\n"))
                .context("stage omicron-rev")?;
            if force {
                std::fs::write(bay.join("omicron-force"), "")
                    .context("stage omicron-force")?;
            }
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
            None => net::node_external_ip(&d, n, false).await?,
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
        println!("builder up at {ip}; `voxel builder login` opens a shell");
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
                eprintln!("[voxel] halting the builder");
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
            State::Absent if !destroy => {
                bail!("no builder; `voxel builder up` creates one")
            }
            State::Absent => {}
        }
        if destroy {
            zfs::destroy_dataset_if_present(&self.disk()?)?;
            let _ = std::fs::remove_file(self.state_file());
            println!("builder disk destroyed");
        } else {
            let mut state = self.read_state();
            state.remove("ip");
            self.write_state(&state)?;
            println!("builder down; the disk is kept for the next `up`");
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

    pub(crate) fn status(&self) -> Result<()> {
        let state = self.state()?;
        let disk = self.disk()?;
        println!(
            "builder: {}",
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
    pub(crate) async fn build(&self, name: &str) -> Result<()> {
        let ip = self.require_up()?;
        // Read before the smf push renders into the checkout.
        let stamp = guest_stamp(&ip)?;
        if !cpbuild::PINNED_OMICRON_REV.starts_with(&stamp.commit) {
            eprintln!(
                "[voxel] WARN: the builder's omicron {} is not the rev voxel is \
                 pinned to ({}); the generated config-rss may not match this image",
                stamp.commit,
                cpbuild::PINNED_OMICRON_REV
            );
        }
        let root = imagebuild::build_root()?;
        let cargo_bay = imagebuild::fresh_cargo_bay("vbuild")?;
        // voxel-config renders the smf configs on the host, from the sled
        // schema sources. Both round-trip through a checkout-shaped stub.
        let stub = imagebuild::fresh_dir(root.join("schema-stub"))?;
        for (rel, file) in
            [("sled-agent/src", "config.rs"), ("sled-hardware/src", "lib.rs")]
        {
            pull(&ip, &[&format!("{rel}/{file}")], &stub.join(rel), &[])?;
        }
        crate::image::render_smf(&stub, cpbuild::num_gimlets())?;
        push(&stub.join("smf"), &ip, &format!("{GUEST_SRC}/smf"))?;

        eprintln!("[voxel] building omicron in the builder");
        net::ssh_run(&ip, &build_script())?;

        let stage = cargo_bay.join("omicron");
        eprintln!("[voxel] pulling the packaged tree -> {stage}");
        pull(&ip, cpbuild::STAGE_PATHS, &stage, &[])?;

        let image_name = format!("voxel-cp-{name}");
        cpbuild::bake_cp(CpBake {
            root: &root,
            cargo_bay: &cargo_bay,
            image_name: &image_name,
            dataset: &self.dataset,
            external: Some(&self.cfg.external),
            schema_root: &stub,
            stamp: &stamp,
        })
        .await?;
        let mut state = self.read_state();
        state.insert("last_build".into(), image_name.into());
        state.insert("omicron".into(), stamp.commit.into());
        self.write_state(&state)
    }

    /// Snapshot the builder disk, live or kept.
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
