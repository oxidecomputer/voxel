//! Build and run Omicron's `commtest` against a Voxel rack.
//!
//! The source defaults to the checkout matching the configured control-plane
//! image. An explicit git ref can select another Omicron era (including the
//! latest upstream `main`), while `--source` runs a local checkout as-is.

use anyhow::{Context, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::net::Ipv4Addr;
use std::process::{Command, ExitStatus, Stdio};
use voxel_config::{Network, VoxelConfig};

const DEFAULT_REPO: &str = "https://github.com/oxidecomputer/omicron";
const DEFAULT_POOL_SIZE: u32 = 16;
/// Commtest's connectivity subcommand. Voxel only derives arguments and demands
/// privileges for this one, so passthrough naming another subcommand is
/// forwarded untouched.
const RUN_SUBCOMMAND: &str = "run";
/// Group handed to commtest when `--traffic multicast`/`both` is selected but
/// no explicit group is passed through. Commtest's own `--mcast-group` has no
/// default (an empty list skips the multicast phase entirely), so voxel picks
/// an administratively scoped group (RFC 2365) to make the phase run.
///
/// TODO: IPv4 only, since commtest rejects v6 groups during validation. Pick a
/// v6 default once its own `validate_mcast` TODO to add the v6 pool buckets and
/// a v6 arm in `test_mcast_connectivity` is discharged.
const DEFAULT_MCAST_GROUP: &str = "239.1.1.1";
const HELIOS_RUSTFLAGS: &str = "--cfg svcadm_autoclear \
    -C link-arg=-R/usr/platform/oxide/lib/amd64 \
    -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable";

/// Watch-facing run artifacts, anchored to the falcon workdir like the
/// `.sp-ip-<node>` cache (`anchor_workdir` chdirs there before we run). A
/// dashboard tails the transcript and labels its pane from the mode file.
const RUN_LOG: &str = ".falcon/commtest.log";
const MODE_FILE: &str = ".falcon/commtest.mode";

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum Traffic {
    #[value(alias = "uni")]
    Unicast,
    #[value(alias = "multi")]
    Multicast,
    Both,
}

impl std::fmt::Display for Traffic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unicast => "unicast",
            Self::Multicast => "multicast",
            Self::Both => "both",
        })
    }
}

/// Where the Omicron checkout to test comes from.
pub(crate) enum Source<'a> {
    /// The commit encoded in the configured control-plane image.
    Image,
    /// An explicit commit, tag, or ref, materialized via the mirror cache.
    Reference(&'a str),
    /// A local checkout built in place, without fetching or changing it.
    Local(&'a Utf8Path),
}

pub(crate) struct Options<'a> {
    pub source: Source<'a>,
    pub rack: usize,
    pub api_override: Option<&'a str>,
    pub traffic: Traffic,
    pub no_build: bool,
    pub allow_root: bool,
    pub passthrough: &'a [String],
}

pub(crate) fn run(
    cfg: &VoxelConfig,
    options: Options<'_>,
) -> anyhow::Result<()> {
    let Options {
        source,
        rack,
        api_override,
        traffic,
        no_build,
        allow_root,
        passthrough,
    } = options;
    ensure_unprivileged(allow_root)?;
    if passthrough.is_empty()
        || passthrough.iter().any(|arg| arg == RUN_SUBCOMMAND)
    {
        ensure_icmp_privilege()?;
    }
    if rack == 0 || rack > cfg.topology.racks() {
        bail!(
            "rack must be between 1 and {} (got {rack})",
            cfg.topology.racks()
        );
    }
    ensure_default_recovery_silo(cfg)?;
    let rack_net = cfg.network.for_rack(rack - 1);
    let source = resolve_source(cfg, source)?;
    let bin = target_dir(&source).join("debug/commtest");
    let mut api = match api_override {
        Some(api) => api.to_string(),
        None => derive_api(&rack_net)?,
    };
    api.truncate(api.trim_end_matches('/').len());
    let args = commtest_args_for(
        &source,
        &rack_net,
        cfg.topology.sleds,
        traffic,
        passthrough,
        supports_multicast(&source)?,
    )?;

    if !no_build {
        eprintln!("[voxel] building Omicron commtest from {}", source);
        let mut cargo = Command::new("cargo");
        cargo.current_dir(&source).args([
            "build",
            "-p",
            "end-to-end-tests",
            "--bin",
            "commtest",
        ]);
        apply_helios_build_env(&mut cargo);
        require_success(cargo.status(), "cargo build commtest")?;
    } else if !bin.is_file() {
        bail!(
            "{} does not exist; omit --no-build or build it with \
             `cargo build -p end-to-end-tests --bin commtest`",
            bin
        );
    }

    eprintln!("[voxel] running {} {api} {}", bin, args.join(" "));
    publish_mode(traffic);
    let status = run_streamed(&bin, &api, &args)?;
    if !status.success() {
        bail!("commtest failed ({status})");
    }
    Ok(())
}

/// Record the traffic mode for dashboards. The write is best-effort, since
/// commtest still runs when the workdir is absent or unwritable.
fn publish_mode(traffic: Traffic) {
    let _ = std::fs::create_dir_all(".falcon");
    let _ = std::fs::write(MODE_FILE, format!("{traffic}\n"));
}

/// Run commtest with both streams mirrored to `RUN_LOG` (truncated per run)
/// while still reaching this terminal, so a dashboard can tail the live
/// transcript. Both copies are joined after the child exits, so the transcript
/// is complete before the exit status is reported.
fn run_streamed(
    bin: &Utf8Path,
    api: &str,
    args: &[String],
) -> anyhow::Result<ExitStatus> {
    let log = std::fs::File::create(RUN_LOG).ok();
    let log_err = log.as_ref().and_then(|f| f.try_clone().ok());
    let mut child = Command::new(bin)
        .arg(api)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", bin))?;
    let out = child.stdout.take().expect("stdout piped above");
    let err = child.stderr.take().expect("stderr piped above");
    let t_out = std::thread::spawn(move || tee(out, std::io::stdout(), log));
    let t_err =
        std::thread::spawn(move || tee(err, std::io::stderr(), log_err));
    let status = child.wait().with_context(|| format!("wait for {}", bin))?;
    join_tee(t_out, "stdout")?;
    join_tee(t_err, "stderr")?;
    Ok(status)
}

fn join_tee(
    handle: std::thread::JoinHandle<std::io::Result<()>>,
    stream: &str,
) -> anyhow::Result<()> {
    match handle.join() {
        Ok(copied) => copied.with_context(|| format!("copy commtest {stream}")),
        Err(_) => bail!("the commtest {stream} copy thread panicked"),
    }
}

/// Copy `src` to `dst` (flushing per chunk, so progress lines render live) and
/// mirror each chunk into `log` when present. Read and `dst` failures propagate
/// so a truncated transcript cannot pass for a complete run, while mirroring
/// into `log` stays best-effort. A broken downstream pipe ends the copy
/// normally, since piping voxel into a pager is not an error.
fn tee(
    mut src: impl std::io::Read,
    mut dst: impl std::io::Write,
    mut log: Option<std::fs::File>,
) -> std::io::Result<()> {
    use std::io::{ErrorKind, Write};
    let mut buf = [0u8; 8192];
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        match dst.write_all(&buf[..n]).and_then(|()| dst.flush()) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(e),
        }
        if let Some(f) = log.as_mut() {
            let _ = f.write_all(&buf[..n]).and_then(|()| f.flush());
        }
    }
}

/// Resolve the Omicron checkout to run from.
fn resolve_source(
    cfg: &VoxelConfig,
    source: Source<'_>,
) -> anyhow::Result<Utf8PathBuf> {
    match source {
        Source::Local(path) => validate_source(path),
        Source::Reference(r) => checkout(r),
        Source::Image => {
            let commit = cfg.image.cp_commit().with_context(|| {
                format!(
                    "configured image '{}' does not encode an Omicron commit; \
                     pass one (`voxel commtest <commit> -- ...`) or use --source",
                    cfg.image.cp_image()
                )
            })?;
            checkout(&commit)
        }
    }
}

fn validate_source(path: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let path = path
        .canonicalize_utf8()
        .with_context(|| format!("resolve Omicron source {}", path))?;
    let commtest = path.join("end-to-end-tests/src/bin/commtest.rs");
    if !path.join("Cargo.toml").is_file() || !commtest.is_file() {
        bail!("{} is not an Omicron checkout with {}", path, commtest);
    }
    Ok(path)
}

fn build_root() -> Utf8PathBuf {
    std::env::var("BUILD_ROOT")
        .map(Utf8PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| Utf8PathBuf::from(home).join("voxel-builds"))
        })
        .unwrap_or_else(|| Utf8PathBuf::from("."))
}

fn ensure_unprivileged(allow_root: bool) -> anyhow::Result<()> {
    // SAFETY: geteuid(2) has no preconditions and does not dereference memory.
    if unsafe { libc::geteuid() } == 0 {
        if allow_root {
            eprintln!(
                "[voxel] warning: running as root; artifacts under {} will \
                 be root-owned and may break later unprivileged runs",
                build_root()
            );
            return Ok(());
        }
        bail!(
            "`voxel commtest` must not run with effective uid 0; run it as your \
             login user with the net_icmpaccess privilege, or pass --allow-root"
        );
    }
    Ok(())
}

#[cfg(target_os = "illumos")]
fn ensure_icmp_privilege() -> anyhow::Result<()> {
    let pid = std::process::id().to_string();
    let out = Command::new("/usr/bin/ppriv")
        .args(["-v", &pid])
        .output()
        .context("inspect effective process privileges with ppriv")?;
    if !out.status.success() {
        bail!("ppriv could not inspect effective process privileges");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let effective = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("E:"))
        .map(str::trim)
        .unwrap_or_default();
    if !effective
        .split(',')
        .map(str::trim)
        .any(|privilege| privilege == "net_icmpaccess")
    {
        bail!(
            "`voxel commtest` needs net_icmpaccess in the effective privilege \
             set. Grant it to this user and start a new login session, verifying \
             with `ppriv $$`, or run under `pfexec` with --allow-root."
        );
    }
    Ok(())
}

#[cfg(not(target_os = "illumos"))]
fn ensure_icmp_privilege() -> anyhow::Result<()> {
    Ok(())
}

/// Reject a customized `[recovery_silo]` before commtest starts. Commtest signs
/// in with fixed credentials (silo `recovery`, user `recovery`, password
/// `oxide`, hardcoded since its introduction), so a customized silo otherwise
/// surfaces as an authentication failure only after commtest's 15-attempt retry
/// loop.
fn ensure_default_recovery_silo(cfg: &VoxelConfig) -> anyhow::Result<()> {
    if cfg.recovery_silo != voxel_config::RecoverySiloCfg::default() {
        bail!(
            "[recovery_silo] departs from the defaults, but commtest signs in \
             with the fixed credentials recovery/recovery/\"oxide\". Restore the \
             defaults (or drop the section) to run commtest against this rack."
        );
    }
    Ok(())
}

fn checkout(reference: &str) -> anyhow::Result<Utf8PathBuf> {
    let root = build_root().join("commtest");
    let repository = root.join("omicron.git");
    let worktrees = root.join("worktrees");
    let repo =
        std::env::var("OMICRON_REPO").unwrap_or_else(|_| DEFAULT_REPO.into());

    std::fs::create_dir_all(&root)
        .with_context(|| format!("create commtest cache {}", root))?;
    if !repository.exists() {
        eprintln!("[voxel] creating Omicron Git cache in {}", repository);
        let mut clone = Command::new("git");
        clone.args(["clone", "--mirror", "--", &repo]).arg(&repository);
        require_success(clone.status(), "git clone Omicron")?;
    } else {
        validate_repository(&repository, &repo)?;
    }

    eprintln!("[voxel] updating Omicron Git cache");
    require_success(
        git_dir_command(&repository)
            .args(["fetch", "--prune", "origin"])
            .status(),
        "git fetch Omicron",
    )?;
    let wanted = resolve_reference(&repository, reference)?;
    let source = worktrees.join(&wanted);

    if source.exists() {
        validate_worktree(&source, &wanted)?;
    } else {
        std::fs::create_dir_all(&worktrees).with_context(|| {
            format!("create worktree directory {}", worktrees)
        })?;
        require_success(
            git_dir_command(&repository).args(["worktree", "prune"]).status(),
            "git worktree prune",
        )?;
        eprintln!("[voxel] creating detached Omicron worktree {}", source);
        require_success(
            git_dir_command(&repository)
                .args(["worktree", "add", "--detach", "--"])
                .arg(&source)
                .arg(&wanted)
                .status(),
            "git worktree add",
        )?;
    }
    validate_source(&source)
}

fn validate_repository(
    repository: &Utf8Path,
    expected_remote: &str,
) -> anyhow::Result<()> {
    if git_dir_output(repository, &["rev-parse", "--is-bare-repository"])?
        != "true"
    {
        bail!("{} exists but is not a bare Git repository", repository);
    }
    // `remote get-url` applies the user's `url.<base>.insteadOf` rewrites and
    // can false-mismatch the configured URL. Read the raw remote instead.
    let actual_remote =
        git_dir_output(repository, &["config", "--get", "remote.origin.url"])?;
    if actual_remote != expected_remote {
        bail!(
            "{} uses origin '{}', but OMICRON_REPO is '{}'; use a different \
             BUILD_ROOT for the other repository",
            repository,
            actual_remote,
            expected_remote
        );
    }
    Ok(())
}

fn resolve_reference(
    repository: &Utf8Path,
    reference: &str,
) -> anyhow::Result<String> {
    let candidates = reference_candidates(reference)?;
    let mut matches = Vec::new();
    for candidate in &candidates {
        let out = git_dir_command(repository)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(format!("{candidate}^{{commit}}"))
            .output()
            .with_context(|| format!("resolve Omicron ref {reference}"))?;
        if out.status.success() {
            let commit_id =
                String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Dedup so candidates pointing at the same commit (a branch and
            // tag, or a hex-named ref and the commit it names) resolve
            // unambiguously.
            if !matches.contains(&commit_id) {
                matches.push(commit_id);
            }
        }
    }
    match matches.as_slice() {
        [commit_id] => Ok(commit_id.clone()),
        [] => bail!(
            "Omicron commit or ref '{reference}' was not found in {}",
            repository
        ),
        _ => bail!(
            "Omicron ref '{reference}' is ambiguous; use a full refs/heads/... \
             or refs/tags/... name, or a longer commit ID"
        ),
    }
}

fn reference_candidates(reference: &str) -> anyhow::Result<Vec<String>> {
    if reference == "main" {
        return Ok(vec!["refs/heads/main".into()]);
    }
    if reference.starts_with("refs/") {
        validate_refname(reference)?;
        return Ok(vec![reference.into()]);
    }

    let branch = format!("refs/heads/{reference}");
    let tag = format!("refs/tags/{reference}");
    validate_refname(&branch)?;
    validate_refname(&tag)?;
    let mut candidates = vec![branch, tag];
    // A hex string may also be an abbreviated commit ID. Named refs come
    // first, matching git's own refname-over-object-ID precedence, and a ref
    // and commit that resolve differently surface as ambiguous.
    if (4..=40).contains(&reference.len())
        && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        candidates.push(reference.into());
    }
    Ok(candidates)
}

fn validate_refname(reference: &str) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(["check-ref-format", reference])
        .status()
        .with_context(|| format!("validate Git ref {reference}"))?;
    if !status.success() {
        bail!("'{reference}' is not a valid Git ref");
    }
    Ok(())
}

fn validate_worktree(source: &Utf8Path, wanted: &str) -> anyhow::Result<()> {
    let current = git_worktree_output(source, &["rev-parse", "HEAD"])?;
    if current != wanted {
        bail!(
            "{} is registered for Omicron commit {}, but is currently at {}; \
             move it aside or select a different BUILD_ROOT",
            source,
            &wanted[..wanted.len().min(12)],
            &current[..current.len().min(12)]
        );
    }
    let dirty = git_worktree_output(
        source,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !dirty.is_empty() {
        bail!(
            "{} has tracked local changes; move them to a separate checkout and \
             use --source, or restore this cached worktree",
            source
        );
    }
    Ok(())
}

fn git_dir_command(repository: &Utf8Path) -> Command {
    let mut git = Command::new("git");
    git.arg("--git-dir").arg(repository);
    git
}

fn git_dir_output(
    repository: &Utf8Path,
    args: &[&str],
) -> anyhow::Result<String> {
    let out = git_dir_command(repository)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_worktree_output(
    source: &Utf8Path,
    args: &[&str],
) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            source,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn require_success(
    status: std::io::Result<ExitStatus>,
    operation: &str,
) -> anyhow::Result<()> {
    let status = status.with_context(|| operation.to_string())?;
    if !status.success() {
        bail!("{operation} failed ({status})");
    }
    Ok(())
}

/// The checkout's cargo target directory.
///
/// This honors `CARGO_TARGET_DIR`, resolved against the checkout to match
/// cargo's interpretation (the build runs with the checkout as its working
/// directory), falling back to `<source>/target` otherwise.
fn target_dir(source: &Utf8Path) -> Utf8PathBuf {
    match std::env::var("CARGO_TARGET_DIR") {
        Ok(dir) => source.join(dir),
        Err(_) => source.join("target"),
    }
}

/// Reproduce `voxel-image/build-cp.sh`'s build environment, so commtest links
/// against the same Helios runtime as the image it tests.
///
/// Caller-supplied flags win out: cargo ignores `RUSTFLAGS` once
/// `CARGO_ENCODED_RUSTFLAGS` is set, so either variable leaves the choice with
/// the caller.
fn apply_helios_build_env(cmd: &mut Command) {
    if cfg!(target_os = "illumos")
        && std::env::var_os("RUSTFLAGS").is_none()
        && std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_none()
    {
        cmd.env("RUSTFLAGS", HELIOS_RUSTFLAGS);
    }
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        paths.push(std::path::PathBuf::from(home).join(".cargo/bin"));
    }
    paths.push(std::path::PathBuf::from("/opt/ooce/bin"));
    if let Some(current) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&current));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        cmd.env("PATH", path);
    }
}

/// Pick the rack API base URL. Nexus's external addresses are allocated from
/// the service pool at RSS time and can move between pool members, so probe
/// the candidates for a live HTTP listener.
///
/// TLS-only racks (`--wicket-setup` uploads a self-signed certificate with
/// DNS-only SANs) are refused rather than guessed at. Therefore, commtest's
/// oxide client has no way to trust that certificate on a raw-IP URL, so
/// handing it a `https://` base would spin its API wait until the 60 minute
/// timeout.
///
/// # Errors
///
/// Returns an error when a candidate answers on 443 but none answer on 80.
/// Falls back to plain HTTP on the first candidate when nothing answers at
/// all (commtest itself waits for the API to come up).
fn derive_api(network: &Network) -> anyhow::Result<String> {
    let candidates = api_candidates(network);
    let live_on = |port: u16| {
        candidates.iter().copied().find(|&addr| {
            std::net::TcpStream::connect_timeout(
                &(addr, port).into(),
                std::time::Duration::from_millis(250),
            )
            .is_ok()
        })
    };
    if let Some(host) = live_on(80) {
        return Ok(format!("http://{host}"));
    }
    if let Some(host) = live_on(443) {
        bail!(
            "the rack API at {host} answers on 443 only (a `--wicket-setup` rack's \
             self-signed certificate); commtest cannot validate that certificate, \
             so pass --api with an endpoint it can reach"
        );
    }
    Ok(match candidates.first() {
        Some(host) => format!("http://{host}"),
        None => format!("http://{}", network.service_pool_first),
    })
}

/// Service-pool addresses in probe order.
///
/// Members external DNS has not claimed come first, since Nexus draws its
/// external address from the same pool and lands on one of those. The DNS
/// addresses follow as a fallback. The range is capped at 32 addresses so a
/// misconfigured pool cannot stall the probe.
fn api_candidates(network: &Network) -> Vec<Ipv4Addr> {
    let Ok(first) = network.service_pool_first.parse::<Ipv4Addr>() else {
        return Vec::new();
    };
    let last = network.service_pool_last.parse::<Ipv4Addr>().unwrap_or(first);
    let lo = u32::from(first);
    let hi = u32::from(last).max(lo).min(lo.saturating_add(31));
    let dns: Vec<&str> =
        network.external_dns_ips.iter().map(String::as_str).collect();
    let (rest, dns_members): (Vec<Ipv4Addr>, Vec<Ipv4Addr>) = (lo..=hi)
        .map(Ipv4Addr::from)
        .partition(|addr| !dns.contains(&addr.to_string().as_str()));
    rest.into_iter().chain(dns_members).collect()
}

/// Whether the checkout's commtest has the multicast phases, detected from its
/// source so older unicast-only eras keep working without a probe run.
fn supports_multicast(source: &Utf8Path) -> anyhow::Result<bool> {
    let source_file = source.join("end-to-end-tests/src/bin/commtest.rs");
    let text = std::fs::read_to_string(&source_file)
        .with_context(|| format!("read {}", source_file))?;
    Ok(text.contains("skip_unicast") && text.contains("mcast_group"))
}

fn commtest_args_for(
    source: &Utf8Path,
    network: &Network,
    sleds: usize,
    traffic: Traffic,
    passthrough: &[String],
    supports_multicast: bool,
) -> anyhow::Result<Vec<String>> {
    let mut args = if passthrough.is_empty() {
        vec![RUN_SUBCOMMAND.to_string()]
    } else {
        passthrough.to_vec()
    };
    if !args.iter().any(|arg| arg == RUN_SUBCOMMAND) {
        return Ok(args);
    }

    apply_traffic(source, traffic, supports_multicast, &mut args)?;

    let has_begin = args
        .iter()
        .any(|a| a == "--ip-pool-begin" || a.starts_with("--ip-pool-begin="));
    let has_end = args
        .iter()
        .any(|a| a == "--ip-pool-end" || a.starts_with("--ip-pool-end="));
    if has_begin && has_end {
        return Ok(args);
    }
    // Deriving the missing half of a partial override would pair a caller's
    // address with one computed from the service pool, yielding a range that
    // either overlaps the service pool (reallocating Nexus's own address) or
    // inverts outright.
    if has_begin || has_end {
        bail!(
            "pass both --ip-pool-begin and --ip-pool-end, or neither. Voxel \
             derives the pair from [network], and mixing the two produces a \
             range that overlaps the service pool."
        );
    }

    let (begin, end) = derive_pool(network, sleds)?;
    args.push("--ip-pool-begin".into());
    args.push(begin.to_string());
    args.push("--ip-pool-end".into());
    args.push(end.to_string());
    Ok(args)
}

fn apply_traffic(
    source: &Utf8Path,
    traffic: Traffic,
    supports_multicast: bool,
    args: &mut Vec<String>,
) -> anyhow::Result<()> {
    match traffic {
        Traffic::Unicast => {
            if has_arg(args, "--skip-unicast") {
                bail!(
                    "--traffic unicast conflicts with commtest argument --skip-unicast"
                );
            }
            // Older commtests are unicast-only and need no phase selector.
            if supports_multicast && !has_arg(args, "--skip-mcast") {
                args.push("--skip-mcast".into());
            }
        }
        Traffic::Multicast | Traffic::Both if !supports_multicast => {
            bail!(
                "{} does not support multicast commtest; select --traffic unicast \
                 or use an Omicron commit containing multicast commtest support",
                source
            );
        }
        Traffic::Multicast => {
            if has_arg(args, "--skip-mcast") {
                bail!(
                    "--traffic multicast conflicts with commtest argument --skip-mcast"
                );
            }
            if !has_arg(args, "--skip-unicast") {
                args.push("--skip-unicast".into());
            }
            add_default_mcast_group(args);
        }
        Traffic::Both => {
            if has_arg(args, "--skip-unicast") || has_arg(args, "--skip-mcast")
            {
                bail!(
                    "--traffic both conflicts with commtest --skip-unicast/--skip-mcast"
                );
            }
            add_default_mcast_group(args);
        }
    }
    Ok(())
}

fn has_arg(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name || a.starts_with(&format!("{name}=")))
}

fn add_default_mcast_group(args: &mut Vec<String>) {
    if !has_arg(args, "--mcast-group") && !has_arg(args, "--mcast-deny-group") {
        args.push("--mcast-group".into());
        args.push(DEFAULT_MCAST_GROUP.into());
    }
}

fn derive_pool(
    network: &Network,
    sleds: usize,
) -> anyhow::Result<(Ipv4Addr, Ipv4Addr)> {
    let last: Ipv4Addr =
        network.service_pool_last.parse().with_context(|| {
            format!(
                "network.service_pool_last '{}' is not IPv4",
                network.service_pool_last
            )
        })?;
    let infra: oxnet::Ipv4Net =
        network.infra_prefix.parse().with_context(|| {
            format!(
                "network.infra_prefix '{}' is not IPv4 CIDR",
                network.infra_prefix
            )
        })?;
    let network_addr = u32::from(infra.first_addr());
    let broadcast = u32::from(infra.last_addr());
    let begin = u32::from(last)
        .checked_add(1)
        .context("service pool ends at IPv4 maximum")?;
    let size = DEFAULT_POOL_SIZE.max(u32::try_from(sleds).unwrap_or(u32::MAX));
    let end = begin
        .checked_add(size.saturating_sub(1))
        .context("derived commtest pool overflows IPv4")?;
    if begin <= network_addr || end >= broadcast {
        bail!(
            "cannot derive a {size}-address commtest pool after service pool {}-{} \
             within {}; pass --ip-pool-begin/--ip-pool-end after `--`",
            network.service_pool_first,
            network.service_pool_last,
            network.infra_prefix
        );
    }
    Ok((Ipv4Addr::from(begin), Ipv4Addr::from(end)))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn derives_voxel_defaults() {
        let network = Network::default().for_rack(0);
        // `derive_api` probes the live host, so pin only the candidate order:
        // non-DNS pool members (.22-.29) ahead of the DNS pair (.20/.21).
        let candidates = api_candidates(&network);
        assert_eq!(candidates.len(), 10);
        assert_eq!(candidates[0], "198.51.100.22".parse::<Ipv4Addr>().unwrap());
        assert_eq!(candidates[8], "198.51.100.20".parse::<Ipv4Addr>().unwrap());
        assert_eq!(
            derive_pool(&network, 4).unwrap(),
            (
                "198.51.100.30".parse().unwrap(),
                "198.51.100.45".parse().unwrap()
            )
        );
        assert_eq!(
            commtest_args_for(
                Utf8Path::new("/tmp/old-omicron"),
                &network,
                4,
                Traffic::Unicast,
                &[],
                false
            )
            .unwrap(),
            [
                "run",
                "--ip-pool-begin",
                "198.51.100.30",
                "--ip-pool-end",
                "198.51.100.45"
            ]
        );
    }

    #[test]
    fn preserves_explicit_args_and_cleanup() {
        let network = Network::default();
        let explicit = vec![
            "--api-timeout".into(),
            "5m".into(),
            "run".into(),
            "--ip-pool-begin=203.0.113.10".into(),
            "--ip-pool-end".into(),
            "203.0.113.20".into(),
            "--test-duration".into(),
            "5s".into(),
        ];
        assert_eq!(
            commtest_args_for(
                Utf8Path::new("/tmp/old-omicron"),
                &network,
                4,
                Traffic::Unicast,
                &explicit,
                false
            )
            .unwrap(),
            explicit
        );
        assert_eq!(
            commtest_args_for(
                Utf8Path::new("/tmp/old-omicron"),
                &network,
                4,
                Traffic::Unicast,
                &["cleanup".into()],
                false
            )
            .unwrap(),
            ["cleanup"]
        );
    }

    #[test]
    fn rejects_partial_pool_override() {
        let network = Network::default();
        for partial in [
            vec!["run".to_string(), "--ip-pool-begin=203.0.113.10".into()],
            vec![
                "run".to_string(),
                "--ip-pool-end".into(),
                "203.0.113.20".into(),
            ],
        ] {
            assert!(
                commtest_args_for(
                    Utf8Path::new("/tmp/old-omicron"),
                    &network,
                    4,
                    Traffic::Unicast,
                    &partial,
                    false
                )
                .is_err()
            );
        }
    }

    #[test]
    fn selects_multicast_phases() {
        let network = Network::default();
        let multicast = commtest_args_for(
            Utf8Path::new("/tmp/new-omicron"),
            &network,
            4,
            Traffic::Multicast,
            &[],
            true,
        )
        .unwrap();
        assert!(multicast.contains(&"--skip-unicast".into()));
        assert!(multicast.contains(&DEFAULT_MCAST_GROUP.into()));

        let both = commtest_args_for(
            Utf8Path::new("/tmp/new-omicron"),
            &network,
            4,
            Traffic::Both,
            &[],
            true,
        )
        .unwrap();
        assert!(!both.contains(&"--skip-unicast".into()));
        assert!(!both.contains(&"--skip-mcast".into()));
        assert!(both.contains(&DEFAULT_MCAST_GROUP.into()));
    }

    #[test]
    fn rejects_multicast_on_old_commtest() {
        assert!(
            commtest_args_for(
                Utf8Path::new("/tmp/old-omicron"),
                &Network::default(),
                4,
                Traffic::Multicast,
                &[],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_pool_past_broadcast() {
        let network = Network {
            infra_prefix: "192.0.2.0/29".into(),
            service_pool_first: "192.0.2.2".into(),
            service_pool_last: "192.0.2.6".into(),
            ..Network::default()
        };
        assert!(derive_pool(&network, 4).is_err());
    }

    #[test]
    fn resolves_only_explicit_git_reference_forms() {
        assert_eq!(reference_candidates("main").unwrap(), ["refs/heads/main"]);
        assert_eq!(
            reference_candidates("43bb5af").unwrap(),
            ["refs/heads/43bb5af", "refs/tags/43bb5af", "43bb5af"]
        );
        assert_eq!(
            reference_candidates("release/test").unwrap(),
            ["refs/heads/release/test", "refs/tags/release/test"]
        );
        assert!(reference_candidates("not a ref").is_err());
    }
}
