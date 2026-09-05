// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The emulated SP fleet, run on the falcon host.
//!
//! `--emu` runs one sp-emu process per board for the whole rack here, rather
//! than a private copy inside each switch zone. Both switch zones reach the same
//! flash over the rack's SP network, so a stateful update cannot start against
//! one copy and then be polled against another, and the SPs outlive the sled
//! reboots they cause.
//!
//! Everything is scoped by rack: the SMF instance names, the fleet address, and
//! the staged state all carry the rack index, so tearing one rack down leaves a
//! co-resident rack running.

use anyhow::{Context, anyhow, bail};
use camino::Utf8Path;
use indicatif::{ProgressBar, ProgressStyle};
use std::future::Future;
use std::process::{Command, Stdio};
use std::time::Duration;

/// SMF service backing the fleet; one instance per SP per rack.
const SVC: &str = "svc:/oxide/voxel-sp-emu";
/// Where each rack's manifest is written for import.
const MANIFEST_DIR: &str = "/var/svc/manifest/site";
/// How long to keep retrying a losing import before giving up.
const IMPORT_WAIT_S: u32 = 60;

/// The sp-emu board an SP runs, which selects its staged hubris archive.
fn board_of(sp: &voxel_config::sp::Sp) -> &'static str {
    match sp.role {
        voxel_config::sp::SpRole::Sidecar => "sidecar",
        voxel_config::sp::SpRole::Gimlet(_) => "gimlet",
    }
}

/// A rack's SMF instance name for an SP. It carries the rack so a second rack's
/// fleet cannot collide with the first on the shared host.
fn instance(rack: usize, port: u16) -> String {
    format!("r{rack}sp{port}")
}

/// The addrobj holding a rack's fleet address.
fn addrobj(rack: usize) -> String {
    format!("spr{rack}")
}

/// A rack's manifest path.
fn manifest_path(rack: usize) -> String {
    format!("{MANIFEST_DIR}/voxel-sp-emu-r{rack}.xml")
}

/// Run a read-only probe (true when it exits 0).
fn probe(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Capture a read-only probe's stdout (`None` on spawn failure or non-zero exit).
fn probe_out(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a mutating host command under pfexec.
fn run(args: &[&str]) -> anyhow::Result<()> {
    let st = Command::new("pfexec")
        .args(args)
        .status()
        .with_context(|| format!("spawn pfexec {}", args.join(" ")))?;
    if !st.success() {
        bail!("pfexec {} failed ({st})", args.join(" "));
    }
    Ok(())
}

/// The SMF manifest for one rack's fleet. Each SP runs `sp-emu run a 0` in the
/// foreground so startd's contract owns it (restart on crash, reboot safe), and
/// binds `SP_EMU_BRIDGE` at the rack's fleet address: sp-emu serves the switch0
/// view there and the switch1 view on the next port, which is exactly the pair
/// `mgs::switch_config` emits for the two switch slots.
fn manifest(
    rack: usize,
    dir: &Utf8Path,
    addr: &str,
    rot: bool,
    fleet: &[&voxel_config::sp::Sp],
) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE service_bundle SYSTEM \"/usr/share/lib/xml/dtd/service_bundle.dtd.1\">\n\
         <service_bundle type=\"manifest\" name=\"voxel-sp-emu-r{rack}\">\n\
         <service name=\"oxide/voxel-sp-emu\" type=\"service\" version=\"1\">\n\
         \x20 <dependency name=\"multi_user\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n\
         \x20   <service_fmri value=\"svc:/milestone/multi-user:default\"/>\n\
         \x20 </dependency>\n"
    );
    for sp in fleet {
        let inst = instance(rack, sp.base_port);
        let state = format!("{dir}/state/{}", sp.base_port);
        let board = board_of(sp);
        // working_directory keeps sp-emu's cwd-relative RoT archive extraction
        // inside this instance's state dir instead of scattering it at /.
        s.push_str(&format!(
            "  <instance name=\"{inst}\" enabled=\"true\">\n\
             \x20   <exec_method type=\"method\" name=\"start\" exec=\"{dir}/sp-emu run a 0\" timeout_seconds=\"0\">\n\
             \x20     <method_context working_directory=\"{state}\">\n\
             \x20       <method_environment>\n\
             \x20         <envvar name=\"SP_EMU_STATE_DIR\" value=\"{state}\"/>\n\
             \x20         <envvar name=\"SP_EMU_BOARD\" value=\"{}\"/>\n\
             \x20         <envvar name=\"SP_EMU_BRIDGE\" value=\"[{addr}]:{}\"/>\n\
             \x20         <envvar name=\"SP_EMU_VPD_SERIAL\" value=\"{}\"/>\n\
             \x20         <envvar name=\"SP_EMU_NO_DEBUG\" value=\"1\"/>\n",
            board, sp.base_port, sp.serial
        ));
        if let Some(part) = &sp.part_number {
            s.push_str(&format!(
                "            <envvar name=\"SP_EMU_VPD_PART\" value=\"{part}\"/>\n"
            ));
        }
        if rot {
            s.push_str(&format!(
                "            <envvar name=\"SP_EMU_ROT_FLASH\" value=\"{dir}/rot.image\"/>\n"
            ));
            if dir.join("bootleby.zip").exists() {
                s.push_str(&format!(
                    "            <envvar name=\"SP_EMU_ROT_BOOTLEBY\" value=\"{dir}/bootleby.zip\"/>\n"
                ));
            } else {
                s.push_str(
                    "            <envvar name=\"SP_EMU_ROT_NO_BOOTLEBY\" value=\"1\"/>\n",
                );
            }
        }
        s.push_str(
            "          </method_environment>\n\
             \x20     </method_context>\n\
             \x20   </exec_method>\n\
             \x20   <exec_method type=\"method\" name=\"stop\" exec=\":kill\" timeout_seconds=\"30\"/>\n\
             \x20   <property_group name=\"startd\" type=\"framework\">\n\
             \x20     <propval name=\"duration\" type=\"astring\" value=\"child\"/>\n\
             \x20   </property_group>\n\
             \x20 </instance>\n",
        );
    }
    s.push_str("</service>\n</service_bundle>\n");
    s
}

/// Whether `link` already carries an IPv6 link-local. Without one, ipadm
/// rejects a global v6 address on that link.
fn has_link_local(link: &str) -> bool {
    let Some(out) =
        probe_out("ipadm", &["show-addr", "-p", "-o", "addrobj,addr"])
    else {
        return false;
    };
    let prefix = format!("{link}/");
    out.lines().any(|l| l.starts_with(&prefix) && l.contains("fe80"))
}

/// Whether the host has the SMF instance backing an SP. The import is only
/// believable once every instance is really there.
fn instance_exists(rack: usize, port: u16) -> bool {
    probe("svcs", &["-H", &format!("{SVC}:{}", instance(rack, port))])
}

// --- the running fleet, for the operator commands ---------------------------

/// A rack's staged fleet directory: the sp-emu binary, the hubris archives, the
/// RoT image and each SP's flashed state. ABSOLUTE, because these paths are
/// handed to sp-emu through its SMF environment and it resolves a relative one
/// against its own working directory, not voxel's.
pub(crate) fn fleet_dir(rack: usize) -> camino::Utf8PathBuf {
    let rel = crate::topo::sp_fleet_dir(rack).join("sp-emu");
    if let Ok(abs) = rel.canonicalize_utf8() {
        return abs;
    }
    // Not staged yet (pre-launch): absolutize against the workdir voxel
    // anchored to, so the answer is still one sp-emu could use.
    match std::env::current_dir()
        .ok()
        .and_then(|d| camino::Utf8PathBuf::from_path_buf(d).ok())
    {
        Some(cwd) => cwd.join(rel.strip_prefix("./").unwrap_or(&rel)),
        None => rel,
    }
}

/// The repo's pins.toml, embedded so a shipped voxel binary carries its own
/// pins. Each entry names a buildomat-published binary and the rev to fetch.
const PINS: &str = include_str!("../../pins.toml");

/// One pins.toml entry.
struct Pin {
    repo: String,
    series: String,
    rev: String,
    artifact: String,
}

/// Look up one entry of the embedded pins.toml.
fn pin(name: &str) -> anyhow::Result<Pin> {
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
    let p = Pin {
        repo: field("repo")?,
        series: field("series")?,
        rev: field("rev")?,
        artifact: field("artifact")?,
    };
    if p.rev.len() != 40 || !p.rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("pins.toml [{name}] rev is not a full git sha: {}", p.rev);
    }
    Ok(p)
}

/// The sp-emu to run the fleet with: [sp].emu_bin, else sp-emu on PATH,
/// else the pinned buildomat build, fetched once into ~/.cache/voxel.
pub(crate) fn ensure_emu_bin(
    cfg: &voxel_config::VoxelConfig,
) -> anyhow::Result<camino::Utf8PathBuf> {
    resolve_bin(cfg.sp.emu_bin.as_deref(), "emu_bin", "sp-emu")
}

/// The faux-mgs for the operator sp commands: [sp].faux_mgs, else faux-mgs
/// on PATH, else the pinned buildomat build (published gzipped).
pub(crate) fn ensure_faux_mgs(
    cfg: &voxel_config::VoxelConfig,
) -> anyhow::Result<camino::Utf8PathBuf> {
    resolve_bin(cfg.sp.faux_mgs.as_deref(), "faux_mgs", "faux-mgs")
}

/// One fleet binary by precedence: the `[sp].<key>` override, `name` on
/// PATH, then the pinned buildomat build (`name` is also its pins.toml key).
fn resolve_bin(
    override_path: Option<&str>,
    key: &str,
    name: &str,
) -> anyhow::Result<camino::Utf8PathBuf> {
    if let Some(p) = override_path {
        let p = camino::Utf8PathBuf::from(p);
        if !p.is_file() {
            bail!("[sp].{key} does not exist: {p}");
        }
        return Ok(p);
    }
    if let Some(p) = find_in_path(name) {
        return Ok(p);
    }
    fetch_buildomat_bin(&pin(name)?, key).with_context(|| {
        format!(
            "no {name}: [sp].{key} is unset, none on PATH, and the pinned \
             build could not be fetched"
        )
    })
}

/// `name` as an executable file on PATH, the way a shell would find it.
fn find_in_path(name: &str) -> Option<camino::Utf8PathBuf> {
    find_in_path_list(&std::env::var_os("PATH")?, name)
}

fn find_in_path_list(
    path: &std::ffi::OsStr,
    name: &str,
) -> Option<camino::Utf8PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|p| {
            p.metadata().is_ok_and(|m| {
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
        .and_then(|p| camino::Utf8PathBuf::from_path_buf(p).ok())
}

/// Fetch one published buildomat binary into a rev-keyed cache under
/// ~/.cache/voxel and verify it against its .sha256.txt sibling. A .gz
/// artifact is hash-checked as published, then decompressed. The final name
/// appears only once the file is verified and executable, so an interrupted
/// fetch is never taken for a cached binary.
fn fetch_buildomat_bin(
    p: &Pin,
    key: &str,
) -> anyhow::Result<camino::Utf8PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let Pin { repo, series, rev, artifact } = p;
    let home = std::env::var("HOME").context("HOME not set")?;
    let bin_name = artifact.strip_suffix(".gz").unwrap_or(artifact);
    let dir = camino::Utf8PathBuf::from(home)
        .join(".cache/voxel/bins")
        .join(format!("{repo}-{}", &rev[..12]));
    let bin = dir.join(bin_name);
    if bin.exists() {
        return Ok(bin);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {dir}"))?;
    let url = format!(
        "{}/{repo}/{series}/{rev}/{artifact}",
        crate::cpbuild::BUILDOMAT_URL
    );
    eprintln!(
        "[voxel] fetching {bin_name} @ {} ([sp].{key} or PATH overrides)",
        &rev[..12]
    );
    let want = fetch_text(&format!("{url}.sha256.txt"), FETCH)?;
    let want = want.split_whitespace().next().unwrap_or("").to_string();
    let fetched = dir.join(format!("{artifact}.part"));
    let got = download(&url, &fetched, FETCH)?;
    if got != want {
        let _ = std::fs::remove_file(&fetched);
        bail!("{artifact} sha256 {got} != published {want}");
    }
    let staged = if artifact.ends_with(".gz") {
        let unpacked = dir.join(format!("{bin_name}.part"));
        let gz = std::fs::File::open(&fetched)
            .with_context(|| format!("open {fetched}"))?;
        let mut out = std::fs::File::create(&unpacked)
            .with_context(|| format!("create {unpacked}"))?;
        std::io::copy(&mut flate2::read::GzDecoder::new(gz), &mut out)
            .with_context(|| format!("gunzip {fetched}"))?;
        let _ = std::fs::remove_file(&fetched);
        unpacked
    } else {
        fetched
    };
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {staged}"))?;
    std::fs::rename(&staged, &bin)
        .with_context(|| format!("move {staged} to {bin}"))?;
    Ok(bin)
}

/// Retry and connect bounds for a fetch.
#[derive(Clone, Copy)]
struct Fetch {
    attempts: u32,
    connect_timeout: Duration,
}

/// Buildomat fetch bounds: an unreachable host fails in under a minute
/// rather than holding a launch in TCP retries.
const FETCH: Fetch =
    Fetch { attempts: 3, connect_timeout: Duration::from_secs(15) };

fn http_client(f: Fetch) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(f.connect_timeout)
        .timeout(Duration::from_secs(3600))
        .build()
        .context("http client")
}

/// Run an async fetch to completion from sync code. The callers already sit
/// on the tokio runtime, so this gets its own thread and runtime instead of
/// blocking the outer one.
fn run_fetch<T: Send>(
    fut: impl Future<Output = anyhow::Result<T>> + Send,
) -> anyhow::Result<T> {
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("fetch runtime")?
                .block_on(fut)
        })
        .join()
        .map_err(|_| anyhow!("fetch thread panicked"))?
    })
}

/// GET a small text body, retried.
fn fetch_text(url: &str, f: Fetch) -> anyhow::Result<String> {
    run_fetch(async {
        let client = http_client(f)?;
        let mut last = None;
        for attempt in 1..=f.attempts {
            let sent = client.get(url).send().await;
            match sent.and_then(|r| r.error_for_status()) {
                Ok(r) => {
                    return r
                        .text()
                        .await
                        .with_context(|| format!("read {url}"));
                }
                Err(e) => last = Some(e),
            }
            if attempt < f.attempts {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        Err(anyhow!(
            "GET {url} failed after {} attempts: {}",
            f.attempts,
            last.expect("at least one attempt")
        ))
    })
}

/// Stream `url` into `dest` behind a progress bar, returning the body's
/// sha256 hex. Retried whole; a partial `dest` is overwritten next attempt.
fn download(url: &str, dest: &Utf8Path, f: Fetch) -> anyhow::Result<String> {
    run_fetch(async {
        let client = http_client(f)?;
        let mut last = None;
        for attempt in 1..=f.attempts {
            match stream_to_file(&client, url, dest).await {
                Ok(sha) => return Ok(sha),
                Err(e) => {
                    eprintln!(
                        "[voxel] fetch attempt {attempt}/{} failed: {e:#}",
                        f.attempts
                    );
                    last = Some(e);
                }
            }
            if attempt < f.attempts {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        Err(last.expect("at least one attempt")).with_context(|| {
            format!("GET {url} failed after {} attempts", f.attempts)
        })
    })
}

async fn stream_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Utf8Path,
) -> anyhow::Result<String> {
    use sha2::Digest;
    use std::io::Write;
    let mut resp = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("GET {url}"))?;
    let pb = match resp.content_length() {
        Some(len) => {
            let pb = ProgressBar::new(len);
            pb.set_style(
                ProgressStyle::with_template(
                    "[{elapsed_precise}] {bar:40.cyan/blue} \
                     {bytes}/{total_bytes} {bytes_per_sec}",
                )
                .context("progress template")?
                .progress_chars("##-"),
            );
            pb
        }
        None => {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template(
                    "[{elapsed_precise}] {spinner} {bytes} {bytes_per_sec}",
                )
                .context("progress template")?,
            );
            pb
        }
    };
    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("create {dest}"))?;
    let mut hash = sha2::Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) =
        resp.chunk().await.with_context(|| format!("read {url}"))?
    {
        file.write_all(&chunk).with_context(|| format!("write {dest}"))?;
        hash.update(&chunk);
        total += chunk.len() as u64;
        pb.inc(chunk.len() as u64);
    }
    // The bar is transient; leave one durable line behind it.
    let secs = pb.elapsed().as_secs_f64();
    pb.finish_and_clear();
    eprintln!(
        "[voxel] fetched {} ({} MiB in {secs:.1}s)",
        dest.file_name().unwrap_or(dest.as_str()).trim_end_matches(".part"),
        total >> 20
    );
    Ok(format!("{:x}", hash.finalize()))
}

/// The sp-emu binary driving a rack's fleet.
pub(crate) fn emu_bin(rack: usize) -> anyhow::Result<camino::Utf8PathBuf> {
    let bin = fleet_dir(rack).join("sp-emu");
    if !bin.exists() {
        bail!("no sp-emu at {bin} - is this a running --emu rack?");
    }
    Ok(bin)
}

/// One SP's flashed state directory.
pub(crate) fn state_dir(rack: usize, port: u16) -> camino::Utf8PathBuf {
    fleet_dir(rack).join("state").join(port.to_string())
}

/// One SP's SMF instance.
pub(crate) fn fmri(rack: usize, port: u16) -> String {
    format!("{SVC}:{}", instance(rack, port))
}

/// Restart one SP, so it picks up new flash or a changed environment.
pub(crate) fn restart(rack: usize, port: u16) -> bool {
    probe("pfexec", &["svcadm", "restart", &fmri(rack, port)])
}

/// Restart every SP in a rack's fleet, returning how many were restarted.
pub(crate) fn restart_rack(rack: usize) -> usize {
    let prefix = format!("{SVC}:r{rack}sp");
    let Some(out) = probe_out("svcs", &["-H", "-o", "fmri", SVC]) else {
        return 0;
    };
    out.split_whitespace()
        .filter(|f| f.starts_with(&prefix))
        .filter(|f| probe("pfexec", &["svcadm", "restart", f]))
        .count()
}

/// Re-flash one SP's slot A from `image` and bring it back. The instance is
/// stopped first so sp-emu is not holding the flash file, and the state dir
/// survives, so the SP keeps its identity and the RoT its NVM. The instance is
/// re-enabled even when the flash fails, rather than leaving the SP down.
pub(crate) fn flash_sp(
    rack: usize,
    port: u16,
    image: &Utf8Path,
) -> anyhow::Result<()> {
    let bin = emu_bin(rack)?;
    let fmri = fmri(rack, port);
    run(&["svcadm", "disable", "-s", &fmri])?;
    let flashed = Command::new(bin.as_str())
        .args(["flash", "a", image.as_str()])
        .env("SP_EMU_STATE_DIR", state_dir(rack, port).as_str())
        .status()
        .with_context(|| format!("spawn sp-emu flash for port {port}"))?;
    let enabled = run(&["svcadm", "enable", &fmri]);
    if !flashed.success() {
        bail!("sp-emu flash failed for port {port} ({flashed})");
    }
    enabled
}

/// One SP's SMF start/environment, as the whitespace-separated tokens svcprop
/// prints. `None` when the instance is absent.
pub(crate) fn read_env(rack: usize, port: u16) -> Option<String> {
    probe_out("svcprop", &["-p", "start/environment", &fmri(rack, port)])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Replace one SP's SMF start/environment and restart it. The tokens are quoted
/// individually so values like `[fdb0:a840:25ff:0::1]:33310` survive svccfg.
pub(crate) fn set_env(
    rack: usize,
    port: u16,
    tokens: &[String],
) -> anyhow::Result<()> {
    let fmri = fmri(rack, port);
    let quoted: Vec<String> =
        tokens.iter().map(|t| format!("\"{t}\"")).collect();
    let body = format!(
        "select {fmri}\nsetprop start/environment = astring: ({})\n",
        quoted.join(" ")
    );
    let file =
        crate::util::temp_dir().join(format!("voxel-sp-env-{port}.scfg"));
    std::fs::write(&file, body).with_context(|| format!("write {file}"))?;
    let applied = run(&["svccfg", "-f", file.as_str()])
        .and_then(|_| run(&["svcadm", "refresh", &fmri]))
        .and_then(|_| run(&["svcadm", "restart", &fmri]));
    let _ = std::fs::remove_file(&file);
    applied.with_context(|| format!("apply the new environment to {fmri}"))
}

/// Bring up one rack's fleet: the fleet address on `link`, a flashed state
/// directory per SP, then the SMF instances. A rack with no emulated SPs (plain
/// sp-sim) stages no `ports` manifest, so this is a no-op there.
pub(crate) fn up(
    cfg: &voxel_config::VoxelConfig,
    rack: usize,
    fleet: &voxel_config::sp::SpFleet,
    rot: bool,
    dir: &Utf8Path,
    link: &str,
) -> anyhow::Result<()> {
    let addr = voxel_config::config::sp_host_addr(rack);
    let prefix_len = voxel_config::config::SP_NET_PREFIX_LEN;
    let fleet = fleet.emu_sps();
    // Only ever called for --emu, so an empty fleet means the caller built one
    // for the wrong rack. Never shrug it off: nothing would answer MGS and the
    // rack would wedge at the nexus handoff.
    if fleet.is_empty() {
        bail!("--emu but rack {rack} has no emulated SPs");
    }
    let dir = dir
        .canonicalize_utf8()
        .with_context(|| format!("resolve fleet dir {dir}"))?;
    let bin = dir.join("sp-emu");
    if !bin.exists() {
        bail!(
            "--emu needs an sp-emu binary staged at {bin}: launch stages \
             [sp].emu_bin, sp-emu on PATH, or the pinned buildomat build"
        );
    }
    // IPv6 refuses a global address on a link with no link-local ("Can't assign
    // requested address"). Link-local only, so we never adopt a prefix the site
    // LAN advertises. Every rack on this link shares it, so down() leaves it.
    if !has_link_local(link) {
        run(&[
            "ipadm",
            "create-addr",
            "-t",
            "-T",
            "addrconf",
            "-p",
            "stateless=no,stateful=no",
            &format!("{link}/voxelll"),
        ])
        .with_context(|| format!("add an IPv6 link-local on {link}"))?;
    }
    // Idempotent: a relaunch re-adds the same address, and ipadm refuses to
    // create over an existing addrobj.
    let obj = format!("{link}/{}", addrobj(rack));
    probe("pfexec", &["ipadm", "delete-addr", &obj]);
    run(&[
        "ipadm",
        "create-addr",
        "-t",
        "-T",
        "static",
        "-a",
        &format!("{addr}/{prefix_len}"),
        &obj,
    ])
    .with_context(|| format!("add rack {rack} SP fleet address {addr}"))?;

    // A switch zone answers from its bootstrap address, not from the SP
    // network, so the host needs a route back to each scrimlet's bootstrap /64.
    // The scrimlet's own SP address is the next hop; the SP network is on-link
    // here, so these install before the sleds have booted to claim them.
    for s in cfg.sleds().iter().filter(|s| s.rack == rack && s.scrimlet) {
        let dest = s.bootstrap_subnet();
        let via = voxel_config::config::sp_scrimlet_addr(rack, s.index);
        probe("pfexec", &["route", "delete", "-inet6", &dest, &via]);
        run(&["route", "add", "-inet6", &dest, &via]).with_context(|| {
            format!("route {dest} back to scrimlet {} via {via}", s.name)
        })?;
    }

    for sp in &fleet {
        let state = dir.join("state").join(sp.base_port.to_string());
        std::fs::create_dir_all(&state)
            .with_context(|| format!("mkdir {state}"))?;
        let archive = dir.join(format!("{}.archive", board_of(sp)));
        let st = Command::new(bin.as_str())
            .args(["flash", "a", archive.as_str()])
            .env("SP_EMU_STATE_DIR", state.as_str())
            .status()
            .with_context(|| {
                format!("spawn sp-emu flash for port {}", sp.base_port)
            })?;
        if !st.success() {
            bail!("sp-emu flash failed for port {} ({st})", sp.base_port);
        }
        // Seed the gimlet's host-boot QSPI with the release's phase 1. The
        // rom is exactly the 32 MiB array sp-emu persists, and sp-emu loads
        // a pre-existing qspi-flash.bin at startup, so Hubris hashes real
        // contents and host phase 1 identifies instead of reading as blank.
        // Launch-time only: a fresh rack starts at the release baseline.
        let rom = dir.join("host-phase1.rom");
        if board_of(sp) == "gimlet" && rom.exists() {
            std::fs::copy(&rom, state.join("qspi-flash.bin")).with_context(
                || format!("seed host phase 1 for port {}", sp.base_port),
            )?;
        }
    }

    let path = manifest_path(rack);
    let body = manifest(rack, &dir, &addr, rot, &fleet);
    let tmp = dir.join("voxel-sp-emu.xml");
    std::fs::write(&tmp, body).with_context(|| format!("write {tmp}"))?;
    run(&["cp", tmp.as_str(), &path])?;
    // svccfg loses races against svc.startd's own repository writes, failing
    // with "changed unexpectedly" after importing the service but no instances.
    let mut waited = 0;
    loop {
        let imported = run(&["svccfg", "import", &path]).is_ok()
            && fleet.iter().all(|sp| instance_exists(rack, sp.base_port));
        if imported {
            break;
        }
        if waited >= IMPORT_WAIT_S {
            bail!(
                "sp-emu fleet NOT started: no SMF instances for rack {rack}; \
                 MGS would find no SPs"
            );
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        waited += 2;
    }
    let ports: Vec<u16> = fleet.iter().map(|sp| sp.base_port).collect();
    println!(
        "[voxel] rack {rack} SP fleet up on {addr} ({} SP(s): {ports:?})",
        ports.len()
    );
    Ok(())
}

/// Remove the host's route to `dest`, using whatever gateway the kernel
/// recorded. A route added with a global next hop is stored against the
/// resolved link-local neighbour, so deleting by the address we created it with
/// misses. route(8) will not delete by destination alone.
fn delete_route(dest: &str) {
    let Some(out) = probe_out("netstat", &["-rn", "-f", "inet6"]) else {
        return;
    };
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&dest) && f.len() >= 2 {
            probe("pfexec", &["route", "delete", "-inet6", dest, f[1]]);
        }
    }
}

/// Tear down one rack's fleet, leaving any other rack's alone. Best effort: a
/// destroy must not fail because a piece was already gone.
pub(crate) fn down(cfg: &voxel_config::VoxelConfig, rack: usize) {
    for s in cfg.sleds().iter().filter(|s| s.rack == rack && s.scrimlet) {
        delete_route(&s.bootstrap_subnet());
    }
    let prefix = format!("{SVC}:r{rack}sp");
    if let Some(out) = probe_out("svcs", &["-H", "-o", "fmri", SVC]) {
        for fmri in out.split_whitespace().filter(|f| f.starts_with(&prefix)) {
            probe("pfexec", &["svcadm", "disable", "-s", fmri]);
            probe("pfexec", &["svccfg", "delete", "-f", fmri]);
        }
    }
    // The addrobj name is unique per rack, so find whichever link carries it.
    let obj = addrobj(rack);
    if let Some(out) = probe_out("ipadm", &["show-addr", "-p", "-o", "addrobj"])
    {
        for a in out.split_whitespace().filter(|a| a.ends_with(&obj)) {
            probe("pfexec", &["ipadm", "delete-addr", a]);
        }
    }
    probe("pfexec", &["rm", "-f", &manifest_path(rack)]);
}

/// The host link a rack's fleet address goes on: the voxel-managed segment in
/// isolated mode, else whatever carries the host's default route, which is the
/// LAN the sleds take their own addresses on.
fn host_link(cfg: &voxel_config::VoxelConfig) -> anyhow::Result<String> {
    if cfg.external.isolated() {
        return Ok(crate::isolated_external::VNIC.to_string());
    }
    let out = probe_out("netstat", &["-rn", "-f", "inet"])
        .context("read the host routing table")?;
    for line in out.lines() {
        // Destination Gateway Flags Ref Use Interface; rows without an
        // interface (a gateway route to another subnet) are shorter.
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&"default") && f.len() >= 6 {
            return Ok(f[f.len() - 1].to_string());
        }
    }
    bail!(
        "no default route: cannot tell which host link reaches the sleds' LAN"
    )
}

/// Bring up every rack's fleet.
pub(crate) fn up_all(
    cfg: &voxel_config::VoxelConfig,
    rot: bool,
) -> anyhow::Result<()> {
    let link = host_link(cfg)?;
    for rack in 0..cfg.topology.racks() {
        up(
            cfg,
            rack,
            &crate::topo::emu_fleet(cfg, rack),
            rot,
            // `stage_sp_emu` writes the fleet into an `sp-emu` subdirectory.
            &crate::topo::sp_fleet_dir(rack).join("sp-emu"),
            &link,
        )?;
    }
    Ok(())
}

/// Tear down every rack's fleet. Best effort, so a destroy still proceeds when
/// a piece is already gone.
pub(crate) fn down_all(cfg: &voxel_config::VoxelConfig) {
    for rack in 0..cfg.topology.racks() {
        down(cfg, rack);
    }
}

#[cfg(test)]
mod tests {
    // Every pins.toml entry must parse and carry a full git sha, so a bad
    // pin fails in CI rather than at fetch time on a user's box.
    #[test]
    fn pins_parse() {
        super::pin("sp-emu").unwrap();
        super::pin("faux-mgs").unwrap();
    }

    /// PATH lookup takes the first executable regular file, skipping dirs
    /// that lack the name or hold a non-executable one.
    #[test]
    fn path_lookup_wants_an_executable() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::util::temp_dir()
            .join(format!("voxel-pathlookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b, c) = (base.join("a"), base.join("b"), base.join("c"));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(b.join("sp-emu"), "#!/bin/sh\n").unwrap();
        std::fs::write(c.join("sp-emu"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            c.join("sp-emu"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path = std::env::join_paths([&a, &b, &c]).unwrap();
        assert_eq!(
            super::find_in_path_list(&path, "sp-emu"),
            Some(c.join("sp-emu"))
        );
        assert_eq!(super::find_in_path_list(&path, "faux-mgs"), None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// A download from an unreachable host fails within the configured
    /// bounds instead of hanging in TCP retries.
    #[test]
    fn download_fails_fast_when_unreachable() {
        let dest = crate::util::temp_dir()
            .join(format!("voxel-dl-{}.part", std::process::id()));
        let f = super::Fetch {
            attempts: 2,
            connect_timeout: std::time::Duration::from_secs(1),
        };
        let t0 = std::time::Instant::now();
        let r = super::download("https://10.255.255.1/nothing", &dest, f);
        let _ = std::fs::remove_file(&dest);
        assert!(r.is_err());
        assert!(t0.elapsed() < std::time::Duration::from_secs(10));
    }
}
