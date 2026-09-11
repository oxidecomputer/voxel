// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SP driven power for an emulated fleet: the sled VMs follow their SPs' host
//! power bridges, and an SP is told when its host goes away. Runs under SMF.

use anyhow::{Context, bail};
use camino::Utf8Path;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use voxel_config::VoxelConfig;
use voxel_config::sp::{Sp, SpRole};

/// SMF service running the loop, one instance per rack.
const SVC: &str = "svc:/oxide/voxel-power";
const MANIFEST_DIR: &str = "/var/svc/manifest/site";
/// Loop tick, and how long a dead SP socket waits before a reconnect.
const TICK: Duration = Duration::from_millis(500);
const RECONNECT: Duration = Duration::from_secs(3);
/// How often the sleds' propolis processes are checked for loss.
const LOSS_CHECK: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The address an SP serves its host power bridge on.
pub(crate) fn power_addr(rack: usize, sp: &Sp) -> String {
    format!(
        "[{}]:{}",
        voxel_config::config::sp_host_addr(rack),
        sp.power_port()
    )
}

/// The sled node a gimlet SP belongs to.
fn sled_of(cfg: &VoxelConfig, rack: usize, sp: &Sp) -> Option<String> {
    match sp.role {
        SpRole::Gimlet(i) => cfg
            .sleds()
            .into_iter()
            .find(|s| s.rack == rack && s.index == i)
            .map(|s| s.name),
        SpRole::Sidecar => None,
    }
}

/// One SP's bridge connection and what it last said about its host.
struct Link {
    sp: Sp,
    rack: usize,
    node: Option<String>,
    addr: String,
    conn: Option<TcpStream>,
    buf: Vec<u8>,
    host_on: Option<bool>,
    /// A host-lost was sent for the current A0 and is not repeated.
    lost_told: bool,
    next_connect: Instant,
}

impl Link {
    fn label(&self) -> String {
        match &self.node {
            Some(n) => format!("{} (SP {})", n, self.sp.selector()),
            None => format!("SP {}", self.sp.selector()),
        }
    }

    fn connect(&mut self) {
        let addr = match self.addr.parse() {
            Ok(a) => a,
            Err(_) => return,
        };
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                if s.set_nonblocking(true).is_ok() {
                    eprintln!(
                        "[power] {}: connected to {}",
                        self.label(),
                        self.addr
                    );
                    self.conn = Some(s);
                    self.buf.clear();
                }
            }
            Err(_) => self.next_connect = Instant::now() + RECONNECT,
        }
    }

    /// Complete lines the SP has sent since the last call. Drops the
    /// connection on EOF or error.
    fn read_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        let Some(s) = self.conn.as_mut() else {
            return lines;
        };
        let mut chunk = [0u8; 512];
        let mut gone = false;
        loop {
            match s.read(&mut chunk) {
                Ok(0) => {
                    gone = true;
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    gone = true;
                    break;
                }
            }
        }
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            lines.push(String::from_utf8_lossy(&line[..nl]).trim().to_string());
        }
        if gone {
            eprintln!("[power] {}: bridge closed", self.label());
            self.conn = None;
            self.host_on = None;
            self.next_connect = Instant::now() + RECONNECT;
        }
        lines
    }

    fn send(&mut self, line: &str) {
        if let Some(s) = self.conn.as_mut()
            && s.write_all(format!("{line}\n").as_bytes()).is_err()
        {
            self.conn = None;
            self.next_connect = Instant::now() + RECONNECT;
        }
    }
}

/// The SP's host power as its bridge reports it now, for host ls.
pub(crate) fn sp_state(
    cfg: &VoxelConfig,
    rack: usize,
    node: &str,
) -> Option<String> {
    let fleet = crate::topo::emu_fleet(cfg, rack);
    let sp = fleet
        .emu_sps()
        .into_iter()
        .find(|sp| sled_of(cfg, rack, sp).as_deref() == Some(node))?;
    let addr = power_addr(rack, sp).parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    s.set_read_timeout(Some(CONNECT_TIMEOUT)).ok()?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).ok()?;
    let line = String::from_utf8_lossy(&buf[..n]);
    line.trim().strip_prefix("state ").map(str::to_string)
}

/// Follow the fleet of one rack, or every rack, and keep its sleds in step.
pub(crate) async fn watch(
    cfg: &VoxelConfig,
    name: &str,
    rack: Option<usize>,
) -> anyhow::Result<()> {
    let racks: Vec<usize> = match rack {
        Some(r) => vec![r],
        None => (0..cfg.topology.racks()).collect(),
    };
    let mut links = Vec::new();
    for &rack in &racks {
        for sp in crate::topo::emu_fleet(cfg, rack).emu_sps() {
            links.push(Link {
                sp: sp.clone(),
                rack,
                node: sled_of(cfg, rack, sp),
                addr: power_addr(rack, sp),
                conn: None,
                buf: Vec::new(),
                host_on: None,
                lost_told: false,
                next_connect: Instant::now(),
            });
        }
    }
    if links.is_empty() {
        bail!("no emulated SPs in rack(s) {racks:?}; nothing to follow");
    }
    // One loop per rack: two would race each other's relaunches.
    let _locks: Vec<std::fs::File> =
        racks.iter().map(|&r| lock(r)).collect::<anyhow::Result<_>>()?;
    // Every running sled needs a captured instance before an SP can cycle it.
    let nodes: Vec<String> =
        links.iter().filter_map(|l| l.node.clone()).collect();
    crate::node::capture_missing(&nodes).await;
    eprintln!(
        "[power] {name}: following {} SP(s) in rack(s) {racks:?}",
        links.len()
    );
    let mut next_loss_check = Instant::now();
    loop {
        for i in 0..links.len() {
            if links[i].conn.is_none()
                && Instant::now() >= links[i].next_connect
            {
                links[i].connect();
            }
            for line in links[i].read_lines() {
                handle(cfg, name, &mut links, i, &line).await;
            }
        }
        if Instant::now() >= next_loss_check {
            next_loss_check = Instant::now() + LOSS_CHECK;
            for l in links.iter_mut() {
                let Some(node) = l.node.clone() else { continue };
                if l.host_on == Some(true)
                    && !l.lost_told
                    && !crate::node::alive(&node)
                {
                    eprintln!(
                        "[power] {}: host gone while the SP reports A0; telling the SP",
                        l.label()
                    );
                    l.send("host-lost");
                    l.lost_told = true;
                }
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Hold a rack's loop lock for as long as the file lives. A second loop for
/// the same rack fails here.
fn lock(rack: usize) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let path =
        crate::node::falcon_dir().join(format!("voxel-power-r{rack}.lock"));
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {path}"))?;
    // SAFETY: flock on a descriptor this File owns, which outlives the call.
    let r =
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        bail!(
            "another power loop already follows rack {rack} ({path} is locked)"
        );
    }
    Ok(f)
}

/// One line from an SP's bridge.
async fn handle(
    cfg: &VoxelConfig,
    name: &str,
    links: &mut [Link],
    i: usize,
    line: &str,
) {
    let mut words = line.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("state"), Some(s), None) => {
            let on = s == "A0";
            links[i].host_on = Some(on);
            follow(cfg, name, &mut links[i], on, "reports").await;
        }
        (Some("event"), Some(e), None) => {
            let on = e == "a0";
            links[i].host_on = Some(on);
            follow(cfg, name, &mut links[i], on, "moved to").await;
        }
        (Some("ignition"), Some(port), Some(op)) => {
            let Ok(port) = port.parse::<u8>() else { return };
            ignition(cfg, links, i, port, op).await;
        }
        _ => {
            eprintln!("[power] {}: unexpected line {line:?}", links[i].label())
        }
    }
}

/// Bring a gimlet's sled in line with what its SP supplies.
async fn follow(
    cfg: &VoxelConfig,
    name: &str,
    l: &mut Link,
    on: bool,
    how: &str,
) {
    let Some(node) = l.node.clone() else {
        // Nothing sits behind the sidecar's Tofino. Note it once on connect.
        if how == "reports" {
            eprintln!(
                "[power] {}: {how} {}",
                l.label(),
                if on { "A0" } else { "A2" }
            );
        }
        return;
    };
    let alive = crate::node::alive(&node);
    eprintln!(
        "[power] {}: SP {how} {}, sled is {}",
        l.label(),
        if on { "A0" } else { "A2" },
        if alive { "up" } else { "down" }
    );
    if on {
        l.lost_told = false;
        if !alive && let Err(e) = crate::node::on(cfg, name, &node).await {
            eprintln!("[power] {node}: power on failed: {e:#}");
        }
    } else if let Err(e) = crate::node::off(&node).await {
        // Runs for a sled already gone too; it clears the pid file and VM.
        eprintln!("[power] {node}: power off failed: {e:#}");
    }
}

/// An ignition request for a target port. Ignition cuts the whole sled: off
/// takes the SP down with the host, on brings the SP up to power the host.
async fn ignition(
    cfg: &VoxelConfig,
    links: &mut [Link],
    from: usize,
    port: u8,
    op: &str,
) {
    let rack = links[from].rack;
    let Some(t) = links.iter().position(|l| {
        l.rack == rack && l.sp.ignition_target == port && l.node.is_some()
    }) else {
        eprintln!(
            "[power] ignition port {port} {op}: no sled at that target, ignored"
        );
        return;
    };
    let node = links[t].node.clone().unwrap_or_default();
    let sp_port = links[t].sp.base_port;
    eprintln!("[power] ignition port {port} {op}: {}", links[t].label());
    match op {
        "off" => {
            crate::sp_host::set_enabled(rack, sp_port, false);
            links[t].host_on = None;
            if let Err(e) = crate::node::off(&node).await {
                eprintln!("[power] {node}: power off failed: {e:#}");
            }
        }
        "on" => {
            crate::sp_host::set_enabled(rack, sp_port, true);
        }
        "reset" => {
            crate::sp_host::set_enabled(rack, sp_port, false);
            links[t].host_on = None;
            if let Err(e) = crate::node::off(&node).await {
                eprintln!("[power] {node}: power off failed: {e:#}");
            }
            crate::sp_host::set_enabled(rack, sp_port, true);
        }
        other => {
            eprintln!("[power] ignition port {port}: unknown op {other:?}")
        }
    }
    let _ = cfg;
}

// The SMF service.

fn instance(rack: usize) -> String {
    format!("r{rack}")
}

fn manifest_path(rack: usize) -> String {
    format!("{MANIFEST_DIR}/voxel-power-r{rack}.xml")
}

pub(crate) fn fmri(rack: usize) -> String {
    format!("{SVC}:{}", instance(rack))
}

/// The SMF manifest for one rack's loop: this voxel binary on the config and
/// workdir the launch used.
fn manifest(
    rack: usize,
    exe: &Utf8Path,
    config: &Utf8Path,
    workdir: &Utf8Path,
    name: &str,
) -> String {
    let dataset = std::env::var("FALCON_DATASET").unwrap_or_default();
    format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE service_bundle SYSTEM \"/usr/share/lib/xml/dtd/service_bundle.dtd.1\">\n\
         <service_bundle type=\"manifest\" name=\"voxel-power-r{rack}\">\n\
         <service name=\"oxide/voxel-power\" type=\"service\" version=\"1\">\n\
         \x20 <dependency name=\"multi_user\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n\
         \x20   <service_fmri value=\"svc:/milestone/multi-user:default\"/>\n\
         \x20 </dependency>\n\
         \x20 <instance name=\"{inst}\" enabled=\"true\">\n\
         \x20   <exec_method type=\"method\" name=\"start\" exec=\"{exe} sp watch --rack {rack}\" timeout_seconds=\"0\">\n\
         \x20     <method_context working_directory=\"{workdir}\">\n\
         \x20       <method_environment>\n\
         \x20         <envvar name=\"PATH\" value=\"/usr/bin:/usr/sbin:/sbin\"/>\n\
         \x20         <envvar name=\"VOXEL_CONFIG\" value=\"{config}\"/>\n\
         \x20         <envvar name=\"VOXEL_WORKDIR\" value=\"{workdir}\"/>\n\
         \x20         <envvar name=\"VOXEL_NAME\" value=\"{name}\"/>\n\
         \x20         <envvar name=\"FALCON_DATASET\" value=\"{dataset}\"/>\n\
         \x20       </method_environment>\n\
         \x20     </method_context>\n\
         \x20   </exec_method>\n\
         \x20   <exec_method type=\"method\" name=\"stop\" exec=\":kill\" timeout_seconds=\"30\"/>\n\
         \x20   <property_group name=\"startd\" type=\"framework\">\n\
         \x20     <propval name=\"duration\" type=\"astring\" value=\"child\"/>\n\
         \x20   </property_group>\n\
         \x20 </instance>\n\
         </service>\n</service_bundle>\n",
        inst = instance(rack),
    )
}

/// Install and start one rack's loop.
fn up(rack: usize, config: &Utf8Path, name: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate the voxel binary")?;
    let exe =
        camino::Utf8PathBuf::try_from(exe).context("voxel binary path")?;
    let workdir = std::env::current_dir().context("workdir")?;
    let workdir =
        camino::Utf8PathBuf::try_from(workdir).context("workdir path")?;
    let config = if config.is_absolute() {
        config.to_path_buf()
    } else {
        workdir.join(config)
    };
    let path = manifest_path(rack);
    let tmp = crate::util::temp_dir().join(format!("voxel-power-r{rack}.xml"));
    std::fs::write(&tmp, manifest(rack, &exe, &config, &workdir, name))
        .with_context(|| format!("write {tmp}"))?;
    crate::sp_host::run(&["cp", tmp.as_str(), &path])?;
    let _ = std::fs::remove_file(&tmp);
    crate::sp_host::import_manifest(&path, &[fmri(rack)])?;
    println!("[voxel] rack {rack} SP power loop up ({})", fmri(rack));
    Ok(())
}

/// Install and start the loop for every rack.
pub(crate) fn up_all(
    cfg: &VoxelConfig,
    name: &str,
    config: &Utf8Path,
) -> anyhow::Result<()> {
    for rack in 0..cfg.topology.racks() {
        up(rack, config, name)?;
    }
    Ok(())
}

/// Stop and remove every rack's loop. Best effort. Runs first in a destroy so
/// the loop does not act on the sleds vanishing.
pub(crate) fn down_all(cfg: &VoxelConfig) {
    for rack in 0..cfg.topology.racks() {
        let f = fmri(rack);
        crate::sp_host::probe("pfexec", &["svcadm", "disable", "-s", &f]);
        crate::sp_host::probe("pfexec", &["svccfg", "delete", "-f", &f]);
        crate::sp_host::probe("pfexec", &["rm", "-f", &manifest_path(rack)]);
    }
}
