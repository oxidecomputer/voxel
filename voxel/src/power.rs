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
/// Loop tick and SP reconnect interval.
const TICK: Duration = Duration::from_millis(500);
const RECONNECT: Duration = Duration::from_secs(3);
/// How often the sleds' propolis processes are checked for loss.
const LOSS_CHECK: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const POWER_ON_ATTEMPTS: u32 = 6;
const POWER_ON_RETRY: Duration = Duration::from_secs(5);

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
    /// The host phase 1 slot the SP reported with its last A0.
    boot_slot: Option<String>,
    /// host-lost was sent for the current A0.
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

/// Sync sled power to the SPs of one rack, or every rack.
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
                boot_slot: None,
                lost_told: false,
                next_connect: Instant::now(),
            });
        }
    }
    if links.is_empty() {
        bail!("no emulated SPs in rack(s) {racks:?}; nothing to follow");
    }
    // One loop per rack.
    let _locks: Vec<std::fs::File> =
        racks.iter().map(|&r| lock(r)).collect::<anyhow::Result<_>>()?;
    // Capture every running sled's instance.
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

/// Take a rack's loop lock, held while the file lives. Fails if another loop
/// holds it.
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
    // A gimlet's A0 line carries its host phase 1 slot.
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["state", s, slot @ ..] if slot.len() <= 1 => {
            let on = *s == "A0";
            links[i].host_on = Some(on);
            links[i].boot_slot = slot.first().map(|s| s.to_string());
            follow(cfg, name, &mut links[i], on, "reports").await;
        }
        ["event", e, slot @ ..] if slot.len() <= 1 => {
            let on = *e == "a0";
            links[i].host_on = Some(on);
            links[i].boot_slot = slot.first().map(|s| s.to_string());
            follow(cfg, name, &mut links[i], on, "moved to").await;
        }
        ["ignition", port, op] => {
            let Ok(port) = port.parse::<u8>() else { return };
            ignition(links, i, port, op).await;
        }
        _ => {
            eprintln!("[power] {}: unexpected line {line:?}", links[i].label())
        }
    }
}

/// Power a gimlet's sled on or off to match its SP.
async fn follow(
    cfg: &VoxelConfig,
    name: &str,
    l: &mut Link,
    on: bool,
    how: &str,
) {
    let Some(node) = l.node.clone() else {
        // Sidecar: no sled. Log its state once, on connect.
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
        "[power] {}: SP {how} {}{}, sled is {}",
        l.label(),
        if on { "A0" } else { "A2" },
        l.boot_slot.as_ref().map(|s| format!(" slot {s}")).unwrap_or_default(),
        if alive { "up" } else { "down" }
    );
    if on {
        l.lost_told = false;
        if !alive {
            // Write the SP's boot slot for the guest.
            if let Some(slot) = &l.boot_slot {
                let path = crate::topo::cargo_bay(&node).join("boot-slot");
                if let Err(e) = std::fs::write(&path, slot) {
                    eprintln!("[power] {node}: write {path}: {e}");
                }
                if let Err(e) = stage_host_os(name, &node, slot) {
                    eprintln!("[power] {node}: host OS not staged: {e:#}");
                }
            }
            // propolis can refuse an instance create with a 500. Retry.
            for attempt in 1..=POWER_ON_ATTEMPTS {
                match crate::node::on(cfg, name, &node).await {
                    Ok(()) => break,
                    Err(e) => eprintln!(
                        "[power] {node}: power on failed \
                         ({attempt}/{POWER_ON_ATTEMPTS}): {e:#}"
                    ),
                }
                tokio::time::sleep(POWER_ON_RETRY).await;
            }
        }
    } else if let Err(e) = crate::node::off(&node).await {
        // Also runs for a sled already gone: clears its pid file and VM.
        eprintln!("[power] {node}: power off failed: {e:#}");
    }
}

/// Copy the global-zone software from the phase 2 in a sled's boot M.2 slot
/// into the sled's cargo-bay, for `voxel-boot` to restage. No-op for sleds on
/// file-backed disks.
fn stage_host_os(name: &str, node: &str, slot: &str) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom};
    /// The guest's sector size (`disks::BLOCK_SIZE`), which its GPT is in.
    const SECTOR: u64 = 4096;
    const BOOT_IMAGE_MAGIC: u32 = 0x1deb0075;
    let index = if slot == "B" { 1 } else { 0 };
    let zvol = format!(
        "/dev/zvol/dsk/{}/topo/{name}/{node}-M2{index}",
        crate::image::falcon_dataset()
    );
    if !Utf8Path::new(&zvol).exists() {
        return Ok(());
    }
    let mut dev =
        std::fs::File::open(&zvol).with_context(|| format!("open {zvol}"))?;
    let u64_at = |b: &[u8], at: usize| {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    };
    // GPT header at LBA 1: the partition entries' LBA at 72. The boot image
    // is partition 0, whose first LBA is at 32 in its entry.
    let mut sector = [0u8; SECTOR as usize];
    dev.seek(SeekFrom::Start(SECTOR))?;
    dev.read_exact(&mut sector)?;
    if &sector[..8] != b"EFI PART" {
        bail!("{zvol}: no GPT");
    }
    dev.seek(SeekFrom::Start(u64_at(&sector, 72) * SECTOR))?;
    dev.read_exact(&mut sector)?;
    let image = u64_at(&sector, 32) * SECTOR;
    // BootImageHeader: magic at 0, data size at 16, sha256 of the data at 40.
    dev.seek(SeekFrom::Start(image))?;
    dev.read_exact(&mut sector)?;
    if sector[..4] != BOOT_IMAGE_MAGIC.to_le_bytes() {
        bail!("{zvol}: slot {slot} holds no boot image");
    }
    let data_size = u64_at(&sector, 16);
    let sha: String =
        sector[40..72].iter().map(|b| format!("{b:02x}")).collect();

    let bay = crate::topo::cargo_bay(node);
    let stamp = bay.join("gz.sha256");
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == sha) {
        return Ok(());
    }
    let cache = crate::imagebuild::build_root()?.join(".tuf-host");
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("mkdir {cache}"))?;
    let payload = cache.join(format!("phase2-{}.img", &sha[..12]));
    if !payload.exists() {
        let part = cache.join(format!("phase2-{}.img.part", &sha[..12]));
        let mut out = std::fs::File::create(&part)
            .with_context(|| format!("create {part}"))?;
        let copied = std::io::copy(&mut (&mut dev).take(data_size), &mut out)
            .with_context(|| format!("read phase 2 from {zvol}"))?;
        if copied != data_size {
            bail!("{zvol}: phase 2 is {copied} bytes, header says {data_size}");
        }
        std::fs::rename(&part, &payload)
            .with_context(|| format!("move {part} to {payload}"))?;
    }
    let gz = bay.join("gz");
    let _ = std::fs::remove_dir_all(&gz);
    crate::cpbuild::stage_gz_from_phase2(
        &payload,
        &cache.join(format!("mnt-{node}")),
        &gz,
    )?;
    crate::cpbuild::recompose_lifted_gz(&gz)?;
    std::fs::write(&stamp, &sha).with_context(|| format!("write {stamp}"))?;
    eprintln!(
        "[power] {node}: staged host OS from slot {slot} ({})",
        &sha[..12]
    );
    Ok(())
}

/// Handle an ignition request for a target port. Off disables the target's
/// SP and powers its sled off. On enables the SP. Reset does both.
async fn ignition(links: &mut [Link], from: usize, port: u8, op: &str) {
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

/// SMF manifest for one rack's loop, run with the launch's config and workdir.
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

/// Stop and remove every rack's loop. Best effort. Called first in destroy.
pub(crate) fn down_all(cfg: &VoxelConfig) {
    for rack in 0..cfg.topology.racks() {
        let f = fmri(rack);
        crate::sp_host::probe("pfexec", &["svcadm", "disable", "-s", &f]);
        crate::sp_host::probe("pfexec", &["svccfg", "delete", "-f", &f]);
        crate::sp_host::probe("pfexec", &["rm", "-f", &manifest_path(rack)]);
    }
}
