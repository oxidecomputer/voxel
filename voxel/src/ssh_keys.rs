// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{Context, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::fs;
use voxel_config::VoxelConfig;

use crate::topo::CARGO_BAY;

const SSH_PUBKEY_CANDIDATES: &[&str] =
    &["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"];

const SSH_PUBKEY_PREFIXES: &[&str] = &[
    "ssh-ed25519 ",
    "ssh-rsa ",
    "ssh-dss ",
    "ecdsa-sha2-",
    "sk-ssh-ed25519@",
    "sk-ecdsa-",
];

/// Stage the operator's SSH public key into every node's cargo-bay as
/// `root_authorized_keys`.
///
/// An empty file is staged when `ssh_pubkey` is `""` (empty) or when no key is
/// configured or found, driving `voxel-init` to drop its block.
pub(crate) fn stage_ssh_pubkey(cfg: &VoxelConfig) -> anyhow::Result<()> {
    let ssh_dir = std::env::var("HOME")
        .ok()
        .map(|dir| Utf8PathBuf::from(dir).join(".ssh"));
    stage_ssh_pubkey_in(cfg, Utf8Path::new(CARGO_BAY), ssh_dir.as_deref())
}

fn stage_ssh_pubkey_in(
    cfg: &VoxelConfig,
    cargo_root: &Utf8Path,
    ssh_dir: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    let Some(path) = ssh_pubkey_source(cfg, ssh_dir)? else {
        match cfg.falcon.ssh_pubkey.as_deref() {
            Some("") => eprintln!(
                "[voxel] [falcon].ssh_pubkey is empty; staging no ssh key"
            ),
            _ => {
                let looked = SSH_PUBKEY_CANDIDATES.join(", ");
                let dir =
                    ssh_dir.map_or("$HOME/.ssh".into(), |d| d.to_string());
                eprintln!(
                    "[voxel] no ssh public key found in {dir} (looked for \
                     {looked}); staging no ssh key. Set falcon.ssh_pubkey to \
                     the .pub you want on the nodes, or to \"\" to stop \
                     looking"
                );
            }
        }
        return write_root_authorized_keys(cfg, cargo_root, "");
    };
    let body = read_ssh_pubkey(&path)
        .with_context(|| format!("ssh public key {path}"))?;
    eprintln!(
        "[voxel] staging ssh public key {path} [{}] into every node's \
         root authorized_keys",
        describe_keys(&body)
    );
    write_root_authorized_keys(cfg, cargo_root, &body)
}

fn describe_keys(body: &str) -> String {
    body.lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let kind = fields.next().unwrap_or("?");
            let comment = fields.nth(1).unwrap_or("no comment");
            format!("{kind} {comment}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn ssh_pubkey_source(
    cfg: &VoxelConfig,
    ssh_dir: Option<&Utf8Path>,
) -> anyhow::Result<Option<Utf8PathBuf>> {
    if let Some(p) = cfg.falcon.ssh_pubkey.as_deref() {
        if p.is_empty() {
            return Ok(None);
        }
        let p = Utf8PathBuf::from(p);
        if !p.is_file() {
            bail!("[falcon].ssh_pubkey '{p}' does not exist");
        }
        return Ok(Some(p));
    }
    let Some(ssh) = ssh_dir else { return Ok(None) };
    Ok(SSH_PUBKEY_CANDIDATES
        .iter()
        .map(|file| ssh.join(file))
        .find(|path| path.is_file()))
}

fn write_root_authorized_keys(
    cfg: &VoxelConfig,
    cargo_root: &Utf8Path,
    body: &str,
) -> anyhow::Result<()> {
    cfg.sleds()
        .into_iter()
        .map(|s| s.name)
        .chain(cfg.topology.routers.iter().cloned())
        .try_for_each(|node| {
            let dir = cargo_root.join(&node);
            fs::create_dir_all(&dir)
                .with_context(|| format!("create {dir}"))?;
            let path = dir.join("root_authorized_keys");
            fs::write(&path, body).with_context(|| format!("write {path}"))
        })
}

/// Read and validate a public-key file.
///
/// Validation is by content and not filename. With cargo-bay mounting into
/// every guest, an `ssh_pubkey` somehow pointing at a private key
/// would share the secret with the whole rack! Every non-empty
/// line in the file must carry a recognized public-key algorithm prefix.
///
/// Returns the key file normalized.
fn read_ssh_pubkey(path: &Utf8Path) -> anyhow::Result<String> {
    validate_ssh_pubkey(&fs::read_to_string(path)?)
}

/// The content check behind `read_ssh_pubkey`, split out for testing.
fn validate_ssh_pubkey(raw: &str) -> anyhow::Result<String> {
    if raw.contains("PRIVATE KEY") {
        bail!("this is a private key; refusing to stage it into the guests");
    }
    let mut body = String::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if !SSH_PUBKEY_PREFIXES.iter().any(|p| line.starts_with(p)) {
            bail!(
                "line does not match an OpenSSH public key: '{}...'",
                line.chars().take(24).collect::<String>()
            );
        }
        body.push_str(line);
        body.push('\n');
    }
    if body.is_empty() {
        bail!("no keys found");
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged_ssh_paths(
        root: &Utf8Path,
    ) -> impl Iterator<Item = Utf8PathBuf> + '_ {
        ["g0", "g1", "g2", "g3", "ce", "cr1", "cr2"]
            .into_iter()
            .map(|node| root.join(node).join("root_authorized_keys"))
    }

    #[test]
    fn stage_ssh_pubkey_empty_path_overrides_discovery() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cargo = root.join("cargo-bay");
        let ssh = root.join("ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(ssh.join("id_ed25519.pub"), "ssh-ed25519 AAA discovered\r\n")
            .unwrap();
        let mut cfg = VoxelConfig::default();

        stage_ssh_pubkey_in(&cfg, &cargo, Some(&ssh)).unwrap();
        for path in staged_ssh_paths(&cargo) {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                "ssh-ed25519 AAA discovered\n"
            );
        }

        cfg.falcon.ssh_pubkey = Some(String::new());
        stage_ssh_pubkey_in(&cfg, &cargo, Some(&ssh)).unwrap();
        for path in staged_ssh_paths(&cargo) {
            assert!(fs::read(path).unwrap().is_empty());
        }
    }

    #[test]
    fn discovery_prefers_ed25519_then_ecdsa_then_rsa() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let ssh = tmp.path();
        let cfg = VoxelConfig::default();

        assert_eq!(ssh_pubkey_source(&cfg, Some(ssh)).unwrap(), None);

        fs::write(ssh.join("id_rsa.pub"), "ssh-rsa AAA rsa\n").unwrap();
        assert_eq!(
            ssh_pubkey_source(&cfg, Some(ssh)).unwrap(),
            Some(ssh.join("id_rsa.pub"))
        );

        fs::write(ssh.join("id_ecdsa.pub"), "ecdsa-sha2-nistp256 AAA ecdsa\n")
            .unwrap();
        fs::write(ssh.join("id_ed25519.pub"), "ssh-ed25519 AAA ed\n").unwrap();
        assert_eq!(
            ssh_pubkey_source(&cfg, Some(ssh)).unwrap(),
            Some(ssh.join("id_ed25519.pub"))
        );
    }

    #[test]
    fn stage_ssh_pubkey_no_key_path_stages_empty_files() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cargo = root.join("cargo-bay");
        for path in staged_ssh_paths(&cargo) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "ssh-ed25519 AAA previous\n").unwrap();
        }
        let cfg = VoxelConfig::default();

        for ssh_dir in [Some(root), None] {
            stage_ssh_pubkey_in(&cfg, &cargo, ssh_dir).unwrap();
            for path in staged_ssh_paths(&cargo) {
                assert!(fs::read(path).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn stage_ssh_pubkey_explicit_path_is_validated_before_staging() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cargo = root.join("cargo-bay");
        let selected = root.join("selected.pub");
        fs::write(&selected, "ssh-ed25519 AAA selected\n").unwrap();
        fs::write(root.join("id_ed25519.pub"), "ssh-ed25519 AAA discovered\n")
            .unwrap();
        let mut cfg = VoxelConfig::default();
        cfg.falcon.ssh_pubkey = Some(selected.into_string());

        stage_ssh_pubkey_in(&cfg, &cargo, Some(root)).unwrap();
        for path in staged_ssh_paths(&cargo) {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                "ssh-ed25519 AAA selected\n"
            );
        }

        let private = root.join("id_ed25519");
        fs::write(&private, "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n")
            .unwrap();
        for bad in [root.join("missing.pub"), private] {
            cfg.falcon.ssh_pubkey = Some(bad.into_string());
            assert!(stage_ssh_pubkey_in(&cfg, &cargo, Some(root)).is_err());
            for path in staged_ssh_paths(&cargo) {
                assert_eq!(
                    fs::read_to_string(path).unwrap(),
                    "ssh-ed25519 AAA selected\n"
                );
            }
        }
    }

    #[test]
    fn accepts_and_normalizes_public_keys() {
        let out = validate_ssh_pubkey(
            "ssh-ed25519 AAAC3Nza me@host\r\n\nssh-rsa AAAAB3Nza me@host",
        )
        .unwrap();

        assert_eq!(
            out,
            "ssh-ed25519 AAAC3Nza me@host\nssh-rsa AAAAB3Nza me@host\n"
        );
    }

    /// The cargo-bay is mounted into every guest. A `[falcon].ssh_pubkey`
    /// pointing at the private half must fail outright.
    #[test]
    fn rejects_private_keys_and_non_keys() {
        let openssh = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk...\n\
             -----END OPENSSH PRIVATE KEY-----\n";
        assert!(validate_ssh_pubkey(openssh).is_err());
        assert!(validate_ssh_pubkey("not a key at all\n").is_err());
        assert!(validate_ssh_pubkey("ssh-ed25519 AAA ok\njunk\n").is_err());
        assert!(validate_ssh_pubkey("").is_err());
    }

    #[test]
    fn describes_keys_by_type_and_comment() {
        assert_eq!(
            describe_keys("ssh-ed25519 AAAC3Nza me@host\nssh-rsa AAAAB3Nza\n"),
            "ssh-ed25519 me@host; ssh-rsa no comment"
        );
    }
}
