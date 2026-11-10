// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Firmware for an emulated fleet, from the sources omicron pins for its own
//! releases. A source-built image gets the same SP, RoT and bootloader images
//! releng would ship, and `launch --emu` then runs the firmware paired with
//! the omicron commit.
//!
//! - permslip, Oxide's firmware signing service: `tools/permslip_staging`
//!   pins manifests by blake3 hash. RoT and bootloader images come from here,
//!   and SP images did too until omicron#10760.
//! - GitHub Releases: since omicron#10760, `tools/brussels/hubris-sp.sigstore.json`
//!   pins the SP release with GitHub's release attestation, which names the
//!   repository, the tag, and the sha256 of every file in the release. A
//!   checkout with that file gets its SP images from there.
//!
//! Every download is checked against a hash from the omicron checkout, so
//! the checkout is the root of trust either way. The attestation's Sigstore
//! signature is not verified here (omicron's releng does that); the file is
//! read as the pinned list of release hashes.

use crate::sp_host::{FETCH, download, fetch_text};
use crate::tufrepo::{
    BOOTLOADER_VARIANT, GIMLET_BOARD, ROT_VARIANT, SIDECAR_BOARD,
};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::collections::BTreeMap;

const BASE_URL: &str = "https://permslip-staging.corp.oxide.computer";
/// omicron's list of pinned manifests: `<blake3> <name>` per line.
pub(crate) const PIN_FILE: &str = "tools/permslip_staging";
/// omicron's pin of the SP release on GitHub (omicron#10760 onwards).
pub(crate) const SP_RELEASE_FILE: &str =
    "tools/brussels/hubris-sp.sigstore.json";
/// The `omicron-metadata.json` format the SP release lists boards in; the
/// version omicron's releng expects.
const SP_RELEASE_METADATA_VERSION: u64 = 4;

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
/// `launch --emu` reads. With an SP release pinned, the two SP entries come
/// from it by board name instead.
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
/// keyed by the pin files' content. A complete directory is reused.
pub(crate) fn fetch_firmware(
    root: &Utf8Path,
    cache: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let pin_file = root.join(PIN_FILE);
    let pins = std::fs::read_to_string(&pin_file)
        .with_context(|| format!("read {pin_file}"))?;
    let release_file = root.join(SP_RELEASE_FILE);
    let release = match std::fs::read_to_string(&release_file) {
        Ok(text) => Some(
            SpRelease::parse(&text)
                .with_context(|| format!("parse {release_file}"))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("read {release_file}"));
        }
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(pins.as_bytes());
    if let Some(r) = &release {
        hasher.update(r.tag.as_bytes());
    }
    let key: String = hasher.finalize().to_hex()[..12].to_string();
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
        if let Some(release) = &release
            && kind.ends_with("_sp")
        {
            release
                .fetch_board(name, &dir.join(file))
                .with_context(|| format!("SP {name} from {release_file}"))?;
            continue;
        }
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

/// An SP release pinned by omicron: the repository and tag it names, and the
/// sha256 of each file in it, all from GitHub's release attestation.
struct SpRelease {
    repo: String,
    tag: String,
    sha256: BTreeMap<String, String>,
}

/// The parts of a Sigstore bundle's in-toto statement read here.
#[derive(Deserialize)]
struct Bundle {
    #[serde(rename = "dsseEnvelope")]
    dsse_envelope: Envelope,
}

#[derive(Deserialize)]
struct Envelope {
    payload: String,
}

#[derive(Deserialize)]
struct Statement {
    predicate: ReleasePredicate,
    subject: Vec<Subject>,
}

#[derive(Deserialize)]
struct ReleasePredicate {
    repository: String,
    tag: String,
}

#[derive(Deserialize)]
struct Subject {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    digest: BTreeMap<String, String>,
}

/// The release's `omicron-metadata.json`, which maps board names to files.
#[derive(Deserialize)]
struct ReleaseMetadata {
    metadata_version: u64,
    boards: BTreeMap<String, Board>,
}

#[derive(Deserialize)]
struct Board {
    kind: String,
    #[serde(default)]
    archive: Option<String>,
}

impl SpRelease {
    fn parse(text: &str) -> Result<Self> {
        let bundle: Bundle =
            serde_json::from_str(text).context("not a Sigstore bundle")?;
        let payload = base64::engine::general_purpose::STANDARD
            .decode(bundle.dsse_envelope.payload.trim())
            .context("decode the DSSE payload")?;
        let statement: Statement = serde_json::from_slice(&payload)
            .context("not a release attestation")?;
        let sha256 = statement
            .subject
            .into_iter()
            .filter_map(|s| Some((s.name?, s.digest.get("sha256")?.clone())))
            .collect();
        Ok(Self {
            repo: statement.predicate.repository,
            tag: statement.predicate.tag,
            sha256,
        })
    }

    fn url(&self, name: &str) -> String {
        format!(
            "https://github.com/{}/releases/download/{}/{name}",
            self.repo, self.tag
        )
    }

    /// Download one release file to `dest`, checked against the pinned
    /// sha256. `dest` appears only once verified.
    fn download_verified(&self, name: &str, dest: &Utf8Path) -> Result<()> {
        let want = self.sha256.get(name).with_context(|| {
            format!("the attestation for {} has no {name}", self.tag)
        })?;
        let url = self.url(name);
        let part = Utf8PathBuf::from(format!("{dest}.part"));
        let got = download(&url, &part, FETCH)?;
        if &got != want {
            let _ = std::fs::remove_file(&part);
            bail!("{url}: sha256 {got}, expected {want}");
        }
        std::fs::rename(&part, dest)
            .with_context(|| format!("move {part} into place"))
    }

    /// Fetch the SP image for `board` into `dest`.
    fn fetch_board(&self, board: &str, dest: &Utf8Path) -> Result<()> {
        let meta_path = Utf8PathBuf::from(format!("{dest}.metadata.json"));
        self.download_verified("omicron-metadata.json", &meta_path)?;
        let text = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("read {meta_path}"))?;
        let _ = std::fs::remove_file(&meta_path);
        let meta: ReleaseMetadata = serde_json::from_str(&text)
            .context("parse omicron-metadata.json")?;
        if meta.metadata_version != SP_RELEASE_METADATA_VERSION {
            bail!(
                "omicron-metadata.json is version {}, voxel reads version {}",
                meta.metadata_version,
                SP_RELEASE_METADATA_VERSION
            );
        }
        let archive = meta
            .boards
            .get(board)
            .filter(|b| b.kind == "sp")
            .and_then(|b| b.archive.as_deref())
            .with_context(|| format!("{} has no SP board {board}", self.tag))?;
        eprintln!("[voxel] fetching {board} {} -> {dest}", self.tag);
        self.download_verified(archive, dest)
    }
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

    #[test]
    fn sp_release_reads_repo_tag_and_hashes_from_the_attestation() {
        // The shape of omicron's tools/brussels/hubris-sp.sigstore.json:
        // a Sigstore bundle whose DSSE payload is an in-toto release
        // statement. Subjects without a name (the purl) are skipped.
        let statement = r#"{
            "predicateType": "https://in-toto.io/attestation/release/v0.2",
            "predicate": {
                "repository": "oxidecomputer/hubris",
                "tag": "all-sp-v1.86.0"
            },
            "subject": [
                {"uri": "pkg:github/oxidecomputer/hubris@all-sp-v1.86.0"},
                {"name": "omicron-metadata.json", "digest": {"sha256": "aa"}},
                {"name": "build-gimlet-c-image-default.zip",
                 "digest": {"sha256": "bb"}}
            ]
        }"#;
        let payload =
            base64::engine::general_purpose::STANDARD.encode(statement);
        let bundle = format!(
            r#"{{"mediaType": "x", "dsseEnvelope": {{"payload": "{payload}",
                "payloadType": "application/vnd.in-toto+json"}}}}"#
        );
        let r = SpRelease::parse(&bundle).unwrap();
        assert_eq!(r.repo, "oxidecomputer/hubris");
        assert_eq!(r.tag, "all-sp-v1.86.0");
        assert_eq!(r.sha256.len(), 2);
        assert_eq!(r.sha256["build-gimlet-c-image-default.zip"], "bb");
        assert_eq!(
            r.url("omicron-metadata.json"),
            "https://github.com/oxidecomputer/hubris/releases/download/\
             all-sp-v1.86.0/omicron-metadata.json"
        );
    }
}
