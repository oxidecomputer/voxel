//! `voxel rack patch` / `voxel image patch` - surgically swap a single rack
//! component without a full image rebuild or online-update.
//!
//! Every patchable component is a prebuilt artifact omicron already pins in its
//! `package-manifest.toml`: a `<pkg>.tar.gz` published on buildomat under
//! `oxidecomputer/<repo>/image/<commit>/`, with its sha256 alongside as
//! `<pkg>.sha256.txt`. So a patch is just: re-fetch the tarball at a NEW ref,
//! sha-verify it, place it on the live nodes (or into the image's `@base`), and
//! restart the service.
//!
//! Two on-node shapes, both confirmed against a live rack:
//!  - **Service** - a long-running SMF service (maghemite `mgd`/`mg-ddm`,
//!    `dendrite`, `lldpd`). The buildomat tarball is an omicron-package zone
//!    image (`oxide.json` + a `root/` subtree mirroring the on-disk layout, e.g.
//!    `root/opt/oxide/mgd/bin/mgd`), so we overlay its `root/` onto the target
//!    zone root and `svcadm restart` the service. Switch-zone services live under
//!    `/zone/oxz_switch/root` on the scrimlets; the GZ ddm lives at `/` on every
//!    sled.
//!  - **ZoneImage** - propolis. It isn't a running service: sled-agent installs
//!    `/opt/oxide/propolis-server.tar.gz` as a zone per *instance*. The buildomat
//!    artifact is exactly that zone image, so we just replace the on-disk tarball
//!    on every sled; it takes effect on the next instance (nothing to restart).
//!
//! `rack patch` is live + ephemeral: it lasts the rack's lifetime and a clean
//! relaunch reverts to the image. `image patch` (below) folds the same artifact
//! into a new pinned `@base` so it persists across relaunches.

use anyhow::{anyhow, Context};
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::path::{Path, PathBuf};
use std::time::Duration;
use voxel_config::VoxelConfig;

use crate::net::{node_external_ip, scp_to, ssh_capture};
use crate::topo::{build_topo, Topo};

const BUILDOMAT: &str = "https://buildomat.eng.oxide.computer/public/file/oxidecomputer";

/// Which nodes a component lands on.
#[derive(Clone, Copy)]
enum Targets {
    /// Every sled (propolis, the GZ ddm).
    AllSleds,
    /// The scrimlets only (switch-zone infra).
    Scrimlets,
}

/// The filesystem root a Service's `root/` subtree overlays onto, as seen from
/// the sled global zone.
#[derive(Clone, Copy)]
enum Root {
    /// The sled global zone itself (`/`).
    Gz,
    /// The switch zone, visible from the GZ at `/zone/oxz_switch/root`.
    Switch,
}

impl Root {
    fn dir(self) -> &'static str {
        match self {
            Root::Gz => "/",
            Root::Switch => "/zone/oxz_switch/root",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Root::Gz => "global zone",
            Root::Switch => "oxz_switch",
        }
    }
}

/// How a component is applied on a node.
#[derive(Clone, Copy)]
enum Shape {
    /// A zone image installed on demand (no running service): replace the on-disk
    /// `dest` tarball. Effective on the next instantiation.
    ZoneImage { dest: &'static str },
    /// A running SMF service: overlay the tarball's `root/` onto `root` and
    /// `svcadm restart fmri`.
    Service { root: Root, fmri: &'static str },
}

/// A patchable component: its buildomat coordinates (`repo`/`pkg`) and its on-node
/// shape. The artifact URL is `<BUILDOMAT>/<repo>/image/<ref>/<pkg>.tar.gz`.
struct Component {
    /// The CLI name (`voxel rack patch <name> <ref>`).
    name: &'static str,
    /// buildomat repo the artifact is published under.
    repo: &'static str,
    /// Artifact basename (`<pkg>.tar.gz` / `<pkg>.sha256.txt`); = omicron's
    /// package name.
    pkg: &'static str,
    shape: Shape,
    targets: Targets,
    /// One-line operator hint shown in the plan.
    note: &'static str,
}

/// The component registry, grounded in `package-manifest.toml` @ 43bb5af and a
/// live rack. The clean "everything except host OS + control-plane zones" set:
/// prebuilt switch infra (restart in place) + propolis (zone image swap).
fn registry() -> Vec<Component> {
    vec![
        Component {
            name: "propolis",
            repo: "propolis",
            pkg: "propolis-server",
            shape: Shape::ZoneImage { dest: "/opt/oxide/propolis-server.tar.gz" },
            targets: Targets::AllSleds,
            note: "zone image; effective on the next instance (no service restart)",
        },
        Component {
            name: "mgd",
            repo: "maghemite",
            pkg: "mgd",
            shape: Shape::Service { root: Root::Switch, fmri: "svc:/oxide/mgd:default" },
            targets: Targets::Scrimlets,
            note: "BGP/static routing daemon; restart briefly flaps BGP (reconverges)",
        },
        Component {
            name: "mg-ddm",
            repo: "maghemite",
            pkg: "mg-ddm",
            shape: Shape::Service { root: Root::Switch, fmri: "svc:/oxide/mg-ddm:default" },
            targets: Targets::Scrimlets,
            note: "switch-zone underlay ddm router",
        },
        Component {
            name: "ddm-gz",
            repo: "maghemite",
            pkg: "mg-ddm-gz",
            shape: Shape::Service { root: Root::Gz, fmri: "svc:/oxide/mg-ddm:default" },
            targets: Targets::AllSleds,
            note: "global-zone underlay ddm router (every sled)",
        },
        Component {
            name: "dendrite",
            repo: "dendrite",
            pkg: "dendrite-softnpu",
            shape: Shape::Service { root: Root::Switch, fmri: "svc:/oxide/dendrite:default" },
            targets: Targets::Scrimlets,
            note: "the data-plane controller (dpd); restart disrupts switching briefly",
        },
        Component {
            name: "lldp",
            repo: "lldp",
            pkg: "lldp",
            shape: Shape::Service { root: Root::Switch, fmri: "svc:/oxide/lldpd:default" },
            targets: Targets::Scrimlets,
            note: "link-layer discovery daemon; restart is benign",
        },
    ]
}

fn lookup(name: &str) -> anyhow::Result<Component> {
    registry().into_iter().find(|c| c.name == name).ok_or_else(|| {
        let names: Vec<&str> = registry().iter().map(|c| c.name).collect();
        anyhow!("unknown component '{name}'. patchable components: {}", names.join(", "))
    })
}

/// Print the component registry (`voxel rack patch --list`).
pub(crate) fn list() {
    println!("{:<10}  {:<10}  {:<16}  {:<10}  {}", "COMPONENT", "REPO", "PKG", "TARGETS", "KIND");
    for c in registry() {
        let (targets, kind) = (
            match c.targets {
                Targets::AllSleds => "all sleds",
                Targets::Scrimlets => "scrimlets",
            },
            match c.shape {
                Shape::ZoneImage { .. } => "zone image",
                Shape::Service { root, .. } => match root {
                    Root::Gz => "service (gz)",
                    Root::Switch => "service (switch)",
                },
            },
        );
        println!("{:<10}  {:<10}  {:<16}  {:<10}  {}", c.name, c.repo, c.pkg, targets, kind);
        println!("{:<54}{}", "", c.note);
    }
}

// --- acquire (on the box) --------------------------------------------------

/// Where downloaded artifacts are cached on the box: `<build_root>/patch-cache`
/// (build_root from `[falcon].build_root`/`$BUILD_ROOT`, else `$HOME/voxel-builds`).
fn cache_dir() -> PathBuf {
    let root = std::env::var("BUILD_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join("voxel-builds")))
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("patch-cache")
}

/// sha256 of a file via illumos `digest -a sha256` (matches the buildomat
/// `.sha256.txt` the manifest pins).
fn sha256(file: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("digest")
        .args(["-a", "sha256"])
        .arg(file)
        .output()
        .map_err(|e| anyhow!("run digest: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!("digest {}: {}", file.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `curl` a small text URL (the `.sha256.txt`) and return its first whitespace
/// token (the hex digest).
fn fetch_sha(url: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|e| anyhow!("run curl: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!("fetch {url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("empty sha256 at {url}"))
}

/// Download `comp`'s artifact at `reference` to the box cache and sha-verify it
/// against the buildomat-published `.sha256.txt`. Reuses a cached, already-correct
/// download. Returns the local tarball path.
fn acquire(comp: &Component, reference: &str) -> anyhow::Result<PathBuf> {
    let dir = cache_dir().join(comp.repo).join(reference);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let tarball = dir.join(format!("{}.tar.gz", comp.pkg));

    let base = format!("{BUILDOMAT}/{}/image/{reference}", comp.repo);
    let want = fetch_sha(&format!("{base}/{}.sha256.txt", comp.pkg)).with_context(|| {
        format!("no published artifact for {} {} at {reference} (check the ref)", comp.repo, comp.pkg)
    })?;

    // Reuse a cached download only if it already matches the published digest.
    if tarball.exists() && sha256(&tarball).map(|s| s == want).unwrap_or(false) {
        eprintln!("[voxel] {} {reference}: using cached {} (sha ok)", comp.pkg, tarball.display());
        return Ok(tarball);
    }
    let url = format!("{base}/{}.tar.gz", comp.pkg);
    eprintln!("[voxel] downloading {url}");
    // `-sS`: no progress bar (it renders as carriage-return noise over the
    // non-TTY ssh voxel runs under) but still surface errors.
    let status = std::process::Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()
        .map_err(|e| anyhow!("run curl: {e}"))?;
    if !status.success() {
        return Err(anyhow!("download failed: {url}"));
    }
    let got = sha256(&tarball)?;
    if got != want {
        let _ = std::fs::remove_file(&tarball);
        return Err(anyhow!("sha256 mismatch for {}: got {got}, want {want}", comp.pkg));
    }
    eprintln!("[voxel] {} {reference}: sha256 verified ({want})", comp.pkg);
    Ok(tarball)
}

// --- rack patch (live nodes) -----------------------------------------------

/// Resolve a node's host-LAN IP, bounded so a wedged serial console fails fast.
async fn node_ip(d: &Runner, n: NodeRef) -> anyhow::Result<String> {
    tokio::time::timeout(Duration::from_secs(20), node_external_ip(d, n, false))
        .await
        .map_err(|_| anyhow!("timed out resolving node IP over the serial console (is the rack up?)"))?
}

/// The target `(name, NodeRef)` set for a component.
fn targets<'a>(topo: &'a Topo, comp: &Component) -> Vec<(String, NodeRef)> {
    topo.sleds
        .iter()
        .filter(|(s, _)| match comp.targets {
            Targets::AllSleds => true,
            Targets::Scrimlets => s.scrimlet,
        })
        .map(|(s, n)| (s.name.clone(), *n))
        .collect()
}

/// Single-quote for safe interpolation into a remote shell command.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Overlay the tarball's `root/` subtree onto `root_dir` on the node, using only
/// SVR4 `tar` + `gzcat` (the sleds/switch zone have no `gtar`): unpack the `root/`
/// member into a temp dir, then stream its contents into place via a tar pipe
/// (both ends SVR4 tar, no flags). `remote` is the scp'd tarball path on the node.
fn place_service_cmd(pkg: &str, remote: &str, root_dir: &str) -> String {
    let tmp = format!("/var/tmp/voxel-patch-{pkg}");
    format!(
        "TMP={tmp}; rm -rf \"$TMP\" && mkdir -p \"$TMP\" && \
         ( cd \"$TMP\" && gzcat {r} | tar xf - root ) && \
         ( cd \"$TMP/root\" && tar cf - . | ( cd {d} && tar xf - ) ) && \
         rm -rf \"$TMP\" && echo PATCH_PLACED_OK",
        r = q(remote),
        d = q(root_dir),
    )
}

/// Poll an SMF service until it reaches `online` (restart is async). `root`
/// selects the GZ vs the switch zone. Returns the final state seen.
fn wait_online(ip: &str, root: Root, fmri: &str) -> String {
    let query = match root {
        Root::Gz => format!("svcs -H -o state {fmri}"),
        Root::Switch => format!("zlogin oxz_switch svcs -H -o state {fmri}"),
    };
    let mut last = String::from("(unknown)");
    for _ in 0..15 {
        if let Some(s) = ssh_capture(ip, &query) {
            last = s.trim().to_string();
            if last == "online" {
                return last;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    last
}

/// Apply a Service patch on one node: scp'd tarball already at `remote`. Overlay
/// + restart + verify online.
fn apply_service(d: &Runner, node: &str, ip: &str, comp: &Component, remote: &str, root: Root, fmri: &str) {
    let placed = ssh_capture(ip, &place_service_cmd(comp.pkg, remote, root.dir()))
        .map(|o| o.contains("PATCH_PLACED_OK"))
        .unwrap_or(false);
    if !placed {
        warn!(d.log, "{node}: failed to overlay {} into {}", comp.pkg, root.label());
        return;
    }
    let restart = match root {
        Root::Gz => format!("svcadm restart {fmri} && echo RESTART_OK"),
        Root::Switch => format!("zlogin oxz_switch svcadm restart {fmri} && echo RESTART_OK"),
    };
    let restarted = ssh_capture(ip, &restart).map(|o| o.contains("RESTART_OK")).unwrap_or(false);
    if !restarted {
        warn!(d.log, "{node}: placed {} but `svcadm restart {fmri}` failed", comp.pkg);
        return;
    }
    let state = wait_online(ip, root, fmri);
    if state == "online" {
        info!(d.log, "{node}: {} patched, {fmri} online ({})", comp.pkg, root.label());
    } else {
        warn!(d.log, "{node}: {} placed + restarted but {fmri} is '{state}' (check `voxel tp login`)", comp.pkg);
    }
}

/// Apply a ZoneImage patch on one node: replace the on-disk tarball.
fn apply_zone_image(d: &Runner, node: &str, ip: &str, comp: &Component, remote: &str, dest: &str) {
    let ok = ssh_capture(ip, &format!("cp {} {} && echo PATCH_PLACED_OK", q(remote), q(dest)))
        .map(|o| o.contains("PATCH_PLACED_OK"))
        .unwrap_or(false);
    if ok {
        info!(d.log, "{node}: {} replaced ({dest}) - {}", comp.pkg, comp.note);
    } else {
        warn!(d.log, "{node}: failed to replace {dest}");
    }
}

pub(crate) async fn cmd_rack_patch(
    cfg: &VoxelConfig,
    name: &str,
    component: &str,
    reference: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let comp = lookup(component)?;
    let topo = build_topo(cfg, name)?;
    let d = &topo.runner;
    let nodes = targets(&topo, &comp);
    if nodes.is_empty() {
        return Err(anyhow!("no target nodes for {component} in this topology"));
    }
    let where_ = match comp.targets {
        Targets::AllSleds => "every sled",
        Targets::Scrimlets => "the scrimlets",
    };
    info!(
        d.log,
        "patch plan: {} ({}/{}) @ {reference} -> {where_} [{}] - {}",
        comp.name, comp.repo, comp.pkg,
        nodes.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "),
        comp.note
    );
    if dry_run {
        info!(d.log, "dry-run: nothing applied");
        return Ok(());
    }

    // Fetch + sha-verify once on the box, then distribute the same artifact.
    let tarball = acquire(&comp, reference)?;
    let remote = format!("/var/tmp/{}.tar.gz", comp.pkg);
    let local = tarball.to_str().ok_or_else(|| anyhow!("non-utf8 tarball path"))?;

    for (node, n) in nodes {
        let ip = match node_ip(d, n).await {
            Ok(ip) => ip,
            Err(e) => {
                warn!(d.log, "{node}: {e}");
                continue;
            }
        };
        if !scp_to(&ip, local, &remote) {
            warn!(d.log, "{node}: scp of {} failed", comp.pkg);
            continue;
        }
        match comp.shape {
            Shape::ZoneImage { dest } => apply_zone_image(d, &node, &ip, &comp, &remote, dest),
            Shape::Service { root, fmri } => apply_service(d, &node, &ip, &comp, &remote, root, fmri),
        }
        let _ = ssh_capture(&ip, &format!("rm -f {}", q(&remote)));
    }
    info!(d.log, "patch complete ({} @ {reference}); reverts to the image on a clean relaunch", comp.name);
    Ok(())
}

// --- image patch (persist into a new @base) --------------------------------

/// Locate `voxel-image/patch-image.sh` (mirrors `image::build_cp_script`).
fn patch_image_script() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("VOXEL_PATCH_IMAGE") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("../../voxel-image/patch-image.sh");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let cwd = PathBuf::from("voxel-image/patch-image.sh");
    if cwd.exists() {
        return Ok(cwd);
    }
    Err(anyhow!("can't find patch-image.sh - set VOXEL_PATCH_IMAGE to its path"))
}

/// Fold a component patch into a NEW pinned `@base` (boot-modify-capture via
/// `patch-image.sh`) so it persists across relaunches. Slower than `rack patch`
/// but durable. `src_image` is the image to patch; `out_image` the captured
/// result (defaults to `<src>-<component>-<shortref>`).
pub(crate) fn cmd_image_patch(
    component: &str,
    reference: &str,
    src_image: &str,
    out_image: Option<&str>,
) -> anyhow::Result<()> {
    let comp = lookup(component)?;
    // Map the on-node shape to an in-image placement. Switch-zone services live
    // INSIDE `/opt/oxide/switch.tar.gz` in the image (the switch zone isn't
    // instantiated until RSS), so persisting them needs a switch.tar.gz repack -
    // not done yet. propolis (zone image) and the GZ ddm land directly in the
    // sled filesystem, so they overlay cleanly.
    let (place_kind, dest): (&str, Option<&str>) = match comp.shape {
        Shape::ZoneImage { dest } => ("zone-image", Some(dest)),
        Shape::Service { root: Root::Gz, .. } => ("gz-overlay", None),
        Shape::Service { root: Root::Switch, .. } => {
            return Err(anyhow!(
                "`image patch` for switch-zone service '{}' isn't supported yet - it lives inside \
                 /opt/oxide/switch.tar.gz in the image and needs a zone-image repack. Use \
                 `voxel rack patch {}` for a live (ephemeral) patch.",
                comp.name, comp.name
            ));
        }
    };

    crate::image::ensure_image(src_image)?;
    let tarball = acquire(&comp, reference)?;
    let short = &reference[..reference.len().min(12)];
    let default_out = format!("{src_image}-{}-{short}", comp.name);
    let out = out_image.unwrap_or(&default_out);

    let script = patch_image_script()?;
    eprintln!(
        "[voxel] image patch: {} ({} @ {reference}) {src_image} -> {out} (boot-modify-capture; ~minutes)",
        comp.name, comp.pkg
    );
    let mut cmd = std::process::Command::new("bash");
    cmd.arg(&script)
        .env("SRC_IMAGE", src_image)
        .env("OUT_IMAGE", out)
        .env("ARTIFACT", &tarball)
        .env("PKG", comp.pkg)
        .env("PLACE_KIND", place_kind)
        .env("COMPONENT", comp.name)
        .env("REF", reference);
    if let Some(d) = dest {
        cmd.env("DEST", d);
    }
    // FALCON_DATASET is already exported by resolve_falcon_env; patch-image.sh +
    // build-image.sh read it.
    let status = cmd.status().map_err(|e| anyhow!("run {}: {e}", script.display()))?;
    if !status.success() {
        return Err(anyhow!("patch-image.sh failed"));
    }
    println!("patched image {out} (component {} @ {reference}); set it with: voxel config set image.cp {out}", comp.name);
    Ok(())
}
