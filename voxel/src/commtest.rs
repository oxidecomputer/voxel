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
//! versions the port does not have yet (the multicast phases).

use anyhow::{Context, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::net::Ipv4Addr;
use std::process::{Command, ExitStatus, Stdio};
use voxel_config::{Network, VoxelConfig};

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
    pub rack: usize,
    pub api_override: Option<&'a str>,
    pub traffic: Traffic,
    pub no_build: bool,
    pub allow_root: bool,
    pub passthrough: &'a [String],
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

pub(crate) fn run(
    cfg: &VoxelConfig,
    options: Options<'_>,
) -> anyhow::Result<()> {
    let Options {
        config_path,
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
    if source.is_none() && no_build {
        bail!(
            "--no-build applies to --source; the built-in commtest needs no build"
        );
    }
    ensure_default_recovery_silo(cfg)?;
    let rack_net = cfg.network.for_rack(rack - 1);
    let mut api = match api_override {
        Some(url) => Api { url: url.to_string(), resolve: None, ca: None },
        None => derive_api(&rack_net, &cfg.recovery_silo.silo_name, rack - 1)?,
    };
    api.url.truncate(api.url.trim_end_matches('/').len());

    let Some(source) = source else {
        let args = commtest_args_for(
            BUILTIN,
            &rack_net,
            cfg.topology.sleds,
            traffic,
            passthrough,
            false,
        )?;
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
    };

    let source = validate_source(source)?;
    let bin = target_dir(&source).join("debug/commtest");
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
    // Upstream commtest has no resolver override: the name must resolve here.
    if let Some((host, ip)) = &api.resolve {
        use std::net::ToSocketAddrs;
        if (host.as_str(), 443).to_socket_addrs().is_err() {
            bail!(
                "{host} does not resolve on this host. Omicron's commtest (from \
                 --source) uses the system resolver, and the rack serves its \
                 API over TLS by name only. Add it to /etc/hosts:\n\n  \
                 {ip} {host}\n\nor run without --source (the built-in \
                 commtest resolves it itself)"
            );
        }
    }
    let args = commtest_args_for(
        source.as_str(),
        &rack_net,
        cfg.topology.sleds,
        traffic,
        passthrough,
        supports_multicast(&source)?,
    )?;
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
    let commtest = path.join("end-to-end-tests/src/bin/commtest.rs");
    if !path.join("Cargo.toml").is_file() || !commtest.is_file() {
        bail!("{} is not an Omicron checkout with {}", path, commtest);
    }
    Ok(path)
}

fn ensure_unprivileged(allow_root: bool) -> anyhow::Result<()> {
    // SAFETY: geteuid(2) has no preconditions and does not dereference memory.
    if unsafe { libc::geteuid() } == 0 {
        if allow_root {
            eprintln!(
                "[voxel] warning: running as root; the files commtest writes \
                 (its report, and a --source build) will be root-owned and may \
                 break later unprivileged runs"
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

/// Pick the rack API: its base URL, and for a TLS rack the name mapping and
/// CA a client needs.
///
/// With the rack's CA (saved by a commission-driven launch, the default) the
/// API is HTTPS on 443 only, reached by the silo's DNS name: Nexus picks its
/// certificate from the TLS SNI name and refuses a handshake without one,
/// which is every handshake to a bare IP. The name is looked up in the rack's
/// own external DNS, so the host's resolver never has to know the zone.
///
/// Without the CA (an `--init-rss` rack), Nexus serves plain HTTP. Its
/// external addresses come from the service pool, so probe the candidates
/// for a live listener.
///
/// # Errors
///
/// With the CA, when the rack's DNS does not answer for the silo name.
/// Without it, when a candidate answers on 443 but none on 80: a TLS rack
/// launched before voxel kept its CA. Falls back to the first candidate when
/// nothing answers at all (commtest itself waits for the API to come up).
fn derive_api(
    network: &Network,
    silo: &str,
    rack: usize,
) -> anyhow::Result<Api> {
    let ca = crate::commission::rack_ca_path(rack);
    if ca.is_file() {
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
                    "the rack's DNS ({dns}) has no address for {host} yet; \
                     is the rack up? (`voxel status`)"
                )
            })?;
        // Absolute, for a child whose working directory may differ.
        let ca = std::env::current_dir()
            .ok()
            .and_then(|d| Utf8PathBuf::try_from(d).ok())
            .map(|d| d.join(&ca))
            .unwrap_or(ca);
        return Ok(Api {
            url: format!("https://{host}"),
            resolve: Some((host, ip)),
            ca: Some(ca),
        });
    }
    let candidates = api_candidates(network);
    let live_on = |port: u16| {
        candidates.iter().copied().find(|&addr| {
            std::net::TcpStream::connect_timeout(
                &(addr, port).into(),
                std::time::Duration::from_secs(1),
            )
            .is_ok()
        })
    };
    let http = |host: String| Api {
        url: format!("http://{host}"),
        resolve: None,
        ca: None,
    };
    if let Some(host) = live_on(80) {
        return Ok(http(host.to_string()));
    }
    if let Some(host) = live_on(443) {
        bail!(
            "the rack API at {host} answers on 443 only, and this rack has no \
             saved CA ({ca}) for commtest to trust its certificate; relaunch \
             with this voxel, pass --api with an endpoint commtest can \
             reach, or launch with --init-rss for a plain-HTTP rack"
        );
    }
    Ok(http(match candidates.first() {
        Some(host) => host.to_string(),
        None => network.service_pool_first.clone(),
    }))
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
            // Older commtests are unicast-only and need no phase selector.
            if supports_multicast && !has_arg(args, "--skip-mcast") {
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
        assert!(
            commtest_args_for(
                "/tmp/old-omicron",
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
}
