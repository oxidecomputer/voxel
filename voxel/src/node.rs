// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Power for one falcon node: off, on and reset at the propolis level. On
//! replays the instance captured from the live server; disks and uuid persist.

use anyhow::{Context, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use voxel_config::VoxelConfig;

use crate::topo::build_topo;

/// The propolis API version voxel speaks, the one falcon's client was generated
/// from. Older versions leave the SMBIOS table out of the spec.
const API_VERSION: &str = "2.0.0";
/// How long propolis gets to stop the VM before it is killed.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// How long a fresh propolis gets to log its listen address.
const PORT_WAIT: Duration = Duration::from_secs(10);
/// How long a fresh propolis gets to start answering before it is ensured.
const READY_WAIT: Duration = Duration::from_secs(30);

/// What voxel host off, on or reset asks of a node.
pub(crate) enum Power {
    Off,
    On,
    Reset,
}

pub(crate) async fn cmd_host_power(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
    op: Power,
) -> anyhow::Result<()> {
    check_node(cfg, name, node)?;
    match op {
        Power::Off => off(node).await,
        Power::On => on(cfg, name, node).await,
        Power::Reset => reset(node).await,
    }
}

/// Every node in the topology, sleds first.
fn node_names(cfg: &VoxelConfig, name: &str) -> anyhow::Result<Vec<String>> {
    let topo = build_topo(cfg, name)?;
    Ok(topo
        .sleds
        .iter()
        .map(|(s, _)| s.name.clone())
        .chain(topo.routers.iter().map(|(r, _)| r.clone()))
        .collect())
}

fn check_node(cfg: &VoxelConfig, name: &str, node: &str) -> anyhow::Result<()> {
    if node_names(cfg, name)?.iter().any(|n| n == node) {
        return Ok(());
    }
    bail!("no such node: {node}")
}

// The falcon workspace.

/// The falcon workspace under the anchored workdir.
pub(crate) fn falcon_dir() -> &'static Utf8Path {
    Utf8Path::new(".falcon")
}

fn node_file(node: &str, ext: &str) -> Utf8PathBuf {
    falcon_dir().join(format!("{node}.{ext}"))
}

/// Read a node's propolis pid. Anything but a plain number is rejected, so a
/// bad pidfile cannot become a kill of something unrelated.
pub(crate) fn read_pidfile(node: &str) -> Option<String> {
    let raw = std::fs::read_to_string(node_file(node, "pid")).ok()?;
    let pid = raw.trim();
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(pid.to_string())
}

/// Whether the node's propolis is running, checked through /proc. An
/// unprivileged kill -0 against the root-owned process fails with EPERM.
pub(crate) fn alive(node: &str) -> bool {
    match read_pidfile(node) {
        Some(pid) => std::path::Path::new(&format!("/proc/{pid}")).exists(),
        None => false,
    }
}

fn pfexec(args: &[&str]) -> bool {
    Command::new("pfexec")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// falcon's hyperstop: SIGKILL the propolis from the pidfile, then destroy the
/// bhyve VM by uuid. Best effort, so a node already gone still cleans up.
pub(crate) fn hyperstop(node: &str) {
    if let Some(pid) = read_pidfile(node) {
        pfexec(&["kill", "-9", &pid]);
    }
    let _ = std::fs::remove_file(node_file(node, "pid"));
    if let Ok(uuid) = std::fs::read_to_string(node_file(node, "uuid")) {
        let uuid = uuid.trim();
        if !uuid.is_empty() {
            pfexec(&["bhyvectl", "--destroy", &format!("--vm={uuid}")]);
        }
    }
}

/// Give a halting guest time to flush. Reaching the timeout is normal; the
/// caller takes propolis down next either way.
pub(crate) fn wait_for_propolis_exit(node: &str, timeout: Duration) {
    if !alive(node) {
        return;
    }
    eprintln!("[voxel] flushing, up to {}s", timeout.as_secs());
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !alive(node) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!(
        "[voxel] still running after {}s; forcing it down",
        timeout.as_secs()
    );
}

// The propolis server.

/// One node's propolis server, on the port falcon or a relaunch recorded.
struct Propolis {
    http: reqwest::Client,
    base: String,
}

impl Propolis {
    fn for_node(node: &str) -> anyhow::Result<Self> {
        let port = std::fs::read_to_string(node_file(node, "port"))
            .with_context(|| format!("{node}: no propolis port recorded"))?;
        Self::at(port.trim().parse().context("propolis port")?)
    }

    fn at(port: u16) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("build the propolis client")?;
        Ok(Propolis { http, base: format!("http://127.0.0.1:{port}") })
    }

    async fn get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let r = self
            .http
            .get(format!("{}{path}", self.base))
            .header("api-version", API_VERSION)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("GET {path}: {status} {body}");
        }
        serde_json::from_str(&body).with_context(|| format!("decode {path}"))
    }

    async fn put(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let r = self
            .http
            .put(format!("{}{path}", self.base))
            .header("api-version", API_VERSION)
            .json(body)
            .send()
            .await
            .with_context(|| format!("PUT {path}"))?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            bail!("PUT {path}: {status} {body}");
        }
        Ok(())
    }

    /// The instance state propolis reports.
    async fn state(&self) -> anyhow::Result<String> {
        let v = self.get("/instance").await?;
        v.pointer("/instance/state")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no instance state in the propolis reply"))
    }

    /// Request a state: Run, Stop or Reboot.
    async fn request(&self, state: &str) -> anyhow::Result<()> {
        self.put("/instance/state", &serde_json::Value::String(state.into()))
            .await
    }

    /// The ensure request that recreates this instance: the properties and the
    /// running spec, verbatim.
    async fn ensure_body(&self) -> anyhow::Result<serde_json::Value> {
        let v = self.get("/instance/spec").await?;
        let properties = v
            .get("properties")
            .cloned()
            .ok_or_else(|| anyhow!("no properties in the spec reply"))?;
        let status =
            v.get("spec").ok_or_else(|| anyhow!("no spec in the reply"))?;
        if status.get("type").and_then(|t| t.as_str()) != Some("Present") {
            bail!("propolis has no spec for this instance yet");
        }
        // API 2.0.0 returns the spec under value; 1.0.0 wrapped it in a
        // versioned envelope, which an older capture may carry.
        let value = status
            .get("value")
            .ok_or_else(|| anyhow!("no spec value in the reply"))?;
        let spec = value.get("spec").unwrap_or(value).clone();
        Ok(serde_json::json!({
            "properties": properties,
            "init": { "method": "Spec", "value": { "spec": spec } },
        }))
    }
}

/// Capture a running node's instance for a later power on. Returns the file.
pub(crate) async fn capture(node: &str) -> anyhow::Result<Utf8PathBuf> {
    if !alive(node) {
        bail!("not running");
    }
    let body = Propolis::for_node(node)?.ensure_body().await?;
    let path = node_file(node, "ensure.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&body)?)
        .with_context(|| format!("write {path}"))?;
    Ok(path)
}

/// Capture every running node that has no capture yet. Best effort; a failure
/// is reported, not fatal.
pub(crate) async fn capture_missing(nodes: &[String]) {
    for n in nodes {
        if node_file(n, "ensure.json").exists() || !alive(n) {
            continue;
        }
        match capture(n).await {
            Ok(p) => eprintln!("[voxel] {n}: instance captured to {p}"),
            Err(e) => eprintln!("[voxel] {n}: instance not captured ({e:#})"),
        }
    }
}

/// Power a node off: a bounded propolis stop, then the process and the bhyve
/// VM. The instance is captured first if it never was.
pub(crate) async fn off(node: &str) -> anyhow::Result<()> {
    if alive(node) {
        if !node_file(node, "ensure.json").exists() {
            match capture(node).await {
                Ok(p) => eprintln!("[voxel] {node}: instance captured to {p}"),
                Err(e) => eprintln!(
                    "[voxel] {node}: instance not captured ({e:#}); `on` will \
                     need a capture from a running node"
                ),
            }
        }
        if let Ok(p) = Propolis::for_node(node) {
            let _ = p.request("Stop").await;
            let deadline = Instant::now() + STOP_WAIT;
            while Instant::now() < deadline && alive(node) {
                match p.state().await {
                    Ok(s) if s == "Stopped" || s == "Destroyed" => break,
                    _ => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            }
        }
    }
    hyperstop(node);
    eprintln!("[voxel] {node}: off");
    Ok(())
}

/// Power a node on: a fresh propolis on a kernel assigned port, the captured
/// instance ensured, then run. Pid and port are recorded where falcon does.
pub(crate) async fn on(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
) -> anyhow::Result<()> {
    if alive(node) {
        eprintln!("[voxel] {node}: already running");
        return Ok(());
    }
    let ensure = node_file(node, "ensure.json");
    let mut body: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&ensure).with_context(|| {
            format!(
                "{node}: no captured instance at {ensure}; capture one from the \
                 running node: a launch or the power loop records one"
            )
        })?,
    )
    .with_context(|| format!("decode {ensure}"))?;
    // propolis omits the SMBIOS table from its spec. The sled's baseboard
    // identity comes from it, so restore it from the topology.
    if let Some(smbios) = smbios_of(cfg, name, node)? {
        match body.pointer_mut("/init/value/spec") {
            Some(spec) => {
                spec["smbios"] = smbios;
            }
            None => bail!("{ensure}: no spec to carry the SMBIOS table"),
        }
    }
    // Clear the last run; a stale bhyve VM keeps the uuid busy.
    hyperstop(node);
    let dir = falcon_dir();
    let bin = cfg
        .falcon
        .propolis_binary
        .clone()
        .unwrap_or_else(|| dir.join("bin/propolis-server").to_string());
    let bootrom = dir.join("bin/OVMF_CODE.fd");
    let out = std::fs::File::create(node_file(node, "out"))?;
    let err = std::fs::File::create(node_file(node, "err"))?;
    let child = Command::new("pfexec")
        .args([bin.as_str(), "run", bootrom.as_str(), "[::]:0"])
        .env("RUST_BACKTRACE", "FULL")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .with_context(|| format!("start {bin} for {node}"))?;
    std::fs::write(node_file(node, "pid"), child.id().to_string())?;
    let port = wait_for_port(node).await?;
    std::fs::write(node_file(node, "port"), port.to_string())?;
    let p = Propolis::at(port)?;
    // Ensure once the server accepts a connection. Only a transport failure
    // is retried; any answer from propolis is final.
    let deadline = Instant::now() + READY_WAIT;
    loop {
        match p.put("/instance", &body).await {
            Ok(()) => break,
            Err(e)
                if e.downcast_ref::<reqwest::Error>().is_some()
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                hyperstop(node);
                return Err(e.context(format!(
                    "{node}: propolis refused the instance"
                )));
            }
        }
    }
    p.request("Run").await.with_context(|| format!("{node}: run"))?;
    eprintln!("[voxel] {node}: on (propolis pid {}, port {port})", child.id());
    Ok(())
}

/// The SMBIOS Type 1 table voxel gave a node at launch, as the topology builds
/// it. None for a router.
fn smbios_of(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let topo = build_topo(cfg, name)?;
    let smbios = topo
        .runner
        .deployment
        .nodes
        .iter()
        .find(|n| n.name == node)
        .and_then(|n| n.smbios.clone());
    smbios
        .map(|s| serde_json::to_value(s).context("encode the SMBIOS table"))
        .transpose()
}

/// The listen port a fresh propolis logs, as falcon scrapes it.
async fn wait_for_port(node: &str) -> anyhow::Result<u16> {
    let out = node_file(node, "out");
    let deadline = Instant::now() + PORT_WAIT;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&out)
            && let Some(port) = find_port(&text)
        {
            return Ok(port);
        }
        if !alive(node) {
            bail!("{node}: propolis exited before listening (see {out})");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    hyperstop(node);
    bail!("{node}: propolis did not report a listen port within {PORT_WAIT:?}")
}

/// The port in propolis's local_addr startup line.
fn find_port(log: &str) -> Option<u16> {
    for marker in ["\"local_addr\":\"[::]:", "\"local_addr\":\"[::1]:"] {
        if let Some(i) = log.find(marker) {
            let rest = &log[i + marker.len()..];
            let digits: String =
                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse() {
                return Some(p);
            }
        }
    }
    None
}

/// Reset a node: a propolis reboot. The VM keeps its process and port.
pub(crate) async fn reset(node: &str) -> anyhow::Result<()> {
    if !alive(node) {
        bail!("{node}: not running (use `voxel host on {node}`)");
    }
    Propolis::for_node(node)?
        .request("Reboot")
        .await
        .with_context(|| format!("{node}: reboot"))?;
    eprintln!("[voxel] {node}: reset");
    Ok(())
}

/// The instance state propolis reports for a node, or why it cannot say.
pub(crate) async fn propolis_state(node: &str) -> String {
    if !alive(node) {
        return "down".into();
    }
    match Propolis::for_node(node) {
        Ok(p) => p.state().await.unwrap_or_else(|_| "(no reply)".into()),
        Err(_) => "(no port)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_propolis_port_in_its_log() {
        let log = r#"{"msg":"listening","local_addr":"[::]:34995","v":0}"#;
        assert_eq!(find_port(log), Some(34995));
        assert_eq!(find_port(r#"{"local_addr":"[::1]:8"}"#), Some(8));
        assert_eq!(find_port("nothing yet"), None);
    }
}
