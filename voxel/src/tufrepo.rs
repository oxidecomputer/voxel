//! Read a TUF repo zip as an image source: the control plane zones, the
//! measurement corpus, the host OS phase 2 payload, and the omicron commit
//! the repo was built from. Targets are streamed out with `unzip -p`; only
//! the index is held in memory. Composites are GNU tar format, which illumos
//! tar rejects, so streams are unpacked in process.

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::fs;
use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};

/// `BootImageHeader` magic + fixed size (nexus_sled_agent_shared); the phase 2
/// artifact is this header followed by a raw ZFS pool image.
const BOOT_IMAGE_MAGIC: u32 = 0x1deb0075;
const BOOT_IMAGE_HEADER_SIZE: usize = 4096;

type MemberArchive = tar::Archive<flate2::read::GzDecoder<ChildStdout>>;

/// A parsed TUF repo zip (`tufaceous` v1 layout: `repo/targets/<sha>.<name>`).
pub(crate) struct TufRepoSource {
    pub path: Utf8PathBuf,
    /// Repo system version, e.g. `23.0.0-0.ci+git2e55f4ddac2`.
    pub system_version: String,
    /// Short omicron sha from the system version's `+git` suffix.
    pub commit: String,
    /// Zip member holding the composite control plane tarball.
    control_plane: String,
    /// Zip member holding the composite host OS tarball.
    host: String,
    /// Zip members holding measurement corpus artifacts, with their target
    /// names (member basename minus the sha prefix).
    corpus: Vec<(String, String)>,
}

impl TufRepoSource {
    pub(crate) fn load(path: &Utf8Path) -> Result<Self> {
        if !path.exists() {
            bail!("TUF repo {path} not found");
        }
        let members = zip_members(path)?;
        // Prefer the v1 index; every repo that carries v2 carries v1 too.
        let index = members
            .iter()
            .find(|m| m.ends_with(".artifacts.json"))
            .or_else(|| {
                members.iter().find(|m| m.ends_with("artifacts-v2.json"))
            })
            .with_context(|| {
                format!("{path} has no artifacts index under repo/targets/")
            })?;
        let raw = zip_read(path, index)?;
        let json: serde_json::Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parse {index}"))?;
        let system_version = json
            .get("system_version")
            .and_then(|v| v.as_str())
            .context("artifacts index has no system_version")?
            .to_string();
        let commit = commit_of_version(&system_version)?;

        let artifacts = json
            .get("artifacts")
            .and_then(|v| v.as_array())
            .context("artifacts index has no artifacts array")?;
        let mut control_plane = None;
        let mut host = None;
        let mut corpus = Vec::new();
        for a in artifacts {
            let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let Some(target) = a.get("target").and_then(|v| v.as_str()) else {
                continue;
            };
            // The index names targets without the sha prefix the zip uses.
            let member = members
                .iter()
                .find(|m| m.ends_with(&format!(".{target}")))
                .with_context(|| {
                    format!("target {target} listed but absent from {path}")
                })?;
            let unique = |slot: &mut Option<String>| {
                if slot.replace(member.clone()).is_some() {
                    bail!("{path} has more than one {kind} target");
                }
                Ok(())
            };
            match kind {
                "control_plane" => unique(&mut control_plane)?,
                "host" => unique(&mut host)?,
                "measurement_corpus" => {
                    corpus.push((member.clone(), target.to_string()));
                }
                _ => {}
            }
        }
        let need = |slot: Option<String>, kind: &str| {
            slot.with_context(|| format!("{path} has no {kind} target"))
        };
        Ok(Self {
            path: path.to_owned(),
            system_version,
            commit,
            control_plane: need(control_plane, "control_plane")?,
            host: need(host, "host")?,
            corpus,
        })
    }

    /// Unpack the composite's `zones/*.tar.gz` into `dir` under their service
    /// names, which are also their `/opt/oxide` and TUF artifact names.
    pub(crate) fn extract_zones_into(
        &self,
        dir: &Utf8Path,
    ) -> Result<Vec<String>> {
        fs::create_dir_all(dir).with_context(|| format!("mkdir {dir}"))?;
        let (mut child, mut archive) = self.member_tar(&self.control_plane)?;
        let mut names = Vec::new();
        for entry in
            archive.entries().context("read control plane composite entries")?
        {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let Some(name) = path
                .strip_prefix("zones")
                .ok()
                .and_then(|p| p.to_str())
                .filter(|n| n.ends_with(".tar.gz"))
                .map(str::to_string)
            else {
                continue;
            };
            entry
                .unpack(dir.join(&name))
                .with_context(|| format!("unpack {name} into {dir}"))?;
            names.push(name);
        }
        wait_ok(&mut child, &self.control_plane)?;
        if names.is_empty() {
            bail!("control plane composite in {} carried no zones", self.path);
        }
        names.sort();
        Ok(names)
    }

    /// Write each measurement corpus artifact, as is, into `dir` under its
    /// target name. The install dataset must carry the exact artifact bytes
    /// for the sled's measurement manifest to hash match the repo.
    pub(crate) fn extract_corpus_into(&self, dir: &Utf8Path) -> Result<usize> {
        if self.corpus.is_empty() {
            bail!("{} has no measurement_corpus targets", self.path);
        }
        fs::create_dir_all(dir).with_context(|| format!("mkdir {dir}"))?;
        for (member, target) in &self.corpus {
            let bytes = zip_read(&self.path, member)?;
            fs::write(dir.join(target), bytes)
                .with_context(|| format!("write corpus {target}"))?;
        }
        Ok(self.corpus.len())
    }

    /// Write the host artifact's phase 2 ZFS pool image to `dest`, stripping
    /// the 4096 byte boot image header (verified by magic). The result is a
    /// lofi mountable pool holding the host OS root, whose /opt/oxide carries
    /// the global zone software.
    pub(crate) fn extract_host_phase2_payload(
        &self,
        dest: &Utf8Path,
    ) -> Result<u64> {
        let (mut child, mut archive) = self.member_tar(&self.host)?;
        let mut written = None;
        for entry in archive.entries().context("read host composite entries")? {
            let mut entry = entry?;
            let is_zfs =
                entry.path()?.file_name().is_some_and(|n| n == "zfs.img");
            if !is_zfs {
                continue;
            }
            let mut header = [0u8; BOOT_IMAGE_HEADER_SIZE];
            entry.read_exact(&mut header).context("read boot image header")?;
            let magic = u32::from_le_bytes(header[..4].try_into().unwrap());
            if magic != BOOT_IMAGE_MAGIC {
                bail!(
                    "zfs.img in {} has boot image magic {magic:#x}, \
                     expected {BOOT_IMAGE_MAGIC:#x}",
                    self.path
                );
            }
            let mut out = fs::File::create(dest)
                .with_context(|| format!("create {dest}"))?;
            let n = std::io::copy(&mut entry, &mut out)
                .with_context(|| format!("write phase 2 payload to {dest}"))?;
            written = Some(n);
            break;
        }
        // Entries can follow zfs.img; drop the reader so unzip sees EPIPE
        // instead of blocking on a full pipe under wait().
        drop(archive);
        wait_ok(&mut child, &self.host)?;
        written.with_context(|| {
            format!("host artifact in {} carries no zfs.img", self.path)
        })
    }

    /// Every `repo/targets/<sha256>.<name>` member with its sha.
    pub(crate) fn target_members(&self) -> Result<Vec<(String, String)>> {
        let mut v = Vec::new();
        for m in zip_members(&self.path)? {
            let Some(base) = m.strip_prefix("repo/targets/") else {
                continue;
            };
            let Some((sha, _)) = base.split_once('.') else { continue };
            if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                v.push((sha.to_string(), m));
            }
        }
        Ok(v)
    }

    /// Stream one composite member as a tar archive.
    fn member_tar(&self, member: &str) -> Result<(Child, MemberArchive)> {
        let mut child = Command::new("unzip")
            .args(["-p", self.path.as_str(), member])
            .stdout(Stdio::piped())
            .spawn()
            .context("spawn unzip -p")?;
        let stdout = child.stdout.take().context("unzip stdout")?;
        let archive = tar::Archive::new(flate2::read::GzDecoder::new(stdout));
        Ok((child, archive))
    }
}

fn wait_ok(child: &mut Child, member: &str) -> Result<()> {
    let status = child.wait().context("wait for unzip")?;
    // The tar reader stops at the archive's logical end; unzip may still be
    // writing zip padding and exit on EPIPE, which is not a failure here.
    if !status.success() && status.code().is_some_and(|c| c != 141) {
        bail!("unzip -p {member} exited with {status}");
    }
    Ok(())
}

/// `unzip -Z1`: one member path per line.
fn zip_members(path: &Utf8Path) -> Result<Vec<String>> {
    let out = Command::new("unzip")
        .args(["-Z1", path.as_str()])
        .output()
        .context("run unzip -Z1")?;
    if !out.status.success() {
        bail!("unzip -Z1 {path} failed (not a zip?)");
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// `unzip -p`: stream one member.
fn zip_read(path: &Utf8Path, member: &str) -> Result<Vec<u8>> {
    let out = Command::new("unzip")
        .args(["-p", path.as_str(), member])
        .output()
        .with_context(|| format!("unzip -p {member}"))?;
    if !out.status.success() {
        bail!("unzip -p {path} {member} failed");
    }
    Ok(out.stdout)
}

/// Parse the short omicron sha out of `<semver>+git<sha>`.
fn commit_of_version(system_version: &str) -> Result<String> {
    let sha = system_version
        .rsplit_once("+git")
        .map(|(_, s)| s)
        .filter(|s| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .with_context(|| {
            format!(
                "system version {system_version} has no +git<sha> suffix; \
                 pass an explicit <COMMIT> to pin the omicron build"
            )
        })?;
    Ok(sha.to_string())
}
