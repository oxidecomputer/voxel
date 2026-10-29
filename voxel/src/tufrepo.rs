// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read a TUF repo zip as an image source: the control plane zones, the
//! measurement corpus, the host OS phase 2 payload, and the omicron commit
//! the repo was built from. Members are streamed out of the zip in process
//! (no unzip binary on the host); only the index is held in memory.
//! Composites are GNU tar format, which illumos tar rejects, so those streams
//! are unpacked in process too.
//!
//! releng emits two layouts of the same build. v1 wraps the zones, the host
//! OS and each RoT's slots in composite tarballs; v2 carries each as its own
//! target.

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use zip::ZipArchive;
use zip::read::ZipFile;

/// `BootImageHeader` magic + fixed size (nexus_sled_agent_shared); the phase 2
/// artifact is this header followed by a raw ZFS pool image.
const BOOT_IMAGE_MAGIC: u32 = 0x1deb0075;
const BOOT_IMAGE_HEADER_SIZE: usize = 4096;

type MemberArchive<'a> =
    tar::Archive<flate2::read::GzDecoder<ZipFile<'a, File>>>;

/// A parsed TUF repo zip, either layout.
pub(crate) struct TufRepoSource {
    pub path: Utf8PathBuf,
    /// Repo system version, e.g. `23.0.0-0.ci+git2e55f4ddac2`.
    pub system_version: String,
    /// Short omicron sha from the system version's `+git` suffix.
    pub commit: String,
    /// The members that differ between the two layouts.
    layout: Layout,
    /// sha256 of every target, by zip member. v1 encodes it in the member
    /// name, v2 carries it in the TUF metadata.
    hashes: BTreeMap<String, String>,
    /// Zip members holding measurement corpus artifacts, with their target
    /// names (member basename minus the sha prefix).
    corpus: Vec<(String, String)>,
    /// SP, RoT and RoT bootloader artifacts, by kind and name.
    firmware: Vec<Firmware>,
}

/// The members whose packaging differs by layout. The SPs, the bootloader
/// and the measurement corpus are plain targets in both.
enum Layout {
    /// Composite tarballs, addressed as `repo/targets/<sha>.<name>`.
    V1 { control_plane: String, host: String },
    /// Individual targets, addressed by their path under `repo/targets/`.
    V2 {
        /// Zone member and the file name to write it as.
        zones: Vec<(String, String)>,
        phase1: String,
        phase2: String,
        rot_a: String,
    },
}

/// One firmware artifact in the repo. A repo carries every board and keyset
/// variant, so `kind` alone does not identify an image; `name` picks the board
/// (`gimlet-c`) or the signing variant (`oxide-rot-1-selfsigned-bart`).
struct Firmware {
    kind: String,
    name: String,
    member: String,
}

/// The hubris boards a voxel emulated fleet presents.
const GIMLET_BOARD: &str = "gimlet-c";
const SIDECAR_BOARD: &str = "sidecar-c";
/// The RoT signing variant to take. "bart" is the hubris dev keyset: its SIGN
/// and the CMPA the emulated RoTs enforce match, so these images are drop-in.
/// The production and staging variants are signed for keysets only real
/// hardware holds.
const ROT_VARIANT: &str = "oxide-rot-1-selfsigned-bart";
/// The RoT bootloader (bootleby) variant, matching [`ROT_VARIANT`]'s keyset.
const BOOTLOADER_VARIANT: &str = "bart";
/// The bart keyset's root key table hash, which the emulated RoTs enforce
/// from their CMPA. v2 tags each RoT artifact with its keyset, so the image is
/// selected by this hash.
const BART_RKTH: &str =
    "84332ef8279df87fbb759dc3866cbc50cd246fbb5a64705a7e60ba86bf01c27d";
/// The host OS board and variant voxel takes from a v2 repo. v1 carries one
/// host composite holding both pieces.
const HOST_BOARD: &str = "gimlet";
const HOST_VARIANT: &str = "host";

/// Firmware extracted from a repo, as the paths `[sp]` wants.
pub(crate) struct FirmwareSet {
    pub gimlet: Utf8PathBuf,
    pub sidecar: Utf8PathBuf,
    pub rot_a: Utf8PathBuf,
    pub bootleby: Utf8PathBuf,
}

impl TufRepoSource {
    pub(crate) fn load(path: &Utf8Path) -> Result<Self> {
        if !path.exists() {
            bail!("TUF repo {path} not found");
        }
        let mut zip = open_zip(path)?;
        let members: Vec<String> =
            zip.file_names().map(str::to_string).collect();
        // A repo carries one index or the other; releng emits each layout
        // as its own zip.
        let Some(index) =
            members.iter().find(|m| m.ends_with(".artifacts.json")).cloned()
        else {
            return Self::load_v2(path, zip, members);
        };
        let json = read_json(&mut zip, &index)?;
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
        let mut firmware = Vec::new();
        for a in artifacts {
            let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let Some(target) = a.get("target").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
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
                "gimlet_sp"
                | "switch_sp"
                | "gimlet_rot"
                | "switch_rot"
                | "gimlet_rot_bootloader"
                | "switch_rot_bootloader" => {
                    firmware.push(Firmware {
                        kind: kind.to_string(),
                        name: name.to_string(),
                        member: member.clone(),
                    });
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
            layout: Layout::V1 {
                control_plane: need(control_plane, "control_plane")?,
                host: need(host, "host")?,
            },
            hashes: sha_keyed_members(&members),
            corpus,
            firmware,
        })
    }

    /// Parse a v2 index: entries carry their kind and board in `tags`, and
    /// name their target by path rather than by sha.
    fn load_v2(
        path: &Utf8Path,
        mut zip: ZipArchive<File>,
        members: Vec<String>,
    ) -> Result<Self> {
        let index = members
            .iter()
            .find(|m| m.ends_with("artifacts-v2.json"))
            .cloned()
            .with_context(|| {
                format!("{path} has no artifacts index under repo/targets/")
            })?;
        let json = read_json(&mut zip, &index)?;
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

        let mut zones = Vec::new();
        let mut phase1 = None;
        let mut phase2 = None;
        let mut rot_a = None;
        let mut corpus = Vec::new();
        let mut firmware = Vec::new();
        for a in artifacts {
            let Some(target) = a.get("target_name").and_then(|v| v.as_str())
            else {
                continue;
            };
            let tags = a.get("tags");
            let tag =
                |k: &str| tags.and_then(|t| t.get(k)).and_then(|v| v.as_str());
            let member = format!("repo/targets/{target}");
            if !members.contains(&member) {
                bail!("target {target} listed but absent from {path}");
            }
            let base = target.rsplit('/').next().unwrap_or(target);
            // A repo carries every board and keyset, so the tags that narrow
            // an artifact to this fleet are part of the match.
            match tag("kind").unwrap_or("") {
                "zone" => zones.push((member, base.to_string())),
                "measurement_corpus" => corpus.push((member, base.to_string())),
                "os_phase1" if tag("os_board") == Some(HOST_BOARD) => {
                    phase1 = Some(member);
                }
                "os_phase2" if tag("os_variant") == Some(HOST_VARIANT) => {
                    phase2 = Some(member);
                }
                "rot"
                    if tag("rot_rkth") == Some(BART_RKTH)
                        && tag("rot_slot") == Some("a") =>
                {
                    rot_a = Some(member);
                }
                "rot_bootloader" if tag("rot_rkth") == Some(BART_RKTH) => {
                    firmware.push(Firmware {
                        kind: "gimlet_rot_bootloader".to_string(),
                        name: format!(
                            "gimlet_rot_bootloader-{BOOTLOADER_VARIANT}"
                        ),
                        member,
                    });
                }
                "sp" => {
                    let board = tag("sp_board").unwrap_or("");
                    let kind = if board == GIMLET_BOARD {
                        "gimlet_sp"
                    } else if board == SIDECAR_BOARD {
                        "switch_sp"
                    } else {
                        continue;
                    };
                    firmware.push(Firmware {
                        kind: kind.to_string(),
                        name: board.to_string(),
                        member,
                    });
                }
                _ => {}
            }
        }
        let need = |slot: Option<String>, what: &str| {
            slot.with_context(|| format!("{path} has no {what} target"))
        };
        if zones.is_empty() {
            bail!("{path} has no zone targets");
        }
        Ok(Self {
            path: path.to_owned(),
            system_version,
            commit,
            layout: Layout::V2 {
                zones,
                phase1: need(phase1, "os_phase1")?,
                phase2: need(phase2, "os_phase2")?,
                rot_a: need(rot_a, "slot A RoT for the bart keyset")?,
            },
            hashes: v2_target_hashes(&mut zip)?,
            corpus,
            firmware,
        })
    }

    /// Unpack the composite's `zones/*.tar.gz` into `dir` under their service
    /// names, which are also their `/opt/oxide` and TUF artifact names.
    pub(crate) fn extract_zones_into(
        &self,
        dir: &Utf8Path,
    ) -> Result<Vec<String>> {
        fs::create_dir_all(dir).with_context(|| format!("mkdir {dir}"))?;
        let mut zip = self.open()?;
        let mut names = Vec::new();
        match &self.layout {
            Layout::V1 { control_plane, .. } => {
                let mut archive = member_tar(&mut zip, control_plane)?;
                for entry in archive
                    .entries()
                    .context("read control plane composite entries")?
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
                if names.is_empty() {
                    bail!(
                        "control plane composite in {} carried no zones",
                        self.path
                    );
                }
            }
            Layout::V2 { zones, .. } => {
                for (member, name) in zones {
                    extract_member(&mut zip, member, &dir.join(name))?;
                    names.push(name.clone());
                }
            }
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
        let mut zip = self.open()?;
        for (m, target) in &self.corpus {
            extract_member(&mut zip, m, &dir.join(target))?;
        }
        Ok(self.corpus.len())
    }

    /// Write the host artifact pieces a TUF image carries: the boot image
    /// (4096 byte header, verified by magic, plus the phase 2 ZFS image; the
    /// exact bytes installinator writes to a boot partition) and the gimlet
    /// phase 1 ROM. Returns their sizes.
    pub(crate) fn extract_host_artifacts(
        &self,
        boot_image_dest: &Utf8Path,
        phase1_dest: &Utf8Path,
    ) -> Result<(u64, u64)> {
        let mut zip = self.open()?;
        let host = match &self.layout {
            Layout::V1 { host, .. } => host,
            Layout::V2 { phase1, phase2, .. } => {
                let rom = extract_member(&mut zip, phase1, phase1_dest)?;
                let boot = extract_member(&mut zip, phase2, boot_image_dest)?;
                check_boot_image(boot_image_dest)?;
                return Ok((boot, rom));
            }
        };
        let mut archive = member_tar(&mut zip, host)?;
        let mut boot_image = None;
        let mut phase1 = None;
        for entry in archive.entries().context("read host composite entries")? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let name = path.file_name().and_then(|n| n.to_str());
            match name {
                Some("zfs.img") => {
                    let mut header = [0u8; BOOT_IMAGE_HEADER_SIZE];
                    entry
                        .read_exact(&mut header)
                        .context("read boot image header")?;
                    let magic =
                        u32::from_le_bytes(header[..4].try_into().unwrap());
                    if magic != BOOT_IMAGE_MAGIC {
                        bail!(
                            "zfs.img in {} has boot image magic {magic:#x}, \
                             expected {BOOT_IMAGE_MAGIC:#x}",
                            self.path
                        );
                    }
                    let mut out = fs::File::create(boot_image_dest)
                        .with_context(|| format!("create {boot_image_dest}"))?;
                    out.write_all(&header)
                        .context("write boot image header")?;
                    let n = std::io::copy(&mut entry, &mut out).with_context(
                        || format!("write boot image to {boot_image_dest}"),
                    )?;
                    boot_image = Some(n + BOOT_IMAGE_HEADER_SIZE as u64);
                }
                Some("gimlet.rom") => {
                    let mut out = fs::File::create(phase1_dest)
                        .with_context(|| format!("create {phase1_dest}"))?;
                    let n = std::io::copy(&mut entry, &mut out).with_context(
                        || format!("write phase 1 rom to {phase1_dest}"),
                    )?;
                    phase1 = Some(n);
                }
                _ => continue,
            }
            if boot_image.is_some() && phase1.is_some() {
                break;
            }
        }
        let need = |v: Option<u64>, what: &str| {
            v.with_context(|| {
                format!("host artifact in {} carries no {what}", self.path)
            })
        };
        Ok((need(boot_image, "zfs.img")?, need(phase1, "gimlet.rom")?))
    }

    /// Extract the firmware a voxel emulated fleet boots into `dir`: the
    /// gimlet and sidecar SP images, the RoT slot A image, and the RoT
    /// bootloader. SP and bootloader artifacts are hubris zips carrying a
    /// `.tar.gz` name; RoT artifacts really are gzipped tarballs, holding the
    /// per-slot archives.
    pub(crate) fn extract_firmware_into(
        &self,
        dir: &Utf8Path,
    ) -> Result<FirmwareSet> {
        fs::create_dir_all(dir).with_context(|| format!("mkdir {dir}"))?;
        let find = |kind: &str, name: &str| -> Result<&str> {
            self.firmware
                .iter()
                .find(|f| f.kind == kind && f.name == name)
                .map(|f| f.member.as_str())
                .with_context(|| {
                    format!("{} has no {kind} named {name}", self.path)
                })
        };

        let mut zip = self.open()?;
        let gimlet = dir.join(format!("sp-{GIMLET_BOARD}.zip"));
        extract_member(&mut zip, find("gimlet_sp", GIMLET_BOARD)?, &gimlet)?;
        let sidecar = dir.join(format!("sp-{SIDECAR_BOARD}.zip"));
        extract_member(&mut zip, find("switch_sp", SIDECAR_BOARD)?, &sidecar)?;

        let bootleby = dir.join("bootleby.zip");
        let boot_name = format!("gimlet_rot_bootloader-{BOOTLOADER_VARIANT}");
        extract_member(
            &mut zip,
            find("gimlet_rot_bootloader", &boot_name)?,
            &bootleby,
        )?;

        // The RoT composite holds archive-a.zip and archive-b.zip; slot A is
        // what launch flashes, and bootleby verifies it. v2 tags the slots
        // apart, so load already picked slot A.
        if let Layout::V2 { rot_a, .. } = &self.layout {
            let dest = dir.join("rot-a.zip");
            extract_member(&mut zip, rot_a, &dest)?;
            return Ok(FirmwareSet { gimlet, sidecar, rot_a: dest, bootleby });
        }
        let rot_member = find("gimlet_rot", ROT_VARIANT)?;
        let mut archive = member_tar(&mut zip, rot_member)?;
        let mut rot_a = None;
        for entry in archive.entries().context("read RoT composite entries")? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let is_a = path.file_name().is_some_and(|n| n == "archive-a.zip");
            if !is_a {
                continue;
            }
            let dest = dir.join("rot-a.zip");
            entry
                .unpack(&dest)
                .with_context(|| format!("unpack RoT slot A into {dir}"))?;
            rot_a = Some(dest);
            break;
        }
        let rot_a = rot_a.with_context(|| {
            format!("{ROT_VARIANT} in {} carries no archive-a.zip", self.path)
        })?;

        Ok(FirmwareSet { gimlet, sidecar, rot_a, bootleby })
    }

    /// The host artifact's sha256, parsed from its target member name. Cache
    /// keys use it rather than the commit: releng rebuilds of the same commit
    /// produce different artifacts.
    pub(crate) fn host_sha(&self) -> &str {
        let member = match &self.layout {
            Layout::V1 { host, .. } => host,
            Layout::V2 { phase2, .. } => phase2,
        };
        self.hashes.get(member).map(String::as_str).unwrap_or(&self.commit)
    }

    /// Every target member with its sha.
    pub(crate) fn target_members(&self) -> Vec<(String, String)> {
        self.hashes.iter().map(|(m, sha)| (sha.clone(), m.clone())).collect()
    }

    fn open(&self) -> Result<ZipArchive<File>> {
        open_zip(&self.path)
    }
}

/// Confirm a phase 2 artifact carries the boot image header.
fn check_boot_image(path: &Utf8Path) -> Result<()> {
    let mut f = File::open(path).with_context(|| format!("open {path}"))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)
        .with_context(|| format!("read boot image header from {path}"))?;
    let magic = u32::from_le_bytes(magic);
    if magic != BOOT_IMAGE_MAGIC {
        bail!(
            "{path} has boot image magic {magic:#x}, \
             expected {BOOT_IMAGE_MAGIC:#x}"
        );
    }
    Ok(())
}

/// Read and parse a zip member as JSON.
fn read_json(
    zip: &mut ZipArchive<File>,
    name: &str,
) -> Result<serde_json::Value> {
    serde_json::from_reader(member(zip, name)?)
        .with_context(|| format!("parse {name}"))
}

/// v1 members are `repo/targets/<sha256>.<name>`, so their sha is in the path.
fn sha_keyed_members(members: &[String]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for m in members {
        let Some(base) = m.strip_prefix("repo/targets/") else { continue };
        let Some((sha, _)) = base.split_once('.') else { continue };
        if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            map.insert(m.clone(), sha.to_string());
        }
    }
    map
}

/// v2 members are named by path, so their hashes come from the TUF metadata.
fn v2_target_hashes(
    zip: &mut ZipArchive<File>,
) -> Result<BTreeMap<String, String>> {
    let json = read_json(zip, "repo/metadata/targets.json")?;
    let targets = json
        .get("signed")
        .and_then(|s| s.get("targets"))
        .and_then(|t| t.as_object())
        .context("targets.json has no signed.targets")?;
    let mut map = BTreeMap::new();
    for (target, meta) in targets {
        let Some(sha) = meta.pointer("/hashes/sha256").and_then(|v| v.as_str())
        else {
            continue;
        };
        map.insert(format!("repo/targets/{target}"), sha.to_string());
    }
    Ok(map)
}

/// Open a repo zip; the central directory is parsed here, members are read
/// on demand.
fn open_zip(path: &Utf8Path) -> Result<ZipArchive<File>> {
    let file = File::open(path).with_context(|| format!("open {path}"))?;
    ZipArchive::new(file).with_context(|| format!("read {path} (not a zip?)"))
}

/// A streaming reader over one member.
fn member<'a>(
    zip: &'a mut ZipArchive<File>,
    name: &str,
) -> Result<ZipFile<'a, File>> {
    zip.by_name(name).with_context(|| format!("zip member {name}"))
}

/// Stream one composite member as a tar archive.
fn member_tar<'a>(
    zip: &'a mut ZipArchive<File>,
    name: &str,
) -> Result<MemberArchive<'a>> {
    let file = member(zip, name)?;
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(file)))
}

/// Stream one member to `dest`, as is, returning its size.
fn extract_member(
    zip: &mut ZipArchive<File>,
    name: &str,
    dest: &Utf8Path,
) -> Result<u64> {
    let mut src = member(zip, name)?;
    let mut out =
        File::create(dest).with_context(|| format!("create {dest}"))?;
    std::io::copy(&mut src, &mut out)
        .with_context(|| format!("write {name} to {dest}"))
}

/// Copy one member of the repo zip at `path` to `dest`, returning its size.
pub(crate) fn extract_from(
    path: &Utf8Path,
    name: &str,
    dest: &Utf8Path,
) -> Result<u64> {
    extract_member(&mut open_zip(path)?, name, dest)
}

/// Copy `boot_image` minus its 4096 byte header to `dest`: the raw ZFS pool
/// image, lofi mountable for the global zone software lift.
pub(crate) fn strip_boot_image_header(
    boot_image: &Utf8Path,
    dest: &Utf8Path,
) -> Result<u64> {
    let mut src = fs::File::open(boot_image)
        .with_context(|| format!("open {boot_image}"))?;
    src.seek(SeekFrom::Start(BOOT_IMAGE_HEADER_SIZE as u64))
        .context("seek past boot image header")?;
    let mut out =
        fs::File::create(dest).with_context(|| format!("create {dest}"))?;
    std::io::copy(&mut src, &mut out)
        .with_context(|| format!("write phase 2 payload to {dest}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Extraction against a real repo. Ignored by default (needs a multi-GiB
    /// zip); run with `VOXEL_TEST_REPO=<repo.zip> cargo test -p voxel
    /// firmware_from_repo -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn firmware_from_repo() {
        let Ok(repo) = std::env::var("VOXEL_TEST_REPO") else {
            panic!("set VOXEL_TEST_REPO to a repo.zip");
        };
        let repo = Utf8PathBuf::from(repo);
        let t = TufRepoSource::load(&repo).expect("loaded repo");
        let dir = Utf8PathBuf::from(format!(
            "/var/tmp/voxel-fw-test-{}",
            std::process::id()
        ));
        let fw = t.extract_firmware_into(&dir).expect("extracted firmware");

        // Every one of these is a hubris archive, i.e. a zip: SP and
        // bootloader artifacts carry a .tar.gz name but are zips, and the RoT
        // slot archives come out of a real tarball.
        for path in [&fw.gimlet, &fw.sidecar, &fw.rot_a, &fw.bootleby] {
            let bytes = std::fs::read(path).expect("read extracted firmware");
            assert!(bytes.len() > 1024, "{path} is implausibly small");
            assert_eq!(&bytes[..2], b"PK", "{path} is not a zip");
            println!("{} {} bytes", path, bytes.len());
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
