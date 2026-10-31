// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Firmware for an emulated fleet, from permslip, Oxide's firmware signing
//! service. omicron pins the signed hubris releases a build ships in
//! `tools/permslip_staging`, as blake3 hashes of permslip manifests, and
//! releng assembles TUF repos from that list. A source-built image gets the
//! same SP, RoT and bootloader images here. `launch --emu` then runs the
//! firmware paired with the omicron commit.

use crate::sp_host::{FETCH, fetch_text};
use crate::tufrepo::{
    BOOTLOADER_VARIANT, GIMLET_BOARD, ROT_VARIANT, SIDECAR_BOARD,
};
use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::collections::BTreeMap;

const BASE_URL: &str = "https://permslip-staging.corp.oxide.computer";
/// omicron's list of pinned manifests: `<blake3> <name>` per line.
pub(crate) const PIN_FILE: &str = "tools/permslip_staging";

/// A permslip manifest: artifacts by kind, each named by board or variant.
#[derive(Deserialize)]
struct Manifest {
    artifact: BTreeMap<String, Vec<Artifact>>,
}

#[derive(Deserialize)]
struct Artifact {
    name: String,
    source: Source,
}

/// Where an artifact's bytes are. A RoT ships both slots; launch flashes
/// slot A.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Source {
    File { hash: String },
    CompositeRot { archive_a: FileSource },
}

#[derive(Deserialize)]
struct FileSource {
    hash: String,
}

impl Source {
    /// The hash of the image launch flashes.
    fn image_hash(&self) -> &str {
        match self {
            Source::File { hash } => hash,
            Source::CompositeRot { archive_a } => &archive_a.hash,
        }
    }
}

/// The images an emulated fleet boots, as (manifest kind, artifact name,
/// file name). The file names are the layout `--from-tuf` extracts and
/// `launch --emu` reads.
fn wanted() -> [(&'static str, String, String); 4] {
    [
        ("gimlet_sp", GIMLET_BOARD.into(), format!("sp-{GIMLET_BOARD}.zip")),
        ("switch_sp", SIDECAR_BOARD.into(), format!("sp-{SIDECAR_BOARD}.zip")),
        ("gimlet_rot", ROT_VARIANT.into(), "rot-a.zip".into()),
        (
            "gimlet_rot_bootloader",
            format!("gimlet_rot_bootloader-{BOOTLOADER_VARIANT}"),
            "bootleby.zip".into(),
        ),
    ]
}

/// Fetch the fleet's firmware for the checkout at `root` into `cache/<key>`,
/// keyed by the pin file's content. A complete directory is reused.
pub(crate) fn fetch_firmware(
    root: &Utf8Path,
    cache: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let pin_file = root.join(PIN_FILE);
    let pins = std::fs::read_to_string(&pin_file)
        .with_context(|| format!("read {pin_file}"))?;
    let key: String = blake3::hash(pins.as_bytes()).to_hex()[..12].to_string();
    let dir = cache.join(key);
    let wanted = wanted();
    if wanted.iter().all(|(_, _, file)| dir.join(file).exists()) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {dir}"))?;

    let mut manifests = Vec::new();
    for line in pins.lines() {
        let mut fields = line.split_whitespace();
        let (Some(hash), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let text = fetch_artifact(hash)?;
        let manifest: Manifest = toml::from_str(
            &String::from_utf8(text)
                .with_context(|| format!("manifest {name} is not UTF-8"))?,
        )
        .with_context(|| format!("parse manifest {name}"))?;
        manifests.push(manifest);
    }
    for (kind, name, file) in &wanted {
        let source = manifests
            .iter()
            .flat_map(|m| m.artifact.get(*kind).into_iter().flatten())
            .find(|a| a.name == *name)
            .map(|a| &a.source)
            .with_context(|| format!("{pin_file} names no {kind} {name}"))?;
        eprintln!("[voxel] fetching {name} -> {file}");
        let data = fetch_artifact(source.image_hash())?;
        let part = dir.join(format!("{file}.part"));
        std::fs::write(&part, &data)
            .with_context(|| format!("write {part}"))?;
        std::fs::rename(&part, dir.join(file))
            .with_context(|| format!("move {part} into place"))?;
    }
    Ok(dir)
}

/// GET one content-addressed artifact and verify it. permslip returns the
/// bytes as a JSON array.
fn fetch_artifact(hash: &str) -> Result<Vec<u8>> {
    let url = format!("{BASE_URL}/artifact/{hash}");
    let body = fetch_text(&url, FETCH)?;
    let data: Vec<u8> =
        serde_json::from_str(&body).with_context(|| format!("decode {url}"))?;
    let got = blake3::hash(&data).to_hex();
    if got.as_str() != hash {
        bail!("{url}: blake3 {got}, expected {hash}");
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_picks_slot_a_for_a_rot_and_the_file_otherwise() {
        let text = r#"
[[artifact.gimlet_rot]]
name = "oxide-rot-1-selfsigned-bart"
version = "1.0.38"
[artifact.gimlet_rot.source]
kind = "composite-rot"
[artifact.gimlet_rot.source.archive_a]
kind = "file"
hash = "aa"
[artifact.gimlet_rot.source.archive_b]
kind = "file"
hash = "bb"

[[artifact.gimlet_sp]]
name = "gimlet-c"
version = "1.84.0"
[artifact.gimlet_sp.source]
kind = "file"
hash = "cc"
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        let rot = &m.artifact["gimlet_rot"][0];
        assert_eq!(rot.name, ROT_VARIANT);
        assert_eq!(rot.source.image_hash(), "aa");
        assert_eq!(m.artifact["gimlet_sp"][0].source.image_hash(), "cc");
    }
}
