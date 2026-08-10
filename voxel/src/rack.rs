//! Rack lifecycle commands: launch, route, destroy, info, status.

use anyhow::{Context, anyhow, bail};
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use voxel_config::VoxelConfig;

use crate::isolated_external::{DryRun, link_mtu, up as external_up};
use crate::net::{
    ce_static_ip, resolve_external_ip, set_external_route, ssh_capture,
    wait_external_reachable, zlogin,
};
use crate::rss::watch_rss;
use crate::topo::{
    Topo, build_topo, reset_node_cargo_bay, stage_config, stage_sprockets,
};

/// A per-rack progress/label tag: `rackN` (1-based) when the deployment has more
/// than one rack, else the single-rack fallback the caller passes ("rack",
/// "rack-init", ...).
fn rack_label(racks: usize, rack: usize, single: &str) -> String {
    if racks > 1 { format!("rack{}", rack + 1) } else { single.to_string() }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuestRole {
    Gimlet,
    Router,
}

const FALCON_BOOT_ATTEMPTS: u32 = 3;
const FALCON_BOOT_ATTEMPT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);

#[derive(Debug, Eq, PartialEq)]
enum FalconLaunchAttemptError {
    Failed(String),
    TimedOut(std::time::Duration),
}

impl std::fmt::Display for FalconLaunchAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(error) => write!(f, "Falcon launch failed: {error}"),
            Self::TimedOut(cap) => {
                write!(f, "Falcon launch timed out after {}s", cap.as_secs())
            }
        }
    }
}

fn classify_falcon_launch_attempt<E: std::fmt::Display>(
    result: Result<Result<(), E>, tokio::time::error::Elapsed>,
    cap: std::time::Duration,
) -> Result<(), FalconLaunchAttemptError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            Err(FalconLaunchAttemptError::Failed(error.to_string()))
        }
        Err(_) => Err(FalconLaunchAttemptError::TimedOut(cap)),
    }
}

impl GuestRole {
    fn label(self) -> &'static str {
        match self {
            Self::Gimlet => "gimlet",
            Self::Router => "router",
        }
    }

    fn sentinel(self) -> &'static str {
        match self {
            Self::Gimlet => "[voxel-init] gimlet bring-up complete",
            Self::Router => "[voxel-init] router bring-up complete",
        }
    }

    fn base_command(self) -> &'static str {
        match self {
            Self::Gimlet => "/opt/oxide/voxel-init gimlet",
            Self::Router => "/opt/oxide/voxel-init router",
        }
    }

    fn command_timeout(self) -> std::time::Duration {
        std::time::Duration::from_secs(match self {
            Self::Gimlet => 15 * 60,
            Self::Router => 2 * 60,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuestInitOutput {
    Complete,
    MissingSentinel,
    Fatal,
}

fn classify_voxel_init_output(
    role: GuestRole,
    output: &str,
) -> GuestInitOutput {
    if output
        .lines()
        .any(|line| line.trim_start().starts_with("[voxel-init] FATAL"))
    {
        GuestInitOutput::Fatal
    } else if output.lines().any(|line| line.trim() == role.sentinel()) {
        GuestInitOutput::Complete
    } else {
        GuestInitOutput::MissingSentinel
    }
}

fn launch_log_matches_invocation(marker: &str, output: &str) -> bool {
    output.lines().any(|line| line.trim() == marker)
}

fn next_launch_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "[voxel-launch] invocation={nanos}-{}-{sequence}",
        std::process::id()
    )
}

fn voxel_init_command(base_command: &str, marker: &str) -> String {
    format!(
        "printf '%s\\n' '{marker}' > /tmp/launch.log && \
         ({base_command} 2>&1 | tee -a /tmp/launch.log); \
         printf '%s\\n' '{marker}' >> /tmp/launch.log"
    )
}

fn aggregate_voxel_init_results(
    results: Vec<Result<(), String>>,
) -> anyhow::Result<()> {
    let failures: Vec<String> =
        results.into_iter().filter_map(Result::err).collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "voxel-init failed on {} node(s):\n- {}",
            failures.len(),
            failures.join("\n- ")
        ))
    }
}

fn validate_launch_gate_bypasses(
    no_progress: bool,
    no_route: bool,
) -> anyhow::Result<()> {
    if no_progress && !no_route {
        Err(anyhow!(
            "--no-progress skips the RSS completion barrier, so external reachability cannot be validated; also pass --no-route to make both bypasses explicit"
        ))
    } else {
        Ok(())
    }
}

fn classify_interconnect_retry(
    attempt: Result<bool, String>,
    deadline_reached: bool,
    last_observed: &mut String,
) -> anyhow::Result<bool> {
    match attempt {
        Ok(true) => return Ok(true),
        Ok(false) => {
            *last_observed = "link-local readback missing :ok".to_string();
        }
        Err(error) => *last_observed = error,
    }
    if deadline_reached {
        bail!("deadline reached; last observed error: {last_observed}");
    }
    Ok(false)
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn role_specific_voxel_init_sentinels_are_required() {
        assert_eq!(
            classify_voxel_init_output(
                GuestRole::Router,
                "noise\n[voxel-init] router bring-up complete\n"
            ),
            GuestInitOutput::Complete
        );
        assert_eq!(
            classify_voxel_init_output(
                GuestRole::Gimlet,
                "noise\n[voxel-init] gimlet bring-up complete\n"
            ),
            GuestInitOutput::Complete
        );
        assert_eq!(
            classify_voxel_init_output(
                GuestRole::Router,
                "[voxel-init] gimlet bring-up complete\n"
            ),
            GuestInitOutput::MissingSentinel
        );
    }

    #[test]
    fn missing_voxel_init_sentinel_is_not_success() {
        assert_eq!(
            classify_voxel_init_output(
                GuestRole::Gimlet,
                "[voxel-init] setup finished\n"
            ),
            GuestInitOutput::MissingSentinel
        );
    }

    #[test]
    fn success_looking_substring_is_not_a_voxel_init_sentinel() {
        assert_eq!(
            classify_voxel_init_output(
                GuestRole::Router,
                "diagnostic: expected [voxel-init] router bring-up complete but did not see it\n"
            ),
            GuestInitOutput::MissingSentinel
        );
    }

    #[test]
    fn launch_log_recheck_requires_the_current_invocation_marker() {
        let marker = "[voxel-launch] invocation=123";
        assert!(!launch_log_matches_invocation(
            marker,
            "[voxel-init] router bring-up complete\n"
        ));
        assert!(launch_log_matches_invocation(
            marker,
            "[voxel-launch] invocation=123\n[voxel-init] router bring-up complete\n"
        ));
    }

    #[test]
    fn voxel_init_command_owns_one_fresh_launch_log_pipeline() {
        let command = voxel_init_command(
            GuestRole::Router.base_command(),
            "[voxel-launch] invocation=123",
        );
        assert_eq!(command.matches("tee").count(), 1, "{command}");
        assert!(
            command
                .contains("> /tmp/launch.log && (/opt/oxide/voxel-init router")
        );
        assert!(command.contains("| tee -a /tmp/launch.log)"));
    }

    #[test]
    fn voxel_init_fatal_wins_over_success_looking_output() {
        let output = "[voxel-init] gimlet bring-up complete\n\
                      [voxel-init] FATAL: activation failed\n";
        assert_eq!(
            classify_voxel_init_output(GuestRole::Gimlet, output),
            GuestInitOutput::Fatal
        );
    }

    #[test]
    fn aggregate_voxel_init_failures_reports_every_node() {
        let error = aggregate_voxel_init_results(vec![
            Err("ce (router): missing sentinel".to_string()),
            Ok(()),
            Err("g2 (gimlet): activation failed".to_string()),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("ce (router): missing sentinel"));
        assert!(error.contains("g2 (gimlet): activation failed"));
    }

    #[test]
    fn skipping_rss_progress_requires_skipping_external_route_validation() {
        assert!(validate_launch_gate_bypasses(false, false).is_ok());
        assert!(validate_launch_gate_bypasses(false, true).is_ok());
        assert!(validate_launch_gate_bypasses(true, true).is_ok());
        let error =
            validate_launch_gate_bypasses(true, false).unwrap_err().to_string();
        assert!(error.contains("--no-progress"));
        assert!(error.contains("--no-route"));
    }

    fn assert_transient_interconnect_failure_retries(error: &str) {
        let mut last = String::new();
        assert!(
            !classify_interconnect_retry(
                Err(error.to_string()),
                false,
                &mut last
            )
            .unwrap()
        );
        assert_eq!(last, error);
        assert!(
            classify_interconnect_retry(Ok(true), false, &mut last).unwrap()
        );
    }

    #[test]
    fn interconnect_external_ip_resolution_failure_retries_then_succeeds() {
        assert_transient_interconnect_failure_retries(
            "resolve switch IP: lease unavailable",
        );
    }

    #[test]
    fn interconnect_address_create_failure_retries_then_succeeds() {
        assert_transient_interconnect_failure_retries(
            "create link-local: address object busy",
        );
    }

    #[test]
    fn interconnect_ssh_inspection_failure_retries_then_succeeds() {
        assert_transient_interconnect_failure_retries(
            "inspect link-local: SSH transport reset",
        );
    }

    #[test]
    fn interconnect_missing_or_non_ok_readback_retries_then_succeeds() {
        let mut last = String::new();
        assert!(
            !classify_interconnect_retry(Ok(false), false, &mut last).unwrap()
        );
        assert_eq!(last, "link-local readback missing :ok");
        assert!(
            classify_interconnect_retry(Ok(true), false, &mut last).unwrap()
        );
    }

    #[test]
    fn persistent_interconnect_failure_reports_last_error_at_deadline() {
        let mut last = String::new();
        assert!(
            !classify_interconnect_retry(
                Err("first transient error".to_string()),
                false,
                &mut last
            )
            .unwrap()
        );
        let error = classify_interconnect_retry(
            Err("last observed SSH failure".to_string()),
            true,
            &mut last,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("last observed SSH failure"), "{error}");
    }

    #[tokio::test]
    async fn falcon_launch_attempt_distinguishes_success_failure_and_timeout() {
        let cap = std::time::Duration::from_secs(600);
        assert_eq!(
            classify_falcon_launch_attempt::<&str>(Ok(Ok(())), cap),
            Ok(())
        );
        assert_eq!(
            classify_falcon_launch_attempt(Ok(Err("serial failed")), cap)
                .unwrap_err(),
            FalconLaunchAttemptError::Failed("serial failed".to_string())
        );

        let timed_out = tokio::time::timeout(
            std::time::Duration::ZERO,
            std::future::pending::<Result<(), &str>>(),
        )
        .await;
        assert_eq!(
            classify_falcon_launch_attempt(timed_out, cap).unwrap_err(),
            FalconLaunchAttemptError::TimedOut(cap)
        );
    }

    #[test]
    fn verifies_host_zfs_set_value_and_local_source() {
        let output = "sync\tdisabled\tlocal\n";
        assert!(verify_zfs_set_readback(output, "sync", "disabled").is_ok());
        assert!(verify_zfs_set_readback(output, "sync", "standard").is_err());
        assert!(
            verify_zfs_set_readback(
                "sync\tdisabled\tinherited from rpool\n",
                "sync",
                "disabled"
            )
            .is_err()
        );
    }

    #[test]
    fn verifies_host_zfs_reset_is_no_longer_local() {
        assert!(
            verify_zfs_inherit_readback(
                "compression\toff\tinherited from rpool\n",
                "compression"
            )
            .is_ok()
        );
        assert!(
            verify_zfs_inherit_readback(
                "compression\tlz4\tlocal\n",
                "compression"
            )
            .is_err()
        );
    }

    #[test]
    fn classifies_zfs_dataset_presence_without_hiding_probe_errors() {
        let dataset = "rpool/falcon/topo/voxel";
        assert!(
            classify_zfs_dataset_list(
                true,
                "rpool/falcon/topo/voxel\n",
                "",
                dataset
            )
            .unwrap()
        );
        assert!(
            !classify_zfs_dataset_list(
                false,
                "",
                "cannot open 'rpool/falcon/topo/voxel': dataset does not exist",
                dataset
            )
            .unwrap()
        );
        assert!(
            classify_zfs_dataset_list(false, "", "permission denied", dataset)
                .is_err()
        );
        assert!(
            classify_zfs_dataset_list(
                true,
                "some/other/dataset\n",
                "",
                dataset
            )
            .is_err()
        );
    }

    #[test]
    fn teardown_skips_fallback_wipe_when_falcon_removed_dataset() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let zfs = temp.path().join("zfs");
        let calls = temp.path().join("calls");
        let dataset = "rpool/falcon/topo/voxel";
        std::fs::write(
            &zfs,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\nif [ \"$1\" = list ]; then\n  printf \"cannot open '{}': dataset does not exist\\n\" >&2\n  exit 1\nfi\nexit 99\n",
                calls, dataset
            ),
        )
        .unwrap();
        std::fs::set_permissions(&zfs, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let (dataset_remains, wipe_error) =
            cleanup_topology_dataset(zfs.as_path(), dataset).unwrap();

        assert!(!dataset_remains);
        assert_eq!(wipe_error, None);
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            format!("list -H -o name {dataset}\n")
        );
    }

    #[test]
    fn teardown_fallback_wipes_dataset_that_falcon_left_behind() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let zfs = temp.path().join("zfs");
        let calls = temp.path().join("calls");
        let present = temp.path().join("present");
        let dataset = "rpool/falcon/topo/voxel";
        std::fs::write(&present, "").unwrap();
        std::fs::write(
            &zfs,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\ncase \"$1\" in\n  list)\n    if [ -f {:?} ]; then\n      printf '%s\\n' '{}'\n      exit 0\n    fi\n    printf \"cannot open '{}': dataset does not exist\\n\" >&2\n    exit 1\n    ;;\n  destroy)\n    rm -f {:?}\n    ;;\nesac\n",
                calls, present, dataset, dataset, present
            ),
        )
        .unwrap();
        std::fs::set_permissions(&zfs, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let (dataset_remains, wipe_error) =
            cleanup_topology_dataset(zfs.as_path(), dataset).unwrap();

        assert!(!dataset_remains);
        assert_eq!(wipe_error, None);
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            format!(
                "list -H -o name {dataset}\ndestroy -r {dataset}\nlist -H -o name {dataset}\n"
            )
        );
    }

    #[test]
    fn teardown_retries_a_transiently_busy_dataset() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let zfs = temp.path().join("zfs");
        let calls = temp.path().join("calls");
        let attempts = temp.path().join("attempts");
        let present = temp.path().join("present");
        let dataset = "rpool/falcon/topo/voxel";
        std::fs::write(&attempts, "0\n").unwrap();
        std::fs::write(&present, "").unwrap();
        std::fs::write(
            &zfs,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\ncase \"$1\" in\n  list)\n    if [ -f {:?} ]; then\n      printf '%s\\n' '{}'\n      exit 0\n    fi\n    printf \"cannot open '{}': dataset does not exist\\n\" >&2\n    exit 1\n    ;;\n  destroy)\n    n=$(cat {:?})\n    n=$((n + 1))\n    printf '%s\\n' \"$n\" > {:?}\n    if [ \"$n\" -eq 1 ]; then\n      printf \"cannot destroy '{}/g3': dataset is busy\\n\" >&2\n      exit 1\n    fi\n    rm -f {:?}\n    ;;\nesac\n",
                calls, present, dataset, dataset, attempts, attempts, dataset, present
            ),
        )
        .unwrap();
        std::fs::set_permissions(&zfs, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let (dataset_remains, wipe_error) =
            cleanup_topology_dataset(zfs.as_path(), dataset).unwrap();

        assert!(!dataset_remains);
        assert_eq!(wipe_error, None);
        assert_eq!(std::fs::read_to_string(attempts).unwrap(), "2\n");
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            format!(
                "list -H -o name {dataset}\ndestroy -r {dataset}\nlist -H -o name {dataset}\ndestroy -r {dataset}\nlist -H -o name {dataset}\n"
            )
        );
    }

    #[test]
    fn teardown_is_idempotent_only_after_every_resource_is_proven_absent() {
        let mut evidence = TeardownEvidence {
            name: "voxel",
            dataset: "rpool/falcon/topo/voxel",
            file_backing: "/var/falcon/dsk/voxel",
            dataset_remains: false,
            workspace_artifacts: &[],
            file_backing_remains: false,
            destroy_error: Some("workspace already absent"),
            wipe_error: Some("dataset already absent"),
        };
        assert!(classify_teardown_evidence(&evidence).is_ok());

        evidence.dataset_remains = true;
        assert!(classify_teardown_evidence(&evidence).is_err());
        evidence.dataset_remains = false;
        let artifacts = ["voxel.json".to_string()];
        evidence.workspace_artifacts = &artifacts;
        assert!(classify_teardown_evidence(&evidence).is_err());
        evidence.workspace_artifacts = &[];
        evidence.file_backing_remains = true;
        assert!(classify_teardown_evidence(&evidence).is_err());
    }
}

pub(crate) async fn cmd_route(
    cfg: &VoxelConfig,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let ce = topo
        .node_ref("ce")
        .ok_or_else(|| anyhow!("no ce router in topology"))?;
    // One host route per rack's external prefix - all racks egress via the shared ce.
    let racks = cfg.topology.racks();
    for rack in 0..racks {
        let prefix = cfg.network.for_rack(rack).infra_prefix;
        set_external_route(
            &topo.runner,
            ce,
            &prefix,
            !dry_run,
            ce_static_ip(cfg).as_deref(),
        )
        .await?;
    }
    Ok(())
}

/// Physical RAM in GiB via `prtconf -m` (illumos prints total memory in MB).
fn physical_ram_gb() -> Option<u64> {
    let out = Command::new("prtconf").arg("-m").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|mb| mb / 1024)
}

/// Refuse a launch that can't physically fit. Guest RAM shows up as `VMM Memory`
/// (~1.2× the requested guest RAM, from bhyve overhead) and must leave room for
/// the kernel + a minimal ZFS ARC, or the all-VMs-at-once boot thrashes - which
/// is what makes falcon's cargo-bay mount time out on the serial console. Better
/// a clear "won't fit" up front than a cryptic boot-spike timeout. Best-effort:
/// if physical RAM can't be read we skip; `VOXEL_SKIP_MEM_PREFLIGHT=1` overrides.
fn memory_preflight(cfg: &VoxelConfig) -> anyhow::Result<()> {
    if std::env::var("VOXEL_SKIP_MEM_PREFLIGHT").is_ok() {
        return Ok(());
    }
    let Some(phys) = physical_ram_gb() else {
        return Ok(());
    };
    let guest = cfg.topology.guest_memory_gb();
    let vmm = (guest as f64 * 1.2).ceil() as u64;
    const RESERVE_GB: u64 = 22; // kernel (~14G observed) + minimal ARC (~8G)
    if vmm + RESERVE_GB > phys {
        return Err(anyhow!(
            "topology needs ~{vmm} GB guest RAM (VMM) + ~{RESERVE_GB} GB kernel/ARC headroom, \
             but this box has {phys} GB. Lower topology.sled_memory_gb (now {}) or the sled count \
             (or set VOXEL_SKIP_MEM_PREFLIGHT=1 to override).",
            cfg.topology.sled_memory_gb
        ));
    }
    Ok(())
}

fn default_route_iface() -> Option<String> {
    let out =
        Command::new("route").args(["-n", "get", "default"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(|interface| interface.trim().to_string())
}

fn lan_mtu_preflight() -> anyhow::Result<()> {
    let link = match std::env::var("EXT_INTERFACE") {
        Ok(link) => link,
        Err(_) => match default_route_iface() {
            Some(link) => link,
            None => return Ok(()),
        },
    };
    if let Some(mtu) = link_mtu(&link)
        && mtu.parse::<u32>().is_ok_and(|mtu| mtu >= 9000)
    {
        bail!(
            "external link {link} has mtu {mtu}: sled NICs are classified as underlay iff they accept mtu=9000; point EXT_INTERFACE at a sub-9000-mtu link or use isolated mode"
        );
    }
    Ok(())
}

/// Apply the host-side disk-wear levers (1 + 2) to the falcon dataset before
/// launch: `zfs set <props> <dataset>`, so every `topo/<name>/gN` zvol inherits
/// them and the bulk install/RSS writes land under the tuning. Voxel storage is
/// fully ephemeral, so `sync=disabled` is safe (nothing here is meant to survive
/// a crash). Each requested property is set and read back from the target
/// dataset; command failure or a mismatched value/source blocks launch. No-op
/// unless a host lever is enabled, so the default launch leaves the host pool untouched.
/// (Lever 3 is guest-side, staged as a cargo-bay flag; lever 4 is
/// `topology.rss_sleds`.)
fn apply_host_disk_wear_tuning(cfg: &VoxelConfig) -> anyhow::Result<()> {
    let props = cfg.disk_wear.host_zfs_props();
    if props.is_empty() {
        return Ok(());
    }
    let dataset = crate::image::falcon_dataset();
    for prop in props {
        let (property, expected_value) =
            prop.split_once('=').ok_or_else(|| {
                anyhow!("invalid host ZFS property request {prop:?}")
            })?;
        let output = Command::new("zfs")
            .args(["set", prop, &dataset])
            .output()
            .map_err(|e| anyhow!("run `zfs set {prop} {dataset}`: {e}"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "`zfs set {prop} {dataset}` failed with {}: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let readback = read_zfs_property(&dataset, property)?;
        verify_zfs_set_readback(&readback, property, expected_value).map_err(
            |e| anyhow!("host ZFS lever readback for {dataset}: {e}"),
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct ZfsPropertyReadback<'a> {
    property: &'a str,
    value: &'a str,
    source: &'a str,
}

fn parse_zfs_property_readback(
    output: &str,
) -> anyhow::Result<ZfsPropertyReadback<'_>> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let line = lines.next().ok_or_else(|| anyhow!("empty `zfs get` output"))?;
    if lines.next().is_some() {
        return Err(anyhow!("expected one `zfs get` row, got {output:?}"));
    }
    let mut fields = line.splitn(3, '\t');
    let property = fields.next().unwrap_or_default().trim();
    let value = fields.next().unwrap_or_default().trim();
    let source = fields.next().unwrap_or_default().trim();
    if property.is_empty() || value.is_empty() || source.is_empty() {
        return Err(anyhow!("malformed `zfs get` row {line:?}"));
    }
    Ok(ZfsPropertyReadback { property, value, source })
}

fn verify_zfs_set_readback(
    output: &str,
    property: &str,
    expected_value: &str,
) -> anyhow::Result<()> {
    let observed = parse_zfs_property_readback(output)?;
    if observed.property != property
        || observed.value != expected_value
        || observed.source != "local"
    {
        return Err(anyhow!(
            "expected {property}={expected_value} source=local, observed {}={} source={}",
            observed.property,
            observed.value,
            observed.source
        ));
    }
    Ok(())
}

pub(crate) fn verify_zfs_inherit_readback(
    output: &str,
    property: &str,
) -> anyhow::Result<()> {
    let observed = parse_zfs_property_readback(output)?;
    if observed.property != property || observed.source == "local" {
        return Err(anyhow!(
            "expected {property} to have a non-local source after inherit, observed {}={} source={}",
            observed.property,
            observed.value,
            observed.source
        ));
    }
    Ok(())
}

pub(crate) fn read_zfs_property(
    dataset: &str,
    property: &str,
) -> anyhow::Result<String> {
    let output = Command::new("zfs")
        .args(["get", "-H", "-o", "property,value,source", property, dataset])
        .output()
        .map_err(|e| anyhow!("run `zfs get {property} {dataset}`: {e}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`zfs get {property} {dataset}` failed with {}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `/opt/oxide/voxel-init <role>` on each given node concurrently, surfacing
/// each node's `[voxel-init]` milestone lines (the raw `+ cmd` echoes stay in the
/// guest's `/tmp/launch.log`). Falcon's serial exec can return `Ok` without
/// preserving the guest pipeline's exit status, so only the role sentinel proves
/// success. If the initial capture omits it, read the freshly truncated launch
/// log once before failing; an explicit FATAL marker always wins.
async fn run_voxel_init(
    d: &Runner,
    items: Vec<(NodeRef, GuestRole, String)>,
) -> anyhow::Result<()> {
    const LOG_RECHECK: &str = "tail -n 80 /tmp/launch.log 2>&1";
    const LOG_RECHECK_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(30);

    let handles = items.into_iter().map(|(n, role, node)| async move {
        info!(d.log, "{node}: launch start");
        let marker = next_launch_marker();
        let command = voxel_init_command(role.base_command(), &marker);
        let output = match tokio::time::timeout(role.command_timeout(), d.exec(n, &command)).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return Err(format!(
                    "{node} ({}): command `{command}` failed: {error}; captured output unavailable from Falcon exec error",
                    role.label()
                ));
            }
            Err(_) => {
                return Err(format!(
                    "{node} ({}): command `{command}` timed out after {}s; captured output unavailable from timed-out Falcon exec",
                    role.label(),
                    role.command_timeout().as_secs()
                ));
            }
        };
        for line in output.lines().filter(|line| line.contains("[voxel-init]")) {
            info!(d.log, "{node}: {}", line.trim());
        }

        match classify_voxel_init_output(role, &output) {
            GuestInitOutput::Complete => {
                info!(d.log, "{node}: launch ok");
                Ok(())
            }
            GuestInitOutput::Fatal => Err(format!(
                "{node} ({}): command `{command}` reported FATAL; captured output:\n{}",
                role.label(),
                output.trim()
            )),
            GuestInitOutput::MissingSentinel => {
                info!(d.log, "{node}: launch output omitted {}; checking /tmp/launch.log once", role.sentinel());
                let recheck = match tokio::time::timeout(
                    LOG_RECHECK_TIMEOUT,
                    d.exec(n, LOG_RECHECK),
                )
                .await
                {
                    Ok(Ok(recheck)) => recheck,
                    Ok(Err(error)) => {
                        return Err(format!(
                            "{node} ({}): command `{command}` omitted {}; log recheck `{LOG_RECHECK}` failed: {error}; captured output:\n{}",
                            role.label(),
                            role.sentinel(),
                            output.trim()
                        ));
                    }
                    Err(_) => {
                        return Err(format!(
                            "{node} ({}): command `{command}` omitted {}; log recheck `{LOG_RECHECK}` timed out after {}s; captured output:\n{}",
                            role.label(),
                            role.sentinel(),
                            LOG_RECHECK_TIMEOUT.as_secs(),
                            output.trim()
                        ));
                    }
                };
                for line in recheck.lines().filter(|line| line.contains("[voxel-init]")) {
                    info!(d.log, "{node}: log: {}", line.trim());
                }

                if !launch_log_matches_invocation(&marker, &recheck) {
                    return Err(format!(
                        "{node} ({}): command `{command}` omitted {} and `{LOG_RECHECK}` did not contain the current invocation marker {marker:?}; refusing stale log output; initial output:\n{}\nlog output:\n{}",
                        role.label(),
                        role.sentinel(),
                        output.trim(),
                        recheck.trim()
                    ));
                }

                match classify_voxel_init_output(role, &recheck) {
                    GuestInitOutput::Complete => {
                        info!(d.log, "{node}: launch ok (sentinel recovered from log)");
                        Ok(())
                    }
                    GuestInitOutput::Fatal => Err(format!(
                        "{node} ({}): command `{command}` reported FATAL in `{LOG_RECHECK}`; initial output:\n{}\nlog output:\n{}",
                        role.label(),
                        output.trim(),
                        recheck.trim()
                    )),
                    GuestInitOutput::MissingSentinel => Err(format!(
                        "{node} ({}): command `{command}` omitted {} from both captured output and `{LOG_RECHECK}`; initial output:\n{}\nlog output:\n{}",
                        role.label(),
                        role.sentinel(),
                        output.trim(),
                        recheck.trim()
                    )),
                }
            }
        }
    });
    aggregate_voxel_init_results(futures::future::join_all(handles).await)
}

async fn bring_up_interconnect(
    d: &Runner,
    topo: &Topo,
    cfg: &VoxelConfig,
    rack: usize,
) -> anyhow::Result<()> {
    let scrimlets: Vec<(NodeRef, String)> = topo
        .sleds
        .iter()
        .filter(|(sled, _)| sled.rack == rack && sled.scrimlet)
        .map(|(sled, node)| (*node, sled.name.clone()))
        .collect();
    for (switch, port) in cfg.interconnect_ports(rack) {
        let slot = switch
            .strip_prefix("switch")
            .and_then(|slot| slot.parse::<usize>().ok())
            .ok_or_else(|| {
                anyhow!(
                    "rack{}: invalid interconnect switch {switch}",
                    rack + 1
                )
            })?;
        let (node, sled) = scrimlets.get(slot).ok_or_else(|| {
            anyhow!(
                "rack{}: no scrimlet for interconnect {switch}:{port}",
                rack + 1
            )
        })?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(600);
        let mut last = String::new();
        loop {
            let attempt = match resolve_external_ip(cfg, d, sled, *node, false)
                .await
            {
                Err(error) => Err(format!("resolve switch IP: {error:#}")),
                Ok(ip) if !crate::network::switch_ready(&ip) => {
                    Err("waiting for switch zone".to_string())
                }
                Ok(ip) => match crate::network::enable_link(
                    &ip, sled, &port, "100G", "none",
                ) {
                    Err(error) => Err(format!("link create/enable: {error}")),
                    Ok(()) => {
                        let create = zlogin(&format!(
                            "ipadm create-addr -T addrconf tfport{port}_0/ll 2>/dev/null || ipadm show-addr -po addrobj | grep -Fx tfport{port}_0/ll >/dev/null"
                        ));
                        match ssh_capture(&ip, &create) {
                            None => Err(
                                "create interconnect link-local: SSH transport or remote command failure"
                                    .to_string(),
                            ),
                            Some(_) => match ssh_capture(
                                &ip,
                                &zlogin(&format!(
                                    "ipadm show-addr -po addrobj,state | grep tfport{port}_0"
                                )),
                            ) {
                                None => Err(
                                    "inspect interconnect link-local: SSH transport or remote command failure"
                                        .to_string(),
                                ),
                                Some(state) => Ok(state.contains(":ok")),
                            },
                        }
                    }
                },
            };
            let previous = last.clone();
            let complete = classify_interconnect_retry(
                attempt,
                std::time::Instant::now() >= deadline,
                &mut last,
            )
            .with_context(|| {
                format!(
                    "rack{}: interconnect {sled}:{port} not up within 600s",
                    rack + 1
                )
            })?;
            if complete {
                info!(d.log, "rack{}: interconnect {sled}:{port} up", rack + 1);
                break;
            }
            if last != previous {
                info!(
                    d.log,
                    "rack{}: interconnect {sled}:{port}: {last}",
                    rack + 1
                );
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
    Ok(())
}

pub(crate) async fn cmd_launch(
    cfg: &VoxelConfig,
    name: &str,
    no_progress: bool,
    no_route: bool,
    emu_sp: bool,
    emu_rot: bool,
    wicket_setup: bool,
) -> anyhow::Result<()> {
    validate_launch_gate_bypasses(no_progress, no_route)?;
    cfg.topology.validate_rss_membership().map_err(anyhow::Error::msg)?;
    // Floor (per rack - each is an independent RSS domain): omicron's control
    // plane can't form below 3 sleds (Crucible 3-way replication,
    // CockroachDB/trust-quorum majority), and the RSS->Nexus handoff needs both
    // switches, i.e. exactly 2 scrimlets.
    let sleds = cfg.sleds();
    let racks = cfg.topology.racks();
    if cfg.topology.sleds < 3 {
        return Err(anyhow!(
            "each rack needs ≥3 sleds (Crucible 3x replication + Cockroach/trust-quorum quorum); got {} per rack",
            cfg.topology.sleds
        ));
    }
    for rack in 0..racks {
        let scrimlets =
            sleds.iter().filter(|s| s.rack == rack && s.scrimlet).count();
        if scrimlets != 2 {
            return Err(anyhow!(
                "rack {rack} needs exactly 2 scrimlets for the dual-switch RSS->Nexus handoff; got {scrimlets}"
            ));
        }
    }
    const MAX_FRONT_PORTS: usize = 128;
    let customer_routers = cfg
        .topology
        .routers
        .iter()
        .filter(|router| router.as_str() != "ce")
        .count();
    for sled in sleds.iter().filter(|sled| sled.scrimlet) {
        let front_ports =
            customer_routers + cfg.topology.interconnect_count_for(sled.index);
        if front_ports > MAX_FRONT_PORTS {
            bail!(
                "scrimlet {} needs {front_ports} SoftNPU front ports (> {MAX_FRONT_PORTS}); reduce racks or switches-per-rack",
                sled.name
            );
        }
    }
    if !no_route {
        if !cfg.topology.routers.iter().any(|router| router == "ce") {
            return Err(anyhow!(
                "external routing is enabled but topology.routers does not contain `ce` (use --no-route to bypass external routing)"
            ));
        }
        for rack in 0..1.min(racks) {
            if cfg.network.for_rack(rack).external_dns_ips.is_empty() {
                return Err(anyhow!(
                    "external routing is enabled but rack{} has no external DNS probe target (use --no-route to bypass external routing)",
                    rack + 1
                ));
            }
        }
    }
    // Fail fast if the configured images aren't built yet - a clear message
    // beats the cryptic clone error falcon would throw partway through launch.
    crate::image::ensure_image(&cfg.image.cp_image())?;
    crate::image::ensure_image(&cfg.image.frr_image())?;
    memory_preflight(cfg)?;
    apply_host_disk_wear_tuning(cfg)?;
    if cfg.external.isolated() {
        external_up(&cfg.external, DryRun::No)
            .context("bringing up the isolated external segment")?;
    } else {
        lan_mtu_preflight()?;
    }
    reset_node_cargo_bay(cfg)?;
    stage_config(cfg, emu_sp, emu_rot, wicket_setup)?;
    stage_sprockets(cfg)?;
    let mut topo = build_topo(cfg, name)?;
    // The all-VMs-at-once boot grabs ~all the guest RAM in one spike; under that
    // pressure falcon's cargo-bay mount over the serial console can transiently
    // time out ("[sc] <node>: timeout waiting for data") and abort the whole
    // boot. It's recoverable on a clean retry, so do that automatically: tear
    // down the partial boot (releasing VNICs/zvols) and rebuild a fresh topology.
    let mut attempt = 1;
    loop {
        info!(
            topo.runner.log,
            "Falcon boot attempt {attempt}/{FALCON_BOOT_ATTEMPTS} starting (deadline {}s)",
            FALCON_BOOT_ATTEMPT_TIMEOUT.as_secs()
        );
        let outcome = classify_falcon_launch_attempt(
            tokio::time::timeout(
                FALCON_BOOT_ATTEMPT_TIMEOUT,
                topo.runner.launch(),
            )
            .await,
            FALCON_BOOT_ATTEMPT_TIMEOUT,
        );
        match outcome {
            Ok(()) => {
                info!(
                    topo.runner.log,
                    "Falcon boot attempt {attempt}/{FALCON_BOOT_ATTEMPTS} completed"
                );
                break;
            }
            Err(error) if attempt < FALCON_BOOT_ATTEMPTS => {
                warn!(
                    topo.runner.log,
                    "Falcon boot attempt {attempt}/{FALCON_BOOT_ATTEMPTS}: {error}; tearing down + retrying"
                );
                teardown(&topo.runner, name).with_context(|| {
                    format!(
                        "Falcon boot attempt {attempt}/{FALCON_BOOT_ATTEMPTS}: {error}; cannot establish a clean retry boundary"
                    )
                })?;
                std::thread::sleep(std::time::Duration::from_secs(3));
                topo = build_topo(cfg, name)?;
                attempt += 1;
            }
            Err(error) => {
                return Err(anyhow!(
                    "Falcon launch failed after {attempt} attempts: {error}"
                ));
            }
        }
    }

    // Run the in-guest agent, baked into the images at /opt/oxide/voxel-init.
    let d = &topo.runner;

    // Customer routers (the shared transit) first - quick, and must be up for
    // the racks' uplink BGP.
    let routers: Vec<(NodeRef, GuestRole, String)> = topo
        .routers
        .iter()
        .map(|(r, n)| (*n, GuestRole::Router, r.clone()))
        .collect();
    run_voxel_init(d, routers).await?;

    if no_progress {
        // No RSS watcher to use as a barrier, so bring every sled up at once.
        let sleds: Vec<(NodeRef, GuestRole, String)> = topo
            .sleds
            .iter()
            .map(|(s, n)| (*n, GuestRole::Gimlet, s.name.clone()))
            .collect();
        run_voxel_init(d, sleds).await?;
    } else {
        // **Stagger by rack.** Bring up each rack's sleds and watch its RSS to
        // completion before starting the next rack. Running two racks' heavy
        // zone-init concurrently thrashes the box hard enough to knock a scrimlet
        // over mid-bring-up - which loses its runtime switch-slot identity and
        // wedges that rack's Nexus handoff (the switch1-reverts-to-switch0 bug).
        // One rack at a time keeps the box within its I/O budget. A single rack
        // behaves exactly as before.
        for rack in 0..racks {
            let rack_sleds: Vec<(NodeRef, GuestRole, String)> = topo
                .sleds
                .iter()
                .filter(|(s, _)| s.rack == rack)
                .map(|(s, n)| (*n, GuestRole::Gimlet, s.name.clone()))
                .collect();
            if racks > 1 {
                info!(
                    d.log,
                    "rack{}: bringing up {} sleds",
                    rack + 1,
                    rack_sleds.len()
                );
            }
            run_voxel_init(d, rack_sleds).await?;
            if rack > 0 {
                info!(
                    d.log,
                    "rack{}: booted, left pre-RSS (unclaimed - multirack join not yet supported)",
                    rack + 1
                );
                continue;
            }
            let tag = rack_label(racks, rack, "rack-init");
            let (s, n) = topo
                .rss_sleds()
                .into_iter()
                .find(|(s, _)| s.rack == rack)
                .ok_or_else(|| {
                    anyhow!("{tag}: no RSS sled selected for rack{}", rack + 1)
                })?;
            // --wicket-setup: nothing auto-inited (no staged config-rss), so
            // drive RSS through wicketd (upload config + cert + recovery
            // password, then POST to start). watch_rss then reports the
            // wicketd-triggered bring-up exactly as for the file path.
            if wicket_setup {
                let net = cfg.network.for_rack(rack);
                let config_rss = camino::Utf8Path::new("wicket-setup")
                    .join(format!("rack{rack}"))
                    .join("config-rss.toml");
                // wicketd's bootstrap_sleds must be THIS rack's cubby slots =
                // its sleds' GLOBAL indices (rack 1 -> 3,4,5), matching what the
                // MGS sim reports (`location = ["sled", global_index]`); a flat
                // 0..n only correlates for rack 0.
                let slots: Vec<u16> = topo
                    .sleds
                    .iter()
                    .filter(|(s, _)| s.rack == rack)
                    .map(|(s, _)| s.index as u16)
                    .collect();
                crate::wicket_setup::drive(
                    cfg,
                    d,
                    crate::wicket_setup::RackSetup {
                        scrimlet: *n,
                        scrimlet_name: &s.name,
                        bootstrap_slots: &slots,
                        config_rss_path: &config_rss,
                        zone: &net.dns_zone,
                        tag: &tag,
                    },
                )
                .await
                .map_err(|e| anyhow!("{tag}: wicket-setup failed: {e}"))?;
            }
            let watch_cap = rss_watch_cap(emu_sp, racks);
            let known_ip = if cfg.external.isolated() {
                cfg.static_external_ips()
                    .into_iter()
                    .find(|(name, _)| name == &s.name)
                    .map(|(_, ip)| ip)
            } else {
                None
            };
            watch_rss(d, *n, &s.bootstrap_addr(), &tag, watch_cap, known_ip)
                .await?;
        }
    }

    // Point the host route at this launch's ce for each rack's external prefix
    // (all racks egress via the shared ce; ce's DHCP IP changes every bring-up),
    // then confirm the rack is actually reachable before declaring it usable - a
    // route isn't reachability (the shared transit can briefly flap the first
    // rack's path as the second rack joins).
    if no_route {
        info!(d.log, "external route and reachability skipped (--no-route)");
    } else {
        let ce = topo.node_ref("ce").ok_or_else(|| {
            anyhow!("external routing is enabled but topology has no `ce` node")
        })?;
        for rack in 0..1.min(racks) {
            let net = cfg.network.for_rack(rack);
            let label = rack_label(racks, rack, "rack");
            set_external_route(
                d,
                ce,
                &net.infra_prefix,
                true,
                ce_static_ip(cfg).as_deref(),
            )
            .await
            .map_err(|e| anyhow!("{label} external route: {e}"))?;
            let dns_ip = net.external_dns_ips.first().ok_or_else(|| {
                anyhow!("{label}: external routing is enabled but no external DNS probe target is configured")
            })?;
            wait_external_reachable(&d.log, dns_ip, &net.dns_zone, &label)?;
        }
    }

    for rack in 1..racks {
        bring_up_interconnect(d, &topo, cfg, rack).await?;
    }

    if no_progress {
        info!(d.log, "launch complete (RSS progress watch skipped)");
    } else {
        info!(d.log, "launch complete");
    }

    // --emu-rot: nothing to attach here anymore. voxel-init stands up a shared
    // `voxel-rot-emu` service per switch zone and points every SP at it via
    // SP_EMU_ROT_SERVICE from boot, so each SP stays single-core and the RoT
    // bridge is live through RSS -- MGS/Nexus pin the real RoT at rack-init.
    Ok(())
}

/// Kill propolis processes that belong to this deployment but that falcon won't
/// reap itself. A node whose `.falcon/<node>.pid` went missing (a partially
/// failed prior teardown) leaves an orphaned propolis holding that node's VNICs
/// and zvol busy - which then wedges *this* destroy: link teardown aborts with
/// "Device busy", and the follow-up zvol wipe can't proceed either. We identify
/// orphans by the deployment-prefixed VNIC paths in their open files
/// (`/dev/net/<name>_*`, e.g. `/dev/net/voxel_g3_sn_vnic0`), so this is scoped
/// to this rack and never touches another deployment's propolis. Pids falcon
/// already tracks via the workspace pid files are left for falcon to kill.
/// Returns how many it reaped.
fn reap_orphan_propolis(name: &str, log: &slog::Logger) -> usize {
    // Pids falcon tracks via the workspace pid files - leave those to falcon.
    let mut tracked: HashSet<i32> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(".falcon") {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("pid")
                && let Ok(s) = std::fs::read_to_string(&p)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                tracked.insert(pid);
            }
        }
    }

    let out =
        match Command::new("pgrep").args(["-f", "propolis-server"]).output() {
            Ok(o) if o.status.success() => o.stdout,
            // pgrep exits non-zero when there are no matches - nothing to reap.
            _ => return 0,
        };
    let needle = format!("/dev/net/{name}_");
    let mut reaped = 0;
    for line in String::from_utf8_lossy(&out).lines() {
        let pid: i32 = match line.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if tracked.contains(&pid) {
            continue;
        }
        // Does this propolis hold one of THIS deployment's VNICs?
        let pf = match Command::new("pfiles").arg(pid.to_string()).output() {
            Ok(o) => o.stdout,
            Err(_) => continue,
        };
        if String::from_utf8_lossy(&pf).contains(&needle) {
            warn!(
                log,
                "reaping orphaned propolis {pid} holding {name} resources (no falcon pid file)"
            );
            let _ =
                Command::new("kill").args(["-9", &pid.to_string()]).status();
            reaped += 1;
        }
    }
    reaped
}

/// Tear down a deployment's falcon resources and guarantee a clean slate. Reap
/// orphan propolis the workspace can't (a node whose `.falcon/<node>.pid` went
/// missing leaves one holding VNICs/zvol busy, which would wedge the teardown),
/// run falcon's own destroy, then wipe the node disks only if falcon left their
/// dataset behind. Falcon kills Propolis without waiting for the process to
/// exit, and it does not surface a failed ZFS destroy. A transiently busy zvol
/// can therefore leave the persistent `topo/<name>` datasets (and their stale
/// crucible/trust-quorum ledger) behind while Falcon reports success, so the
/// next launch boots dirty (RSS falsely reports an already-initialized rack).
/// Ok if the rack is gone + disks clean, even when falcon reports an
/// already-absent workspace. Shared by `cmd_destroy` and the boot-retry path.
fn classify_zfs_dataset_list(
    success: bool,
    stdout: &str,
    stderr: &str,
    dataset: &str,
) -> anyhow::Result<bool> {
    if success {
        let names = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if names == [dataset] {
            Ok(true)
        } else {
            Err(anyhow!(
                "`zfs list {dataset}` succeeded without the exact dataset row; stdout={stdout:?} stderr={stderr:?}"
            ))
        }
    } else if stderr.contains(dataset)
        && stderr.to_ascii_lowercase().contains("dataset does not exist")
    {
        Ok(false)
    } else {
        Err(anyhow!(
            "`zfs list {dataset}` failed without proving absence; stdout={stdout:?} stderr={stderr:?}"
        ))
    }
}

fn zfs_dataset_exists(zfs: &Path, dataset: &str) -> anyhow::Result<bool> {
    let output = Command::new(zfs)
        .args(["list", "-H", "-o", "name", dataset])
        .output()
        .map_err(|e| anyhow!("run `zfs list {dataset}`: {e}"))?;
    classify_zfs_dataset_list(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        dataset,
    )
}

const ZFS_BUSY_DESTROY_ATTEMPTS: usize = 30;
const ZFS_BUSY_DESTROY_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(1);

fn cleanup_topology_dataset(
    zfs: &Path,
    dataset: &str,
) -> anyhow::Result<(bool, Option<String>)> {
    if !zfs_dataset_exists(zfs, dataset)? {
        return Ok((false, None));
    }

    for attempt in 1..=ZFS_BUSY_DESTROY_ATTEMPTS {
        let wipe = Command::new(zfs).args(["destroy", "-r", dataset]).output();
        let dataset_remains = zfs_dataset_exists(zfs, dataset)?;
        let (wipe_error, busy) = match wipe {
            Ok(output) if output.status.success() => (None, false),
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let error = format!(
                    "status {}: {}{}",
                    output.status,
                    stdout.trim(),
                    stderr.trim()
                );
                let busy =
                    stderr.to_ascii_lowercase().contains("dataset is busy");
                (Some(error), busy)
            }
            Err(error) => (Some(error.to_string()), false),
        };
        if !dataset_remains || !busy || attempt == ZFS_BUSY_DESTROY_ATTEMPTS {
            return Ok((dataset_remains, wipe_error));
        }
        // Falcon sends SIGKILL to Propolis but does not wait for process exit or
        // the kernel to release its raw zvol handle. Retry the actual readiness
        // condition rather than relying on a fixed post-destroy sleep.
        std::thread::sleep(ZFS_BUSY_DESTROY_INTERVAL);
    }
    unreachable!("bounded ZFS destroy loop always returns")
}

fn falcon_workspace_artifacts() -> anyhow::Result<Vec<String>> {
    let entries = match std::fs::read_dir(".falcon") {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(anyhow!("read .falcon after teardown: {error}"));
        }
    };
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|e| anyhow!("read .falcon entry after teardown: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "bin" {
            artifacts.push(name);
        }
    }
    artifacts.sort();
    Ok(artifacts)
}

struct TeardownEvidence<'a> {
    name: &'a str,
    dataset: &'a str,
    file_backing: &'a str,
    dataset_remains: bool,
    workspace_artifacts: &'a [String],
    file_backing_remains: bool,
    destroy_error: Option<&'a str>,
    wipe_error: Option<&'a str>,
}

fn classify_teardown_evidence(
    evidence: &TeardownEvidence<'_>,
) -> anyhow::Result<()> {
    if !evidence.dataset_remains
        && evidence.workspace_artifacts.is_empty()
        && !evidence.file_backing_remains
    {
        return Ok(());
    }
    Err(anyhow!(
        "teardown left resources for {}: dataset {} present={}; \
         workspace artifacts={:?}; file backing {} present={}; \
         falcon destroy error={}; dataset wipe error={}",
        evidence.name,
        evidence.dataset,
        evidence.dataset_remains,
        evidence.workspace_artifacts,
        evidence.file_backing,
        evidence.file_backing_remains,
        evidence.destroy_error.unwrap_or("none"),
        evidence.wipe_error.unwrap_or("none"),
    ))
}

fn teardown(runner: &Runner, name: &str) -> anyhow::Result<()> {
    if reap_orphan_propolis(name, &runner.log) > 0 {
        // Give the kernel a moment to release the freed VNIC/zvol handles.
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let result = runner.destroy();
    let topo_ds = format!("{}/topo/{name}", crate::image::falcon_dataset());
    let (dataset_remains, wipe_error) =
        cleanup_topology_dataset(Path::new("zfs"), &topo_ds)?;
    let workspace_artifacts = falcon_workspace_artifacts()?;
    let file_backing = format!("/var/falcon/dsk/{name}");
    let file_backing_remains =
        Path::new(&file_backing).try_exists().map_err(|e| {
            anyhow!("check file-backed topology {file_backing}: {e}")
        })?;

    let destroy_error = result.as_ref().err().map(ToString::to_string);
    let evidence = TeardownEvidence {
        name,
        dataset: &topo_ds,
        file_backing: &file_backing,
        dataset_remains,
        workspace_artifacts: &workspace_artifacts,
        file_backing_remains,
        destroy_error: destroy_error.as_deref(),
        wipe_error: wipe_error.as_deref(),
    };
    let classification = classify_teardown_evidence(&evidence);
    if classification.is_ok() {
        if let Some(error) = &destroy_error {
            warn!(
                runner.log,
                "falcon destroy reported '{error}', but no topology workspace or disk resources remain."
            );
        }
        if let Some(error) = &wipe_error {
            warn!(
                runner.log,
                "extra dataset wipe reported '{error}', but {topo_ds} is confirmed absent."
            );
        }
    }
    classification
}

pub(crate) fn cmd_destroy(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    teardown(&topo.runner, name)
}

pub(crate) fn cmd_info(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    println!("topology: {name}");
    println!("  cp image:  {}", cfg.image.cp_image());
    println!("  frr image: {}", cfg.image.frr_image());
    let racks = cfg.topology.racks();
    if racks > 1 {
        println!("  racks: {racks} × {} sleds", cfg.topology.sleds);
    }
    println!("  sleds:");
    for s in cfg.sleds() {
        let role = if s.scrimlet { "scrimlet" } else { "gimlet  " };
        let rss = if s.rss { "rss" } else { "   " };
        let rack = if racks > 1 {
            format!("rack{} ", s.rack + 1)
        } else {
            String::new()
        };
        println!(
            "    {} {rack}[{role}] {rss}  bootstrap {}",
            s.name,
            s.bootstrap_addr()
        );
    }
    println!("  routers: {}", cfg.topology.routers.join(", "));
    Ok(())
}

/// RSS watch budget: emulated SPs slow every MGS RPC and multi-rack racks
/// converge under each other's load, so both get 60m vs the 30m a single sp-sim
/// rack needs. (`cmd_status` watches a running rack with no emu_sp context, so it
/// passes `false`.)
fn rss_watch_cap(emu_sp: bool, racks: usize) -> std::time::Duration {
    std::time::Duration::from_secs(if emu_sp || racks > 1 {
        3600
    } else {
        1800
    })
}

pub(crate) async fn cmd_status(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let racks = cfg.topology.racks();
    let rss_nodes: Vec<_> = topo
        .rss_sleds()
        .into_iter()
        .filter(|(sled, _)| sled.rack == 0)
        .collect();
    if rss_nodes.is_empty() {
        return Err(anyhow!("no RSS sled in topology"));
    }
    let d = &topo.runner;
    // Multi-rack racks converge under each other's load - watch longer (matches
    // cmd_launch). Duration is Copy, so each watcher closure gets its own.
    let watch_cap = rss_watch_cap(false, racks);
    let watchers = rss_nodes.into_iter().map(|(s, n)| {
        let tag = rack_label(racks, s.rack, "rack-init");
        let addr = s.bootstrap_addr();
        let known_ip = if cfg.external.isolated() {
            cfg.static_external_ips()
                .into_iter()
                .find(|(name, _)| name == &s.name)
                .map(|(_, ip)| ip)
        } else {
            None
        };
        async move { watch_rss(d, *n, &addr, &tag, watch_cap, known_ip).await }
    });
    futures::future::try_join_all(watchers).await?;
    Ok(())
}
