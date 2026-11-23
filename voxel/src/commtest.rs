// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Run Omicron's `commtest` against a Voxel rack.
//!
//! By default this is omicron's `commtest` library, linked into voxel and run
//! as a `voxel commtest-exec` child so its transcript can be mirrored for
//! dashboards. Voxel hands it HTTP clients that resolve the rack's API name
//! themselves and trust the rack's CA, so the host needs no `/etc/hosts`
//! entry and no toolchain.
//!
//! `--source` instead builds and runs a local Omicron checkout's commtest, for
//! versions the port does not have yet (the multicast phases). `--builder`
//! runs the commtest a builder compiled for its commit (`builder::COMMTEST`),
//! for hosts without a toolchain.

use anyhow::{Context, bail, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use std::net::Ipv4Addr;
use std::process::{Command, ExitStatus, Stdio};
use voxel_config::{Network, VoxelConfig};

use crate::multicast;

const DEFAULT_POOL_SIZE: u32 = 16;
/// commtest's source in an omicron checkout.
pub(crate) const COMMTEST_SRC: &str = "end-to-end-tests/src/bin/commtest.rs";
/// Commtest's connectivity subcommand. Voxel only derives arguments and demands
/// privileges for this one, so passthrough naming another subcommand is
/// forwarded untouched.
const RUN_SUBCOMMAND: &str = "run";
/// Group handed to commtest when `--traffic multicast`/`both` is selected but
/// no explicit group is passed through. Commtest's own `--mcast-group` has no
/// default (an empty list skips the multicast phase entirely), so voxel picks
/// an administratively scoped group (RFC 2365) to make the phase run.
///
/// TODO: IPv4 only; commtest rejects v6 groups during validation. Pick a
/// v6 default after adding v6 pool buckets in `validate_mcast` and a v6 arm
/// in `test_mcast_connectivity`.
pub(crate) const DEFAULT_MCAST_GROUP: &str = "239.1.1.1";
/// The flags `image create` builds omicron with, so commtest links against
/// the same Helios runtime as the image it tests.
const HELIOS_RUSTFLAGS: &str = crate::cpbuild::OMICRON_RUSTFLAGS;
/// What the built-in commtest is called in messages.
const BUILTIN: &str = "the built-in commtest";

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

pub(crate) struct Options<'a> {
    /// The voxel.toml in use, handed to the `commtest-exec` child.
    pub config_path: &'a Utf8Path,
    /// A local Omicron checkout whose commtest to build and run instead of
    /// the built-in one.
    pub source: Option<&'a Utf8Path>,
    /// A builder's commtest dir (`commtest` and its `commtest.rs`), run
    /// as-is instead of the built-in one.
    pub prebuilt: Option<&'a Utf8Path>,
    pub rack: usize,
    pub api_override: Option<&'a str>,
    pub traffic: Traffic,
    pub no_build: bool,
    pub passthrough: &'a [String],
    /// Add (or refresh) a marked `/etc/hosts` line for the API name when it
    /// does not resolve, rather than failing. Upstream commtest only uses the
    /// system resolver.
    pub hosts_entry: bool,
}

/// Where and how to reach the rack's API.
struct Api {
    url: String,
    /// For a TLS rack: the silo name and the Nexus address it resolves to
    /// in the rack's own DNS.
    resolve: Option<(String, String)>,
    /// The rack's CA, for a TLS rack.
    ca: Option<Utf8PathBuf>,
}

pub(crate) async fn run(
    cfg: &VoxelConfig,
    name: &str,
    options: Options<'_>,
) -> anyhow::Result<()> {
    let Options {
        config_path,
        source,
        prebuilt,
        rack,
        api_override,
        traffic,
        no_build,
        passthrough,
        hosts_entry,
    } = options;
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
    if source.is_none() && no_build {
        bail!(
            "--no-build applies to --source; the built-in commtest needs no build"
        );
    }
    ensure!(
        source.is_none() || prebuilt.is_none(),
        "--source and --builder both name a commtest; pick one"
    );
    ensure_default_recovery_silo(cfg)?;
    let rack_net = cfg.network.for_rack(rack - 1);
    let mut api = match api_override {
        Some(url) => Api { url: url.to_string(), resolve: None, ca: None },
        None => derive_api(&rack_net, &cfg.recovery_silo.silo_name, rack - 1)?,
    };
    api.url.truncate(api.url.trim_end_matches('/').len());

    if source.is_none() && prebuilt.is_none() {
        let args = commtest_args_for(
            BUILTIN,
            &rack_net,
            cfg.topology.sleds,
            traffic,
            passthrough,
            false,
        )?;
        ensure_mcast_plumbing(
            cfg,
            name,
            &preflight_groups(traffic, &args),
            rack,
        )
        .await?;
        let exe = std::env::current_exe().context("locate the voxel binary")?;
        let exe = Utf8PathBuf::try_from(exe).context("non-utf8 voxel path")?;
        let workdir = std::env::current_dir().context("current dir")?;
        let mut argv: Vec<String> = vec![
            "--config".into(),
            config_path.to_string(),
            "--workdir".into(),
            workdir.to_string_lossy().into_owned(),
            "commtest-exec".into(),
        ];
        if let Some(ca) = &api.ca {
            argv.extend(["--cacert".into(), ca.to_string()]);
        }
        if let Some((host, ip)) = &api.resolve {
            argv.extend(["--resolve".into(), format!("{host}={ip}")]);
        }
        argv.push("--".into());
        let shown = argv.len();
        argv.push(api.url.clone());
        argv.extend(args);
        eprintln!(
            "[voxel] running {BUILTIN} ({}): {}",
            argv[5..shown - 1].join(" "),
            argv[shown..].join(" ")
        );
        publish_mode(traffic);
        let status = run_streamed(&exe, &argv, None)?;
        if !status.success() {
            bail!("commtest failed ({status})");
        }
        return Ok(());
    }

    let (bin, label, multicast) = match (source, prebuilt) {
        (Some(source), _) => {
            let (bin, multicast) = build_source(source, no_build)?;
            (bin, source.to_string(), multicast)
        }
        (None, Some(dir)) => {
            let bin = dir.join("commtest");
            ensure!(
                bin.is_file(),
                "{bin} does not exist; build it with `voxel builder commtest`"
            );
            let multicast = prebuilt_has_multicast(dir)?;
            (bin, format!("the commtest in {dir}"), multicast)
        }
        (None, None) => unreachable!("the built-in commtest returned above"),
    };
    // Upstream commtest has no resolver override: the name must resolve here.
    if let Some((host, ip)) = &api.resolve {
        use std::net::ToSocketAddrs;
        if (host.as_str(), 443).to_socket_addrs().is_err() {
            if hosts_entry {
                set_hosts_entry(host, ip)?;
            } else {
                bail!(
                    "{host} does not resolve on this host. Omicron's commtest \
                     (from --source or --builder) uses the system resolver, and \
                     the rack serves its API over TLS by name only. Add it to \
                     /etc/hosts:\n\n  {ip} {host}\n\nor run without them \
                     (the built-in commtest resolves it itself)"
                );
            }
        }
    }
    let args = commtest_args_for(
        &label,
        &rack_net,
        cfg.topology.sleds,
        traffic,
        passthrough,
        multicast,
    )?;
    ensure_mcast_plumbing(cfg, name, &preflight_groups(traffic, &args), rack)
        .await?;
    let mut argv = vec![api.url.clone()];
    argv.extend(args);
    eprintln!("[voxel] running {} {}", bin, argv.join(" "));
    publish_mode(traffic);
    // Upstream commtest's rustls client takes extra roots from SSL_CERT_FILE.
    let status = run_streamed(&bin, &argv, api.ca.as_deref())?;
    if !status.success() {
        bail!("commtest failed ({status})");
    }
    Ok(())
}

/// Build (unless `no_build`) a checkout's commtest: its binary and whether it
/// has the multicast phases.
fn build_source(
    source: &Utf8Path,
    no_build: bool,
) -> anyhow::Result<(Utf8PathBuf, bool)> {
    let source = validate_source(source)?;
    let bin = target_dir(&source).join("debug/commtest");
    if !no_build {
        refuse_root_build(&source)?;
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
    let multicast = source_has_multicast(&source.join(COMMTEST_SRC))?;
    Ok((bin, multicast))
}

/// Upsert `/etc/hosts`'s voxel-marked line for `host`, so upstream commtest
/// can reach a TLS rack by name. A relaunch moves the address; the old line
/// is replaced, never duplicated.
fn set_hosts_entry(host: &str, ip: &str) -> anyhow::Result<()> {
    const HOSTS: &str = "/etc/hosts";
    let text = std::fs::read_to_string(HOSTS)
        .with_context(|| format!("read {HOSTS}"))?;
    let updated = hosts_with_entry(&text, host, ip);
    std::fs::write(HOSTS, updated).with_context(|| format!("write {HOSTS}"))?;
    eprintln!("[voxel] added `{ip} {host}` to {HOSTS} for upstream commtest");
    Ok(())
}

const HOSTS_MARK: &str = "# voxel commtest";

fn hosts_with_entry(text: &str, host: &str, ip: &str) -> String {
    let mut out: String = text
        .lines()
        .filter(|l| {
            !(l.ends_with(HOSTS_MARK)
                && l.split_whitespace().nth(1) == Some(host))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{ip}\t{host}\t{HOSTS_MARK}\n"));
    out
}

/// `voxel commtest-exec`: run omicron's commtest library with `args` (its own
/// command line, without the program name), building every HTTP client it
/// makes with `resolve` (`HOST=IP`) and `cacert` applied.
pub(crate) async fn exec(
    resolve: Option<&str>,
    cacert: Option<&Utf8Path>,
    args: &[String],
) -> anyhow::Result<()> {
    use clap::Parser;
    let cli = commtest::Cli::try_parse_from(
        std::iter::once("commtest").chain(args.iter().map(String::as_str)),
    )?;
    let resolve = resolve
        .map(|r| -> anyhow::Result<(String, std::net::SocketAddr)> {
            let (host, ip) = r
                .split_once('=')
                .with_context(|| format!("--resolve {r:?} is not HOST=IP"))?;
            let ip: std::net::IpAddr =
                ip.parse().with_context(|| format!("--resolve {r:?}"))?;
            // Port 0 keeps the URL's own port.
            Ok((host.to_string(), std::net::SocketAddr::new(ip, 0)))
        })
        .transpose()?;
    let ca = cacert
        .map(|path| -> anyhow::Result<reqwest::Certificate> {
            let pem =
                std::fs::read(path).with_context(|| format!("read {path}"))?;
            reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("parse {path}"))
        })
        .transpose()?;
    let client_builder = move || {
        let mut b = reqwest::ClientBuilder::new();
        if let Some((host, addr)) = &resolve {
            b = b.resolve(host, *addr);
        }
        if let Some(ca) = &ca {
            b = b.add_root_certificate(ca.clone());
        }
        b
    };
    commtest::run_cli_with(&cli, &client_builder).await
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
    argv: &[String],
    ca: Option<&Utf8Path>,
) -> anyhow::Result<ExitStatus> {
    let log = std::fs::File::create(RUN_LOG).ok();
    let log_err = log.as_ref().and_then(|f| f.try_clone().ok());
    let mut cmd = Command::new(bin);
    if let Some(ca) = ca {
        cmd.env("SSL_CERT_FILE", ca);
    }
    let mut child = cmd
        .args(argv)
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

fn validate_source(path: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let path = path
        .canonicalize_utf8()
        .with_context(|| format!("resolve Omicron source {}", path))?;
    let commtest = path.join(COMMTEST_SRC);
    if !path.join("Cargo.toml").is_file() || !commtest.is_file() {
        bail!("{} is not an Omicron checkout with {}", path, commtest);
    }
    Ok(path)
}

/// A root `cargo build` in a checkout leaves root-owned build output that
/// breaks the owner's next build there, so build it as its owner instead.
/// Running commtest as root is fine; only the build is refused.
fn refuse_root_build(source: &Utf8Path) -> anyhow::Result<()> {
    // SAFETY: geteuid(2) has no preconditions and does not dereference memory.
    if unsafe { libc::geteuid() } == 0 {
        bail!(
            "refusing to build {source} as root: the build output would be \
             root-owned and break your next build there. Build it as yourself \
             (`cargo build -p end-to-end-tests --bin commtest` in {source}), \
             then rerun with --no-build"
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
             with `ppriv $$`, or run it under `pfexec`."
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

/// Reproduce `image create`'s omicron build environment, so commtest links
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

/// The rack API, derived from the configuration alone: its base URL, the
/// address its name resolves to, and the CA that issued its certificate.
///
/// A launch sets the rack up through wicketd's commission API, which serves
/// the API over TLS on 443 only, under a certificate from the per-rack CA the
/// launch saves (`.falcon/rack<N>-ca.pem` in the workdir). Nexus picks its
/// certificate from the TLS SNI name and refuses a handshake without one, so
/// the API is reached by the silo's name, `<silo>.sys.<rack zone>`. That name
/// is looked up in the rack's own external DNS, so the host's resolver never
/// has to know the zone.
///
/// Nothing is probed or guessed. A rack launched with `--init-rss` (plain
/// HTTP, no CA) is not described by the configuration, so it takes `--api`.
///
/// # Errors
///
/// When the rack's CA is missing, or its DNS has no address for the silo.
fn derive_api(
    network: &Network,
    silo: &str,
    rack: usize,
) -> anyhow::Result<Api> {
    let ca = crate::commission::rack_ca_path(rack);
    // Absolute, for messages and for a child whose directory may differ.
    let ca = std::env::current_dir()
        .ok()
        .and_then(|d| Utf8PathBuf::try_from(d).ok())
        .map(|d| d.join(&ca))
        .unwrap_or(ca);
    if !ca.is_file() {
        bail!(
            "rack{n} has no CA at {ca}, where a launch saves it. The rack \
             was launched by a voxel that did not save its CA (relaunch it), \
             or with --init-rss (plain HTTP: pass --api http://<nexus ip>)",
            n = rack + 1
        );
    }
    let host = format!("{silo}.sys.{}", network.dns_zone);
    let dns = network
        .external_dns_ips
        .first()
        .context("no external DNS address for this rack")?;
    let ip = crate::net::dig_a(dns, &host)
        .context("`dig` is needed to look the rack API up in its DNS")?
        .into_iter()
        .next()
        .with_context(|| {
            format!(
                "the rack's DNS ({dns}) has no address for {host}; is the \
                 rack up? (`voxel status`)"
            )
        })?;
    Ok(Api {
        url: format!("https://{host}"),
        resolve: Some((host, ip)),
        ca: Some(ca),
    })
}

/// Whether a builder's commtest (`--builder`) has the multicast phases.
pub(crate) fn prebuilt_has_multicast(dir: &Utf8Path) -> anyhow::Result<bool> {
    source_has_multicast(&dir.join("commtest.rs"))
}

/// Whether the checkout's commtest has the multicast phases, detected from its
/// source so older unicast-only eras keep working without a probe run.
fn source_has_multicast(source_file: &Utf8Path) -> anyhow::Result<bool> {
    let text = std::fs::read_to_string(source_file)
        .with_context(|| format!("read {}", source_file))?;
    Ok(text.contains("skip_unicast") && text.contains("mcast_group"))
}

fn commtest_args_for(
    source: &str,
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
    apply_loss_tolerance(&mut args)?;

    let has_begin = has_arg(&args, "--ip-pool-begin");
    let has_end = has_arg(&args, "--ip-pool-end");
    if has_begin && has_end {
        check_explicit_pool(network, &args)?;
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
             range that either overlaps the service pool or becomes inverted."
        );
    }

    let (begin, end) = derive_pool(network, sleds)?;
    args.push("--ip-pool-begin".into());
    args.push(begin.to_string());
    args.push("--ip-pool-end".into());
    args.push(end.to_string());
    Ok(args)
}

/// Refuse an explicit pool that is inverted or overlaps the service pool:
/// commtest's floating IPs would collide with the rack services.
fn check_explicit_pool(
    network: &Network,
    args: &[String],
) -> anyhow::Result<()> {
    let ip =
        |flag| arg_value(args, flag).and_then(|v| v.parse::<Ipv4Addr>().ok());
    let (Some(begin), Some(end)) = (ip("--ip-pool-begin"), ip("--ip-pool-end"))
    else {
        return Ok(());
    };
    if begin > end {
        bail!(
            "--ip-pool-begin {begin} is greater than --ip-pool-end {end}: the \
             range's first address must be less than or equal to the last"
        );
    }
    let first: Option<Ipv4Addr> = network.service_pool_first.parse().ok();
    let last: Option<Ipv4Addr> = network.service_pool_last.parse().ok();
    if let (Some(first), Some(last)) = (first, last)
        && begin <= last
        && first <= end
    {
        bail!(
            "--ip-pool-begin {begin} and --ip-pool-end {end} overlap the \
             service pool {first}-{last}; commtest's floating IPs would collide \
             with the rack services. Pick a range outside it."
        );
    }
    Ok(())
}

fn apply_traffic(
    source: &str,
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
            if has_arg(args, "--mcast-deny-group")
                && has_arg(args, "--skip-mcast")
            {
                bail!(
                    "--mcast-deny-group conflicts with commtest argument --skip-mcast"
                );
            }
            // Older commtests are unicast-only and need no phase selector.
            if supports_multicast
                && !has_arg(args, "--skip-mcast")
                && !has_arg(args, "--mcast-deny-group")
            {
                args.push("--skip-mcast".into());
            }
        }
        Traffic::Multicast | Traffic::Both if !supports_multicast => {
            bail!(
                "{} does not support multicast commtest; select --traffic unicast \
                 or pass --source with an Omicron checkout that has multicast \
                 commtest support",
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

/// commtest's run defaults, for sizing the loss tolerance.
const DEFAULT_TEST_DURATION: &str = "100s";
const DEFAULT_PACKET_RATE: u64 = 10;
/// Share of each probe's ICMP packets that may be lost. commtest's own
/// default is zero, which an emulated rack's software switch can't promise.
const LOSS_TOLERANCE_PCT: u64 = 5;

/// Add `--icmp-loss-tolerance` (a packet count) as 5% of the packets the run
/// sends, unless the caller set it.
fn apply_loss_tolerance(args: &mut Vec<String>) -> anyhow::Result<()> {
    if has_arg(args, "--icmp-loss-tolerance") {
        return Ok(());
    }
    let duration =
        arg_value(args, "--test-duration").unwrap_or(DEFAULT_TEST_DURATION);
    let duration = humantime::parse_duration(duration)
        .with_context(|| format!("parse --test-duration {duration:?}"))?;
    let rate = match arg_value(args, "--packet-rate") {
        Some(r) => r
            .parse::<u64>()
            .with_context(|| format!("parse --packet-rate {r:?}"))?,
        None => DEFAULT_PACKET_RATE,
    };
    let packets = duration.as_secs() * rate;
    let tolerance = packets * LOSS_TOLERANCE_PCT / 100;
    args.push("--icmp-loss-tolerance".into());
    args.push(tolerance.to_string());
    Ok(())
}

/// The value of `--name value` or `--name=value`.
fn arg_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let eq = format!("{name}=");
    args.iter().enumerate().find_map(|(i, a)| {
        if a == name {
            args.get(i + 1).map(String::as_str)
        } else {
            a.strip_prefix(&eq)
        }
    })
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

/// The groups the multicast preflight covers.
///
/// No groups are checked when `--skip-mcast` is present, or when
/// `--traffic unicast` is used without `--mcast-deny-group`. An explicit
/// `--mcast-group` does not override either condition.
fn preflight_groups(traffic: Traffic, args: &[String]) -> Vec<String> {
    if (matches!(traffic, Traffic::Unicast)
        && !has_arg(args, "--mcast-deny-group"))
        || has_arg(args, "--skip-mcast")
    {
        return Vec::new();
    }
    mcast_groups(args)
}

/// Collect the group addresses from the assembled commtest arguments,
/// defaulted or passed through, in either `--flag value` or `--flag=value`
/// spellings.
///
/// Deny groups are included; their members must receive no traffic.
fn mcast_groups(args: &[String]) -> Vec<String> {
    const FLAGS: [&str; 2] = ["--mcast-group", "--mcast-deny-group"];
    let mut out = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(group) =
            FLAGS.iter().find_map(|f| arg.strip_prefix(&format!("{f}=")))
        {
            out.push(group.to_string());
        } else if FLAGS.contains(&arg.as_str())
            && let Some(group) = rest.next()
        {
            out.push(group.clone());
        }
    }
    out
}

/// Refuse to start a multicast run before the host plumbing is in place.
///
/// Without this check, missing host routes or router assignments would cause
/// commtest receive timeouts.
///
/// This check covers every fabric router in the topology.
async fn ensure_mcast_plumbing(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
    rack: usize,
) -> anyhow::Result<()> {
    if groups.is_empty() {
        return Ok(());
    }
    let groups = groups
        .iter()
        .map(|group| {
            group.parse::<multicast::GroupSpec>().with_context(|| {
                format!("multicast group '{group}' from the commtest args")
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let multicast::MissingPlumbing { missing, unreached } =
        multicast::missing_plumbing(cfg, name, &groups, rack - 1).await?;

    // A groupless `up` command already covers the default group, but explicit
    // --group flags suppress the default.
    let default_only = matches!(groups.as_slice(),
        [only] if only.to_string() == DEFAULT_MCAST_GROUP);
    let flags: String = if default_only {
        String::new()
    } else {
        groups.iter().map(|g| format!(" --group {g}")).collect()
    };

    ensure!(
        missing.is_empty(),
        "host multicast plumbing is incomplete:\n  {}\nrun `voxel network multicast up{flags}` \
         first",
        missing.join("\n  ")
    );

    ensure!(
        unreached.is_empty(),
        "multicast group(s) {} have no ingress path into rack {rack}. \
         Steer one there, e.g. `voxel network multicast up --group {} \
         --steer {}=<router>:<one of rack {rack}'s switches>`",
        unreached
            .iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        unreached[0],
        unreached[0],
    );
    Ok(())
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
mod tests {

    #[test]
    fn hosts_entry_replaces_its_own_line_only() {
        let hosts = "127.0.0.1\tlocalhost\n\
                     10.0.0.9\trecovery.sys.oxide.test\t# voxel commtest\n\
                     10.0.0.7\trecovery.sys.oxide.test\n";
        let out =
            hosts_with_entry(hosts, "recovery.sys.oxide.test", "10.0.0.5");
        assert_eq!(
            out,
            "127.0.0.1\tlocalhost\n\
             10.0.0.7\trecovery.sys.oxide.test\n\
             10.0.0.5\trecovery.sys.oxide.test\t# voxel commtest\n"
        );
    }

    use super::*;

    #[test]
    fn derives_voxel_defaults() {
        let network = Network::default().for_rack(0);
        assert_eq!(
            derive_pool(&network, 4).unwrap(),
            (
                "198.51.100.30".parse().unwrap(),
                "198.51.100.45".parse().unwrap()
            )
        );
        assert_eq!(
            commtest_args_for(
                "/tmp/old-omicron",
                &network,
                4,
                Traffic::Unicast,
                &[],
                false
            )
            .unwrap(),
            [
                "run",
                "--icmp-loss-tolerance",
                "50",
                "--ip-pool-begin",
                "198.51.100.30",
                "--ip-pool-end",
                "198.51.100.45"
            ]
        );
    }

    #[test]
    fn loss_tolerance_scales_and_yields() {
        let tolerance = |extra: &[&str]| {
            let mut args = vec!["run".to_string()];
            args.extend(extra.iter().map(|a| a.to_string()));
            apply_loss_tolerance(&mut args).unwrap();
            arg_value(&args, "--icmp-loss-tolerance").map(str::to_string)
        };
        assert_eq!(tolerance(&[]).as_deref(), Some("50"));
        assert_eq!(
            tolerance(&["--test-duration", "10m", "--packet-rate=20"])
                .as_deref(),
            Some("600")
        );
        assert_eq!(
            tolerance(&["--icmp-loss-tolerance", "3"]).as_deref(),
            Some("3")
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
            "--icmp-loss-tolerance".into(),
            "0".into(),
        ];
        assert_eq!(
            commtest_args_for(
                "/tmp/old-omicron",
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
                "/tmp/old-omicron",
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
                    "/tmp/old-omicron",
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
            "/tmp/new-omicron",
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
            "/tmp/new-omicron",
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
        for traffic in [Traffic::Multicast, Traffic::Both] {
            assert!(
                commtest_args_for(
                    "/tmp/old-omicron",
                    &Network::default(),
                    4,
                    traffic,
                    &[],
                    false
                )
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_pool_passes_through_untouched() {
        let network = Network::default();
        let explicit = vec![
            "run".to_string(),
            "--ip-pool-begin=203.0.113.10".into(),
            "--ip-pool-end=203.0.113.20".into(),
            "--icmp-loss-tolerance=0".into(),
        ];
        let args = commtest_args_for(
            "/tmp/old-omicron",
            &network,
            4,
            Traffic::Unicast,
            &explicit,
            false,
        )
        .unwrap();
        assert_eq!(args, explicit);
    }

    #[test]
    fn rejects_pool_overlapping_the_service_pool() {
        let network = Network::default();
        for (begin, end) in [
            ("198.51.100.20", "198.51.100.29"),
            ("198.51.100.10", "198.51.100.22"),
            ("198.51.100.25", "198.51.100.40"),
            ("198.51.100.10", "198.51.100.40"),
        ] {
            let explicit = vec![
                "run".to_string(),
                format!("--ip-pool-begin={begin}"),
                format!("--ip-pool-end={end}"),
            ];
            assert!(
                commtest_args_for(
                    "/tmp/old-omicron",
                    &network,
                    4,
                    Traffic::Unicast,
                    &explicit,
                    false,
                )
                .is_err(),
                "{begin}-{end} overlaps the service pool"
            );
        }
    }

    #[test]
    fn rejects_inverted_pool_override() {
        let network = Network::default();
        for inverted in [
            vec![
                "run".to_string(),
                "--ip-pool-begin=203.0.113.20".into(),
                "--ip-pool-end=203.0.113.10".into(),
            ],
            vec![
                "run".to_string(),
                "--ip-pool-begin".into(),
                "203.0.113.20".into(),
                "--ip-pool-end".into(),
                "203.0.113.10".into(),
            ],
        ] {
            assert!(
                commtest_args_for(
                    "/tmp/old-omicron",
                    &network,
                    4,
                    Traffic::Unicast,
                    &inverted,
                    false
                )
                .is_err()
            );
        }
    }

    #[test]
    fn preflight_reads_back_commtest_groups() {
        // The preflight has to cover passed-through groups, in either flag
        // spelling and not just the default this module appends.
        let network = Network::default();
        let args = |traffic, passthrough: &[String]| {
            commtest_args_for(
                "/tmp/new-omicron",
                &network,
                4,
                traffic,
                passthrough,
                true,
            )
            .unwrap()
        };
        assert!(mcast_groups(&args(Traffic::Unicast, &[])).is_empty());
        assert_eq!(
            mcast_groups(&args(Traffic::Multicast, &[])),
            [DEFAULT_MCAST_GROUP]
        );
        let passthrough = [
            "--mcast-group".to_string(),
            "224.0.2.5".to_string(),
            "--mcast-group=224.0.2.6".to_string(),
        ];
        assert_eq!(
            mcast_groups(&args(Traffic::Multicast, &passthrough)),
            ["224.0.2.5", "224.0.2.6"]
        );
    }

    #[test]
    fn unicast_never_preflights_a_passthrough_group() {
        let network = Network::default();
        let passthrough = vec![
            "run".to_string(),
            "--mcast-group".to_string(),
            "224.0.2.5".to_string(),
        ];
        let args = commtest_args_for(
            "/tmp/new-omicron",
            &network,
            4,
            Traffic::Unicast,
            &passthrough,
            true,
        )
        .unwrap();
        // The group makes it through to commtest, but it's ignored under
        // the appended --skip-mcast.
        assert!(args.contains(&"--skip-mcast".into()));
        assert_eq!(mcast_groups(&args), ["224.0.2.5"]);
        assert!(preflight_groups(Traffic::Unicast, &args).is_empty());

        // The same passthrough under "--traffic both" is preflighted.
        let both = commtest_args_for(
            "/tmp/new-omicron",
            &network,
            4,
            Traffic::Both,
            &passthrough,
            true,
        )
        .unwrap();
        assert_eq!(preflight_groups(Traffic::Both, &both), ["224.0.2.5"]);
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
}
