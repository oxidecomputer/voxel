// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The revisions voxel builds against, read from the embedded `pins.toml`.

use anyhow::{Context, ensure};

/// The repo's `pins.toml`, embedded so a shipped voxel binary carries its own
/// pins. Each entry names a buildomat-published binary and the rev to fetch.
const PINS: &str = include_str!("../../pins.toml");

/// A single pins.toml entry.
pub(crate) struct Pin {
    pub(crate) repo: String,
    pub(crate) series: String,
    pub(crate) rev: String,
    pub(crate) artifact: String,
}

fn ensure_full_sha(name: &str, rev: &str) -> anyhow::Result<()> {
    ensure!(
        rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
        "pins.toml [{name}] rev is not a full git sha: {rev}"
    );
    Ok(())
}

/// Look up one entry's rev in the embedded pins.toml.
///
/// Entries that name no buildomat artifact, such as sidecar-lite, carry only
/// a rev.
pub(crate) fn pin_rev(name: &str) -> anyhow::Result<String> {
    let doc: toml::Table = PINS.parse().context("parse embedded pins.toml")?;
    let rev = doc
        .get(name)
        .and_then(|v| v.as_table())
        .with_context(|| format!("pins.toml has no [{name}]"))?
        .get("rev")
        .and_then(|v| v.as_str())
        .with_context(|| format!("pins.toml [{name}] missing rev"))?;
    ensure_full_sha(name, rev)?;
    Ok(rev.to_string())
}

/// Look up one entry of the embedded pins.toml.
pub(crate) fn pin(name: &str) -> anyhow::Result<Pin> {
    let doc: toml::Table = PINS.parse().context("parse embedded pins.toml")?;
    let entry = doc
        .get(name)
        .and_then(|v| v.as_table())
        .with_context(|| format!("pins.toml has no [{name}]"))?;
    let field = |key: &str| -> anyhow::Result<String> {
        entry
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .with_context(|| format!("pins.toml [{name}] missing {key}"))
    };
    Ok(Pin {
        repo: field("repo")?,
        series: field("series")?,
        rev: pin_rev(name)?,
        artifact: field("artifact")?,
    })
}

#[cfg(test)]
mod tests {
    // Every pins.toml entry must parse and carry a full git sha, so a bad
    // pin fails in CI rather than at fetch time on a user's box.
    #[test]
    fn pins_parse() {
        super::pin("sp-emu").unwrap();
        super::pin("faux-mgs").unwrap();
        super::pin_rev("sidecar-lite").unwrap();
    }
}
