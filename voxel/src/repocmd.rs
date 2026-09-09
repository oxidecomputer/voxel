// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel repo` - TUF repo operator helpers against a live rack.
//!
//! `seed` fills every sled's artifact stores: the repo's own targets are
//! staged locally under their sha names and pushed as one tar stream per
//! sled, and Nexus-derived artifacts (composite splits, generated manifests,
//! which exist only where TUF replication has already reached) are
//! cross-synced between sleds, one stream per source/destination pair.
//! Per-file ssh loops trip sshd's connection rate limit, so everything
//! moves in batched streams over a handful of connections. The
//! reconfigurator's measurement and zone updates reject a sled config until
//! every referenced artifact is in that sled's store, so a freshly uploaded
//! release stalls behind replication; seeding collapses that wait to LAN
//! copy time.

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::{Command, Stdio};
use voxel_config::VoxelConfig;

use crate::net::{EPHEMERAL_HOST_OPTS, PASSWORD_AUTH_OPTS, ensure_askpass};
use crate::topo::build_topo;
use crate::tufrepo::TufRepoSource;

struct Sled {
    name: String,
    ip: String,
    /// Artifact store dirs, one per M.2 pool.
    pools: Vec<String>,
    /// Files present in the first pool's store.
    have: BTreeSet<String>,
}

/// Removes the staging dir when seeding returns, error paths included.
struct Staging(Utf8PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn cmd_repo_seed(
    cfg: &VoxelConfig,
    name: &str,
    repo: &Utf8Path,
) -> Result<()> {
    let t = TufRepoSource::load(repo)?;
    // Targets can share bytes (e.g. identical slot a/b archives); the store
    // is sha-keyed, so stage and push each sha once.
    let targets: BTreeMap<String, String> =
        t.target_members()?.into_iter().collect();
    eprintln!(
        "[voxel] seeding {} distinct targets of {} ({})",
        targets.len(),
        t.path,
        t.system_version
    );

    let topo = build_topo(cfg, name)?;
    let mut sleds = Vec::new();
    for (s, n) in topo.sleds.iter() {
        let ip = crate::net::resolve_external_ip(
            cfg,
            &topo.runner,
            &s.name,
            *n,
            false,
        )
        .await
        .with_context(|| format!("resolve {} (is the rack up?)", s.name))?;
        let sled = discover_stores(&s.name, &ip)?;
        sleds.push(sled);
    }

    // Stage the targets some sled lacks under their sha names. 64 char
    // names stay under the ustar limit illumos tar silently truncates at,
    // and the pushed file lands directly under its store key.
    let needed: Vec<&String> = targets
        .keys()
        .filter(|sha| sleds.iter().any(|s| !s.have.contains(*sha)))
        .collect();
    let staging = Staging(
        staging_base().join(format!("voxel-repo-seed-{}", std::process::id())),
    );
    fs::create_dir_all(&staging.0)
        .with_context(|| format!("mkdir {}", staging.0))?;
    let mut staged_bytes = 0;
    for sha in &needed {
        staged_bytes += stage_member(repo, &targets[*sha], &staging.0, sha)?;
    }
    eprintln!(
        "[voxel] staged {} artifacts ({} MiB)",
        needed.len(),
        staged_bytes >> 20
    );

    // One tar stream per sled carries everything it lacks.
    let mut pushed = 0;
    for sled in &mut sleds {
        let missing: Vec<String> = needed
            .iter()
            .filter(|sha| !sled.have.contains(**sha))
            .map(|s| (*s).clone())
            .collect();
        if missing.is_empty() {
            continue;
        }
        eprintln!("[voxel] {}: pushing {} artifacts", sled.name, missing.len());
        push_staged(&staging.0, &missing, &sled.ip, &sled.pools[0])
            .with_context(|| format!("push targets to {}", sled.name))?;
        pushed += missing.len();
        sled.have.extend(missing);
    }

    // Nexus-derived artifacts: present only where replication has reached.
    // Union everyone's stores and fill the gaps sled to sled, batched into
    // one stream per source/destination pair.
    let union: BTreeMap<String, usize> = {
        let mut m = BTreeMap::new();
        for (i, sled) in sleds.iter().enumerate() {
            for f in &sled.have {
                m.entry(f.clone()).or_insert(i);
            }
        }
        m
    };
    let mut synced = 0;
    for dst in 0..sleds.len() {
        let mut by_src: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (sha, src) in &union {
            if !sleds[dst].have.contains(sha) {
                by_src.entry(*src).or_default().push(sha.clone());
            }
        }
        for (src, shas) in by_src {
            eprintln!(
                "[voxel] {}: relaying {} derived artifacts from {}",
                sleds[dst].name,
                shas.len(),
                sleds[src].name
            );
            relay(&sleds[src], &sleds[dst], &shas).with_context(|| {
                format!("relay {} -> {}", sleds[src].name, sleds[dst].name)
            })?;
            synced += shas.len();
            let dst = &mut sleds[dst];
            dst.have.extend(shas);
        }
    }

    // Mirror the first pool's store onto each sled's remaining pools.
    for sled in &sleds {
        for pool in &sled.pools[1..] {
            let cmd = format!(
                "for f in {}/*; do [ -f \"$f\" ] && cp -n \"$f\" {}/; done; true",
                sled.pools[0], pool
            );
            ssh_status(&sled.ip, &cmd, Stdio::null())
                .with_context(|| format!("mirror stores on {}", sled.name))?;
        }
        eprintln!(
            "[voxel] {}: {} artifacts across {} stores",
            sled.name,
            sled.have.len(),
            sled.pools.len()
        );
    }
    println!(
        "seeded {pushed} repo artifacts, cross-synced {synced} derived ones"
    );
    Ok(())
}

/// Multi-GiB staging must not land on tmpfs (/tmp on illumos is swap
/// backed); prefer the disk backed /var/tmp.
fn staging_base() -> Utf8PathBuf {
    let var_tmp = Utf8Path::new("/var/tmp");
    if var_tmp.is_dir() { var_tmp.to_owned() } else { crate::util::temp_dir() }
}

/// One connection per sled: the store dirs, then the first store's files.
fn discover_stores(name: &str, ip: &str) -> Result<Sled> {
    let out = crate::net::ssh_output(
        ip,
        "ls -d /pool/int/*/update 2>/dev/null; echo ---; \
         cd \"$(ls -d /pool/int/*/update 2>/dev/null | head -1)\" \
         2>/dev/null && ls",
    )
    .with_context(|| format!("{name}: list artifact stores"))?;
    let (pools_raw, files_raw) =
        out.split_once("---").unwrap_or((out.as_str(), ""));
    let pools: Vec<String> = pools_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if pools.is_empty() {
        bail!("{name}: no artifact stores under /pool/int");
    }
    // The store keeps a tmp/ staging dir; everything else is a sha256
    // named file.
    let have = files_raw
        .lines()
        .map(str::trim)
        .filter(|l| l.len() == 64)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    Ok(Sled { name: name.to_string(), ip: ip.to_string(), pools, have })
}

/// Stream one zip member into the staging dir under its sha name,
/// returning its size.
fn stage_member(
    repo: &Utf8Path,
    member: &str,
    staging: &Utf8Path,
    sha: &str,
) -> Result<u64> {
    let mut unzip = Command::new("unzip")
        .args(["-p", repo.as_str(), member])
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn unzip -p")?;
    let mut out = unzip.stdout.take().context("unzip stdout")?;
    let dest = staging.join(sha);
    let mut file =
        fs::File::create(&dest).with_context(|| format!("create {dest}"))?;
    let n = std::io::copy(&mut out, &mut file)
        .with_context(|| format!("stage {member}"))?;
    let status = unzip.wait().context("wait for unzip")?;
    if !status.success() {
        bail!("unzip -p {member} exited with {status}");
    }
    Ok(n)
}

/// An ssh command with voxel's usual empty-root-password access.
fn ssh_base(ip: &str) -> Result<Command> {
    let askpass = ensure_askpass().context("ssh askpass helper")?;
    let mut c = Command::new("ssh");
    c.env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .args(EPHEMERAL_HOST_OPTS)
        .args(PASSWORD_AUTH_OPTS)
        .arg(format!("root@{ip}"));
    Ok(c)
}

/// ssh to the sled with a command whose stdin is `input`, gating on exit.
fn ssh_status(ip: &str, remote: &str, input: Stdio) -> Result<()> {
    let status = ssh_base(ip)?
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(remote)
        .status()
        .context("run ssh")?;
    if !status.success() {
        bail!("remote command on {ip} failed");
    }
    Ok(())
}

/// Push staged files to one sled's store as a single tar stream. The
/// archive is built in process (plain ustar, which illumos tar reads).
fn push_staged(
    staging: &Utf8Path,
    shas: &[String],
    ip: &str,
    pool: &str,
) -> Result<()> {
    let mut recv = ssh_base(ip)?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(format!("cd {pool} && tar xf -"))
        .spawn()
        .context("spawn receiving ssh")?;
    let stdin = recv.stdin.take().context("ssh stdin")?;
    drop(write_store_tar(staging, shas, stdin)?);
    let status = recv.wait().context("wait for receiving ssh")?;
    if !status.success() {
        bail!("store write on {ip} failed");
    }
    Ok(())
}

/// Write the staged files to `out` as a ustar archive, entries named by sha.
fn write_store_tar<W: std::io::Write>(
    staging: &Utf8Path,
    shas: &[String],
    out: W,
) -> Result<W> {
    let mut tarb = tar::Builder::new(out);
    for sha in shas {
        let path = staging.join(sha.as_str());
        let mut file =
            fs::File::open(&path).with_context(|| format!("open {path}"))?;
        let len =
            file.metadata().with_context(|| format!("stat {path}"))?.len();
        let mut h = tar::Header::new_ustar();
        h.set_size(len);
        h.set_mode(0o644);
        h.set_mtime(0);
        tarb.append_data(&mut h, sha, &mut file)
            .with_context(|| format!("append {sha}"))?;
    }
    tarb.into_inner().context("finish tar stream")
}

/// Copy store files from a sled that has them to one that lacks them as a
/// single tar stream relayed through this host.
fn relay(src: &Sled, dst: &Sled, shas: &[String]) -> Result<()> {
    let mut source = ssh_base(&src.ip)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .arg(format!("cd {} && tar cf - {}", src.pools[0], shas.join(" ")))
        .spawn()
        .context("spawn source ssh")?;
    let out = source.stdout.take().context("source ssh stdout")?;
    let result = ssh_status(
        &dst.ip,
        &format!("cd {} && tar xf -", dst.pools[0]),
        Stdio::from(out),
    );
    let src_status = source.wait().context("wait for source ssh")?;
    result?;
    if !src_status.success() {
        bail!("store read on {} failed", src.ip);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // illumos tar rejects GNU format archives and mishandles names over
    // 100 chars; prove the system tar reads our in-process ustar stream.
    #[test]
    fn system_tar_reads_store_stream() {
        let base = Utf8PathBuf::try_from(std::env::temp_dir()).unwrap();
        let dir = Staging(
            base.join(format!("voxel-tar-interop-{}", std::process::id())),
        );
        let staging = dir.0.join("staging");
        let extract = dir.0.join("extract");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&extract).unwrap();
        let sha = "ab".repeat(32);
        let body: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        fs::write(staging.join(&sha), &body).unwrap();
        let tarball = dir.0.join("stream.tar");
        let out = fs::File::create(&tarball).unwrap();
        write_store_tar(&staging, std::slice::from_ref(&sha), out).unwrap();
        let status = Command::new("tar")
            .args(["xf", tarball.as_str()])
            .current_dir(&extract)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read(extract.join(&sha)).unwrap(), body);
    }
}
