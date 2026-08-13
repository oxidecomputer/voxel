//! `voxel perftest` — disk-wear A/B measurement (INTERNAL).
//!
//!   This whole module is temporary instrumentation for the "murders disks"
//! work (see docs/voxel-roadmap.md) and is NOT part of the shipped operator
//! surface.
//! * this file (`voxel/src/perftest.rs`),
//! * the `mod perftest;` line in `main.rs`,
//! * the hidden `Perftest { cmd: perftest::PerftestCmd }` arm of `Cmd` in
//!   `main.rs`,
//! * its one dispatch line (`Cmd::Perftest { cmd } => perftest::run(...)`), and
//! * the `serde` dependency line in `voxel/Cargo.toml` (added only for this
//!   harness's JSON run docs — nothing else in the bin derives serde).
//!
//! Nothing else references it. (The `[disk_wear]` config section + `wear-*`
//! cargo-bay flags are NOT part of this harness — they are the actual fix and
//! stay.)
//!
//! It measures NVMe endurance burn on the Helios host so we can A/B the
//! disk-wear levers:
//!
//! * `sample` — snapshot the drives' own write counters (NVMe *Data Units
//!   Written* — the ground-truth wear metric) + per-pool ZFS allocation, to a
//!   JSON file. Take one before a run, one after.
//! * `sample-report` — diff two samples into bytes-written, write rate, and
//!   drive-lifetime projections (per lever combination), with a headline total
//!   scoped to the drives backing the falcon pool (unrelated OS/other-pool
//!   writes on shared drives are shown per-device but excluded from the total).
//! * `report` — normalize one or more raw perftest results into a portable
//!   interactive report and optional archive.
//! * `superreport` — combine portable report archives, deduplicate their
//!   underlying result digests, and recompute cohort-local recommendations over
//!   the larger aggregate sample.
//! * `preflight` — destructively prove that the API disk lifecycle workload can
//!   provision, probe, and cleanly tear down before running a matrix.
//! * `smooth` — the "feels snappy" axis: measure the per-operation *latency
//!   distribution* (p50/p90/p99, max, and a p99/p50 jitter ratio) of serial
//!   disk create/settle/delete requests. Complements the wear/time metrics with
//!   a jitter signal the matrix can't see.
//! * `levers` — print the active four-lever matrix for the current config.
//! * `matrix` — the A/B driver: for each lever combination, (re)launch the
//!   rack, measure bring-up wear + launch time + baseline-adjusted launch RAM
//!   growth (and optional workload wear + RAM growth), tear down, and emit a
//!   comparison table (+ `--out`
//!   CSV, + `--json-out` JSON). `--repeat N` runs each combo N times and
//!   summarizes the spread (mean/median/stddev/CV). Helios host only; each
//!   combination is a full launch, run strictly serially (see the nextest note
//!   in the roadmap).
//! * `compare` — diff two `matrix --json-out` runs (baseline vs candidate):
//!   per-combo, per-metric relative deltas, flagging which changes exceed the
//!   measured run-to-run noise rather than any fixed threshold.
//!
//! All values are reported in decimal units (GB = 1e9, TB = 1e12) to match NVMe
//! data units (512000 bytes) and drive TBW datasheets.

use crate::net::{node_external_ip, ssh_output_timeout, zlogin};
use crate::topo::build_topo;
use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use cookout::{CounterSample, ProjectionPolicy, project_samples};
use oxide_session::{OxideSession, ProvisionError};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use voxel_config::VoxelConfig;

mod cookout_adapter;
mod cookout_commands;
mod oxide_session;

/// NVMe spec: one "data unit" written/read = 1000 * 512 = 512000 bytes.
const DATA_UNIT_BYTES: u64 = 512_000;
/// Synthetic-disk block size for the load generator's blank disks.
const LOAD_BLOCK_SIZE: u64 = 512;
/// Logical storage needed by the complete fixed 20-disk workload recipe.
const DISK_LIFECYCLE_STORAGE_QUOTA_BYTES: u64 = 20 << 30;
/// How often the peak-RAM sampler polls host memory-in-use during a launch.
const RAM_SAMPLE_INTERVAL_MS: u64 = 1_000;

#[derive(Subcommand)]
pub enum PerftestCmd {
    /// Snapshot host NVMe write counters + per-pool ZFS allocation to a JSON
    /// sample (stdout, or `--out FILE`). Run on the Helios host (pfexec). Take
    /// one sample before a launch/workload and one after; feed both to
    /// `sample-report`.
    Sample {
        /// Label for this sample, e.g. "baseline-before" / "lever3-after".
        #[arg(long, default_value = "")]
        label: String,
        /// Write the sample JSON here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Diff two samples into bytes-written, rate, and lifetime projections.
    SampleReport {
        /// The earlier sample (from `perftest sample`).
        before: PathBuf,
        /// The later sample.
        after: PathBuf,
        /// Rated endurance per drive in TB (e.g. 1200) to project drive life.
        #[arg(long)]
        rated_tbw: Option<f64>,
    },
    /// Load one or more typed perftest results for an extensible offline report.
    Report {
        /// Supported perftest result JSON files.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Directory where report artifacts will be published.
        #[arg(long, value_name = "DIRECTORY")]
        out: PathBuf,
        /// Also publish `<DIRECTORY>.tar.gz`.
        #[arg(long)]
        archive: bool,
    },
    /// Aggregate evidence from one or more native report archives.
    Superreport {
        /// Archives produced by `perftest report` or `perftest superreport`.
        #[arg(required = true)]
        reports: Vec<PathBuf>,
        /// Directory where aggregate report artifacts will be published.
        #[arg(long, value_name = "DIRECTORY")]
        out: PathBuf,
        /// Also publish `<DIRECTORY>.tar.gz`.
        #[arg(long)]
        archive: bool,
    },
    /// Validate that a named workload can run before starting a matrix.
    Preflight {
        #[arg(long)]
        workload: WorkloadKind,
        #[arg(long)]
        oxide_auth_helper: Option<PathBuf>,
    },
    /// Measure control-plane *smoothness*: the per-operation latency
    /// distribution (p50/p90/p99, max, and a p99/p50 jitter ratio) of serial
    /// disk create / settle / delete (and optionally snapshot) requests against
    /// the running rack. Unlike the parallel API disk lifecycle workload, this
    /// runs one operation at a time so each latency is uncontended and jitter
    /// reflects real control-plane/storage stalls — the "does it feel snappy"
    /// axis. Requires `oxide` auth (OXIDE_HOST/OXIDE_TOKEN).
    Smooth {
        /// Number of measured create/settle/delete cycles (per phase sample
        /// count). More cycles tighten the high percentiles.
        #[arg(long, default_value_t = 50)]
        count: usize,
        /// Disk size for each cycle (bytes, or a k/m/g/t suffix).
        #[arg(long, default_value = "1G")]
        size: String,
        /// Project to create/use.
        #[arg(long, default_value = "perftestsmooth")]
        project: String,
        /// Also measure snapshot create/delete latency each cycle.
        #[arg(long)]
        snapshot: bool,
        /// Keep the project instead of cleaning up at the end.
        #[arg(long)]
        keep: bool,
        /// Write the per-phase latency samples + percentiles as JSON here.
        #[arg(long)]
        json_out: Option<PathBuf>,
    },
    /// Show the state of all four disk-wear levers for the current `voxel.toml`
    /// (levers 1-3 from `[disk_wear]`, lever 4 from `topology.rss_sleds`), so an
    /// A/B run's configuration is captured alongside its samples.
    Levers,
    /// Run the full A/B lever matrix (Helios host only). For each lever
    /// combination: tear down + reset host props, sample, launch the rack,
    /// sample bring-up wear, optionally run a workload + sample it, tear down;
    /// then print a comparison table (+ optional CSV). Each combination is a
    /// full multi-minute launch, run strictly serially.
    Matrix {
        /// Combinations to run, `;`-separated; each a `+`-list of lever numbers
        /// (1-4), or `none`/`all`. E.g. `"none;1;1+2;1+2+3;all"`. Default: a
        /// cumulative ladder (none, +1, +1+2, +1+2+3, +1+2+3+4) so each row
        /// shows one lever's marginal effect.
        #[arg(long)]
        combos: Option<String>,
        /// After each rack converges, run the named workload and measure its
        /// wear separately from bring-up.
        #[arg(long)]
        workload: Option<WorkloadKind>,
        /// Trusted provider for a non-default Oxide authentication setup.
        #[arg(long, requires = "workload")]
        oxide_auth_helper: Option<PathBuf>,
        /// RSS sled count for combos that include lever 4 (reduce replication);
        /// default 3, omicron's floor. See the caveat in `run_combo`.
        #[arg(long, default_value_t = 3)]
        rss_sleds: usize,
        /// Rated endurance per drive in TB, to project drive lifetime per combo.
        #[arg(long)]
        rated_tbw: Option<f64>,
        /// Repeat each combination this many times and summarize the spread
        /// (mean/median/stddev/CV per metric) so you can tell a real change from
        /// measurement noise. Each repeat is a full launch and a failed attempt
        /// is retried once after cleanup — cost is at least N x.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Write the results as CSV here (also printed as a table to stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write the full run (metadata + per-repeat samples + per-combo stats)
        /// as JSON here, for `perftest compare`. Additive to `--out`.
        #[arg(long)]
        json_out: Option<PathBuf>,
        /// Continue to the next combination if both execution attempts at a
        /// repeat fail (default: stop). A failed first execution is retried once
        /// only after cleanup proves a clean boundary; boundary failures abort.
        #[arg(long)]
        keep_going: bool,
    },
    /// Compare two `matrix --json-out` runs (baseline vs candidate): per-combo,
    /// per-metric relative deltas, flagging which exceed measured noise
    /// (`[*]`). Combos are matched by label; `[?]` means variance is unknown
    /// (either run had `--repeat 1`, so noise can't be estimated).
    Compare {
        /// The baseline run's JSON (from `matrix --json-out`).
        baseline: PathBuf,
        /// The candidate run's JSON to compare against the baseline.
        candidate: PathBuf,
    },
}

pub async fn run(
    cmd: &PerftestCmd,
    cfg: Option<&VoxelConfig>,
    name: &str,
) -> Result<()> {
    match cmd {
        PerftestCmd::Sample { label, out } => cmd_sample(label, out.as_deref()),
        PerftestCmd::SampleReport { before, after, rated_tbw } => {
            cmd_report(before, after, *rated_tbw)
        }
        PerftestCmd::Report { inputs, out, archive } => {
            cookout_commands::run_report(inputs, out, *archive)
        }
        PerftestCmd::Superreport { reports, out, archive } => {
            cookout_commands::run_superreport(reports, out, *archive)
        }
        PerftestCmd::Preflight { workload, oxide_auth_helper } => {
            cmd_preflight(cfg, name, *workload, oxide_auth_helper.as_deref())
                .await
        }
        PerftestCmd::Smooth {
            count,
            size,
            project,
            snapshot,
            keep,
            json_out,
        } => cmd_smooth(
            *count,
            size,
            project,
            *snapshot,
            *keep,
            json_out.as_deref(),
        ),
        PerftestCmd::Levers => cmd_levers(cfg),
        PerftestCmd::Matrix {
            combos,
            workload,
            oxide_auth_helper,
            rss_sleds,
            rated_tbw,
            repeat,
            out,
            json_out,
            keep_going,
        } => {
            cmd_matrix(
                cfg,
                name,
                combos.as_deref(),
                *workload,
                oxide_auth_helper.as_deref(),
                *rss_sleds,
                *rated_tbw,
                *repeat,
                out.as_deref(),
                json_out.as_deref(),
                *keep_going,
            )
            .await
        }
        PerftestCmd::Compare { baseline, candidate } => {
            cookout_commands::run_compare(baseline, candidate)
        }
    }
}

/// Print the state of all four disk-wear levers for the current `voxel.toml`, so
/// an A/B run's configuration is unambiguous. Levers 1-3 come from the
/// `[disk_wear]` section; lever 4 (reduce replication) is `topology.rss_sleds`
/// (a rack with fewer RSS sleds than total drops a sled's write load entirely).
fn cmd_levers(cfg: Option<&VoxelConfig>) -> Result<()> {
    let cfg = cfg.ok_or_else(|| {
        anyhow!("no voxel.toml found - run from a project dir or pass --config")
    })?;
    let w = &cfg.disk_wear;
    let on = |b: bool| if b { "ON " } else { "off" };

    // Lever 4: RSS membership below the total sled count means fewer sleds do
    // control-plane writes. `rss_sleds = 0` means "all sleds" (no reduction).
    let total = cfg.topology.sleds;
    let rss = cfg.topology.rss_count();
    let reduced = rss < total;

    println!(
        "disk-wear levers (see docs/voxel-roadmap.md; A/B via `voxel perftest sample`/`sample-report`):"
    );
    println!(
        "  [{}] 1 host sync=disabled      (host falcon dataset)",
        on(w.host_sync_disabled)
    );
    println!(
        "  [{}] 2 host compression+atime  (lz4, atime=off, logbias, redundant_metadata)",
        on(w.host_compression)
    );
    println!(
        "  [{}] 3 guest rpool/oxp tuning  (sync=disabled + compression in-guest)",
        on(w.guest_zfs_tuning)
    );
    println!(
        "  [{}] 4 reduce replication      (topology.rss_sleds = {} of {} sleds)",
        on(reduced),
        rss,
        total
    );
    let host = w.host_zfs_props();
    if !host.is_empty() {
        println!("\nhost dataset props applied at launch: {}", host.join(" "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// matrix — the A/B driver over lever combinations
// ---------------------------------------------------------------------------

/// JSON schema version for `matrix --json-out` documents. `compare` checks this
/// and refuses / warns on a mismatch rather than silently misreading a file.
const MATRIX_SCHEMA_VERSION: u32 = 4;
/// One initial attempt plus one retry after a proven clean boundary. Only the
/// successful attempt contributes a sample to the requested repeat count.
const MATRIX_REPEAT_ATTEMPTS: usize = 2;

/// One repeat's measured metrics for a single combo run. Bytes are gross NVMe
/// writes (Data Units Written * 512000) summed across the falcon pool's drives
/// (see [`falcon_pool_controllers`]; falls back to all drives if unresolved).
/// Successful schema-v4 repeats require a launch memory delta. Workload fields
/// are either all present for a measured workload or all absent when skipped.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatSample {
    /// Gross NVMe bytes written during rack bring-up (the primary wear metric).
    bringup_bytes: u64,
    /// Bring-up wall-clock in seconds (the launch-time metric).
    launch_secs: u64,
    /// Peak launch-window host RAM increase above its baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peak_ram_bytes: Option<u64>,
    /// Gross NVMe bytes written during the optional workload phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload_bytes: Option<u64>,
    /// Workload wall-clock in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload_secs: Option<u64>,
    /// Peak workload-window host RAM increase above its baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload_peak_delta_bytes: Option<u64>,
}

/// One combination's results across `repeat` runs, plus a launch error if the
/// combo never came up (`repeats` empty). The table and `compare` summarize the
/// repeats per metric via [`stats`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComboAggregate {
    label: String,
    levers: BTreeSet<u8>,
    repeats: Vec<RepeatSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ComboAggregate {
    /// Stats over a numeric metric extracted from each repeat that measured it.
    fn stat(&self, f: impl Fn(&RepeatSample) -> Option<f64>) -> Stats {
        let xs: Vec<f64> = self.repeats.iter().filter_map(&f).collect();
        stats(&xs)
    }
    fn bringup_bytes(&self) -> Stats {
        self.stat(|r| Some(r.bringup_bytes as f64))
    }
    fn launch_secs(&self) -> Stats {
        self.stat(|r| Some(r.launch_secs as f64))
    }
    fn workload_bytes(&self) -> Stats {
        self.stat(|r| r.workload_bytes.map(|b| b as f64))
    }
    fn workload_secs(&self) -> Stats {
        self.stat(|r| r.workload_secs.map(|s| s as f64))
    }
    fn peak_ram_bytes(&self) -> Stats {
        self.stat(|r| r.peak_ram_bytes.map(|b| b as f64))
    }
    fn workload_peak_delta_bytes(&self) -> Stats {
        self.stat(|r| r.workload_peak_delta_bytes.map(|b| b as f64))
    }
    /// Whether any repeat measured a workload phase.
    fn has_workload(&self) -> bool {
        self.repeats.iter().any(|r| r.workload_bytes.is_some())
    }
}

/// A full `matrix` run: metadata + per-combo aggregates. Serialized by
/// `matrix --json-out` and consumed by `perftest compare`.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    clap::ValueEnum,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadKind {
    ApiDiskLifecycle,
}

#[derive(
    Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkloadSpec {
    kind: WorkloadKind,
    count: usize,
    parallel: usize,
    size_bytes: u64,
    snapshot: bool,
}

impl WorkloadSpec {
    fn api_disk_lifecycle() -> Self {
        Self {
            kind: WorkloadKind::ApiDiskLifecycle,
            count: 20,
            parallel: 4,
            size_bytes: 1 << 30,
            snapshot: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OxideSessionMetadata {
    profile: String,
    host: String,
    provider: OxideAuthProviderMetadata,
    oxide_cli_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum OxideAuthProviderMetadata {
    Builtin,
    Helper { path: PathBuf },
}

const REDACTED_CREDENTIAL: &str = "<redacted>";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case", deny_unknown_fields)]
enum EvidenceValue<T> {
    Available { value: T },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixProvenance {
    voxel_build: EvidenceValue<String>,
    voxel_binary: EvidenceValue<String>,
    configured_image: EvidenceValue<String>,
    omicron_commit: EvidenceValue<String>,
    host: EvidenceValue<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixSessionIdentity {
    workload: Option<WorkloadSpec>,
    oxide_session: Option<OxideSessionMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixComboEvidence {
    label: String,
    levers: BTreeSet<u8>,
    effective_config: VoxelConfig,
}

/// A capability status is intentionally closed and shape-strict. Pass/fail
/// text names the proof boundary that was actually crossed; unavailable text
/// explains why the matrix did not measure the capability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum CapabilityStatus {
    Pass { evidence: String },
    Fail { evidence: String },
    Unavailable { reason: String },
}

/// Fixed capability ledger for evidence version 1. A struct (rather than a
/// map) makes every capability mandatory and rejects invented capability names.
///
/// Proof boundaries are deliberately narrow: scope is the strict Falcon/NVMe
/// sample validation; launch/teardown is the clean boundary around every
/// successful repeat; disk lifecycle is the completed measured API recipe;
/// zpool preparation is the inventory/buffer/dry-run checks executed before
/// that recipe. Broader Fleet, Silo, and multirack probes belong to future
/// contract versions once they have concrete proof boundaries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixCapabilityLedger {
    ledger_version: u32,
    matrix_host_storage_scope: CapabilityStatus,
    clean_launch_teardown_boundaries: CapabilityStatus,
    api_disk_lifecycle: CapabilityStatus,
    simulated_zpool_preparation: CapabilityStatus,
}

/// Report evidence has an explicit version so capability evidence can be added
/// later without turning this envelope into an untyped map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixReportEvidence {
    evidence_version: u32,
    base_config: VoxelConfig,
    combos: Vec<MatrixComboEvidence>,
    provenance: MatrixProvenance,
    session: MatrixSessionIdentity,
    capabilities: MatrixCapabilityLedger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunStatus {
    Running,
    Completed,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum BoundaryOutcome {
    Pending {},
    Clean {},
    Failure { error: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchAttemptFailure {
    error: String,
    clean_boundary: BoundaryOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchMetrics {
    bringup_bytes: u64,
    launch_secs: u64,
    peak_ram_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum LaunchOutcome {
    Pending {},
    Success {
        metrics: LaunchMetrics,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prior_attempt_failures: Vec<LaunchAttemptFailure>,
    },
    Failure {
        attempt_failures: Vec<LaunchAttemptFailure>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkloadMetrics {
    workload_bytes: u64,
    workload_secs: u64,
    workload_peak_delta_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum WorkloadOutcome {
    Pending {},
    NotRequested {},
    Success { metrics: WorkloadMetrics },
    Failure { error: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum PreparationOutcome {
    Pending {},
    NotRequested {},
    Success {},
    Failure { error: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixCheckpointRepeat {
    index: usize,
    pre_boundary: BoundaryOutcome,
    launch: LaunchOutcome,
    preparation: PreparationOutcome,
    workload: WorkloadOutcome,
    post_boundary: BoundaryOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixCheckpointCombo {
    label: String,
    levers: BTreeSet<u8>,
    effective_config: VoxelConfig,
    repeats: Vec<MatrixCheckpointRepeat>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixCheckpoint {
    #[serde(deserialize_with = "deserialize_schema_v5")]
    schema_version: u32,
    checkpoint_sequence: u64,
    status: RunStatus,
    abort_error: Option<String>,
    name: String,
    started: u64,
    updated: u64,
    ended: Option<u64>,
    rated_tbw: Option<f64>,
    workload: Option<WorkloadSpec>,
    oxide_session: Option<OxideSessionMetadata>,
    scope_proof: CapabilityStatus,
    report_evidence: Option<MatrixReportEvidence>,
    rss_sleds: usize,
    repeat: usize,
    combos: Vec<MatrixCheckpointCombo>,
}

/// Execute one requested repeat slot while making every durable transition
/// observable. The callbacks keep the state machine independently testable;
/// production supplies rack operations and `CheckpointPublisher::publish`.
#[derive(Debug)]
struct CheckpointedRepeatOutcome<L, W> {
    launch_data: Option<L>,
    workload_metadata: Option<W>,
}

async fn checkpointed_repeat_with<
    L,
    W,
    PF,
    BF,
    BFut,
    LF,
    LFut,
    PR,
    PRFut,
    WF,
    WFut,
>(
    repeat: &mut MatrixCheckpointRepeat,
    workload_requested: bool,
    mut publish: PF,
    mut boundary: BF,
    mut launch: LF,
    mut prepare: PR,
    mut workload: WF,
) -> Result<CheckpointedRepeatOutcome<L, W>>
where
    L: Clone,
    PF: FnMut(&MatrixCheckpointRepeat) -> Result<()>,
    BF: FnMut() -> BFut,
    BFut: std::future::Future<Output = Result<()>>,
    LF: FnMut() -> LFut,
    LFut: std::future::Future<Output = Result<(LaunchMetrics, L)>>,
    PR: FnMut(L) -> PRFut,
    PRFut: std::future::Future<Output = Result<L>>,
    WF: FnMut(L) -> WFut,
    WFut: std::future::Future<Output = Result<(WorkloadMetrics, L, W)>>,
{
    if let Err(error) = boundary().await {
        repeat.pre_boundary =
            BoundaryOutcome::Failure { error: format!("{error:#}") };
        publish(repeat)?;
        return Err(error).context("pre-repeat clean boundary");
    }
    repeat.pre_boundary = BoundaryOutcome::Clean {};
    publish(repeat)?;

    let mut prior_attempt_failures = Vec::new();
    let (metrics, launch_data) = loop {
        match launch().await {
            Ok(success) => break success,
            Err(error) => {
                prior_attempt_failures.push(LaunchAttemptFailure {
                    error: format!("{error:#}"),
                    clean_boundary: BoundaryOutcome::Pending {},
                });
                repeat.launch = LaunchOutcome::Failure {
                    attempt_failures: prior_attempt_failures.clone(),
                };
                publish(repeat)?;
                if let Err(cleanup) = boundary().await {
                    prior_attempt_failures.last_mut().unwrap().clean_boundary =
                        BoundaryOutcome::Failure {
                            error: format!("{cleanup:#}"),
                        };
                    repeat.launch = LaunchOutcome::Failure {
                        attempt_failures: prior_attempt_failures,
                    };
                    repeat.post_boundary = BoundaryOutcome::Failure {
                        error: format!("{cleanup:#}"),
                    };
                    publish(repeat)?;
                    return Err(cleanup)
                        .context("post-launch-failure clean boundary");
                }
                prior_attempt_failures.last_mut().unwrap().clean_boundary =
                    BoundaryOutcome::Clean {};
                repeat.launch = LaunchOutcome::Failure {
                    attempt_failures: prior_attempt_failures.clone(),
                };
                if prior_attempt_failures.len() == MATRIX_REPEAT_ATTEMPTS {
                    repeat.post_boundary = BoundaryOutcome::Clean {};
                    publish(repeat)?;
                    return Ok(CheckpointedRepeatOutcome {
                        launch_data: None,
                        workload_metadata: None,
                    });
                } else {
                    repeat.launch = LaunchOutcome::Failure {
                        attempt_failures: prior_attempt_failures.clone(),
                    };
                    publish(repeat)?;
                }
            }
        }
    };
    repeat.launch = LaunchOutcome::Success { metrics, prior_attempt_failures };
    publish(repeat)?;

    let mut launch_data = Some(launch_data);
    let mut workload_metadata = None;
    if workload_requested {
        match prepare(launch_data.as_ref().unwrap().clone()).await {
            Ok(returned_launch_data) => {
                launch_data = Some(returned_launch_data);
                repeat.preparation = PreparationOutcome::Success {};
            }
            Err(error) => {
                let error = format!("{error:#}");
                repeat.preparation =
                    PreparationOutcome::Failure { error: error.clone() };
                publish(repeat)?;
                repeat.workload = WorkloadOutcome::Failure {
                    error: format!(
                        "blocked by simulated zpool preparation failure: {error}"
                    ),
                };
            }
        }
        if matches!(repeat.preparation, PreparationOutcome::Success {}) {
            publish(repeat)?;
            repeat.workload =
                match workload(launch_data.as_ref().unwrap().clone()).await {
                    Ok((metrics, returned_launch_data, metadata)) => {
                        launch_data = Some(returned_launch_data);
                        workload_metadata = Some(metadata);
                        WorkloadOutcome::Success { metrics }
                    }
                    Err(error) => {
                        WorkloadOutcome::Failure { error: format!("{error:#}") }
                    }
                };
        }
    } else {
        repeat.preparation = PreparationOutcome::NotRequested {};
        repeat.workload = WorkloadOutcome::NotRequested {};
    }
    publish(repeat)?;

    if let Err(error) = boundary().await {
        repeat.post_boundary =
            BoundaryOutcome::Failure { error: format!("{error:#}") };
        publish(repeat)?;
        return Err(error).context("post-repeat clean boundary");
    }
    repeat.post_boundary = BoundaryOutcome::Clean {};
    publish(repeat)?;
    Ok(CheckpointedRepeatOutcome { launch_data, workload_metadata })
}

fn deserialize_schema_v5<'de, D>(
    deserializer: D,
) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version == 5 {
        Ok(version)
    } else {
        Err(D::Error::custom(format!(
            "unsupported matrix checkpoint schema version {version}; expected 5"
        )))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[derive(Clone, Copy, Debug)]
struct FileOwner {
    uid: libc::uid_t,
    gid: libc::gid_t,
}

fn requested_artifact_owner<'a>(
    destinations: impl IntoIterator<Item = &'a Path>,
) -> Result<Option<FileOwner>> {
    const OWNER_ENV: &str = "VOXEL_PERFTEST_ARTIFACT_OWNER";
    let value = match std::env::var(OWNER_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error).context(format!("read {OWNER_ENV}")),
    };
    let (uid, gid) = value
        .split_once(':')
        .with_context(|| format!("{OWNER_ENV} must be UID:GID"))?;
    let owner = FileOwner {
        uid: uid
            .parse()
            .with_context(|| format!("parse UID in {OWNER_ENV}"))?,
        gid: gid
            .parse()
            .with_context(|| format!("parse GID in {OWNER_ENV}"))?,
    };
    for destination in destinations {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let metadata = std::fs::metadata(parent).with_context(|| {
            format!("stat artifact parent {}", parent.display())
        })?;
        if metadata.uid() != owner.uid || metadata.gid() != owner.gid {
            bail!(
                "{OWNER_ENV} {value} does not own artifact parent {}",
                parent.display()
            );
        }
    }
    Ok(Some(owner))
}

fn write_artifact_sibling(
    parent: &Path,
    bytes: &[u8],
    owner: Option<FileOwner>,
    artifact: &str,
) -> Result<tempfile::NamedTempFile> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!("create {artifact} sibling in {}", parent.display())
        })?;
    temporary.write_all(bytes).with_context(|| {
        format!("write {artifact} sibling in {}", parent.display())
    })?;
    if let Some(owner) = owner {
        // Bind the privileged ownership handoff to the open inode after all
        // bytes are present, before its final name becomes visible.
        let result = unsafe {
            libc::fchown(temporary.as_raw_fd(), owner.uid, owner.gid)
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("set {artifact} owner in {}", parent.display())
            });
        }
    }
    temporary.as_file().sync_all().with_context(|| {
        format!("sync {artifact} sibling in {}", parent.display())
    })?;
    Ok(temporary)
}

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self { dev: metadata.dev(), ino: metadata.ino() }
    }

    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("stat checkpoint {}", path.display()))?;
        Ok(Self::from_metadata(&metadata))
    }

    fn read_file(file: &std::fs::File) -> Result<Self> {
        Ok(Self::from_metadata(
            &file.metadata().context("stat owned checkpoint")?,
        ))
    }
}

struct CheckpointPublisher {
    destination: PathBuf,
    destination_file: Option<std::fs::File>,
    owner: Option<FileOwner>,
    #[cfg(test)]
    fail_before_rename: bool,
    #[cfg(test)]
    fail_before_initial_install: bool,
    #[cfg(test)]
    fail_parent_sync: bool,
}

impl CheckpointPublisher {
    fn new(destination: &Path, owner: Option<FileOwner>) -> Self {
        Self {
            destination: destination.to_owned(),
            destination_file: None,
            owner,
            #[cfg(test)]
            fail_before_rename: false,
            #[cfg(test)]
            fail_before_initial_install: false,
            #[cfg(test)]
            fail_parent_sync: false,
        }
    }

    fn write_sibling(
        &self,
        parent: &Path,
        bytes: &[u8],
    ) -> Result<tempfile::NamedTempFile> {
        write_artifact_sibling(parent, bytes, self.owner, "checkpoint")
    }

    fn publish(&mut self, checkpoint: &mut MatrixCheckpoint) -> Result<()> {
        let mut next = checkpoint.clone();
        next.checkpoint_sequence = next
            .checkpoint_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow!("checkpoint sequence overflow"))?;
        next.updated = now_secs();
        let mut bytes = serde_json::to_vec_pretty(&next)
            .context("serialize matrix checkpoint")?;
        bytes.push(b'\n');

        if self.destination_file.is_none() {
            self.publish_initial(&bytes)?;
        } else {
            self.publish_replacement(&bytes)?;
        }
        *checkpoint = next;
        Ok(())
    }

    fn publish_initial(&mut self, bytes: &[u8]) -> Result<()> {
        let parent =
            self.destination.parent().unwrap_or_else(|| Path::new("."));
        let temporary = self.write_sibling(parent, bytes)?;

        #[cfg(test)]
        if self.fail_before_initial_install {
            return Err(anyhow!("injected checkpoint pre-install failure"));
        }

        let destination_file = temporary
            .persist_noclobber(&self.destination)
            .map_err(|error| {
            anyhow!(
                "install initial checkpoint {} (refusing overwrite): {}",
                self.destination.display(),
                error.error
            )
        })?;
        self.destination_file = Some(destination_file);
        self.sync_parent_after_install()?;
        Ok(())
    }

    fn publish_replacement(&mut self, bytes: &[u8]) -> Result<()> {
        let expected = FileIdentity::read_file(
            self.destination_file
                .as_ref()
                .expect("replacement requires a published destination file"),
        )?;
        let actual = FileIdentity::read(&self.destination)?;
        if actual != expected {
            return Err(anyhow!(
                "refusing to replace checkpoint {}: filesystem identity changed",
                self.destination.display()
            ));
        }
        let parent =
            self.destination.parent().unwrap_or_else(|| Path::new("."));
        let temporary = self.write_sibling(parent, bytes)?;

        #[cfg(test)]
        if self.fail_before_rename {
            return Err(anyhow!("injected checkpoint pre-rename failure"));
        }

        let actual = FileIdentity::read(&self.destination)?;
        if actual != expected {
            return Err(anyhow!(
                "refusing to replace checkpoint {}: filesystem identity changed",
                self.destination.display()
            ));
        }
        let destination_file =
            temporary.persist(&self.destination).map_err(|error| {
                anyhow!(
                    "replace checkpoint {}: {}",
                    self.destination.display(),
                    error.error
                )
            })?;
        self.destination_file = Some(destination_file);
        self.sync_parent_after_install()?;
        Ok(())
    }

    /// Once rename/persist makes complete bytes visible, a parent-directory
    /// sync error is ambiguous: the snapshot is visible but its name may not
    /// survive a crash. The error therefore requires the caller to stop rather
    /// than treating this as an ordinary pre-publication failure. The
    /// destination is never deleted or rolled back after installation.
    fn sync_parent_after_install(&self) -> Result<()> {
        #[cfg(test)]
        if self.fail_parent_sync {
            return Err(anyhow!("injected checkpoint parent sync failure"))
                .context("complete checkpoint was installed and is visible, but parent sync failed; durability is uncertain and execution must stop");
        }
        sync_parent(&self.destination).with_context(|| {
            "complete checkpoint was installed and is visible, but parent sync failed; durability is uncertain and execution must stop"
        })
    }
}

fn publish_checkpoint(
    publisher: &mut Option<CheckpointPublisher>,
    checkpoint: &mut MatrixCheckpoint,
) -> Result<()> {
    let derived = checkpoint_capability_ledger(checkpoint);
    if let Some(evidence) = &mut checkpoint.report_evidence {
        evidence.session.workload = checkpoint.workload.clone();
        evidence.session.oxide_session = checkpoint.oxide_session.clone();
        evidence.capabilities = derived;
    }
    if let Some(publisher) = publisher {
        publisher
            .publish(checkpoint)
            .map_err(|error| anyhow::Error::new(PublicationError(error)))?;
    }
    Ok(())
}

fn checkpoint_capability_ledger(
    checkpoint: &MatrixCheckpoint,
) -> MatrixCapabilityLedger {
    let repeats = checkpoint.combos.iter().flat_map(|combo| &combo.repeats);
    let boundary_failure = repeats.clone().any(|repeat| {
        matches!(repeat.pre_boundary, BoundaryOutcome::Failure { .. })
            || matches!(repeat.post_boundary, BoundaryOutcome::Failure { .. })
            || match &repeat.launch {
                LaunchOutcome::Success { prior_attempt_failures, .. } => {
                    prior_attempt_failures.as_slice()
                }
                LaunchOutcome::Failure { attempt_failures } => {
                    attempt_failures.as_slice()
                }
                LaunchOutcome::Pending {} => &[],
            }
            .iter()
            .any(|attempt| {
                matches!(
                    attempt.clean_boundary,
                    BoundaryOutcome::Failure { .. }
                )
            })
    });
    let all_boundaries_clean = checkpoint.status == RunStatus::Completed
        && checkpoint.combos.iter().flat_map(|combo| &combo.repeats).all(
            |repeat| {
                matches!(repeat.pre_boundary, BoundaryOutcome::Clean {})
                    && matches!(repeat.post_boundary, BoundaryOutcome::Clean {})
                    && match &repeat.launch {
                        LaunchOutcome::Success {
                            prior_attempt_failures,
                            ..
                        } => prior_attempt_failures.as_slice(),
                        LaunchOutcome::Failure { attempt_failures } => {
                            attempt_failures.as_slice()
                        }
                        LaunchOutcome::Pending {} => &[],
                    }
                    .iter()
                    .all(|attempt| {
                        matches!(
                            attempt.clean_boundary,
                            BoundaryOutcome::Clean {}
                        )
                    })
            },
        );
    let preparation_failure =
        checkpoint.combos.iter().flat_map(|combo| &combo.repeats).any(
            |repeat| {
                matches!(repeat.preparation, PreparationOutcome::Failure { .. })
            },
        );
    let preparations_complete = checkpoint.status == RunStatus::Completed
        && checkpoint.combos.iter().flat_map(|combo| &combo.repeats).all(
            |repeat| {
                matches!(repeat.preparation, PreparationOutcome::Success {})
            },
        );
    let workload_failure = checkpoint
        .combos
        .iter()
        .flat_map(|combo| &combo.repeats)
        .any(|repeat| {
            matches!(repeat.preparation, PreparationOutcome::Success {})
                && matches!(repeat.workload, WorkloadOutcome::Failure { .. })
        });
    let workloads_complete = checkpoint.status == RunStatus::Completed
        && checkpoint.combos.iter().flat_map(|combo| &combo.repeats).all(
            |repeat| matches!(repeat.workload, WorkloadOutcome::Success { .. }),
        );
    let boundaries = if boundary_failure {
        capability_fail("a required clean boundary failed".into())
    } else if all_boundaries_clean {
        capability_pass(
            "every required repeat completed with clean pre-launch and post-run boundaries",
        )
    } else {
        capability_unavailable("clean boundary proof is in progress")
    };
    let workload = match checkpoint.workload {
        None => capability_unavailable(
            "API disk lifecycle workload was not enabled",
        ),
        Some(_) if workload_failure => capability_fail(
            "a required API disk lifecycle workload failed".into(),
        ),
        Some(_) if workloads_complete => capability_pass(
            "every required repeat completed the measured API disk lifecycle workload",
        ),
        Some(_) => {
            capability_unavailable("API disk lifecycle proof is in progress")
        }
    };
    let preparation = match checkpoint.workload {
        None => capability_unavailable(
            "simulated zpool preparation was not enabled",
        ),
        Some(_) if preparation_failure => capability_fail(
            "a required simulated zpool preparation failed".into(),
        ),
        Some(_) if preparations_complete => capability_pass(
            "every required repeat completed simulated zpool preparation",
        ),
        Some(_) => capability_unavailable(
            "simulated zpool preparation proof is in progress",
        ),
    };
    MatrixCapabilityLedger {
        ledger_version: 1,
        matrix_host_storage_scope: checkpoint.scope_proof.clone(),
        clean_launch_teardown_boundaries: boundaries,
        api_disk_lifecycle: workload.clone(),
        simulated_zpool_preparation: preparation,
    }
}

#[derive(Debug)]
struct PublicationError(anyhow::Error);

impl std::fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "checkpoint publication failed: {:#}", self.0)
    }
}

impl std::error::Error for PublicationError {}

fn may_publish_aborted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PublicationError>().is_none()
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!("sync checkpoint parent directory {}", parent.display())
        })
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct MatrixRun {
    schema_version: u32,
    name: String,
    /// Unix seconds at the start / end of the whole matrix run.
    started: u64,
    ended: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rated_tbw: Option<f64>,
    workload: Option<WorkloadSpec>,
    oxide_session: Option<OxideSessionMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report_evidence: Option<MatrixReportEvidence>,
    rss_sleds: usize,
    repeat: usize,
    combos: Vec<String>,
    results: Vec<ComboAggregate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixRunWire {
    schema_version: u32,
    name: String,
    started: u64,
    ended: u64,
    #[serde(default)]
    rated_tbw: Option<f64>,
    #[serde(default)]
    workload: Option<WorkloadSpec>,
    #[serde(default)]
    oxide_session: Option<OxideSessionMetadata>,
    #[serde(default)]
    report_evidence: Option<MatrixReportEvidence>,
    rss_sleds: usize,
    repeat: usize,
    combos: Vec<String>,
    results: Vec<ComboAggregate>,
}

impl<'de> Deserialize<'de> for MatrixRun {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value.as_object().ok_or_else(|| {
            D::Error::custom("matrix run must be a JSON object")
        })?;
        let has_load = object.contains_key("load");
        let wire: MatrixRunWire =
            serde_json::from_value(value).map_err(D::Error::custom)?;
        match wire.schema_version {
            4 => {}
            version @ (2 | 3) => {
                return Err(D::Error::custom(format!(
                    "matrix schema v{version} records absolute peak_ram_bytes; schema v4 baseline-adjusted memory metrics are required"
                )));
            }
            version => {
                return Err(D::Error::custom(format!(
                    "unsupported matrix schema version {version}"
                )));
            }
        }
        if has_load {
            return Err(D::Error::custom(
                "schema v4 cannot contain legacy load",
            ));
        }
        if wire.workload.is_some() != wire.oxide_session.is_some() {
            return Err(D::Error::custom(
                "schema v4 workload and oxide_session must either both be present or both be null",
            ));
        }
        let run = Self {
            schema_version: MATRIX_SCHEMA_VERSION,
            name: wire.name,
            started: wire.started,
            ended: wire.ended,
            rated_tbw: wire.rated_tbw,
            workload: wire.workload,
            oxide_session: wire.oxide_session,
            report_evidence: wire.report_evidence,
            rss_sleds: wire.rss_sleds,
            repeat: wire.repeat,
            combos: wire.combos,
            results: wire.results,
        };
        if let Some(evidence) = &run.report_evidence {
            validate_report_evidence(&run, evidence)
                .map_err(D::Error::custom)?;
        }
        Ok(run)
    }
}

fn combine_operation_and_cleanup<T>(
    operation: Result<T>,
    cleanup: Result<()>,
    what: &str,
) -> Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => {
            Err(cleanup).with_context(|| format!("{what} cleanup failed"))
        }
        (Err(error), Err(cleanup)) => Err(anyhow!(
            "{what} failed: {error:#}; additionally cleanup failed: {cleanup:#}"
        )),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum RepeatFailureDisposition {
    Retry,
    Exhausted(String),
}

#[derive(Debug)]
enum RepeatRunError {
    Execution(anyhow::Error),
    Permanent(anyhow::Error),
    Boundary(anyhow::Error),
}

impl std::fmt::Display for RepeatRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Execution(error)
            | Self::Permanent(error)
            | Self::Boundary(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for RepeatRunError {}

fn finish_repeat_execution<T>(
    execution: std::result::Result<T, RepeatRunError>,
    cleanup: Result<()>,
) -> std::result::Result<T, RepeatRunError> {
    match (execution, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(RepeatRunError::Boundary(cleanup)),
        (Err(error), Err(cleanup)) => Err(RepeatRunError::Boundary(anyhow!(
            "repeat execution failed: {error:#}; additionally cleanup failed: {cleanup:#}"
        ))),
    }
}

fn record_repeat_failure(
    errors: &mut Vec<String>,
    attempt: usize,
    error: &anyhow::Error,
) -> RepeatFailureDisposition {
    errors
        .push(format!("attempt {attempt}/{MATRIX_REPEAT_ATTEMPTS}: {error:#}"));
    if attempt < MATRIX_REPEAT_ATTEMPTS {
        RepeatFailureDisposition::Retry
    } else {
        RepeatFailureDisposition::Exhausted(errors.join("; "))
    }
}

fn preflight_output_paths(
    out: Option<&Path>,
    json_out: Option<&Path>,
) -> Result<()> {
    if out.is_some() && out == json_out {
        return Err(anyhow!("--out and --json-out must name different paths"));
    }
    for (flag, path) in [("--out", out), ("--json-out", json_out)] {
        if let Some(path) = path
            && path.try_exists().with_context(|| {
                format!("check {flag} path {}", path.display())
            })?
        {
            return Err(anyhow!(
                "{flag} path {} already exists; refusing to overwrite",
                path.display()
            ));
        }
    }
    Ok(())
}

fn write_new(
    path: &Path,
    bytes: &[u8],
    owner: Option<FileOwner>,
) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = write_artifact_sibling(parent, bytes, owner, "output")?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow!(
            "install output {} (refusing overwrite): {}",
            path.display(),
            error.error
        )
    })?;
    sync_parent(path).with_context(|| {
        format!("sync output parent for {}", path.display())
    })?;
    Ok(())
}

fn publish_matrix_outputs(
    csv: Option<(&Path, &[u8])>,
    json: Option<(&Path, &[u8])>,
    owner: Option<FileOwner>,
) -> Result<()> {
    let mut csv_created = None;
    if let Some((path, bytes)) = csv {
        write_new(path, bytes, owner).context("publish matrix CSV")?;
        csv_created = Some(path);
    }
    if let Some((path, bytes)) = json
        && let Err(error) =
            write_new(path, bytes, owner).context("publish matrix JSON")
    {
        if let Some(csv_path) = csv_created {
            return match std::fs::remove_file(csv_path) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow!(
                    "{error:#}; removing previously published CSV {} also failed: {cleanup}",
                    csv_path.display()
                )),
            };
        }
        return Err(error);
    }
    Ok(())
}

fn publish_final_csv_with<F>(
    checkpoint: &MatrixCheckpoint,
    publish: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if checkpoint.status != RunStatus::Completed {
        return Err(anyhow!(
            "final CSV publication requires a completed checkpoint"
        ));
    }
    publish()
}

fn validate_matrix_run(run: &MatrixRun) -> Result<()> {
    if run.repeat == 0 {
        return Err(anyhow!("matrix repeat count is zero"));
    }
    if run.combos.len() != run.results.len() {
        return Err(anyhow!(
            "matrix planned {} combos but has {} results",
            run.combos.len(),
            run.results.len()
        ));
    }
    for (index, (planned, result)) in
        run.combos.iter().zip(&run.results).enumerate()
    {
        if result.levers.iter().any(|lever| !(1..=4).contains(lever)) {
            return Err(anyhow!(
                "matrix combo {index} contains an unsupported storage lever"
            ));
        }
        let canonical = canonical_combo_label(&result.levers);
        if result.label != *planned || result.label != canonical {
            return Err(anyhow!(
                "matrix combo {index} mismatch: planned '{planned}', result '{}', canonical '{canonical}'",
                result.label
            ));
        }
        if let Some(error) = &result.error {
            return Err(anyhow!(
                "combo '{}' has aggregate error: {error}",
                result.label
            ));
        }
        if result.repeats.len() != run.repeat {
            return Err(anyhow!(
                "combo '{}' has {} repeats, expected {}",
                result.label,
                result.repeats.len(),
                run.repeat
            ));
        }
        for (repeat_index, repeat) in result.repeats.iter().enumerate() {
            if repeat.peak_ram_bytes.is_none() {
                return Err(anyhow!(
                    "combo '{}' repeat {} is missing Helios peak_ram_bytes",
                    result.label,
                    repeat_index + 1
                ));
            }
            let workload_fields = (
                repeat.workload_bytes.is_some(),
                repeat.workload_secs.is_some(),
                repeat.workload_peak_delta_bytes.is_some(),
            );
            if workload_fields.0 != workload_fields.1
                || workload_fields.0 != workload_fields.2
            {
                return Err(anyhow!(
                    "combo '{}' repeat {} has incomplete workload bytes/time/memory fields",
                    result.label,
                    repeat_index + 1
                ));
            }
            if run.workload.is_some() != workload_fields.0 {
                return Err(anyhow!(
                    "combo '{}' repeat {} workload fields do not match workload={}",
                    result.label,
                    repeat_index + 1,
                    run.workload.is_some()
                ));
            }
        }
    }
    if let Some(evidence) = &run.report_evidence {
        validate_report_evidence(run, evidence)?;
    }
    Ok(())
}

fn validate_publishable_matrix_run(run: &MatrixRun) -> Result<()> {
    match validate_matrix_run(run) {
        Ok(()) => Ok(()),
        Err(_) if run.results.iter().any(|result| result.error.is_some()) => {
            validate_report_failed_matrix(run)
                .context("validate retained failed storage matrix")
        }
        Err(error) => Err(error),
    }
}

/// Reporting may retain a matrix aggregate that stopped after an execution
/// failure. Compare and normal matrix readers intentionally remain strict.
fn validate_report_failed_matrix(matrix: &MatrixRun) -> Result<()> {
    if matrix.repeat == 0 || matrix.results.len() != matrix.combos.len() {
        bail!("retained failed matrix has invalid repeat or result count");
    }
    for (result, label) in matrix.results.iter().zip(&matrix.combos) {
        if result.label != *label
            || result.label != canonical_combo_label(&result.levers)
        {
            bail!("retained failed matrix combo identity mismatch");
        }
        for (index, repeat) in result.repeats.iter().enumerate() {
            if repeat.peak_ram_bytes.is_none() {
                bail!(
                    "combo '{}' repeat {} is missing Helios peak_ram_bytes",
                    result.label,
                    index + 1
                );
            }
            let workload = [
                repeat.workload_bytes.is_some(),
                repeat.workload_secs.is_some(),
                repeat.workload_peak_delta_bytes.is_some(),
            ];
            if workload.iter().any(|present| *present != workload[0])
                || matrix.workload.is_some() != workload[0]
            {
                bail!(
                    "combo '{}' repeat {} has invalid workload metrics",
                    result.label,
                    index + 1
                );
            }
        }
        match result.error.as_deref() {
            None if result.repeats.len() == matrix.repeat => {}
            Some(error)
                if !error.is_empty()
                    && result.repeats.len() < matrix.repeat => {}
            _ => bail!(
                "retained failed matrix results must be complete successes or carry an error with fewer than expected repeats"
            ),
        }
    }
    Ok(())
}

fn validate_matrix_checkpoint(matrix: &MatrixCheckpoint) -> Result<()> {
    if matrix.repeat == 0 || matrix.combos.is_empty() {
        bail!("schema-v5 matrix must request at least one combo and repeat");
    }
    match matrix.status {
        RunStatus::Running if matrix.ended.is_some() => {
            bail!("running matrix cannot have ended")
        }
        RunStatus::Completed | RunStatus::Aborted if matrix.ended.is_none() => {
            bail!("terminal matrix must have ended")
        }
        _ => {}
    }
    match (&matrix.status, matrix.abort_error.as_deref()) {
        (RunStatus::Aborted, Some(error)) if !error.is_empty() => {}
        (RunStatus::Aborted, _) => {
            bail!("aborted matrix requires a nonempty abort_error")
        }
        (_, None) => {}
        _ => bail!("only an aborted matrix may contain abort_error"),
    }
    let completed = matrix.status == RunStatus::Completed;
    let mut labels = std::collections::BTreeSet::new();
    for combo in &matrix.combos {
        if combo.label != canonical_combo_label(&combo.levers)
            || !labels.insert(&combo.label)
        {
            bail!("schema-v5 matrix combo identity is not exact and canonical");
        }
        if combo.repeats.len() != matrix.repeat {
            bail!(
                "combo '{}' does not contain every requested repeat slot",
                combo.label
            );
        }
        for (index, repeat) in combo.repeats.iter().enumerate() {
            if repeat.index != index {
                bail!(
                    "combo '{}' repeat slot index is not canonical",
                    combo.label
                );
            }
            let has_pending =
                matches!(repeat.pre_boundary, BoundaryOutcome::Pending {})
                    || matches!(repeat.launch, LaunchOutcome::Pending {})
                    || (matches!(
                        repeat.preparation,
                        PreparationOutcome::Pending {}
                    ) && !matches!(
                        repeat.launch,
                        LaunchOutcome::Failure { .. }
                    ))
                    || (matches!(repeat.workload, WorkloadOutcome::Pending {})
                        && !matches!(
                            repeat.launch,
                            LaunchOutcome::Failure { .. }
                        ))
                    || matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending {}
                    )
                    || match &repeat.launch {
                        LaunchOutcome::Success {
                            prior_attempt_failures,
                            ..
                        } => prior_attempt_failures.iter().any(|failure| {
                            matches!(
                                failure.clean_boundary,
                                BoundaryOutcome::Pending {}
                            )
                        }),
                        LaunchOutcome::Failure { attempt_failures } => {
                            attempt_failures.iter().any(|failure| {
                                matches!(
                                    failure.clean_boundary,
                                    BoundaryOutcome::Pending {}
                                )
                            })
                        }
                        LaunchOutcome::Pending {} => false,
                    };
            if completed && has_pending {
                bail!("completed matrix contains a pending repeat stage");
            }
            let boundary_has_empty_failure = |boundary: &BoundaryOutcome| matches!(boundary, BoundaryOutcome::Failure { error } if error.is_empty());
            let workload_is_expected_pre_launch =
                |workload: &WorkloadOutcome| {
                    matches!(
                        (&matrix.workload, workload),
                        (Some(_), WorkloadOutcome::Pending {})
                            | (None, WorkloadOutcome::NotRequested {})
                    )
                };
            match (
                &matrix.workload,
                &repeat.preparation,
                &repeat.workload,
                &repeat.launch,
            ) {
                (
                    None,
                    PreparationOutcome::NotRequested {},
                    WorkloadOutcome::NotRequested {},
                    _,
                ) => {}
                (
                    Some(_),
                    PreparationOutcome::Pending {},
                    WorkloadOutcome::Pending {},
                    LaunchOutcome::Pending {} | LaunchOutcome::Success { .. },
                ) => {}
                (
                    Some(_),
                    PreparationOutcome::Success {},
                    WorkloadOutcome::Pending {}
                    | WorkloadOutcome::Success { .. }
                    | WorkloadOutcome::Failure { .. },
                    LaunchOutcome::Success { .. },
                ) => {}
                (
                    Some(_),
                    PreparationOutcome::Failure { error },
                    WorkloadOutcome::Pending {},
                    LaunchOutcome::Success { .. },
                ) if !completed
                    && !error.is_empty()
                    && matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending {}
                    ) => {}
                (
                    Some(_),
                    PreparationOutcome::Failure { error },
                    WorkloadOutcome::Failure { error: workload_error },
                    LaunchOutcome::Success { .. },
                ) if !error.is_empty()
                    && workload_error.contains(
                        "blocked by simulated zpool preparation failure",
                    ) => {}
                (
                    Some(_),
                    PreparationOutcome::Pending {},
                    WorkloadOutcome::Pending {},
                    LaunchOutcome::Failure { .. },
                ) => {}
                _ => bail!(
                    "preparation/workload ordering or requested state is invalid"
                ),
            }
            if boundary_has_empty_failure(&repeat.pre_boundary)
                || boundary_has_empty_failure(&repeat.post_boundary)
            {
                bail!("repeat boundary failure has empty error text");
            }
            if matches!(repeat.pre_boundary, BoundaryOutcome::Pending {})
                && (!matches!(repeat.launch, LaunchOutcome::Pending {})
                    || !workload_is_expected_pre_launch(&repeat.workload)
                    || !matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending {}
                    ))
            {
                bail!("pending pre-boundary cannot have later stage evidence");
            }
            if matches!(repeat.pre_boundary, BoundaryOutcome::Failure { .. })
                && (!matches!(repeat.launch, LaunchOutcome::Pending {})
                    || !workload_is_expected_pre_launch(&repeat.workload)
                    || !matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending {}
                    ))
            {
                bail!(
                    "failed pre-boundary cannot have launch or workload evidence"
                );
            }
            match &repeat.launch {
                LaunchOutcome::Pending {}
                    if !workload_is_expected_pre_launch(&repeat.workload)
                        || !matches!(
                            repeat.post_boundary,
                            BoundaryOutcome::Pending {}
                        ) =>
                {
                    bail!("pending launch cannot have later stage evidence")
                }
                LaunchOutcome::Failure { attempt_failures } => {
                    if attempt_failures.is_empty()
                        || attempt_failures.len() > MATRIX_REPEAT_ATTEMPTS
                        || attempt_failures.iter().any(|failure| {
                            failure.error.is_empty()
                                || boundary_has_empty_failure(
                                    &failure.clean_boundary,
                                )
                        })
                        || !workload_is_expected_pre_launch(&repeat.workload)
                    {
                        bail!(
                            "failed launch has invalid attempt or workload evidence"
                        );
                    }
                    if attempt_failures[..attempt_failures.len() - 1]
                        .iter()
                        .any(|failure| {
                            !matches!(
                                failure.clean_boundary,
                                BoundaryOutcome::Clean {}
                            )
                        })
                    {
                        bail!(
                            "failed launch has a dirty non-final attempt boundary"
                        );
                    }
                    match &attempt_failures.last().unwrap().clean_boundary {
                        BoundaryOutcome::Pending {}
                            if !completed
                                && matches!(
                                    repeat.post_boundary,
                                    BoundaryOutcome::Pending {}
                                ) => {}
                        BoundaryOutcome::Failure { error }
                            if matches!(
                                &repeat.post_boundary,
                                BoundaryOutcome::Failure { error: post_error }
                                    if post_error == error
                            ) => {}
                        BoundaryOutcome::Clean {}
                            if attempt_failures.len()
                                == MATRIX_REPEAT_ATTEMPTS
                                && matches!(
                                    repeat.post_boundary,
                                    BoundaryOutcome::Clean {}
                                ) => {}
                        BoundaryOutcome::Clean {}
                            if !completed
                                && attempt_failures.len()
                                    < MATRIX_REPEAT_ATTEMPTS
                                && matches!(
                                    repeat.post_boundary,
                                    BoundaryOutcome::Pending {}
                                ) => {}
                        _ => bail!(
                            "failed launch has an invalid final attempt boundary"
                        ),
                    }
                }
                LaunchOutcome::Success { prior_attempt_failures, .. } => {
                    if prior_attempt_failures.iter().any(|failure| {
                        failure.error.is_empty()
                            || !matches!(
                                failure.clean_boundary,
                                BoundaryOutcome::Clean {}
                            )
                    }) || prior_attempt_failures.len()
                        >= MATRIX_REPEAT_ATTEMPTS
                    {
                        bail!(
                            "successful launch has invalid prior-attempt failure evidence"
                        );
                    }
                    match (&matrix.workload, &repeat.workload) {
                        (None, WorkloadOutcome::NotRequested {})
                        | (Some(_), WorkloadOutcome::Pending {})
                        | (Some(_), WorkloadOutcome::Success { .. }) => {}
                        (Some(_), WorkloadOutcome::Failure { error })
                            if !error.is_empty() => {}
                        _ => bail!(
                            "workload outcome disagrees with requested workload"
                        ),
                    }
                    if matrix.workload.is_some()
                        && matches!(
                            repeat.workload,
                            WorkloadOutcome::Pending {}
                        )
                        && !matches!(
                            repeat.post_boundary,
                            BoundaryOutcome::Pending {}
                        )
                    {
                        bail!(
                            "pending requested workload requires a pending post-boundary"
                        );
                    }
                }
                LaunchOutcome::Pending {} => {}
            }
            if completed
                && (!matches!(repeat.pre_boundary, BoundaryOutcome::Clean {})
                    || matches!(repeat.launch, LaunchOutcome::Pending {})
                    || !matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Clean {}
                    ))
            {
                bail!(
                    "completed matrix requires every pre and post boundary to be clean"
                );
            }
        }
    }
    if let Some(evidence) = &matrix.report_evidence {
        if evidence.evidence_version != 1
            || evidence.capabilities.ledger_version != 1
            || evidence.session.workload != matrix.workload
            || evidence.session.oxide_session != matrix.oxide_session
        {
            bail!(
                "report evidence version/session disagrees with checkpoint configuration"
            );
        }
        if evidence.combos.len() != matrix.combos.len() {
            bail!("report evidence combo count disagrees with checkpoint plan");
        }
        for (planned, reported) in matrix.combos.iter().zip(&evidence.combos) {
            if planned.label != reported.label
                || planned.levers != reported.levers
                || planned.effective_config != reported.effective_config
            {
                bail!(
                    "report evidence combo identity/configuration disagrees with checkpoint plan"
                );
            }
            if planned.effective_config.recovery_silo.user_password_hash
                != REDACTED_CREDENTIAL
            {
                bail!(
                    "checkpoint effective configuration contains an unredacted credential"
                );
            }
        }
        if evidence.base_config.recovery_silo.user_password_hash
            != REDACTED_CREDENTIAL
        {
            bail!(
                "checkpoint report evidence contains an unredacted credential"
            );
        }
        if evidence.capabilities != checkpoint_capability_ledger(matrix) {
            bail!(
                "report evidence capability ledger disagrees with checkpoint stages"
            );
        }
    }
    Ok(())
}

fn validate_report_evidence(
    run: &MatrixRun,
    evidence: &MatrixReportEvidence,
) -> Result<()> {
    if evidence.evidence_version != 1 {
        return Err(anyhow!(
            "unsupported matrix report evidence version {}",
            evidence.evidence_version
        ));
    }
    if evidence.base_config.recovery_silo.user_password_hash
        != REDACTED_CREDENTIAL
    {
        return Err(anyhow!(
            "matrix report evidence base config contains an unredacted credential"
        ));
    }
    if evidence.session.workload != run.workload
        || evidence.session.oxide_session != run.oxide_session
    {
        return Err(anyhow!(
            "matrix report evidence session identity does not match matrix workload/session"
        ));
    }
    if evidence.combos.len() != run.combos.len()
        || evidence.combos.len() != run.results.len()
    {
        return Err(anyhow!("matrix report evidence combo count mismatch"));
    }
    for (index, (combo, result)) in
        evidence.combos.iter().zip(&run.results).enumerate()
    {
        if combo.levers.iter().any(|lever| !(1..=4).contains(lever)) {
            return Err(anyhow!(
                "matrix report evidence combo {index} contains an unsupported storage lever"
            ));
        }
        if combo.label != run.combos[index]
            || combo.label != result.label
            || combo.levers != result.levers
        {
            return Err(anyhow!(
                "matrix report evidence combo {index} identity mismatch"
            ));
        }
        let expected =
            apply_combo(&evidence.base_config, &combo.levers, run.rss_sleds);
        if combo.effective_config != expected {
            return Err(anyhow!(
                "matrix report evidence combo '{}' effective config mismatch",
                combo.label
            ));
        }
        if combo.effective_config.recovery_silo.user_password_hash
            != REDACTED_CREDENTIAL
        {
            return Err(anyhow!(
                "matrix report evidence combo '{}' contains an unredacted credential",
                combo.label
            ));
        }
    }
    for (name, value) in [
        ("voxel_build", &evidence.provenance.voxel_build),
        ("voxel_binary", &evidence.provenance.voxel_binary),
        ("configured_image", &evidence.provenance.configured_image),
        ("omicron_commit", &evidence.provenance.omicron_commit),
        ("host", &evidence.provenance.host),
    ] {
        let text = match value {
            EvidenceValue::Available { value } => value,
            EvidenceValue::Unavailable { reason } => reason,
        };
        if text.trim().is_empty() {
            return Err(anyhow!(
                "matrix report evidence provenance {name} is blank"
            ));
        }
    }
    if evidence.capabilities.ledger_version != 1 {
        return Err(anyhow!(
            "unsupported matrix capability ledger version {}",
            evidence.capabilities.ledger_version
        ));
    }
    for (name, status) in [
        (
            "matrix_host_storage_scope",
            &evidence.capabilities.matrix_host_storage_scope,
        ),
        (
            "clean_launch_teardown_boundaries",
            &evidence.capabilities.clean_launch_teardown_boundaries,
        ),
        ("api_disk_lifecycle", &evidence.capabilities.api_disk_lifecycle),
        (
            "simulated_zpool_preparation",
            &evidence.capabilities.simulated_zpool_preparation,
        ),
    ] {
        let text = match status {
            CapabilityStatus::Pass { evidence }
            | CapabilityStatus::Fail { evidence } => evidence,
            CapabilityStatus::Unavailable { reason } => reason,
        };
        if text.trim().is_empty() {
            return Err(anyhow!(
                "matrix capability {name} has blank evidence/reason"
            ));
        }
    }
    let expected = build_capability_ledger(
        run.workload.as_ref(),
        &run.results,
        run.repeat,
    );
    if evidence.capabilities != expected {
        return Err(anyhow!(
            "matrix capability ledger does not match completed repeat proofs"
        ));
    }
    Ok(())
}

fn capability_pass(evidence: &str) -> CapabilityStatus {
    CapabilityStatus::Pass { evidence: evidence.into() }
}

fn capability_fail(evidence: String) -> CapabilityStatus {
    CapabilityStatus::Fail { evidence }
}

fn capability_unavailable(reason: &str) -> CapabilityStatus {
    CapabilityStatus::Unavailable { reason: reason.into() }
}

fn build_capability_ledger(
    workload: Option<&WorkloadSpec>,
    results: &[ComboAggregate],
    required_repeats: usize,
) -> MatrixCapabilityLedger {
    let complete = !results.is_empty()
        && results.iter().all(|combo| {
            combo.error.is_none()
                && combo.repeats.len() == required_repeats
                && combo
                    .repeats
                    .iter()
                    .all(|repeat| repeat.peak_ram_bytes.is_some())
        });
    let failure =
        results.iter().find_map(|combo| combo.error.as_deref()).unwrap_or(
            "one or more required repeats did not reach the proof boundary",
        );
    let measured_status = |proof: &str| {
        if results.is_empty() {
            capability_unavailable("no matrix combination was measured")
        } else if complete {
            capability_pass(proof)
        } else {
            capability_fail(failure.into())
        }
    };
    let workload_complete = complete
        && results.iter().all(|combo| {
            combo.repeats.iter().all(|repeat| {
                repeat.workload_bytes.is_some()
                    && repeat.workload_secs.is_some()
                    && repeat.workload_peak_delta_bytes.is_some()
            })
        });
    let workload_status = |proof: &str| match workload {
        None => capability_unavailable(
            "API disk lifecycle workload was not enabled",
        ),
        Some(_) if workload_complete => capability_pass(proof),
        Some(_) => capability_fail(failure.into()),
    };

    MatrixCapabilityLedger {
        ledger_version: 1,
        matrix_host_storage_scope: measured_status(
            "every required repeat used strict, stable Falcon-pool NVMe controller scope",
        ),
        clean_launch_teardown_boundaries: measured_status(
            "every required repeat completed its pre-launch and post-run clean boundaries",
        ),
        api_disk_lifecycle: workload_status(
            "every required repeat completed the measured API disk create/probe/delete recipe",
        ),
        simulated_zpool_preparation: workload_status(
            "every required workload repeat passed simulated-zpool inventory, buffer, and allocation checks",
        ),
    }
}

fn unavailable(reason: &str) -> EvidenceValue<String> {
    EvidenceValue::Unavailable { reason: reason.into() }
}

fn observed_command(
    command: &str,
    args: &[&str],
    unavailable_reason: &str,
) -> EvidenceValue<String> {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| EvidenceValue::Available { value })
        .unwrap_or_else(|| unavailable(unavailable_reason))
}

fn build_report_evidence(
    base: &VoxelConfig,
    plan: &[(String, BTreeSet<u8>)],
    rss_sleds: usize,
    workload: Option<WorkloadSpec>,
    oxide_session: Option<OxideSessionMetadata>,
    results: &[ComboAggregate],
    repeat: usize,
) -> MatrixReportEvidence {
    let mut sanitized = base.clone();
    sanitized.recovery_silo.user_password_hash = REDACTED_CREDENTIAL.into();
    let configured_image = sanitized
        .image
        .cp
        .clone()
        .map(|value| EvidenceValue::Available { value })
        .unwrap_or_else(|| unavailable("no control-plane image configured"));
    let omicron_commit = sanitized
        .image
        .cp_commit()
        .map(|value| EvidenceValue::Available { value })
        .unwrap_or_else(|| {
            unavailable("configured image does not expose an Omicron commit")
        });
    let voxel_binary = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .map(|bytes| EvidenceValue::Available {
            value: format!("sha256:{:x}", Sha256::digest(bytes)),
        })
        .unwrap_or_else(|| {
            unavailable("current executable path is not observable")
        });
    MatrixReportEvidence {
        evidence_version: 1,
        combos: plan
            .iter()
            .map(|(label, levers)| MatrixComboEvidence {
                label: label.clone(),
                levers: levers.clone(),
                effective_config: apply_combo(&sanitized, levers, rss_sleds),
            })
            .collect(),
        base_config: sanitized,
        provenance: MatrixProvenance {
            voxel_build: EvidenceValue::Available {
                value: env!("CARGO_PKG_VERSION").into(),
            },
            voxel_binary,
            configured_image,
            omicron_commit,
            host: observed_command(
                "hostid",
                &[],
                "stable host identity is not observable via hostid",
            ),
        },
        session: MatrixSessionIdentity {
            workload: workload.clone(),
            oxide_session,
        },
        capabilities: build_capability_ledger(
            workload.as_ref(),
            results,
            repeat,
        ),
    }
}

#[cfg(test)]
fn update_report_evidence_runtime(
    evidence: &mut MatrixReportEvidence,
    workload: Option<&WorkloadSpec>,
    oxide_session: Option<OxideSessionMetadata>,
    results: &[ComboAggregate],
    repeat: usize,
) {
    evidence.session.oxide_session = oxide_session;
    evidence.capabilities = build_capability_ledger(workload, results, repeat);
}

fn canonical_combo_label(levers: &BTreeSet<u8>) -> String {
    if levers.is_empty() {
        "none".to_string()
    } else {
        levers.iter().map(u8::to_string).collect::<Vec<_>>().join("+")
    }
}

/// Summary statistics over the repeats of one metric.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Stats {
    /// Number of samples the stats were computed over (repeats that measured).
    n: usize,
    mean: f64,
    median: f64,
    /// Sample (n-1) standard deviation; `0.0` when `n < 2`.
    stddev: f64,
    /// Coefficient of variation (stddev / mean); `None` when `mean == 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cv: Option<f64>,
}

/// Mean / median / sample-stddev / coefficient-of-variation over `xs`.
///
/// * empty  -> all-zero, `cv = None`, `n = 0`
/// * n == 1 -> stddev `0.0`, `cv = Some(0.0)` (or `None` if the value is `0`)
/// * n >= 2 -> sample (n-1) standard deviation
fn stats(xs: &[f64]) -> Stats {
    if xs.is_empty() {
        return Stats::default();
    }
    let n = xs.len();
    let nf = n as f64;
    let mean = xs.iter().sum::<f64>() / nf;

    let mut sorted = xs.to_vec();
    sorted
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = n / 2;
    let median = if n.is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    };

    let stddev = if n >= 2 {
        (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nf - 1.0)).sqrt()
    } else {
        0.0
    };
    let cv = (mean != 0.0).then_some(stddev / mean);
    Stats { n, mean, median, stddev, cv }
}

fn peak_ram_delta(baseline: u64, peak: u64) -> u64 {
    peak.saturating_sub(baseline)
}

fn sample_peak_ram_until_stopped(
    baseline: u64,
    stop: &AtomicBool,
    mut probe: impl FnMut() -> Option<u64>,
    mut wait: impl FnMut(),
) -> Option<u64> {
    let mut peak: Option<u64> = None;
    while !stop.load(Ordering::Relaxed) {
        let reading = probe();
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(used) = reading {
            peak = Some(peak.map_or(used, |current| current.max(used)));
        }
        wait();
    }
    peak.map(|peak| peak_ram_delta(baseline, peak))
}

fn require_ram_delta(
    delta: Option<u64>,
    phase: &str,
) -> std::result::Result<u64, RepeatRunError> {
    delta.ok_or_else(|| {
        RepeatRunError::Execution(anyhow!(
            "{phase} host RAM sampler did not record an in-window Helios sample"
        ))
    })
}

/// Best-effort peak host-RAM sampler. Reads memory-in-use before some work runs,
/// polls it on a background thread while the work runs, and returns the largest
/// observed increase above that baseline. If the baseline probe is unavailable
/// (e.g. run off-Helios), `finish()` yields `None`; matrix callers reject that
/// repeat rather than publishing an incomplete memory measurement.
struct PeakRamSampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Option<u64>>>,
}

impl PeakRamSampler {
    fn start() -> Self {
        let baseline = host_mem_in_use_bytes();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = baseline.map(|baseline| {
            std::thread::spawn(move || {
                sample_peak_ram_until_stopped(
                    baseline,
                    &stop_thread,
                    host_mem_in_use_bytes,
                    || {
                        std::thread::sleep(Duration::from_millis(
                            RAM_SAMPLE_INTERVAL_MS,
                        ))
                    },
                )
            })
        });
        PeakRamSampler { stop, handle }
    }

    /// Stop sampling and return the peak increase above the baseline (bytes).
    fn finish(mut self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take()?.join().ok().flatten()
    }
}

/// Host RAM currently in use (bytes) on illumos: `(physmem - pagesfree) *
/// pagesize`, reading the two page counts from kstat and the page size from
/// `pagesize(1)`. Returns `None` if any probe is unavailable — this is the one
/// place that knows how to read host memory, so it is swappable in isolation.
fn host_mem_in_use_bytes() -> Option<u64> {
    let pagesize = capture("pagesize", &[])?.trim().parse::<u64>().ok()?;
    let physmem = kstat_u64("unix:0:system_pages:physmem")?;
    let pagesfree = kstat_u64("unix:0:system_pages:pagesfree")?;
    Some(physmem.saturating_sub(pagesfree).saturating_mul(pagesize))
}

/// Read a single fully-qualified kstat statistic (`module:instance:name:stat`)
/// as a u64. `kstat -p <spec>` prints `<spec><TAB><value>`.
fn kstat_u64(spec: &str) -> Option<u64> {
    let out = capture("kstat", &["-p", spec])?;
    out.lines().next()?.split_whitespace().last()?.parse::<u64>().ok()
}

#[allow(clippy::too_many_arguments)]
async fn cmd_matrix(
    cfg: Option<&VoxelConfig>,
    name: &str,
    combos: Option<&str>,
    workload: Option<WorkloadKind>,
    oxide_auth_helper: Option<&Path>,
    rss_sleds: usize,
    rated_tbw: Option<f64>,
    repeat: usize,
    out: Option<&Path>,
    json_out: Option<&Path>,
    keep_going: bool,
) -> Result<()> {
    let load = workload.is_some();
    preflight_output_paths(out, json_out)?;
    let base = cfg.ok_or_else(|| {
        anyhow!("no voxel.toml found - run from a project dir or pass --config")
    })?;
    let plan = parse_combos(combos)?;
    // Validate every requested topology before the first repeat tears down a
    // rack or resets host ZFS properties. In particular, an invalid lever-4
    // participant count must not fail only after the matrix mutates host state.
    for (label, levers) in &plan {
        apply_combo(base, levers, rss_sleds)
            .topology
            .validate_rss_membership()
            .map_err(|error| {
                anyhow!("combo '{label}' has invalid RSS membership: {error}")
            })?;
    }
    if let Some(json_out) = json_out {
        return cmd_matrix_checkpointed(
            base,
            name,
            plan,
            workload,
            oxide_auth_helper,
            rss_sleds,
            rated_tbw,
            repeat.max(1),
            out,
            json_out,
            keep_going,
        )
        .await;
    }
    if workload.is_some() {
        oxide_session::static_preflight(
            base,
            oxide_auth_helper,
            &std::env::temp_dir(),
        )
        .context("Oxide static preflight before matrix mutation")?;
    }
    // Prove the measurement scope before any teardown, reset, or launch mutates
    // topology. Every subsequent matrix sample is checked against this scope.
    let scope_proof = collect_matrix_sample("matrix-scope-proof", None)?;
    let repeat = repeat.max(1);
    println!(
        "[perftest] matrix: {} combination(s) x {repeat} repeat(s) on '{name}'{}",
        plan.len(),
        if load { ", with workload" } else { "" }
    );
    let rep_note =
        if repeat > 1 { format!(" x {repeat}") } else { String::new() };
    println!(
        "[perftest] WARNING: each combination is a full rack launch (minutes), run serially{rep_note}.\n"
    );

    let started = now_secs();
    let mut results = Vec::new();
    let mut matrix_session = OxideSessionAggregation::Unobserved;
    'combo: for (i, (label, levers)) in plan.iter().enumerate() {
        let cfg2 = apply_combo(base, levers, rss_sleds);
        let mut repeats = Vec::new();
        for rep in 0..repeat {
            println!(
                "=== [{}/{}] combo '{label}'  (levers: {})  repeat {}/{} ===",
                i + 1,
                plan.len(),
                describe_levers(levers),
                rep + 1,
                repeat
            );
            let mut attempt_errors = Vec::new();
            for attempt in 1..=MATRIX_REPEAT_ATTEMPTS {
                match run_combo(
                    &cfg2,
                    name,
                    workload,
                    oxide_auth_helper,
                    &scope_proof,
                )
                .await
                {
                    Ok((sample, metadata)) => {
                        matrix_session.merge(metadata)?;
                        if attempt > 1 {
                            eprintln!(
                                "[perftest] combo '{label}' repeat {}/{repeat} recovered on attempt {attempt}/{MATRIX_REPEAT_ATTEMPTS}",
                                rep + 1
                            );
                        }
                        repeats.push(sample);
                        break;
                    }
                    Err(RepeatRunError::Boundary(error)) => {
                        return Err(error).with_context(|| {
                            format!(
                                "combo '{label}' repeat {}/{} attempt {attempt}/{MATRIX_REPEAT_ATTEMPTS} clean-boundary failure",
                                rep + 1,
                                repeat
                            )
                        });
                    }
                    Err(RepeatRunError::Permanent(error)) => {
                        return Err(error).with_context(|| {
                            format!(
                                "combo '{label}' repeat {}/{} permanent workload failure",
                                rep + 1,
                                repeat
                            )
                        });
                    }
                    Err(RepeatRunError::Execution(error)) => {
                        let error = error.context(format!(
                            "combo '{label}' repeat {}/{} attempt {attempt}/{MATRIX_REPEAT_ATTEMPTS} execution",
                            rep + 1,
                            repeat
                        ));
                        eprintln!(
                            "[perftest] combo '{label}' repeat {}/{repeat} attempt {attempt}/{MATRIX_REPEAT_ATTEMPTS} failed: {error:#}",
                            rep + 1
                        );
                        let disposition = record_repeat_failure(
                            &mut attempt_errors,
                            attempt,
                            &error,
                        );
                        let proof = clean_repeat_boundary(&cfg2, name)
                            .context("final clean-boundary proof after failed repeat attempt");
                        if let Err(cleanup) = proof {
                            return Err(anyhow!(
                                "combo '{label}' repeat {} execution attempts failed: {}; final teardown/reset proof also failed: {cleanup:#}",
                                rep + 1,
                                attempt_errors.join("; "),
                            ));
                        }
                        match disposition {
                            RepeatFailureDisposition::Retry => {
                                eprintln!(
                                    "[perftest] combo '{label}' repeat {}/{repeat}: clean boundary proven; retrying as attempt {}/{MATRIX_REPEAT_ATTEMPTS}",
                                    rep + 1,
                                    attempt + 1
                                );
                            }
                            RepeatFailureDisposition::Exhausted(error) => {
                                results.push(ComboAggregate {
                                    label: label.clone(),
                                    levers: levers.clone(),
                                    repeats,
                                    error: Some(error.clone()),
                                });
                                if !keep_going {
                                    return Err(anyhow!(
                                        "matrix aborted at combo '{label}' after retry exhaustion (pass --keep-going to skip failures): {error}"
                                    ));
                                }
                                continue 'combo;
                            }
                        }
                    }
                }
            }
        }
        results.push(ComboAggregate {
            label: label.clone(),
            levers: levers.clone(),
            repeats,
            error: None,
        });
    }
    let ended = now_secs();

    let matrix_workload = workload.map(|_| WorkloadSpec::api_disk_lifecycle());
    let matrix_session = matrix_session.finish();
    let report_evidence = build_report_evidence(
        base,
        &plan,
        rss_sleds,
        matrix_workload.clone(),
        matrix_session.clone(),
        &results,
        repeat,
    );

    let run = MatrixRun {
        schema_version: MATRIX_SCHEMA_VERSION,
        name: name.to_string(),
        started,
        ended,
        rated_tbw,
        workload: matrix_workload,
        oxide_session: matrix_session,
        report_evidence: Some(report_evidence),
        rss_sleds,
        repeat,
        combos: plan.iter().map(|(l, _)| l.clone()).collect(),
        results,
    };
    validate_publishable_matrix_run(&run)
        .context("matrix result completeness validation")?;
    println!("\n{}", render_table(&run.results, rated_tbw)?);

    let csv = out.map(|_| render_csv(&run.results));
    let json = json_out
        .map(|_| {
            serde_json::to_string_pretty(&run).context("serialize matrix run")
        })
        .transpose()?
        .map(|json| format!("{json}\n"));
    publish_matrix_outputs(
        out.zip(csv.as_deref().map(str::as_bytes)),
        json_out.zip(json.as_deref().map(str::as_bytes)),
        None,
    )?;
    if let Some(path) = out {
        println!("[perftest] wrote CSV -> {}", path.display());
    }
    if let Some(path) = json_out {
        println!(
            "[perftest] wrote JSON -> {} (feed to `perftest compare`)",
            path.display()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_matrix_checkpointed(
    base: &VoxelConfig,
    name: &str,
    plan: Vec<(String, BTreeSet<u8>)>,
    workload: Option<WorkloadKind>,
    oxide_auth_helper: Option<&Path>,
    rss_sleds: usize,
    rated_tbw: Option<f64>,
    repeat_count: usize,
    out: Option<&Path>,
    json_out: &Path,
    keep_going: bool,
) -> Result<()> {
    let started = now_secs();
    let workload_spec = workload.map(|_| WorkloadSpec::api_disk_lifecycle());
    let evidence = build_report_evidence(
        base,
        &plan,
        rss_sleds,
        workload_spec.clone(),
        None,
        &[],
        repeat_count,
    );
    let mut checkpoint = MatrixCheckpoint {
        schema_version: 5,
        checkpoint_sequence: 0,
        status: RunStatus::Running,
        abort_error: None,
        name: name.into(),
        started,
        updated: started,
        ended: None,
        rated_tbw,
        workload: workload_spec.clone(),
        oxide_session: None,
        scope_proof: capability_unavailable(
            "matrix scope has not yet been sampled",
        ),
        report_evidence: Some(evidence.clone()),
        rss_sleds,
        repeat: repeat_count,
        combos: evidence
            .combos
            .iter()
            .map(|combo| MatrixCheckpointCombo {
                label: combo.label.clone(),
                levers: combo.levers.clone(),
                effective_config: combo.effective_config.clone(),
                repeats: (0..repeat_count)
                    .map(|index| MatrixCheckpointRepeat {
                        index,
                        pre_boundary: BoundaryOutcome::Pending {},
                        launch: LaunchOutcome::Pending {},
                        preparation: if workload.is_some() {
                            PreparationOutcome::Pending {}
                        } else {
                            PreparationOutcome::NotRequested {}
                        },
                        workload: if workload.is_some() {
                            WorkloadOutcome::Pending {}
                        } else {
                            WorkloadOutcome::NotRequested {}
                        },
                        post_boundary: BoundaryOutcome::Pending {},
                    })
                    .collect(),
            })
            .collect(),
    };
    let owner = requested_artifact_owner(std::iter::once(json_out).chain(out))?;
    let mut publisher = Some(CheckpointPublisher::new(json_out, owner));
    publish_checkpoint(&mut publisher, &mut checkpoint)?;

    let fatal = async {
        if workload.is_some() {
            oxide_session::static_preflight(
                base,
                oxide_auth_helper,
                &std::env::temp_dir(),
            )
            .context("Oxide static preflight before matrix mutation")?;
        }
        let scope = match collect_matrix_sample("matrix-scope-proof", None) {
            Ok(scope) => {
                checkpoint.scope_proof = capability_pass(
                    "strict Falcon/NVMe matrix scope sample succeeded",
                );
                publish_checkpoint(&mut publisher, &mut checkpoint)?;
                scope
            }
            Err(error) => {
                checkpoint.scope_proof = capability_fail(format!("{error:#}"));
                publish_checkpoint(&mut publisher, &mut checkpoint)?;
                return Err(error);
            }
        };
        let mut results = Vec::new();
        let mut sessions = OxideSessionAggregation::Unobserved;
        for combo_index in 0..checkpoint.combos.len() {
            let cfg = apply_combo(
                base,
                &checkpoint.combos[combo_index].levers,
                rss_sleds,
            );
            let label = checkpoint.combos[combo_index].label.clone();
            let levers = checkpoint.combos[combo_index].levers.clone();
            let mut samples = Vec::new();
            let mut combo_error = None;
            for repeat_index in 0..repeat_count {
                let mut slot = checkpoint.combos[combo_index].repeats
                    [repeat_index]
                    .clone();
                let outcome = checkpointed_repeat_with(
                    &mut slot,
                    workload.is_some(),
                    |updated_slot| {
                        checkpoint.combos[combo_index].repeats[repeat_index] =
                            updated_slot.clone();
                        publish_checkpoint(&mut publisher, &mut checkpoint)
                    },
                    || std::future::ready(clean_repeat_boundary(&cfg, name)),
                    || async {
                        let (sample, after_launch, external_reachable) =
                            run_launch_phase(&cfg, name, &scope).await?;
                        let metrics = LaunchMetrics {
                            bringup_bytes: sample.bringup_bytes,
                            launch_secs: sample.launch_secs,
                            peak_ram_bytes: sample
                                .peak_ram_bytes
                                .expect("launch phase requires RAM"),
                        };
                        Ok((
                            metrics,
                            (sample, after_launch, external_reachable),
                        ))
                    },
                    |launch_data| async {
                        prepare_simulated_zpool_capacity(&cfg, name)
                            .await
                            .map_err(classified_anyhow)?;
                        Ok(launch_data)
                    },
                    |(mut sample, after_launch, external_reachable)| {
                        let cfg = &cfg;
                        let scope = &scope;
                        async move {
                            require_workload_external_dns(external_reachable)?;
                            let (metrics, metadata) = run_workload_phase(
                                cfg,
                                name,
                                oxide_auth_helper,
                                scope,
                                &after_launch,
                            )
                            .await?;
                            sample.workload_bytes =
                                Some(metrics.workload_bytes);
                            sample.workload_secs = Some(metrics.workload_secs);
                            sample.workload_peak_delta_bytes =
                                Some(metrics.workload_peak_delta_bytes);
                            Ok((
                                metrics,
                                (sample, after_launch, external_reachable),
                                metadata,
                            ))
                        }
                    },
                )
                .await?;
                let Some((sample, _after_launch, _external_reachable)) =
                    outcome.launch_data
                else {
                    let LaunchOutcome::Failure { attempt_failures } =
                        &slot.launch
                    else {
                        unreachable!(
                            "launch data is absent only after launch exhaustion"
                        )
                    };
                    combo_error = Some(
                        attempt_failures
                            .iter()
                            .map(|failure| failure.error.as_str())
                            .collect::<Vec<_>>()
                            .join("; "),
                    );
                    if !keep_going {
                        return Err(anyhow!(
                            "combo '{label}' launch retry exhausted"
                        ));
                    }
                    continue;
                };
                if let WorkloadOutcome::Failure { error } = &slot.workload {
                    combo_error = Some(error.clone());
                }
                if let Some(metadata) = outcome.workload_metadata {
                    sessions.merge(metadata)?;
                }
                samples.push(sample);
            }
            results.push(ComboAggregate {
                label,
                levers,
                repeats: samples,
                error: combo_error,
            });
        }
        let ended = now_secs();
        let session = sessions.finish();
        checkpoint.oxide_session = session.clone();
        checkpoint.status = RunStatus::Completed;
        checkpoint.ended = Some(ended);
        publish_checkpoint(&mut publisher, &mut checkpoint)?;
        println!("\n{}", render_table(&results, rated_tbw)?);
        Ok((results, out.map(Path::to_path_buf)))
    }
    .await;
    let (results, csv_path) = match fatal {
        Ok(done) => done,
        Err(error) => {
            if !may_publish_aborted(&error) {
                return Err(error);
            }
            checkpoint.status = RunStatus::Aborted;
            checkpoint.abort_error = Some(format!("{error:#}"));
            checkpoint.ended = Some(now_secs());
            publish_checkpoint(&mut publisher, &mut checkpoint)?;
            return Err(error);
        }
    };
    if let Some(path) = csv_path {
        publish_final_csv_with(&checkpoint, || {
            publish_matrix_outputs(
                Some((&path, render_csv(&results).as_bytes())),
                None,
                owner,
            )
        })?;
    }
    Ok(())
}

/// Run a single combination end-to-end and return its wear. Starts from a clean
/// slate (tear down any prior rack + `zfs inherit` the host props so the last
/// combo's levers 1/2 don't leak in), then launches with this combo's config -
/// `cmd_launch` applies levers 1/2 (host `zfs set`) and stages lever 3 as a
/// cargo-bay flag, so passing `cfg` is all it takes to realize the combo.
///
/// Lever 4 uses the topology's scrimlet-safe RSS selection, retaining both
/// scrimlets before filling remaining participant slots with non-scrimlets.
/// Combos that fail to launch are recorded as errors (see `--keep-going`).
fn clean_repeat_boundary(cfg: &VoxelConfig, name: &str) -> Result<()> {
    combine_operation_and_cleanup(
        crate::rack::cmd_destroy(cfg, name).context("topology teardown"),
        reset_host_zfs_props().context("host ZFS property reset"),
        "repeat clean boundary",
    )
}

const SIMULATED_U2_ZPOOLS_PER_SLED: usize = 5;
const ZPOOL_PREPARATION_DEADLINE: Duration = Duration::from_secs(300);
const ZPOOL_PREPARATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const ZPOOL_UPDATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const ZPOOL_UPDATE_RETRY_DELAY: Duration = Duration::from_secs(1);
const OMDB: &str = "/opt/oxide/omdb/bin/omdb";
const ZPOOL_CAPABILITY_SENTINEL: &str = "__VOXEL_ZPOOL_CAPABILITY_DONE__";
const ZPOOL_LIST_SENTINEL: &str = "__VOXEL_ZPOOL_LIST_DONE__";
const ZPOOL_SET_SENTINEL: &str = "__VOXEL_ZPOOL_SET_DONE__";
const REGION_ALLOCATION_SENTINEL: &str = "__VOXEL_REGION_ALLOCATION_DONE__";

fn expected_simulated_zpool_count_per_rack(cfg: &VoxelConfig) -> usize {
    cfg.topology.rss_count() * SIMULATED_U2_ZPOOLS_PER_SLED
}

fn omdb_zpool_capability_command() -> String {
    format!(
        "test -x {OMDB} && {OMDB} -w db zpool set-storage-buffer --help >/dev/null 2>&1 && {OMDB} db region dry-run-region-allocation --help >/dev/null 2>&1 && echo {ZPOOL_CAPABILITY_SENTINEL}"
    )
}

fn omdb_zpool_list_command() -> String {
    format!(
        "LC_ALL=C {OMDB} db zpool list -i 2>/dev/null && echo {ZPOOL_LIST_SENTINEL}"
    )
}

fn omdb_zpool_set_buffer_command(id: uuid::Uuid) -> String {
    format!(
        "LC_ALL=C {OMDB} -w db zpool set-storage-buffer {id} 0 2>&1 && echo {ZPOOL_SET_SENTINEL}"
    )
}

fn omdb_region_allocation_dry_run_command() -> String {
    format!(
        "LC_ALL=C {OMDB} db region dry-run-region-allocation --block-size 512 --size {} --distinct-sleds --num-regions-required 3 2>&1 && echo {REGION_ALLOCATION_SENTINEL}",
        1u64 << 30
    )
}

fn omdb_switch_zone_command(command: &str) -> String {
    zlogin(&format!("'{command}'"))
}

fn has_unique_terminal_sentinel(output: &str, sentinel: &str) -> bool {
    output.lines().next_back() == Some(sentinel)
        && output.matches(sentinel).count() == 1
}

fn parse_omdb_zpool_ids(output: &str) -> Result<Vec<uuid::Uuid>> {
    if !has_unique_terminal_sentinel(output, ZPOOL_LIST_SENTINEL) {
        return Err(anyhow!(
            "zpool list completion sentinel missing or malformed"
        ));
    }
    let (ids, _) = output
        .split_once(ZPOOL_LIST_SENTINEL)
        .expect("validated zpool list sentinel");
    let mut parsed = Vec::new();
    let mut unique = BTreeSet::new();
    for line in ids.lines() {
        let id = uuid::Uuid::parse_str(line).with_context(|| {
            format!("zpool list contained non-UUID row {line:?}")
        })?;
        if id.to_string() != line {
            return Err(anyhow!(
                "zpool list contained non-canonical UUID row {line:?}"
            ));
        }
        if !unique.insert(id) {
            return Err(anyhow!("zpool list contained duplicate UUID {id}"));
        }
        parsed.push(id);
    }
    Ok(parsed)
}

fn omdb_failure_summary(output: &str) -> &'static str {
    if output.contains("Not enough datasets") {
        "region allocation reports not enough provisionable datasets"
    } else if output.contains("Not enough unique zpools selected") {
        "region allocation reports fewer than three unique zpools"
    } else if output.contains("Not enough space") {
        "region allocation reports insufficient accounted pool space"
    } else if output.contains("InsufficientCapacity") {
        "region allocation reports insufficient capacity"
    } else {
        "omdb returned a non-success status"
    }
}

#[allow(clippy::too_many_arguments)]
fn retry_omdb_command_with(
    rack_name: &str,
    operation: &str,
    success_sentinel: &str,
    command_timeout: Duration,
    deadline: Instant,
    retry_delay: Duration,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
    mut run: impl FnMut(Duration) -> Option<String>,
) -> ClassifiedResult<()> {
    let mut attempts = 0u32;
    let mut last_outcome = "no update attempted";
    loop {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            return Err(ClassifiedFailure::Retryable(anyhow!(
                "{rack_name}: {operation} did not complete before the shared deadline after {attempts} attempt(s); last outcome: {last_outcome}"
            )));
        }
        let timeout = remaining.min(command_timeout);
        attempts += 1;
        let started = now();
        match run(timeout) {
            Some(output)
                if has_unique_terminal_sentinel(&output, success_sentinel) =>
            {
                return Ok(());
            }
            Some(output) => last_outcome = omdb_failure_summary(&output),
            None if now().saturating_duration_since(started) >= timeout => {
                last_outcome = "command timed out";
            }
            None => last_outcome = "SSH transport returned no usable output",
        }
        if attempts == 1 || attempts.is_multiple_of(30) {
            eprintln!(
                "[perftest] {rack_name}: {operation} has not completed ({last_outcome}); retrying within the shared deadline"
            );
        }
        let remaining = deadline.saturating_duration_since(now());
        if !remaining.is_zero() {
            sleep(retry_delay.min(remaining));
        }
    }
}

fn set_zpool_storage_buffer_with(
    rack_name: &str,
    id: uuid::Uuid,
    deadline: Instant,
    retry_delay: Duration,
    now: impl FnMut() -> Instant,
    sleep: impl FnMut(Duration),
    run: impl FnMut(Duration) -> Option<String>,
) -> ClassifiedResult<()> {
    retry_omdb_command_with(
        rack_name,
        &format!("setting zpool {id} storage buffer"),
        ZPOOL_SET_SENTINEL,
        ZPOOL_UPDATE_COMMAND_TIMEOUT,
        deadline,
        retry_delay,
        now,
        sleep,
        run,
    )
}

fn wait_for_region_allocation_with(
    rack_name: &str,
    deadline: Instant,
    retry_delay: Duration,
    now: impl FnMut() -> Instant,
    sleep: impl FnMut(Duration),
    run: impl FnMut(Duration) -> Option<String>,
) -> ClassifiedResult<()> {
    retry_omdb_command_with(
        rack_name,
        "waiting for a 1-GiB three-region allocation dry run",
        REGION_ALLOCATION_SENTINEL,
        ZPOOL_UPDATE_COMMAND_TIMEOUT,
        deadline,
        retry_delay,
        now,
        sleep,
        run,
    )
}

fn prepare_simulated_zpools_on_rack(
    rack_name: &str,
    ip: &str,
    expected: usize,
    deadline: Instant,
) -> ClassifiedResult<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ClassifiedFailure::Retryable(anyhow!(
            "{rack_name}: shared zpool preparation deadline elapsed before capability probing"
        )));
    }
    let capability = ssh_output_timeout(
        ip,
        &omdb_switch_zone_command(&omdb_zpool_capability_command()),
        remaining.min(ZPOOL_PREPARATION_COMMAND_TIMEOUT),
    )
    .ok_or_else(|| {
        ClassifiedFailure::Retryable(anyhow!(
            "{rack_name}: omdb capability probe could not reach the RSS sled"
        ))
    })?;
    if !has_unique_terminal_sentinel(&capability, ZPOOL_CAPABILITY_SENTINEL) {
        return Err(ClassifiedFailure::Permanent(anyhow!(
            "{rack_name}: API disk lifecycle perftest requires {OMDB} with `db zpool set-storage-buffer` and `db region dry-run-region-allocation` support"
        )));
    }

    let mut last_observed = None;
    let ids = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClassifiedFailure::Retryable(anyhow!(
                "{rack_name}: expected exactly {expected} simulated U.2 zpools, last observed count {last_observed:?}, after bounded inventory polling"
            )));
        }
        let output = ssh_output_timeout(
            ip,
            &omdb_switch_zone_command(&omdb_zpool_list_command()),
            remaining.min(ZPOOL_PREPARATION_COMMAND_TIMEOUT),
        );
        if let Some(output) = output
            && has_unique_terminal_sentinel(&output, ZPOOL_LIST_SENTINEL)
        {
            let ids = parse_omdb_zpool_ids(&output).map_err(|error| {
                ClassifiedFailure::Permanent(error.context(format!(
                    "{rack_name}: malformed `omdb db zpool list -i` output"
                )))
            })?;
            let observed = ids.len();
            if last_observed != Some(observed) {
                eprintln!(
                    "[perftest] {rack_name}: observed {observed}/{expected} simulated U.2 zpools"
                );
                last_observed = Some(observed);
            }
            if observed == expected {
                break ids;
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        std::thread::sleep(Duration::from_secs(1).min(remaining));
    };

    for id in &ids {
        let command =
            omdb_switch_zone_command(&omdb_zpool_set_buffer_command(*id));
        set_zpool_storage_buffer_with(
            rack_name,
            *id,
            deadline,
            ZPOOL_UPDATE_RETRY_DELAY,
            Instant::now,
            std::thread::sleep,
            |timeout| ssh_output_timeout(ip, &command, timeout),
        )?;
    }
    eprintln!(
        "[perftest] {rack_name}: set the control-plane storage buffer to zero on all {} simulated U.2 zpools",
        ids.len()
    );
    let command =
        omdb_switch_zone_command(&omdb_region_allocation_dry_run_command());
    wait_for_region_allocation_with(
        rack_name,
        deadline,
        ZPOOL_UPDATE_RETRY_DELAY,
        Instant::now,
        std::thread::sleep,
        |timeout| ssh_output_timeout(ip, &command, timeout),
    )?;
    eprintln!(
        "[perftest] {rack_name}: 1-GiB three-region allocation dry run succeeded"
    );
    Ok(())
}

async fn prepare_simulated_zpool_capacity(
    cfg: &VoxelConfig,
    name: &str,
) -> ClassifiedResult<()> {
    let deadline = Instant::now() + ZPOOL_PREPARATION_DEADLINE;
    let topo = build_topo(cfg, name).map_err(|error| {
        ClassifiedFailure::Permanent(
            error.context("build topology for zpool preparation"),
        )
    })?;
    let rss_nodes: Vec<_> = topo
        .rss_sleds()
        .into_iter()
        .map(|(sled, node)| (sled.rack, sled.name.clone(), *node))
        .collect();
    if rss_nodes.len() != cfg.topology.racks() {
        return Err(ClassifiedFailure::Permanent(anyhow!(
            "expected one RSS node per rack for zpool preparation, found {} for {} racks",
            rss_nodes.len(),
            cfg.topology.racks()
        )));
    }
    let expected = expected_simulated_zpool_count_per_rack(cfg);
    let runner = &topo.runner;
    let ip_lookups = rss_nodes.into_iter().map(|(rack, sled_name, node)| async move {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClassifiedFailure::Retryable(anyhow!(
                "rack {} ({sled_name}): shared zpool preparation deadline elapsed before external-IP lookup",
                rack + 1
            )));
        }
        let ip = tokio::time::timeout(
            remaining.min(ZPOOL_PREPARATION_COMMAND_TIMEOUT),
            node_external_ip(runner, node, false),
        )
        .await
        .map_err(|_| {
            ClassifiedFailure::Retryable(anyhow!(
                "rack {} ({sled_name}): external-IP lookup timed out during zpool preparation",
                rack + 1
            ))
        })?
        .map_err(|error| {
            ClassifiedFailure::Retryable(error.context(format!(
                "rack {} ({sled_name}): external-IP lookup for zpool preparation",
                rack + 1
            )))
        })?;
        Ok((rack, sled_name, ip))
    });
    let racks = futures::future::join_all(ip_lookups)
        .await
        .into_iter()
        .collect::<ClassifiedResult<Vec<_>>>()?;

    let preparations = racks.into_iter().map(|(rack, sled_name, ip)| {
        let rack_name = format!("rack {} ({sled_name})", rack + 1);
        let task_name = rack_name.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                prepare_simulated_zpools_on_rack(
                    &rack_name, &ip, expected, deadline,
                )
            })
            .await
            .map_err(|error| {
                ClassifiedFailure::Retryable(anyhow!(
                    "{task_name}: zpool preparation task failed: {error}"
                ))
            })?
        }
    });
    futures::future::join_all(preparations)
        .await
        .into_iter()
        .collect::<ClassifiedResult<Vec<_>>>()?;
    Ok(())
}

async fn cmd_preflight(
    cfg: Option<&VoxelConfig>,
    name: &str,
    _workload: WorkloadKind,
    oxide_auth_helper: Option<&Path>,
) -> Result<()> {
    let cfg = cfg.ok_or_else(|| {
        anyhow!("no voxel.toml found - run from a project dir or pass --config")
    })?;
    oxide_session::static_preflight(
        cfg,
        oxide_auth_helper,
        &std::env::temp_dir(),
    )
    .context("Oxide static preflight before rack mutation")?;
    clean_repeat_boundary(cfg, name)
        .context("preflight initial clean boundary")?;
    let operation = async {
        crate::rack::cmd_launch(
            cfg,
            name,
            false,
            crate::rack::ExternalRoutePolicy::Required,
            false,
            false,
            false,
        )
        .await
        .map(|_| ())
        .context("preflight launch")?;
        prepare_simulated_zpool_capacity(cfg, name)
            .await
            .map_err(classified_anyhow)
            .context("prepare simulated U.2 capacity")?;
        let session =
            oxide_session::provision(cfg, oxide_auth_helper).await.map_err(
                |error| anyhow!(error).context("provision Oxide session"),
            )?;
        let workload = run_disk_lifecycle_preflight(
            &session,
            &cfg.recovery_silo.silo_name,
            Duration::from_secs(2),
        )
        .map_err(classified_anyhow);
        combine_operation_and_cleanup(
            workload,
            session.close().context("close temporary Oxide profile"),
            "preflight profile",
        )
    }
    .await;
    if operation.is_err() {
        eprintln!(
            "[perftest] preflight operation failed; cleaning the rack and restoring the ZFS boundary before reporting the error"
        );
    }
    let cleanup = clean_repeat_boundary(cfg, name)
        .context("preflight final rack/ZFS boundary");
    combine_operation_and_cleanup(operation, cleanup, "destructive preflight")?;
    println!(
        "[perftest] complete 20-disk API lifecycle preflight succeeded for the current voxel.toml"
    );
    Ok(())
}

fn classify_provision(error: ProvisionError) -> RepeatRunError {
    match error {
        ProvisionError::Permanent(error) => RepeatRunError::Permanent(error),
        ProvisionError::Transient(error) => RepeatRunError::Execution(error),
        ProvisionError::Boundary(error) => RepeatRunError::Boundary(error),
    }
}

fn classified_anyhow(error: ClassifiedFailure) -> anyhow::Error {
    match error {
        ClassifiedFailure::Permanent(error)
        | ClassifiedFailure::Retryable(error) => error,
    }
}

fn classify_lifecycle(error: ClassifiedFailure) -> RepeatRunError {
    match error {
        ClassifiedFailure::Permanent(error) => RepeatRunError::Permanent(error),
        ClassifiedFailure::Retryable(error) => RepeatRunError::Execution(error),
    }
}

#[derive(Debug)]
enum OxideSessionAggregation {
    Unobserved,
    Unavailable,
    Available(OxideSessionMetadata),
}

impl OxideSessionAggregation {
    fn merge(&mut self, observed: Option<OxideSessionMetadata>) -> Result<()> {
        match (&*self, observed) {
            (Self::Unobserved, Some(metadata)) => {
                *self = Self::Available(metadata)
            }
            (Self::Unobserved, None) => *self = Self::Unavailable,
            (Self::Available(expected), Some(observed))
                if expected != &observed =>
            {
                return Err(anyhow!(
                    "Oxide session metadata changed across successful repeats"
                ));
            }
            (Self::Available(_), None) | (Self::Unavailable, Some(_)) => {
                return Err(anyhow!(
                    "Oxide session metadata availability changed across successful repeats"
                ));
            }
            (Self::Unavailable, None) | (Self::Available(_), Some(_)) => {}
        }
        Ok(())
    }

    fn finish(self) -> Option<OxideSessionMetadata> {
        match self {
            Self::Available(metadata) => Some(metadata),
            Self::Unobserved | Self::Unavailable => None,
        }
    }
}

async fn run_combo(
    cfg: &VoxelConfig,
    name: &str,
    workload: Option<WorkloadKind>,
    oxide_auth_helper: Option<&Path>,
    scope: &Value,
) -> std::result::Result<
    (RepeatSample, Option<OxideSessionMetadata>),
    RepeatRunError,
> {
    clean_repeat_boundary(cfg, name)
        .context("required pre-repeat clean boundary")
        .map_err(RepeatRunError::Boundary)?;
    let body =
        run_combo_body(cfg, name, workload, oxide_auth_helper, scope).await;
    let cleanup = clean_repeat_boundary(cfg, name)
        .context("required post-repeat clean boundary");
    finish_repeat_execution(body, cleanup)
}

fn measure_workload(
    baseline: &Value,
    run_workload: impl FnOnce() -> ClassifiedResult<()>,
    collect_after: impl FnOnce() -> Result<Value>,
) -> std::result::Result<(u64, u64, u64), RepeatRunError> {
    let ram = PeakRamSampler::start();
    let workload = run_workload().map_err(classify_lifecycle);
    let peak_delta = ram.finish();
    workload?;
    let after_workload = collect_after()
        .context("collect after-workload matrix sample")
        .map_err(RepeatRunError::Execution)?;
    let (bytes, secs) = workload_measurement(baseline, &after_workload)
        .map_err(RepeatRunError::Execution)?;
    let peak_delta = require_ram_delta(peak_delta, "workload")?;
    Ok((bytes, secs, peak_delta))
}

fn workload_measurement(
    baseline: &Value,
    after_workload: &Value,
) -> Result<(u64, u64)> {
    Ok((
        matrix_total_bytes_written(baseline, after_workload)?,
        after_time(after_workload).saturating_sub(after_time(baseline)).max(1),
    ))
}

async fn run_launch_phase(
    cfg: &VoxelConfig,
    name: &str,
    scope: &Value,
) -> Result<(RepeatSample, Value, bool)> {
    let before = collect_matrix_sample("before", Some(scope))?;
    let ram = PeakRamSampler::start();
    let launched = crate::rack::cmd_launch(
        cfg,
        name,
        false,
        crate::rack::ExternalRoutePolicy::BestEffort,
        false,
        false,
        false,
    )
    .await;
    let peak_ram = ram.finish();
    let external_reachable =
        launched.map_err(|error| anyhow!("launch: {error:#}"))?;
    let peak_ram =
        require_ram_delta(peak_ram, "launch").map_err(|error| match error {
            RepeatRunError::Execution(error)
            | RepeatRunError::Permanent(error)
            | RepeatRunError::Boundary(error) => error,
        })?;
    verify_guest_levers(cfg, name).await?;
    let after = collect_matrix_sample("after-launch", Some(scope))?;
    let sample = RepeatSample {
        bringup_bytes: matrix_total_bytes_written(&before, &after)?,
        launch_secs: after_time(&after)
            .saturating_sub(after_time(&before))
            .max(1),
        peak_ram_bytes: Some(peak_ram),
        workload_bytes: None,
        workload_secs: None,
        workload_peak_delta_bytes: None,
    };
    Ok((sample, after, external_reachable))
}

fn require_workload_external_dns(external_reachable: bool) -> Result<()> {
    if external_reachable {
        Ok(())
    } else {
        bail!(
            "workload blocked because no configured external DNS server answered; retaining launch-only evidence"
        )
    }
}

async fn run_workload_phase(
    cfg: &VoxelConfig,
    _name: &str,
    oxide_auth_helper: Option<&Path>,
    scope: &Value,
    _after_launch: &Value,
) -> Result<(WorkloadMetrics, Option<OxideSessionMetadata>)> {
    let session = oxide_session::provision(cfg, oxide_auth_helper)
        .await
        .map_err(|error| anyhow!(error))?;
    let metadata = session.metadata().clone();
    let measured = (|| {
        let prepared = PreparedDiskLifecycle::prepare(
            &session,
            &cfg.recovery_silo.silo_name,
        )
        .map_err(classified_anyhow)?;
        let baseline = collect_matrix_sample("before-workload", Some(scope))?;
        measure_workload(
            &baseline,
            || prepared.run(&WorkloadSpec::api_disk_lifecycle()),
            || collect_matrix_sample("after-workload", Some(scope)),
        )
        .map_err(|error| match error {
            RepeatRunError::Execution(error)
            | RepeatRunError::Permanent(error)
            | RepeatRunError::Boundary(error) => error,
        })
    })();
    let measured = combine_operation_and_cleanup(
        measured,
        session.close().context("close temporary Oxide profile"),
        "workload profile",
    )?;
    Ok((
        WorkloadMetrics {
            workload_bytes: measured.0,
            workload_secs: measured.1,
            workload_peak_delta_bytes: measured.2,
        },
        Some(metadata),
    ))
}

async fn run_combo_body(
    cfg: &VoxelConfig,
    name: &str,
    workload: Option<WorkloadKind>,
    oxide_auth_helper: Option<&Path>,
    scope: &Value,
) -> std::result::Result<
    (RepeatSample, Option<OxideSessionMetadata>),
    RepeatRunError,
> {
    let before = collect_matrix_sample("before", Some(scope))
        .map_err(RepeatRunError::Execution)?;
    // Sample the host RAM increase above the pre-launch baseline. A missing
    // in-window Helios sample invalidates this repeat.
    let ram = PeakRamSampler::start();
    let launched = crate::rack::cmd_launch(
        cfg,
        name,
        false,
        crate::rack::ExternalRoutePolicy::BestEffort,
        false,
        false,
        false,
    )
    .await;
    let peak_ram_delta = ram.finish();
    let external_reachable = launched
        .map_err(|e| RepeatRunError::Execution(anyhow!("launch: {e:#}")))?;
    let peak_ram_bytes = Some(require_ram_delta(peak_ram_delta, "launch")?);
    verify_guest_levers(cfg, name).await.map_err(RepeatRunError::Execution)?;
    let after_launch = collect_matrix_sample("after-launch", Some(scope))
        .map_err(RepeatRunError::Execution)?;
    let bringup_bytes = matrix_total_bytes_written(&before, &after_launch)
        .map_err(RepeatRunError::Execution)?;
    let bringup_secs =
        after_time(&after_launch).saturating_sub(after_time(&before)).max(1);

    let (workload_bytes, workload_secs, workload_peak_delta_bytes, metadata) =
        if workload.is_some() {
            require_workload_external_dns(external_reachable)
                .map_err(RepeatRunError::Execution)?;
            prepare_simulated_zpool_capacity(cfg, name)
                .await
                .map_err(classify_lifecycle)?;
            let session = oxide_session::provision(cfg, oxide_auth_helper)
                .await
                .map_err(classify_provision)?;
            let metadata = session.metadata().clone();
            let measured =
                match PreparedDiskLifecycle::prepare(
                    &session,
                    &cfg.recovery_silo.silo_name,
                ) {
                    Ok(prepared) => {
                        let baseline = collect_matrix_sample(
                            "before-workload",
                            Some(scope),
                        )
                        .map_err(RepeatRunError::Execution);
                        baseline.and_then(|baseline| {
                    measure_workload(
                        &baseline,
                        || prepared.run(&WorkloadSpec::api_disk_lifecycle()),
                        || collect_matrix_sample("after-workload", Some(scope)),
                    )
                })
                    }
                    Err(error) => Err(classify_lifecycle(error)),
                };
            let close =
                session.close().context("close temporary Oxide profile");
            let (bytes, secs, peak_delta) =
                finish_repeat_execution(measured, close)?;
            (Some(bytes), Some(secs), Some(peak_delta), Some(metadata))
        } else {
            (None, None, None, None)
        };

    Ok((
        RepeatSample {
            bringup_bytes,
            launch_secs: bringup_secs,
            peak_ram_bytes,
            workload_bytes,
            workload_secs,
            workload_peak_delta_bytes,
        },
        metadata,
    ))
}

const POOLS_SENTINEL: &str = "__VOXEL_POOLS_DONE__";
const ZFS_SENTINEL: &str = "__VOXEL_ZFS_DONE__";
const ZFS_EVIDENCE_COMMAND: &str = r#"pools=$(LC_ALL=C zpool list -H -o name) || exit; printf '%s\n' "$pools"; echo __VOXEL_POOLS_DONE__; for p in $pools; do case "$p" in rpool|oxi_*|oxp_*) LC_ALL=C zfs get -H -o name,property,value,source sync,compression "$p" || exit;; esac; done; echo __VOXEL_ZFS_DONE__"#;
const GUEST_EVIDENCE_DEADLINE: Duration = Duration::from_secs(60);
const GUEST_EVIDENCE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

fn guest_evidence_attempt_timeout(
    deadline: Instant,
    now: Instant,
) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    (!remaining.is_zero())
        .then_some(remaining.min(GUEST_EVIDENCE_ATTEMPT_TIMEOUT))
}

fn final_guest_evidence_failure(last: Option<anyhow::Error>) -> anyhow::Error {
    last.unwrap_or_else(|| {
        anyhow!(
            "observed evidence deadline with no successful ZFS verification"
        )
    })
}

async fn verify_guest_levers(cfg: &VoxelConfig, name: &str) -> Result<()> {
    let topo = build_topo(cfg, name)
        .context("rebuild topology references for guest evidence")?;
    for (sled, node) in &topo.sleds {
        let started = Instant::now();
        let deadline = started + GUEST_EVIDENCE_DEADLINE;
        let ip_timeout = Duration::from_secs(15)
            .min(deadline.saturating_duration_since(Instant::now()));
        let ip = tokio::time::timeout(
            ip_timeout,
            node_external_ip(&topo.runner, *node, false),
        )
        .await
        .with_context(|| {
            format!(
                "lever 3 node {} expected external IP, observed lookup timeout",
                sled.name
            )
        })?
        .with_context(|| {
            format!(
                "lever 3 node {} expected external IP, observed lookup failure",
                sled.name
            )
        })?;
        let mut last = None;
        let mut verified = false;
        while Instant::now() < deadline {
            let now = Instant::now();
            let Some(attempt_timeout) =
                guest_evidence_attempt_timeout(deadline, now)
            else {
                break;
            };
            let attempt_deadline = now + attempt_timeout;
            let ip = ip.clone();
            let cfg = cfg.clone();
            let handle = tokio::task::spawn_blocking(move || {
                verify_sled_evidence(&ip, &cfg, attempt_deadline)
            });
            match tokio::time::timeout_at(deadline.into(), handle).await {
                Ok(Ok(Ok(()))) => {
                    verified = true;
                    break;
                }
                Ok(Ok(Err(e))) => last = Some(e),
                Ok(Err(e)) => {
                    return Err(e).with_context(|| format!(
                        "lever 3 node {} expected ZFS evidence, observed blocking evidence task JoinError",
                        sled.name
                    ));
                }
                Err(_) => break,
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5).min(remaining)).await;
        }
        if !verified {
            let last = final_guest_evidence_failure(last);
            let elapsed = started.elapsed();
            return Err(last).with_context(|| format!(
                "lever 3 node {} expected guest-zfs-tuning={}, observed final failed evidence after {elapsed:.3?} (deadline {GUEST_EVIDENCE_DEADLINE:.3?})",
                sled.name,
                cfg.disk_wear.guest_zfs_tuning
            ));
        }
    }
    Ok(())
}

fn verify_sled_evidence(
    ip: &str,
    cfg: &VoxelConfig,
    deadline: Instant,
) -> Result<()> {
    let zfs_budget = deadline.saturating_duration_since(Instant::now());
    if zfs_budget.is_zero() {
        return Err(anyhow!(
            "guest evidence deadline elapsed before ZFS query"
        ));
    }
    let zfs = ssh_output_timeout(ip, ZFS_EVIDENCE_COMMAND, zfs_budget).ok_or_else(|| {
        anyhow!("ZFS evidence SSH query returned no usable output within {zfs_budget:.3?}")
    })?;
    validate_zfs_evidence(&zfs, cfg.disk_wear.guest_zfs_tuning)
        .context("lever 3 ZFS evidence")
}

fn is_omicron_pool(pool: &str) -> bool {
    pool.starts_with("oxi_") || pool.starts_with("oxp_")
}

fn validate_zfs_evidence(output: &str, enabled: bool) -> Result<()> {
    let (pool_text, rest) = output
        .split_once(POOLS_SENTINEL)
        .ok_or_else(|| anyhow!("pool query completion sentinel missing"))?;
    let (rows_text, tail) = rest
        .split_once(ZFS_SENTINEL)
        .ok_or_else(|| anyhow!("ZFS query completion sentinel missing"))?;
    if tail.lines().any(|l| !l.trim().is_empty())
        || rest.matches(ZFS_SENTINEL).count() != 1
    {
        return Err(anyhow!("malformed/duplicate ZFS completion evidence"));
    }
    let pool_rows: Vec<&str> =
        pool_text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let pools: BTreeSet<&str> = pool_rows.iter().copied().collect();
    if pools.len() != pool_rows.len() {
        return Err(anyhow!("duplicate pool-list row; observed {pool_rows:?}"));
    }
    if !pools.contains("rpool") || !pools.iter().any(|p| is_omicron_pool(p)) {
        return Err(anyhow!(
            "expected rpool and at least one oxi_*/oxp_* pool, observed {pools:?}"
        ));
    }
    let datasets: BTreeSet<&str> = pools
        .iter()
        .copied()
        .filter(|p| *p == "rpool" || is_omicron_pool(p))
        .collect();
    let mut seen = BTreeMap::new();
    for line in rows_text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 4
            || !datasets.contains(f[0])
            || !matches!(f[1], "sync" | "compression")
        {
            return Err(anyhow!("malformed ZFS evidence row {line:?}"));
        }
        if seen.insert((f[0], f[1]), (f[2], f[3])).is_some() {
            return Err(anyhow!("duplicate ZFS property row {line:?}"));
        }
    }
    for dataset in datasets {
        for prop in ["sync", "compression"] {
            let &(value, source) = seen
                .get(&(dataset, prop))
                .ok_or_else(|| anyhow!("missing {dataset} {prop} evidence"))?;
            let tuned = match prop {
                "sync" => value == "disabled",
                "compression" => matches!(value, "lz4" | "on"),
                _ => unreachable!(),
            };
            if enabled && (!tuned || source != "local") {
                let expected =
                    if prop == "sync" { "disabled" } else { "lz4|on" };
                return Err(anyhow!(
                    "expected {dataset} {prop}={expected} source=local, observed value={value} source={source}"
                ));
            }
            // Older CP images already have compression=on source=local, so
            // compression alone cannot prove that this lever leaked from a
            // preceding run. sync=disabled is definitive because lever 3
            // always sets it, including when compression falls back to `on`.
            if !enabled && prop == "sync" && tuned {
                return Err(anyhow!(
                    "expected {dataset} {prop} not to retain a tuned value, observed {value} source={source}"
                ));
            }
        }
    }
    Ok(())
}

/// A combination's config: clone the base, then set each lever from the set.
/// Levers absent from the set are forced OFF (so a combo is exactly its set,
/// regardless of what the base `voxel.toml` had) - this is what makes the A/B
/// clean. Lever 4 maps to `topology.rss_sleds` (`0` = all sleds = no reduction).
fn apply_combo(
    base: &VoxelConfig,
    set: &BTreeSet<u8>,
    rss_sleds: usize,
) -> VoxelConfig {
    let mut c = base.clone();
    c.disk_wear.host_sync_disabled = set.contains(&1);
    c.disk_wear.host_compression = set.contains(&2);
    c.disk_wear.guest_zfs_tuning = set.contains(&3);
    c.topology.rss_sleds = if set.contains(&4) { rss_sleds } else { 0 };
    c
}

/// Reset the host-dataset props levers 1/2 set, back to inherited defaults, and
/// prove that none remains locally set before the next repeat.
fn reset_host_zfs_props() -> Result<()> {
    let dataset = crate::image::falcon_dataset();
    for prop in
        ["sync", "compression", "atime", "logbias", "redundant_metadata"]
    {
        let output = Command::new("zfs")
            .args(["inherit", prop, &dataset])
            .output()
            .with_context(|| format!("run `zfs inherit {prop} {dataset}`"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "`zfs inherit {prop} {dataset}` failed with {}: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let readback = crate::rack::read_zfs_property(&dataset, prop)?;
        crate::rack::verify_zfs_inherit_readback(&readback, prop)
            .with_context(|| format!("verify reset of {prop} on {dataset}"))?;
    }
    Ok(())
}

/// Gross NVMe bytes written between two samples, summed across the drives that
/// back the falcon pool (per `after`'s `falcon_controllers`). Falls back to
/// summing every drive when the scope is unresolved, so an old sample or a
/// non-NVMe pool degrades to the prior whole-host behavior. Matches `report`'s
/// per-device diff (missing "before" device -> no delta, not a spike).
fn total_bytes_written(before: &Value, after: &Value) -> u64 {
    let scope = falcon_scope(after);
    let no = Vec::new();
    let bdevs = before["devices"].as_array().unwrap_or(&no);
    after["devices"]
        .as_array()
        .unwrap_or(&no)
        .iter()
        .filter(|adev| device_in_scope(&scope, adev))
        .map(|adev| {
            let name = adev["name"].as_str().unwrap_or("?");
            let a = adev["data_units_written"].as_u64().unwrap_or(0);
            let b = bdevs
                .iter()
                .find(|d| d["name"].as_str() == Some(name))
                .and_then(|d| d["data_units_written"].as_u64())
                .unwrap_or(a);
            a.saturating_sub(b).saturating_mul(DATA_UNIT_BYTES)
        })
        .sum()
}

/// Parse the `--combos` spec into `(label, lever-set)` pairs, or the default
/// cumulative ladder when unset.
fn parse_combos(spec: Option<&str>) -> Result<Vec<(String, BTreeSet<u8>)>> {
    match spec {
        None => Ok(default_ladder()),
        Some(s) => s
            .split(';')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(parse_one_combo)
            .collect(),
    }
}

fn parse_one_combo(s: &str) -> Result<(String, BTreeSet<u8>)> {
    let set: BTreeSet<u8> = match s.to_ascii_lowercase().as_str() {
        "none" | "baseline" => BTreeSet::new(),
        "all" => (1u8..=4).collect(),
        _ => {
            let mut set = BTreeSet::new();
            for tok in s.split('+') {
                let n: u8 = tok.trim().parse().map_err(|_| {
                    anyhow!(
                        "bad lever '{}' in combo '{s}' (want 1-4, none, or all)",
                        tok.trim()
                    )
                })?;
                if !(1..=4).contains(&n) {
                    return Err(anyhow!(
                        "lever {n} out of range (1-4) in combo '{s}'"
                    ));
                }
                set.insert(n);
            }
            set
        }
    };
    Ok((combo_label(&set), set))
}

/// The cumulative ladder: none, +1, +1+2, +1+2+3, +1+2+3+4. Each row
/// adds one lever, so the table reads as each lever's marginal effect.
fn default_ladder() -> Vec<(String, BTreeSet<u8>)> {
    let mut out = Vec::new();
    let mut cur = BTreeSet::new();
    out.push((combo_label(&cur), cur.clone()));
    for n in 1u8..=4 {
        cur.insert(n);
        out.push((combo_label(&cur), cur.clone()));
    }
    out
}

/// A combo's canonical label: `none`, or `+`-joined lever numbers (`1+2+3`).
fn combo_label(set: &BTreeSet<u8>) -> String {
    if set.is_empty() {
        "none".to_string()
    } else {
        set.iter().map(u8::to_string).collect::<Vec<_>>().join("+")
    }
}

/// Short human names for a lever set, for the table's LEVERS column.
fn describe_levers(set: &BTreeSet<u8>) -> String {
    if set.is_empty() {
        return "none".to_string();
    }
    set.iter()
        .map(|n| match n {
            1 => "sync",
            2 => "comp",
            3 => "guest",
            4 => "repl",
            _ => "?",
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Render the results as an aligned comparison table. The customer-facing axes
/// lead: BRING-UP (drive wear), LAUNCH (how fast the rack comes up), and the
/// baseline-adjusted launch/workload RAM peaks. Columns adapt to what was
/// measured; values are the per-combo mean over repeats (identical to the single
/// value at `--repeat 1`).
fn render_table(
    results: &[ComboAggregate],
    rated_tbw: Option<f64>,
) -> Result<String> {
    let policy = projection_policy(rated_tbw)?;
    let any_workload = results.iter().any(|r| r.has_workload());
    let any_workload_ram =
        results.iter().any(|r| r.workload_peak_delta_bytes().n >= 1);
    // Only show the variance column once some combo was repeated (n >= 2).
    let any_variance = results.iter().any(|r| r.bringup_bytes().n >= 2);
    // Launch delta RAM only when the (Helios-only) sampler measured it somewhere.
    let any_ram = results.iter().any(|r| r.peak_ram_bytes().n >= 1);
    let mut header = format!(
        "{:<12}  {:<20}  {:>13}  {:>12}  {:>8}",
        "COMBO", "LEVERS", "BRING-UP", "RATE/s", "LAUNCH"
    );
    if any_ram {
        header.push_str(&format!("  {:>11}", "LAUNCH ΔRAM"));
    }
    if any_variance {
        header.push_str(&format!("  {:>8}", "CV%"));
    }
    if any_workload {
        header.push_str(&format!("  {:>13}", "WORKLOAD"));
    }
    if any_workload_ram {
        header.push_str(&format!("  {:>14}", "WORKLOAD ΔRAM"));
    }
    if rated_tbw.is_some() {
        header.push_str(&format!("  {:>10}", "~YEARS"));
    }

    let mut s = String::from(
        "disk-wear matrix results (gross NVMe Data Units Written on the falcon pool's drives, decimal units):\n",
    );
    s.push_str(&header);
    s.push('\n');
    s.push_str(&"-".repeat(header.chars().count()));
    s.push('\n');
    for r in results {
        if let Some(e) = &r.error {
            s.push_str(&format!(
                "{:<12}  {:<20}  FAILED: {e}\n",
                r.label,
                describe_levers(&r.levers)
            ));
            continue;
        }
        let bringup = r.bringup_bytes();
        let bytes = bringup.mean as u64;
        let secs = (r.launch_secs().mean as u64).max(1);
        let rate = bytes as f64 / secs as f64;
        let mut row = format!(
            "{:<12}  {:<20}  {:>13}  {:>12}  {:>8}",
            r.label,
            describe_levers(&r.levers),
            human_bytes(bytes),
            format!("{}/s", human_bytes(rate as u64)),
            human_secs(secs)
        );
        if any_ram {
            let ram = r.peak_ram_bytes();
            let cell = if ram.n >= 1 {
                human_bytes(ram.mean as u64)
            } else {
                "-".to_string()
            };
            row.push_str(&format!("  {cell:>11}"));
        }
        if any_variance {
            let cv = bringup
                .cv
                .map(|c| format!("{:.1}%", c * 100.0))
                .unwrap_or_else(|| "-".to_string());
            row.push_str(&format!("  {cv:>8}"));
        }
        if any_workload {
            let w = if r.has_workload() {
                human_bytes(r.workload_bytes().mean as u64)
            } else {
                "-".to_string()
            };
            row.push_str(&format!("  {w:>13}"));
        }
        if any_workload_ram {
            let ram = r.workload_peak_delta_bytes();
            let cell = if ram.n >= 1 {
                human_bytes(ram.mean as u64)
            } else {
                "-".to_string()
            };
            row.push_str(&format!("  {cell:>14}"));
        }
        if rated_tbw.is_some() {
            let years = project_samples(
                &CounterSample { timestamp_seconds: 0, value: 0 },
                &CounterSample { timestamp_seconds: secs, value: bytes },
                &policy,
            )
            .context("project matrix drive lifetime")?
            .rated_years
            .map(|y| format!("{y:.2}"))
            .unwrap_or_else(|| "-".to_string());
            row.push_str(&format!("  {years:>10}"));
        }
        s.push_str(&row);
        s.push('\n');
    }
    Ok(s)
}

/// Render the results as CSV (one row per combo; lever columns are 0/1) for
/// import into a spreadsheet. Numeric columns carry the per-combo mean over
/// repeats (identical to the single value at `--repeat 1`).
fn render_csv(results: &[ComboAggregate]) -> String {
    let mut s = String::from(
        "combo,sync,compression,guest_zfs,reduce_replication,bringup_bytes,bringup_secs,peak_ram_bytes,workload_bytes,workload_secs,workload_peak_delta_bytes,error\n",
    );
    for r in results {
        let on = |n: u8| u8::from(r.levers.contains(&n));
        let (workload_bytes, workload_secs, workload_peak_delta_bytes) =
            if r.has_workload() {
                (
                    (r.workload_bytes().mean as u64).to_string(),
                    (r.workload_secs().mean as u64).to_string(),
                    (r.workload_peak_delta_bytes().mean as u64).to_string(),
                )
            } else {
                (String::new(), String::new(), String::new())
            };
        let peak_ram = {
            let ram = r.peak_ram_bytes();
            if ram.n >= 1 {
                (ram.mean as u64).to_string()
            } else {
                String::new()
            }
        };
        s.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.label,
            on(1),
            on(2),
            on(3),
            on(4),
            r.bringup_bytes().mean as u64,
            r.launch_secs().mean as u64,
            peak_ram,
            workload_bytes,
            workload_secs,
            workload_peak_delta_bytes,
            r.error.as_deref().unwrap_or("").replace(',', ";"),
        ));
    }
    s
}

pub(super) fn validate_comparison_compatibility(
    base: &MatrixRun,
    candidate: &MatrixRun,
) -> Result<()> {
    if base.workload != candidate.workload {
        return Err(anyhow!(
            "workload mismatch: baseline and candidate matrix runs are not comparable"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sample
// ---------------------------------------------------------------------------

fn cmd_sample(label: &str, out: Option<&Path>) -> Result<()> {
    let sample = collect_sample(label)?;
    let text =
        serde_json::to_string_pretty(&sample).context("serialize sample")?;
    match out {
        Some(path) => {
            std::fs::write(path, format!("{text}\n"))
                .with_context(|| format!("write {}", path.display()))?;
            eprintln!(
                "[perftest] wrote sample '{label}' -> {}",
                path.display()
            );
        }
        None => println!("{text}"),
    }
    Ok(())
}

fn collect_sample(label: &str) -> Result<Value> {
    let devices = nvme_devices()?;
    let pool = falcon_pool();
    let scope = falcon_pool_controllers();
    let scope_error = scope.as_ref().err().map(|error| format!("{error:#}"));
    let controllers: Vec<String> =
        scope.unwrap_or_default().into_iter().collect();
    if controllers.is_empty() {
        // `nvme_devices()` already succeeded (so we're on Helios), yet we
        // couldn't map the falcon pool to any drive — the wear metric will fall
        // back to summing ALL drives, which reintroduces unrelated-write noise.
        // Warn once so a run's numbers aren't silently host-wide.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            let detail = scope_error
                .as_deref()
                .unwrap_or("unknown scope-resolution failure");
            eprintln!(
                "[perftest] WARN: could not resolve NVMe controllers for falcon pool '{pool}' \
                 (checked `zpool status {pool}` + `nvmeadm list`); wear will be summed across \
                 ALL drives, including unrelated OS writes. Point FALCON_DATASET at a dedicated \
                 pool and verify both commands work under pfexec. Detail: {detail}"
            );
        });
    }
    Ok(json!({
        "label": label,
        "unix_time": now_secs(),
        "devices": devices,
        "pools": zpool_alloc(),
        "falcon_pool": pool,
        "falcon_controllers": controllers,
        "falcon_scope_error": scope_error,
    }))
}

fn collect_matrix_sample(
    label: &str,
    expected: Option<&Value>,
) -> Result<Value> {
    let sample = collect_sample(label)?;
    validate_matrix_scope(&sample, expected)?;
    Ok(sample)
}

fn sample_device_names(sample: &Value) -> Result<BTreeSet<String>> {
    sample["devices"]
        .as_array()
        .ok_or_else(|| anyhow!("sample devices is missing or not an array"))?
        .iter()
        .map(|device| {
            device["name"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("sample device is missing string name"))
        })
        .collect()
}

fn validate_matrix_scope(
    sample: &Value,
    expected: Option<&Value>,
) -> Result<()> {
    let pool = sample["falcon_pool"].as_str().unwrap_or("<missing>");
    let controllers = falcon_scope(sample).unwrap_or_default();
    let scope_error = sample["falcon_scope_error"].as_str();
    let devices = sample_device_names(sample)?;
    let expected_controllers = expected.and_then(falcon_scope);
    let missing: BTreeSet<_> =
        controllers.difference(&devices).cloned().collect();
    let changed = expected_controllers
        .as_ref()
        .is_some_and(|scope| scope != &controllers);
    if scope_error.is_some()
        || controllers.is_empty()
        || !missing.is_empty()
        || changed
    {
        return Err(anyhow!(
            "strict matrix scope invalid for Falcon pool '{pool}': expected controllers {:?}, observed controllers {:?}, observed devices {:?}, missing scoped devices {:?}, scope resolution error {:?}; run `pfexec zpool status {pool}` and `pfexec nvmeadm list` and verify every pool leaf maps to a sampled NVMe controller",
            expected_controllers,
            controllers,
            devices,
            missing,
            scope_error
        ));
    }
    Ok(())
}

fn matrix_total_bytes_written(before: &Value, after: &Value) -> Result<u64> {
    validate_matrix_scope(before, None)?;
    validate_matrix_scope(after, Some(before))?;
    let expected = falcon_scope(before).unwrap();
    let before_devices = sample_device_names(before)?;
    let after_devices = sample_device_names(after)?;
    let absent: BTreeSet<_> = expected
        .iter()
        .filter(|name| {
            !before_devices.contains(*name) || !after_devices.contains(*name)
        })
        .cloned()
        .collect();
    if !absent.is_empty() {
        return Err(anyhow!(
            "strict matrix delta for Falcon pool '{}' is missing expected controller devices {:?}; expected {:?}, before devices {:?}, after devices {:?}; inspect `zpool status` and `nvmeadm list`",
            after["falcon_pool"].as_str().unwrap_or("<missing>"),
            absent,
            expected,
            before_devices,
            after_devices
        ));
    }
    Ok(total_bytes_written(before, after))
}

/// Per-NVMe-controller wear counters from the drive's own SMART/health log page.
fn nvme_devices() -> Result<Vec<Value>> {
    let list = capture("nvmeadm", &["list"]).ok_or_else(|| {
        anyhow!("`nvmeadm list` failed — run `voxel perftest` on the Helios host with pfexec (it reads the NVMe health log page)")
    })?;
    let mut out = Vec::new();
    for ctl in parse_nvme_controllers(&list) {
        // The text rendering omits the exact lifetime counters unless verbose,
        // and verbose rounds writes to whole GB. Capture the raw log instead.
        let output = Command::new("nvmeadm")
            .args(["get-logpage", "-O", "/dev/stdout", &ctl, "health"])
            .output()
            .with_context(|| {
                format!("run `nvmeadm get-logpage -O /dev/stdout {ctl} health`")
            })?;
        if !output.status.success() {
            return Err(anyhow!(
                "`nvmeadm get-logpage -O /dev/stdout {ctl} health` failed with {}: {}; run `voxel perftest` with pfexec",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let health =
            parse_nvme_health_log(&output.stdout).with_context(|| {
                format!("decode raw SMART/health log for {ctl}")
            })?;
        out.push(json!({
            "name": ctl,
            "data_units_written": health.data_units_written,
            "percentage_used": health.percentage_used,
            "power_on_hours": health.power_on_hours,
        }));
    }
    Ok(out)
}

#[derive(Debug, Eq, PartialEq)]
struct NvmeHealthLog {
    data_units_written: u64,
    percentage_used: u64,
    power_on_hours: u64,
}

/// Decode the standard 512-byte NVMe SMART/health log. The lifetime counters
/// are 128-bit little-endian values; the JSON sample format uses u64 and fails
/// rather than silently truncating a counter too large to represent.
fn parse_nvme_health_log(log: &[u8]) -> Result<NvmeHealthLog> {
    if log.len() != 512 {
        return Err(anyhow!(
            "expected 512-byte NVMe SMART/health log, got {} bytes",
            log.len()
        ));
    }
    let counter = |offset: usize, name: &str| -> Result<u64> {
        let bytes: [u8; 16] = log[offset..offset + 16]
            .try_into()
            .expect("validated SMART/health log length");
        u64::try_from(u128::from_le_bytes(bytes))
            .map_err(|_| anyhow!("NVMe {name} counter exceeds u64"))
    };
    Ok(NvmeHealthLog {
        data_units_written: counter(48, "Data Units Written")?,
        percentage_used: u64::from(log[5]),
        power_on_hours: counter(128, "Power On Hours")?,
    })
}

/// Per-pool allocated bytes (`zpool list -Hp -o name,alloc`). A coarse secondary
/// signal — allocated space is net (compression + frees skew it), not gross
/// writes — but cheap context alongside the ground-truth NVMe counter.
fn zpool_alloc() -> Vec<Value> {
    parse_zpool_alloc(
        &capture("zpool", &["list", "-Hp", "-o", "name,alloc"])
            .unwrap_or_default(),
    )
}

fn parse_nvme_controllers(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in list.lines() {
        let Some((head, _)) = line.split_once(':') else {
            continue;
        };
        let head = head.trim();
        let Some(rest) = head.strip_prefix("nvme") else {
            continue;
        };
        if !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_digit())
            && !out.iter().any(|c| c == head)
        {
            out.push(head.to_string());
        }
    }
    out
}

fn parse_zpool_alloc(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let name = it.next()?;
            let alloc = it.next()?.parse::<u64>().ok()?;
            Some(json!({ "name": name, "alloc_bytes": alloc }))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// falcon-pool scoping — attribute wear to the drives backing FALCON_DATASET
// ---------------------------------------------------------------------------
//
// NVMe Data Units Written is a *whole-drive* counter, so a naive sum across
// every controller folds in unrelated writes (the OS pool's logging/swap, other
// pools) — exactly the noise a dedicated measurement pool is meant to remove.
// These helpers resolve the falcon dataset to the NVMe controllers that
// physically back it (dataset -> pool -> `zpool status` leaf vdevs ->
// `nvmeadm list` disk->controller map) so the wear metric counts only the
// workload's drives. If resolution fails (off-Helios, or a non-NVMe pool) the
// scope is empty and callers fall back to summing every drive (the prior
// whole-host behavior), so we are never worse than before.

/// The ZFS pool backing the falcon dataset: the first path component of
/// `FALCON_DATASET` (e.g. `voxel/falcon` -> `voxel`).
fn falcon_pool() -> String {
    let ds = crate::image::falcon_dataset();
    ds.split('/').next().unwrap_or(&ds).to_string()
}

/// The NVMe controllers (`nvmeN`) that back every leaf of the falcon pool.
fn falcon_pool_controllers() -> Result<BTreeSet<String>> {
    let pool = falcon_pool();
    let status = capture("zpool", &["status", &pool])
        .ok_or_else(|| anyhow!("`zpool status {pool}` failed"))?;
    let disks = pool_leaf_disks(&status, &pool)?;
    if disks.is_empty() {
        return Err(anyhow!(
            "Falcon pool '{pool}' has no recognized leaf disks"
        ));
    }
    let list = capture("nvmeadm", &["list"]).ok_or_else(|| {
        anyhow!("`nvmeadm list` failed while resolving Falcon pool '{pool}'")
    })?;
    resolve_pool_controllers(&disks, &parse_nvme_disk_map(&list))
}

fn resolve_pool_controllers(
    disks: &BTreeSet<String>,
    map: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let missing: BTreeSet<_> =
        disks.iter().filter(|disk| !map.contains_key(*disk)).cloned().collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "Falcon pool leaf disks {:?} have no NVMe controller mapping",
            missing
        ));
    }
    Ok(disks.iter().filter_map(|disk| map.get(disk).cloned()).collect())
}

/// The leaf-vdev disk names (`cXtYdZ`, slice dropped) in a `zpool status <pool>`
/// output. Every state-bearing row must be either the pool root, a structural
/// vdev, or a recognized disk; silently dropping an unfamiliar active leaf
/// would make a matrix undercount the pool's physical writes.
fn pool_leaf_disks(status: &str, pool: &str) -> Result<BTreeSet<String>> {
    let mut disks = BTreeSet::new();
    let mut in_config = false;
    for line in status.lines() {
        let line = line.trim();
        if line == "config:" {
            in_config = true;
            continue;
        }
        if !in_config || line.is_empty() || line.starts_with("errors:") {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 2 || !is_zpool_vdev_state(fields[1]) {
            continue;
        }
        let name = fields[0];
        if name == pool || is_structural_vdev(name) {
            continue;
        }
        let disk = parse_disk_name(name).ok_or_else(|| {
            anyhow!("Falcon pool '{pool}' has unrecognized active leaf {name:?} in `zpool status`")
        })?;
        disks.insert(disk);
    }
    Ok(disks)
}

fn is_zpool_vdev_state(value: &str) -> bool {
    matches!(
        value,
        "ONLINE"
            | "DEGRADED"
            | "FAULTED"
            | "OFFLINE"
            | "UNAVAIL"
            | "REMOVED"
            | "AVAIL"
            | "INUSE"
    )
}

fn is_structural_vdev(name: &str) -> bool {
    ["mirror-", "raidz", "draid", "replacing-", "spare-", "indirect-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Map each blkdev disk name (`cXtYdZ`) to its NVMe controller (`nvmeN`) from
/// `nvmeadm list`. Controller header lines are `nvmeN: ...` (column 0); the
/// indented namespace lines carry the disk name, e.g. `nvme0/1 (c1t..d0): ...`.
fn parse_nvme_disk_map(list: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in list.lines() {
        // A controller header: the token before the first ':' is exactly
        // `nvme<digits>`. Set the current controller and move on (its own line
        // holds model/serial, not a blkdev name).
        if let Some((head, _)) = line.split_once(':') {
            let h = head.trim();
            if let Some(rest) = h.strip_prefix("nvme")
                && !rest.is_empty()
                && rest.bytes().all(|b| b.is_ascii_digit())
            {
                current = Some(h.to_string());
                continue;
            }
        }
        // Otherwise this is a child (namespace) line: map any disk name on it to
        // the controller we're under.
        if let Some(ctl) = &current {
            for tok in line.split_whitespace() {
                if let Some(disk) = parse_disk_name(tok) {
                    map.insert(disk, ctl.clone());
                }
            }
        }
    }
    map
}

/// Parse a token into a canonical illumos disk name `cXtYdZ` (slice suffix and
/// any `(...)`/`/blkdev` decoration stripped), validating the `c<digits>
/// t<hex> d<digits>` shape so non-disk tokens (pool names, keywords, models)
/// return `None`.
fn parse_disk_name(tok: &str) -> Option<String> {
    // Strip surrounding punctuation and anything from a '/' onward — disk names
    // are all-alphanumeric, so trimming any non-alphanumeric ends safely peels
    // `(c1t..d0):` (nvmeadm), `(c1t..d0)`, and `c1t..d0/blkdev` down to the name.
    let t = tok
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .split('/')
        .next()
        .unwrap_or("");
    // illumos disk names use lowercase c/t/d/s delimiters and UPPERCASE hex in
    // the WWN target. The hex field must accept only 0-9A-F — if it also took
    // lowercase, the `d` delimiter (a hex digit) gets swallowed into the target
    // and the `d<lun>` split fails (e.g. `c5t..6D07d0` -> target eats `..d0`).
    let take = |s: &str, hex: bool| -> (String, usize) {
        let n = s
            .bytes()
            .take_while(|&b| {
                b.is_ascii_digit()
                    || (hex && b.is_ascii_uppercase() && b.is_ascii_hexdigit())
            })
            .count();
        (s[..n].to_string(), n)
    };
    let rest = t.strip_prefix('c')?;
    let (cnum, n) = take(rest, false);
    if cnum.is_empty() {
        return None;
    }
    let rest = rest[n..].strip_prefix('t')?;
    let (target, n) = take(rest, true);
    if target.is_empty() {
        return None;
    }
    let rest = rest[n..].strip_prefix('d')?;
    let (lun, n) = take(rest, false);
    if lun.is_empty() {
        return None;
    }
    // Optional trailing slice `sN`, and nothing else.
    let tail = &rest[n..];
    let tail_ok = tail.is_empty()
        || tail.strip_prefix('s').is_some_and(|s| {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
        });
    if !tail_ok {
        return None;
    }
    Some(format!("c{cnum}t{target}d{lun}"))
}

/// The set of controllers a sample says back the falcon pool, or `None` when the
/// sample didn't resolve any (older sample, or resolution failed) — `None` means
/// "count every drive" for the callers below.
fn falcon_scope(sample: &Value) -> Option<BTreeSet<String>> {
    let set: BTreeSet<String> = sample["falcon_controllers"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!set.is_empty()).then_some(set)
}

/// Whether a device belongs to the measured scope. Unscoped (`None`) counts all.
fn device_in_scope(scope: &Option<BTreeSet<String>>, dev: &Value) -> bool {
    match scope {
        None => true,
        Some(s) => dev["name"].as_str().is_some_and(|n| s.contains(n)),
    }
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

fn cmd_report(
    before: &Path,
    after: &Path,
    rated_tbw: Option<f64>,
) -> Result<()> {
    let b = read_sample(before)?;
    let a = read_sample(after)?;
    for line in report(&b, &a, rated_tbw)? {
        println!("{line}");
    }
    Ok(())
}

fn read_sample(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parse sample {}", path.display()))
}

fn projection_policy(rated_tbw: Option<f64>) -> Result<ProjectionPolicy> {
    let rated_total = rated_tbw
        .map(|rating| {
            let total = rating * 1e12;
            if !rating.is_finite() || rating <= 0.0 || !total.is_finite() || total > u64::MAX as f64
            {
                bail!("rated TBW must be finite, positive, and representable in bytes");
            }
            Ok(total)
        })
        .transpose()?;
    Ok(ProjectionPolicy { rated_total })
}

fn report(
    before: &Value,
    after: &Value,
    rated_tbw: Option<f64>,
) -> Result<Vec<String>> {
    let policy = projection_policy(rated_tbw).context("validate rated TBW")?;
    let window = after_time(after).saturating_sub(after_time(before)).max(1);
    let mut lines = vec![
        format!(
            "window: {window}s   [{}] -> [{}]",
            label_of(before),
            label_of(after)
        ),
        "-".repeat(62),
    ];

    let scope = falcon_scope(after);
    let no_devs = Vec::new();
    let bdevs = before["devices"].as_array().unwrap_or(&no_devs);
    let mut scoped_bytes = 0u64;
    for adev in after["devices"].as_array().unwrap_or(&no_devs) {
        let name = adev["name"].as_str().unwrap_or("?");
        let a_duw = adev["data_units_written"].as_u64().unwrap_or(0);
        // Missing "before" device -> assume no delta rather than a false spike.
        let b_duw = bdevs
            .iter()
            .find(|d| d["name"].as_str() == Some(name))
            .and_then(|d| d["data_units_written"].as_u64())
            .unwrap_or(a_duw);
        let bytes = a_duw.saturating_sub(b_duw).saturating_mul(DATA_UNIT_BYTES);
        let in_pool = device_in_scope(&scope, adev) && scope.is_some();
        if in_pool {
            scoped_bytes += bytes;
        }
        let p = project_samples(
            &CounterSample { timestamp_seconds: 0, value: 0 },
            &CounterSample { timestamp_seconds: window, value: bytes },
            &policy,
        )
        .with_context(|| format!("project write rate for device {name}"))?;
        let tag = if in_pool { "  <- falcon pool" } else { "" };
        lines.push(format!(
            "{name}: wrote {} over window{tag}",
            human_bytes(bytes)
        ));
        lines.push(format!(
            "      rate {}/s   ({:.2} GB/day, {:.2} TB/year)",
            human_bytes(p.units_per_second as u64),
            p.giga_units_per_day,
            p.tera_units_per_year
        ));
        if let (Some(bu), Some(au)) =
            (percentage(before, name), percentage(after, name))
        {
            lines.push(format!("      Used%: {bu} -> {au}"));
        }
        if let (Some(years), Some(rated)) = (p.rated_years, rated_tbw) {
            lines.push(format!(
                "      endurance: {rated:.0} TBW rated -> ~{years:.2} years (~{:.0} days) at this rate",
                years * 365.0
            ));
        }
    }

    // The headline number: wear on the falcon pool's drives only (the workload's
    // storage), so unrelated OS/other-pool writes on shared drives don't count.
    if let Some(s) = &scope {
        let pool = after["falcon_pool"].as_str().unwrap_or("?");
        let drives: Vec<&str> = s.iter().map(String::as_str).collect();
        let p = project_samples(
            &CounterSample { timestamp_seconds: 0, value: 0 },
            &CounterSample { timestamp_seconds: window, value: scoped_bytes },
            &policy,
        )
        .context("project falcon pool write rate")?;
        lines.push("-".repeat(62));
        lines.push(format!(
            "falcon pool '{pool}' total: {} over window   ({} drive(s): {})",
            human_bytes(scoped_bytes),
            drives.len(),
            drives.join(", ")
        ));
        lines.push(format!(
            "      rate {}/s   ({:.2} GB/day, {:.2} TB/year)",
            human_bytes(p.units_per_second as u64),
            p.giga_units_per_day,
            p.tera_units_per_year
        ));
        if let (Some(years), Some(rated)) = (p.rated_years, rated_tbw) {
            lines.push(format!(
                "      endurance: {rated:.0} TBW rated -> ~{years:.2} years (~{:.0} days) at this rate",
                years * 365.0
            ));
        }
    } else {
        lines.push("-".repeat(62));
        lines.push(
            "NOTE: falcon pool drives unresolved — the above is every drive on the host \
             (includes unrelated writes)."
                .to_string(),
        );
    }

    // Coarse per-pool allocated-space delta (context, not gross writes).
    let pool_deltas = pool_alloc_deltas(before, after);
    if !pool_deltas.is_empty() {
        lines.push("-".repeat(62));
        lines.push(
            "pool allocated-space delta (coarse; not gross writes):"
                .to_string(),
        );
        for (name, delta) in pool_deltas {
            let sign = if delta < 0 { "-" } else { "+" };
            let magnitude =
                u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX);
            lines.push(format!(
                "      {name}: {sign}{}",
                human_bytes(magnitude)
            ));
        }
    }

    if rated_tbw.is_none() {
        lines.push("-".repeat(62));
        lines
            .push("tip: pass --rated-tbw <TB> to project drive lifetime at this rate.".to_string());
    }
    Ok(lines)
}

fn after_time(v: &Value) -> u64 {
    v["unix_time"].as_u64().unwrap_or(0)
}

fn label_of(v: &Value) -> &str {
    let l = v["label"].as_str().unwrap_or("");
    if l.is_empty() { "unlabeled" } else { l }
}

fn percentage(sample: &Value, device: &str) -> Option<u64> {
    sample["devices"]
        .as_array()?
        .iter()
        .find(|d| d["name"].as_str() == Some(device))
        .and_then(|d| d["percentage_used"].as_u64())
}

/// Signed allocated-bytes delta per pool present in `after`, matched by name.
fn pool_alloc_deltas(before: &Value, after: &Value) -> Vec<(String, i128)> {
    let empty = Vec::new();
    let bpools = before["pools"].as_array().unwrap_or(&empty);
    after["pools"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|ap| {
            let name = ap["name"].as_str()?;
            let a = ap["alloc_bytes"].as_u64()? as i128;
            let b = bpools
                .iter()
                .find(|p| p["name"].as_str() == Some(name))
                .and_then(|p| p["alloc_bytes"].as_u64())
                .unwrap_or(a as u64) as i128;
            Some((name.to_string(), a - b))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// load — reproducible control-plane write workload via the `oxide` CLI
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum DiskStyle {
    /// omicron main: disk_source wrapped in a `disk_backend`.
    New,
    /// Older omicron: flat `disk_source`.
    Legacy,
}

const AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS: u32 = 90;
const PROJECT_CREATE_POST_ATTEMPTS: u32 = 3;
const DISK_CREATE_POST_ATTEMPTS: u32 = 3;
const EXPLICIT_CREATE_ABSENCE_POLLS: u32 = 5;
const QUOTA_UPDATE_ATTEMPTS: u32 = 3;
const PROJECT_DELETE_RECONCILE_ATTEMPTS: u32 = 150;
const DISK_DELETE_ATTEMPTS: u32 = 10;
const DISK_SETTLEMENT_ATTEMPTS: u32 = 150;
const NETWORK_DELETE_ATTEMPTS: u32 = 3;

#[derive(Debug)]
enum ClassifiedFailure {
    Permanent(anyhow::Error),
    Retryable(anyhow::Error),
}
type ClassifiedResult<T> = std::result::Result<T, ClassifiedFailure>;

impl ClassifiedFailure {
    fn api(error: oxide_session::ApiCommandError) -> Self {
        match error.kind {
            oxide_session::ApiErrorKind::Retryable => {
                Self::Retryable(error.into())
            }
            _ => Self::Permanent(error.into()),
        }
    }
    fn permanent(message: impl Into<String>) -> Self {
        Self::Permanent(anyhow!(message.into()))
    }
}

fn combine_classified(
    operation: ClassifiedResult<()>,
    cleanup: ClassifiedResult<()>,
) -> ClassifiedResult<()> {
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => {
            let permanent =
                matches!(operation, ClassifiedFailure::Permanent(_))
                    || matches!(cleanup, ClassifiedFailure::Permanent(_));
            let error = anyhow!(
                "operation failed: {operation:?}; cleanup failed: {cleanup:?}"
            );
            Err(if permanent {
                ClassifiedFailure::Permanent(error)
            } else {
                ClassifiedFailure::Retryable(error)
            })
        }
    }
}

trait LifecycleApi: Sync {
    fn request(
        &self,
        endpoint: &str,
        method: &str,
        body: Option<&str>,
    ) -> std::result::Result<String, oxide_session::ApiCommandError>;
}

impl LifecycleApi for OxideSession {
    fn request(
        &self,
        endpoint: &str,
        method: &str,
        body: Option<&str>,
    ) -> std::result::Result<String, oxide_session::ApiCommandError> {
        self.api_request(endpoint, method, body)
    }
}

fn set_disk_lifecycle_storage_quota(
    api: &dyn LifecycleApi,
    silo: &str,
    retry_delay: Duration,
) -> ClassifiedResult<()> {
    let endpoint = format!("/v1/system/silos/{silo}/quotas");
    let body =
        json!({"storage": DISK_LIFECYCLE_STORAGE_QUOTA_BYTES}).to_string();
    for attempt in 1..=QUOTA_UPDATE_ATTEMPTS {
        let response = match api.request(&endpoint, "PUT", Some(&body)) {
            Ok(response) => response,
            Err(error) if error.status == Some(404) => {
                return Err(ClassifiedFailure::Permanent(error.into()));
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable
                    && attempt < QUOTA_UPDATE_ATTEMPTS =>
            {
                std::thread::sleep(retry_delay);
                continue;
            }
            Err(error) => return Err(ClassifiedFailure::api(error)),
        };
        let quota: Value =
            serde_json::from_str(&response).map_err(|error| {
                ClassifiedFailure::Permanent(
                    anyhow!(error)
                        .context("parse recovery silo quota update response"),
                )
            })?;
        if quota["storage"].as_u64() != Some(DISK_LIFECYCLE_STORAGE_QUOTA_BYTES)
        {
            return Err(ClassifiedFailure::permanent(
                "recovery silo quota update response did not confirm 20 GiB of storage",
            ));
        }
        return Ok(());
    }
    unreachable!("quota update loop always returns")
}

struct DiskLifecycleOwner {
    nonce: uuid::Uuid,
    project_name: String,
    project_description: String,
}

impl DiskLifecycleOwner {
    fn new(nonce: uuid::Uuid, purpose: &str) -> Self {
        let compact = nonce.simple();
        Self {
            nonce,
            project_name: format!("voxel-perftest-{purpose}-{compact}"),
            project_description: format!(
                "Voxel perftest {purpose}; ownership nonce {nonce}"
            ),
        }
    }
    fn disk_name(&self, batch: usize, disk: usize) -> String {
        format!("voxel-disk-{}-{batch}-{disk}", self.nonce.simple())
    }
    fn disk_batches(&self) -> Vec<Vec<String>> {
        (0..5).map(|b| (0..4).map(|d| self.disk_name(b, d)).collect()).collect()
    }
    fn require_owned_disk(&self, name: &str) -> Result<()> {
        if name.contains(&self.nonce.simple().to_string()) {
            Ok(())
        } else {
            Err(anyhow!("refusing to delete foreign disk {name}"))
        }
    }
    fn reconcile_project(&self, json: &str) -> Result<bool> {
        let value: Value =
            serde_json::from_str(json).context("parse project list JSON")?;
        let items = value["items"]
            .as_array()
            .ok_or_else(|| anyhow!("project list missing items"))?;
        let mut owned = None;
        for project in items {
            let name = project["name"]
                .as_str()
                .ok_or_else(|| anyhow!("project list item missing name"))?;
            if name == self.project_name {
                owned = Some(project);
            }
        }
        let Some(project) = owned else {
            return Ok(false);
        };
        if project["description"].as_str() != Some(&self.project_description) {
            return Err(anyhow!(
                "project {} name collision has missing or wrong ownership nonce",
                self.project_name,
            ));
        }
        Ok(true)
    }
}

struct PreparedDiskLifecycle<'a> {
    api: &'a dyn LifecycleApi,
    style: DiskStyle,
    poll_delay: Duration,
}

fn run_disk_lifecycle_preflight(
    api: &dyn LifecycleApi,
    silo: &str,
    poll_delay: Duration,
) -> ClassifiedResult<()> {
    PreparedDiskLifecycle::prepare_with(api, silo, poll_delay)?
        .run(&WorkloadSpec::api_disk_lifecycle())
}

impl<'a> PreparedDiskLifecycle<'a> {
    fn prepare(
        session: &'a OxideSession,
        silo: &str,
    ) -> ClassifiedResult<Self> {
        Self::prepare_with(session, silo, Duration::from_secs(2))
    }

    fn prepare_with(
        api: &'a dyn LifecycleApi,
        silo: &str,
        poll_delay: Duration,
    ) -> ClassifiedResult<Self> {
        set_disk_lifecycle_storage_quota(api, silo, poll_delay)?;
        let owner = DiskLifecycleOwner::new(uuid::Uuid::new_v4(), "probe");
        let style = prepare_owned_style(api, &owner, poll_delay)?;
        Ok(Self { api, style, poll_delay })
    }

    fn run(&self, spec: &WorkloadSpec) -> ClassifiedResult<()> {
        if spec != &WorkloadSpec::api_disk_lifecycle() {
            return Err(ClassifiedFailure::permanent(
                "invalid API disk lifecycle specification",
            ));
        }
        let owner = DiskLifecycleOwner::new(uuid::Uuid::new_v4(), "measured");
        let operation = (|| {
            create_owned_project(self.api, &owner, self.poll_delay)?;
            for batch in owner.disk_batches() {
                scoped_phase(&batch, |name| {
                    create_owned_disk(
                        self.api,
                        &owner,
                        name,
                        self.style,
                        false,
                        self.poll_delay,
                    )
                })?;
                scoped_phase(&batch, |name| {
                    wait_owned_disk(
                        self.api,
                        &owner,
                        name,
                        self.poll_delay,
                        DiskWaitMode::Measured,
                    )
                })?;
                scoped_phase(&batch, |name| {
                    delete_owned_disk(self.api, &owner, name, self.poll_delay)
                })?;
            }
            Ok(())
        })();
        combine_classified(
            operation,
            cleanup_owned(self.api, &owner, self.poll_delay),
        )
    }
}

fn prepare_owned_style(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    poll_delay: Duration,
) -> ClassifiedResult<DiskStyle> {
    let operation = (|| {
        create_owned_project(api, owner, poll_delay)?;
        let name = owner.disk_name(0, 0);
        match create_owned_disk(
            api,
            owner,
            &name,
            DiskStyle::New,
            true,
            poll_delay,
        ) {
            Ok(()) => Ok(DiskStyle::New),
            Err(ClassifiedFailure::Permanent(error))
                if error
                    .downcast_ref::<oxide_session::ApiCommandError>()
                    .is_some_and(|error| {
                        error.kind == oxide_session::ApiErrorKind::ShapeRejected
                    }) =>
            {
                create_owned_disk(
                    api,
                    owner,
                    &name,
                    DiskStyle::Legacy,
                    true,
                    poll_delay,
                )?;
                Ok(DiskStyle::Legacy)
            }
            Err(error) => Err(error),
        }
    })();
    let cleanup = cleanup_owned(api, owner, poll_delay);
    match operation {
        Ok(style) => {
            combine_classified(Ok(()), cleanup)?;
            Ok(style)
        }
        Err(error) => {
            combine_classified(Err(error), cleanup)?;
            unreachable!()
        }
    }
}

fn scoped_phase<T: Sync, F>(items: &[T], operation: F) -> ClassifiedResult<()>
where
    F: Fn(&T) -> ClassifiedResult<()> + Sync,
{
    let results = std::thread::scope(|scope| {
        items
            .iter()
            .map(|item| scope.spawn(|| operation(item)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| {
                thread.join().unwrap_or_else(|_| {
                    Err(ClassifiedFailure::Permanent(anyhow!(
                        "lifecycle worker panicked"
                    )))
                })
            })
            .collect::<Vec<_>>()
    });
    results.into_iter().fold(Ok(()), combine_classified)
}

fn create_owned_project(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    reconcile_delay: Duration,
) -> ClassifiedResult<()> {
    let listed = api
        .request("/v1/projects", "GET", None)
        .map_err(ClassifiedFailure::api)?;
    if owner.reconcile_project(&listed).map_err(ClassifiedFailure::Permanent)? {
        return Err(ClassifiedFailure::permanent(
            "generated project already exists",
        ));
    }
    let body =
        json!({"name": owner.project_name, "description": owner.project_description}).to_string();
    for create_attempt in 1..=PROJECT_CREATE_POST_ATTEMPTS {
        let error = match api.request("/v1/projects", "POST", Some(&body)) {
            Ok(json) => return validate_project_success(&json, owner),
            Err(error)
                if error.kind != oxide_session::ApiErrorKind::Retryable =>
            {
                return Err(ClassifiedFailure::api(error));
            }
            Err(error) => error,
        };
        let retry_explicit_server_failure =
            error.status.is_some_and(|status| (500..=599).contains(&status))
                && create_attempt < PROJECT_CREATE_POST_ATTEMPTS;
        if !retry_explicit_server_failure {
            return reconcile_ambiguous_project(
                api,
                owner,
                reconcile_delay,
                error,
            );
        }

        // A status-less timeout may leave a project saga running, so it must
        // never cause a second POST. An explicit server response is terminal;
        // retry its identical nonce-owned name only after every short poll has
        // successfully proven that the project is still absent. The per-silo
        // project-name uniqueness constraint makes a late first create collide
        // rather than create a second live project.
        let mut absence_proven = true;
        for poll_attempt in 1..=EXPLICIT_CREATE_ABSENCE_POLLS {
            let listed = match api.request("/v1/projects", "GET", None) {
                Ok(listed) => listed,
                Err(poll_error)
                    if poll_error.kind
                        == oxide_session::ApiErrorKind::Retryable =>
                {
                    absence_proven = false;
                    break;
                }
                Err(poll_error) => {
                    return Err(ClassifiedFailure::api(poll_error));
                }
            };
            match owner.reconcile_project(&listed) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => return Err(ClassifiedFailure::Permanent(error)),
            }
            if poll_attempt < EXPLICIT_CREATE_ABSENCE_POLLS {
                std::thread::sleep(reconcile_delay);
            }
        }
        if !absence_proven {
            return reconcile_ambiguous_project(
                api,
                owner,
                reconcile_delay,
                error,
            );
        }
        eprintln!(
            "[perftest] project {} create attempt {create_attempt}/{PROJECT_CREATE_POST_ATTEMPTS} returned an explicit server failure and remained absent after {EXPLICIT_CREATE_ABSENCE_POLLS} polls; retrying the identical nonce-owned create: {error}",
            owner.project_name,
        );
    }
    unreachable!("project create loop always returns")
}

fn reconcile_ambiguous_project(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    delay: Duration,
    initial_error: oxide_session::ApiCommandError,
) -> ClassifiedResult<()> {
    for attempt in 1..=AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS {
        let listed = api
            .request("/v1/projects", "GET", None)
            .map_err(ClassifiedFailure::api);
        match listed {
            Ok(json) => match owner.reconcile_project(&json) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => return Err(ClassifiedFailure::Permanent(error)),
            },
            Err(ClassifiedFailure::Retryable(_)) => {}
            Err(error) => return Err(error),
        }
        if attempt < AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS {
            std::thread::sleep(delay);
        }
    }
    Err(ClassifiedFailure::Retryable(anyhow!(
        "ambiguous project create was not reconciled after bounded polling for project {}; initial create failure: {initial_error}",
        owner.project_name,
    )))
}

fn validate_project_success(
    json: &str,
    owner: &DiskLifecycleOwner,
) -> ClassifiedResult<()> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| ClassifiedFailure::Permanent(e.into()))?;
    if value["name"].as_str() == Some(&owner.project_name)
        && value["description"].as_str() == Some(&owner.project_description)
    {
        Ok(())
    } else {
        Err(ClassifiedFailure::permanent(
            "incompatible project create response",
        ))
    }
}

fn create_owned_disk(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    name: &str,
    style: DiskStyle,
    probe: bool,
    reconcile_delay: Duration,
) -> ClassifiedResult<()> {
    owner.require_owned_disk(name).map_err(ClassifiedFailure::Permanent)?;
    let endpoint = format!("/v1/disks?project={}", owner.project_name);
    let body = disk_body(name, 1 << 30, style);
    for create_attempt in 1..=DISK_CREATE_POST_ATTEMPTS {
        let error = match api.request(&endpoint, "POST", Some(&body)) {
            Ok(json)
                if serde_json::from_str::<Value>(&json)
                    .ok()
                    .and_then(|v| v["name"].as_str().map(str::to_owned))
                    .as_deref()
                    == Some(name) =>
            {
                return Ok(());
            }
            Ok(_) => {
                return Err(ClassifiedFailure::permanent(
                    "incompatible disk create response",
                ));
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::ShapeRejected
                    && probe =>
            {
                return Err(ClassifiedFailure::api(error));
            }
            Err(error)
                if error.kind != oxide_session::ApiErrorKind::Retryable =>
            {
                return Err(ClassifiedFailure::api(error));
            }
            Err(error) => error,
        };
        let retry_explicit_server_failure =
            error.status.is_some_and(|status| (500..=599).contains(&status))
                && create_attempt < DISK_CREATE_POST_ATTEMPTS;
        if !retry_explicit_server_failure {
            return reconcile_ambiguous_disk(
                api,
                owner,
                name,
                reconcile_delay,
                error,
            );
        }

        let mut absence_proven = true;
        for poll_attempt in 1..=EXPLICIT_CREATE_ABSENCE_POLLS {
            match disk_exists(api, owner, name) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(ClassifiedFailure::Retryable(_)) => {
                    absence_proven = false;
                    break;
                }
                Err(error) => return Err(error),
            }
            if poll_attempt < EXPLICIT_CREATE_ABSENCE_POLLS {
                std::thread::sleep(reconcile_delay);
            }
        }
        if !absence_proven {
            return reconcile_ambiguous_disk(
                api,
                owner,
                name,
                reconcile_delay,
                error,
            );
        }
        eprintln!(
            "[perftest] disk {name} create attempt {create_attempt}/{DISK_CREATE_POST_ATTEMPTS} returned an explicit server failure and remained absent after {EXPLICIT_CREATE_ABSENCE_POLLS} polls; retrying the identical nonce-owned create: {error}"
        );
    }
    unreachable!("disk create loop always returns")
}

fn reconcile_ambiguous_disk(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    name: &str,
    delay: Duration,
    initial_error: oxide_session::ApiCommandError,
) -> ClassifiedResult<()> {
    for attempt in 1..=AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS {
        match disk_exists(api, owner, name) {
            Ok(true) => return Ok(()),
            Ok(false) | Err(ClassifiedFailure::Retryable(_)) => {
                if attempt < AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS {
                    std::thread::sleep(delay);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(ClassifiedFailure::Retryable(anyhow!(
        "ambiguous disk create was not reconciled after bounded polling; initial create failure: {initial_error}"
    )))
}

fn disk_exists(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    name: &str,
) -> ClassifiedResult<bool> {
    let text = api
        .request(
            &format!("/v1/disks?project={}", owner.project_name),
            "GET",
            None,
        )
        .map_err(ClassifiedFailure::api)?;
    Ok(item_names(&text)
        .map_err(ClassifiedFailure::Permanent)?
        .iter()
        .any(|n| n == name))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DiskWaitMode {
    Measured,
    Cleanup,
}

fn wait_owned_disk(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    name: &str,
    delay: Duration,
    mode: DiskWaitMode,
) -> ClassifiedResult<()> {
    for attempt in 1..=DISK_SETTLEMENT_ATTEMPTS {
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let text = match api.request(&endpoint, "GET", None) {
            Ok(text) => text,
            Err(error)
                if mode == DiskWaitMode::Cleanup
                    && error.kind == oxide_session::ApiErrorKind::Retryable
                    && error.status == Some(404) =>
            {
                return Ok(());
            }
            Err(error) => return Err(ClassifiedFailure::api(error)),
        };
        match classify_disk_state(&text)
            .map_err(ClassifiedFailure::Permanent)?
        {
            DiskSettlement::Settled => return Ok(()),
            DiskSettlement::Faulted if mode == DiskWaitMode::Cleanup => {
                return Ok(());
            }
            DiskSettlement::Faulted => {
                return Err(ClassifiedFailure::permanent(
                    "disk entered faulted state",
                ));
            }
            DiskSettlement::Pending if attempt < DISK_SETTLEMENT_ATTEMPTS => {
                std::thread::sleep(delay)
            }
            DiskSettlement::Pending => {}
        }
    }
    Err(ClassifiedFailure::Retryable(anyhow!("disk settlement timed out")))
}

fn delete_owned_disk(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    name: &str,
    delay: Duration,
) -> ClassifiedResult<()> {
    owner.require_owned_disk(name).map_err(ClassifiedFailure::Permanent)?;
    delete_validated_disk(api, owner, name, delay)
}

fn delete_validated_disk(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    name: &str,
    delay: Duration,
) -> ClassifiedResult<()> {
    let endpoint = format!("/v1/disks/{name}?project={}", owner.project_name);
    for attempt in 1..=DISK_DELETE_ATTEMPTS {
        match api.request(&endpoint, "DELETE", None) {
            Ok(_) => return Ok(()),
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable
                    && error.status == Some(404) =>
            {
                return Ok(());
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable
                    && attempt < DISK_DELETE_ATTEMPTS =>
            {
                // Names contain this run's nonce and are never reused. Retrying
                // the same DELETE cannot target a replacement resource; a saga
                // unwind may instead expose a validated `deleted-<id>` name for
                // final cleanup.
                std::thread::sleep(delay);
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable =>
            {
                return Err(ClassifiedFailure::Retryable(anyhow!(
                    "owned disk delete remained ambiguous after {DISK_DELETE_ATTEMPTS} attempts: {error}"
                )));
            }
            Err(error) => return Err(ClassifiedFailure::api(error)),
        }
    }
    unreachable!("disk delete loop always returns")
}

fn is_delete_saga_tombstone(item: &Value) -> bool {
    let Some(name) = item["name"].as_str() else {
        return false;
    };
    let Some(id) =
        item["id"].as_str().and_then(|id| uuid::Uuid::parse_str(id).ok())
    else {
        return false;
    };
    name == format!("deleted-{id}")
        && item.pointer("/state/state").and_then(Value::as_str)
            == Some("faulted")
}

fn owned_disk_names(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
) -> ClassifiedResult<Vec<String>> {
    let endpoint = format!("/v1/disks?project={}", owner.project_name);
    let text =
        api.request(&endpoint, "GET", None).map_err(ClassifiedFailure::api)?;
    let value: Value = serde_json::from_str(&text)
        .context("parse disk list JSON")
        .map_err(ClassifiedFailure::Permanent)?;
    let items = value["items"].as_array().ok_or_else(|| {
        ClassifiedFailure::Permanent(anyhow!(
            "disk list response missing items array"
        ))
    })?;
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        let name = item["name"].as_str().ok_or_else(|| {
            ClassifiedFailure::Permanent(anyhow!(
                "disk list item missing string name"
            ))
        })?;
        if owner.require_owned_disk(name).is_err()
            && !is_delete_saga_tombstone(item)
        {
            return Err(ClassifiedFailure::Permanent(anyhow!(
                "refusing to delete foreign disk {name}"
            )));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

fn cleanup_owned_disks(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    names: &[String],
    delay: Duration,
) -> ClassifiedResult<()> {
    for name in names {
        wait_owned_disk(api, owner, name, delay, DiskWaitMode::Cleanup)?;
        // `owned_disk_names` admitted only a nonce-owned name or an exact
        // faulted `deleted-<id>` artifact from an unwound delete saga.
        delete_validated_disk(api, owner, name, delay)?;
    }
    Ok(())
}

fn delete_owned_network_resource(
    api: &dyn LifecycleApi,
    endpoint: &str,
    delay: Duration,
) -> ClassifiedResult<()> {
    for attempt in 1..=NETWORK_DELETE_ATTEMPTS {
        match api.request(endpoint, "DELETE", None) {
            Ok(_) => return Ok(()),
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable
                    && error.status == Some(404) =>
            {
                return Ok(());
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable
                    && attempt < NETWORK_DELETE_ATTEMPTS =>
            {
                std::thread::sleep(delay);
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable =>
            {
                return Err(ClassifiedFailure::Retryable(anyhow!(
                    "owned default-network delete remained ambiguous after {NETWORK_DELETE_ATTEMPTS} attempts: {error}"
                )));
            }
            Err(error) => return Err(ClassifiedFailure::api(error)),
        }
    }
    unreachable!("network delete loop always returns")
}

fn cleanup_owned(
    api: &dyn LifecycleApi,
    owner: &DiskLifecycleOwner,
    delay: Duration,
) -> ClassifiedResult<()> {
    let projects = api
        .request("/v1/projects", "GET", None)
        .map_err(ClassifiedFailure::api)?;
    if !owner
        .reconcile_project(&projects)
        .map_err(ClassifiedFailure::Permanent)?
    {
        return Ok(());
    }
    let snapshots_endpoint =
        format!("/v1/snapshots?project={}", owner.project_name);
    let snapshots = api
        .request(&snapshots_endpoint, "GET", None)
        .map_err(ClassifiedFailure::api)?;
    let snapshot_names =
        item_names(&snapshots).map_err(ClassifiedFailure::Permanent)?;
    for name in &snapshot_names {
        owner.require_owned_disk(name).map_err(ClassifiedFailure::Permanent)?;
    }
    let mut disk_names = Some(owned_disk_names(api, owner)?);
    for name in &snapshot_names {
        api.request(
            &format!("/v1/snapshots/{name}?project={}", owner.project_name),
            "DELETE",
            None,
        )
        .map_err(ClassifiedFailure::api)?;
    }
    let project_endpoint = format!("/v1/projects/{}", owner.project_name);
    let mut retry_project_absence_proof = false;
    let mut default_network_deleted = false;
    'delete_project: for attempt in 1..=PROJECT_DELETE_RECONCILE_ATTEMPTS {
        if retry_project_absence_proof {
            let projects = match api.request("/v1/projects", "GET", None) {
                Ok(projects) => projects,
                Err(error)
                    if error.kind == oxide_session::ApiErrorKind::Retryable
                        && attempt < PROJECT_DELETE_RECONCILE_ATTEMPTS =>
                {
                    std::thread::sleep(delay);
                    continue 'delete_project;
                }
                Err(error) => return Err(ClassifiedFailure::api(error)),
            };
            if !owner
                .reconcile_project(&projects)
                .map_err(ClassifiedFailure::Permanent)?
            {
                return Ok(());
            }
            retry_project_absence_proof = false;
        }
        let names = match disk_names.take() {
            Some(names) => names,
            None => owned_disk_names(api, owner)?,
        };
        cleanup_owned_disks(api, owner, &names, delay)?;
        if !default_network_deleted {
            delete_owned_network_resource(
                api,
                &format!(
                    "/v1/internet-gateways/default?project={}&vpc=default&cascade=true",
                    owner.project_name
                ),
                delay,
            )?;
            delete_owned_network_resource(
                api,
                &format!(
                    "/v1/vpc-subnets/default?project={}&vpc=default",
                    owner.project_name
                ),
                delay,
            )?;
            delete_owned_network_resource(
                api,
                &format!("/v1/vpcs/default?project={}", owner.project_name),
                delay,
            )?;
            default_network_deleted = true;
        }
        match api.request(&project_endpoint, "DELETE", None) {
            Ok(_) => break 'delete_project,
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::ShapeRejected
                    && attempt < PROJECT_DELETE_RECONCILE_ATTEMPTS =>
            {
                // Disk-create sagas can publish a disk after the list or change
                // the project's resource generation while DELETE evaluates it.
                // Re-list on every retry so only newly visible owned disks can
                // be removed, and never submit another create request.
                std::thread::sleep(delay);
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::ShapeRejected =>
            {
                return Err(ClassifiedFailure::Retryable(anyhow!(
                    "project deletion remained blocked after bounded reconciliation"
                )));
            }
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable =>
            {
                // A timeout or server error may happen before or after the
                // delete commits. Prove absence before deciding whether it is
                // safe to retry against the same exclusively owned project.
                let projects = match api.request("/v1/projects", "GET", None) {
                    Ok(projects) => projects,
                    Err(reconcile_error)
                        if reconcile_error.kind
                            == oxide_session::ApiErrorKind::Retryable
                            && attempt < PROJECT_DELETE_RECONCILE_ATTEMPTS =>
                    {
                        retry_project_absence_proof = true;
                        std::thread::sleep(delay);
                        continue 'delete_project;
                    }
                    Err(reconcile_error) => {
                        return Err(ClassifiedFailure::api(reconcile_error));
                    }
                };
                if !owner
                    .reconcile_project(&projects)
                    .map_err(ClassifiedFailure::Permanent)?
                {
                    return Ok(());
                }
                if attempt == PROJECT_DELETE_RECONCILE_ATTEMPTS {
                    return Err(ClassifiedFailure::api(error));
                }
                std::thread::sleep(delay);
            }
            Err(error) => return Err(ClassifiedFailure::api(error)),
        }
    }
    for attempt in 1..=60 {
        let projects = match api.request("/v1/projects", "GET", None) {
            Ok(projects) => projects,
            Err(error)
                if error.kind == oxide_session::ApiErrorKind::Retryable
                    && attempt < 60 =>
            {
                std::thread::sleep(delay);
                continue;
            }
            Err(error) => return Err(ClassifiedFailure::api(error)),
        };
        if !owner
            .reconcile_project(&projects)
            .map_err(ClassifiedFailure::Permanent)?
        {
            return Ok(());
        }
        if attempt < 60 {
            std::thread::sleep(delay);
        }
    }
    Err(ClassifiedFailure::Retryable(anyhow!(
        "project deletion was not proven"
    )))
}

fn disk_body(name: &str, size_bytes: u64, style: DiskStyle) -> String {
    let source = json!({ "type": "blank", "block_size": LOAD_BLOCK_SIZE });
    let body = match style {
        DiskStyle::New => json!({
            "name": name, "description": "perftest-load", "size": size_bytes,
            "disk_backend": { "type": "distributed", "disk_source": source },
        }),
        DiskStyle::Legacy => json!({
            "name": name, "description": "perftest-load", "size": size_bytes,
            "disk_source": source,
        }),
    };
    body.to_string()
}

fn create_disk(
    project: &str,
    name: &str,
    size_bytes: u64,
    style: DiskStyle,
) -> Result<()> {
    let body = disk_body(name, size_bytes, style);
    oxide_post_result(&format!("/v1/disks?project={project}"), &body)
        .with_context(|| format!("create disk {name}"))
}

fn delete_disk(project: &str, name: &str) -> Result<()> {
    oxide_run(&[
        "api",
        &format!("/v1/disks/{name}?project={project}"),
        "--method",
        "DELETE",
    ])
    .with_context(|| format!("delete disk {name}"))
}

enum DiskSettlement {
    Settled,
    Pending,
    Faulted,
}
fn classify_disk_state(text: &str) -> Result<DiskSettlement> {
    let value: Value =
        serde_json::from_str(text).context("parse disk state JSON")?;
    match value
        .pointer("/state/state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("disk response missing string state.state"))?
    {
        "detached" | "attached" => Ok(DiskSettlement::Settled),
        "faulted" => Ok(DiskSettlement::Faulted),
        _ => Ok(DiskSettlement::Pending),
    }
}

fn create_snapshot(project: &str, disk: &str) -> Result<()> {
    let body =
        json!({ "name": format!("{disk}-snap"), "description": "perftest-load", "disk": disk })
            .to_string();
    oxide_post_result(&format!("/v1/snapshots?project={project}"), &body)
        .with_context(|| format!("create snapshot for disk {disk}"))
}

fn delete_snapshot(project: &str, disk: &str) -> Result<()> {
    oxide_run(&[
        "api",
        &format!("/v1/snapshots/{disk}-snap?project={project}"),
        "--method",
        "DELETE",
    ])
    .with_context(|| format!("delete snapshot for disk {disk}"))
}

fn item_names(list_json: &str) -> Result<Vec<String>> {
    let value: Value =
        serde_json::from_str(list_json).context("parse list JSON")?;
    value["items"]
        .as_array()
        .ok_or_else(|| anyhow!("list response missing items array"))?
        .iter()
        .map(|item| {
            item["name"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("list item missing string name"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// smooth — per-operation latency distribution (the "feels snappy" axis)
// ---------------------------------------------------------------------------

/// One phase's latency distribution over the measured cycles (all in ms).
#[derive(Clone, Debug, Serialize)]
struct LatencySummary {
    phase: String,
    n: usize,
    min_ms: f64,
    p50_ms: f64,
    p90_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    /// p99 / p50 — a single "how spiky" number (1.0 = perfectly even; higher =
    /// the slow tail is far worse than the typical case = janky).
    jitter: f64,
}

#[derive(Default)]
struct SmoothOwnership {
    project: bool,
    disks: BTreeSet<String>,
    snapshots: BTreeSet<String>,
}

impl SmoothOwnership {
    fn project_created(&mut self) {
        self.project = true;
    }

    fn disk_created(&mut self, name: &str) {
        self.disks.insert(name.to_string());
    }

    fn snapshot_created(&mut self, name: &str) {
        self.snapshots.insert(name.to_string());
    }

    fn may_cleanup_project(&self) -> bool {
        self.project
    }
}

fn smooth_should_cleanup(ownership: &SmoothOwnership, keep: bool) -> bool {
    ownership.may_cleanup_project() && !keep
}

fn smooth_project_absent(projects: &str, project: &str) -> Result<()> {
    if item_names(projects)?.iter().any(|name| name == project) {
        bail!("project {project} already exists; refusing to adopt it");
    }
    Ok(())
}

fn publish_smooth_json(path: &Path, bytes: &[u8]) -> Result<()> {
    write_new(path, bytes, None).context("publish smooth JSON")
}

fn cleanup_smooth_owned(
    project: &str,
    ownership: &SmoothOwnership,
) -> Result<()> {
    if !ownership.may_cleanup_project() {
        return Ok(());
    }
    let mut errors = Vec::new();
    for name in &ownership.snapshots {
        if let Err(error) = oxide_run(&[
            "api",
            &format!("/v1/snapshots/{name}?project={project}"),
            "--method",
            "DELETE",
        ]) {
            errors.push(format!("delete snapshot {name}: {error:#}"));
        }
    }
    for name in &ownership.disks {
        if let Err(error) = delete_disk(project, name) {
            errors.push(format!("{error:#}"));
        }
    }
    if let Err(error) = oxide_run(&[
        "api",
        &format!("/v1/projects/{project}"),
        "--method",
        "DELETE",
    ]) {
        errors.push(format!("delete project {project}: {error:#}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("cleanup failures: {}", errors.join("; ")))
    }
}

fn cmd_smooth(
    count: usize,
    size: &str,
    project: &str,
    snapshot: bool,
    keep: bool,
    json_out: Option<&Path>,
) -> Result<()> {
    let size_bytes = size_to_bytes(size)?;
    let count = count.max(1);
    println!(
        "[perftest] smooth: {count} serial create/settle/delete cycle(s) of {size} disks, snapshot={snapshot}"
    );

    let projects = oxide_capture_result(&["api", "/v1/projects"])
        .context("list projects for smooth workload")?;
    smooth_project_absent(&projects, project)?;
    let mut ownership = SmoothOwnership::default();
    let body =
        json!({ "name": project, "description": "voxel smoothness perftest" })
            .to_string();
    oxide_post_result("/v1/projects", &body)
        .with_context(|| format!("create project {project}"))?;
    ownership.project_created();
    println!("[perftest] created project {project}");

    let execution = (|| -> Result<()> {
        let mut failures = Vec::new();
        let mut detected_style = None;
        for style in [DiskStyle::New, DiskStyle::Legacy] {
            let name = "smooth-preflight";
            let body = disk_body(name, size_bytes, style);
            match oxide_post_result(
                &format!("/v1/disks?project={project}"),
                &body,
            ) {
                Ok(()) => {
                    ownership.disk_created(name);
                    delete_disk(project, name)
                        .context("delete successful smooth throwaway disk")?;
                    ownership.disks.remove(name);
                    detected_style = Some(style);
                    break;
                }
                Err(error) => failures.push(format!("{error:#}")),
            }
        }
        let style = detected_style.ok_or_else(|| anyhow!(
            "disk create failed with both API shapes — check `oxide` auth/version against the rack: {}",
            failures.join("; ")
        ))?;

        // One Vec of latencies per phase. Serial by design: each op runs alone so a
        // measured latency isn't inflated by our own concurrent requests.
        let mut create = Vec::with_capacity(count);
        let mut settle = Vec::with_capacity(count);
        let mut snap_create = Vec::new();
        let mut snap_delete = Vec::new();
        let mut delete = Vec::with_capacity(count);

        for i in 0..count {
            let name = format!("sm-{i}");
            let (created, c_ms) =
                time_ms(|| create_disk(project, &name, size_bytes, style));
            created?;
            ownership.disk_created(&name);
            create.push(c_ms);

            match wait_disk_timed(project, &name, Duration::from_secs(120)) {
                Some(s_ms) => settle.push(s_ms),
                None => {
                    eprintln!(
                        "[perftest] WARN: {name} never settled; dropping this cycle's samples"
                    );
                    let (deleted, _) = time_ms(|| delete_disk(project, &name));
                    deleted?;
                    ownership.disks.remove(&name);
                    continue;
                }
            }

            if snapshot {
                let (created, sc) = time_ms(|| create_snapshot(project, &name));
                created?;
                ownership.snapshot_created(&format!("{name}-snap"));
                snap_create.push(sc);
                let (deleted, sd) = time_ms(|| delete_snapshot(project, &name));
                deleted?;
                ownership.snapshots.remove(&format!("{name}-snap"));
                snap_delete.push(sd);
            }

            let (deleted, d_ms) = time_ms(|| delete_disk(project, &name));
            deleted?;
            ownership.disks.remove(&name);
            delete.push(d_ms);

            if (i + 1) % 10 == 0 || i + 1 == count {
                println!("[perftest] smooth: {}/{count} cycles", i + 1);
            }
        }

        // Cycle order, so the table reads top-to-bottom as one cycle.
        let mut phases: Vec<(&str, Vec<f64>)> =
            vec![("create", create), ("settle", settle)];
        if snapshot {
            phases.push(("snap-create", snap_create));
            phases.push(("snap-delete", snap_delete));
        }
        phases.push(("delete", delete));

        let summaries: Vec<LatencySummary> = phases
            .iter()
            .map(|(name, ms)| summarize_latency(name, ms))
            .collect();
        println!("\n{}", render_smooth_table(&summaries));

        if let Some(path) = json_out {
            let doc = json!({
                "kind": "perftest-smooth",
                "project": project,
                "count": count,
                "snapshot": snapshot,
                "phases": phases.iter().zip(&summaries).map(|((name, ms), s)| json!({
                    "phase": name,
                    "samples_ms": ms,
                    "summary": s,
                })).collect::<Vec<_>>(),
            });
            publish_smooth_json(
                path,
                format!("{}\n", serde_json::to_string_pretty(&doc)?).as_bytes(),
            )?;
            println!("[perftest] wrote latency JSON -> {}", path.display());
        }

        Ok(())
    })();

    if !smooth_should_cleanup(&ownership, keep) {
        println!(
            "[perftest] kept project {project} (--keep); remove: oxide api /v1/projects/{project} --method DELETE"
        );
        return execution;
    }
    println!("[perftest] cleaning up project {project}");
    let cleanup = cleanup_smooth_owned(project, &ownership);
    match (execution, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(anyhow!(
            "smooth execution failed: {error:#}; additionally cleanup failed: {cleanup:#}"
        )),
    }
}

/// Run `f`, returning its result and how long it took in milliseconds.
fn time_ms<R>(f: impl FnOnce() -> R) -> (R, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

/// Poll a disk to a settled state at fine (200ms) granularity, returning the
/// elapsed ms — the user-perceived "disk is ready" latency. `None` on
/// fault/timeout. Finer than [`wait_disk`] so the settle distribution has
/// resolution to show jitter.
fn wait_disk_timed(
    project: &str,
    name: &str,
    timeout: Duration,
) -> Option<f64> {
    let start = Instant::now();
    loop {
        let state = oxide_capture(&[
            "api",
            &format!("/v1/disks/{name}?project={project}"),
        ])
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["state"]["state"].as_str().map(str::to_string));
        match state.as_deref() {
            Some("detached") | Some("attached") => {
                return Some(start.elapsed().as_secs_f64() * 1000.0);
            }
            Some("faulted") | None => return None,
            _ => {}
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Linear-interpolated percentile (`p` in 0..=100) over an ascending slice.
/// Empty -> 0. This is the numpy default ("type 7"); ample for the few-hundred
/// samples here — hdrhistogram only earns its keep at far higher volumes.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    match sorted.len() {
        0 => 0.0,
        1 => sorted[0],
        n => {
            let rank = (p / 100.0) * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = rank.ceil() as usize;
            let frac = rank - lo as f64;
            sorted[lo] + (sorted[hi] - sorted[lo]) * frac
        }
    }
}

/// Summarize a phase's latency samples (ms) into min/p50/p90/p99/max/mean and a
/// p99/p50 jitter ratio.
fn summarize_latency(phase: &str, ms: &[f64]) -> LatencySummary {
    let mut sorted = ms.to_vec();
    sorted
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let mean = if n > 0 { sorted.iter().sum::<f64>() / n as f64 } else { 0.0 };
    let p50 = percentile(&sorted, 50.0);
    let p99 = percentile(&sorted, 99.0);
    LatencySummary {
        phase: phase.to_string(),
        n,
        min_ms: sorted.first().copied().unwrap_or(0.0),
        p50_ms: p50,
        p90_ms: percentile(&sorted, 90.0),
        p99_ms: p99,
        max_ms: sorted.last().copied().unwrap_or(0.0),
        mean_ms: mean,
        jitter: if p50 > 0.0 { p99 / p50 } else { 0.0 },
    }
}

/// Compact latency: `"12ms"` under a second, `"1.20s"` above.
fn human_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

fn render_smooth_table(summaries: &[LatencySummary]) -> String {
    let mut s = String::from(
        "control-plane latency by operation (serial; lower + flatter = smoother):\n",
    );
    s.push_str(&format!(
        "{:<12}  {:>4}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}  {:>7}\n",
        "PHASE", "N", "MIN", "p50", "p90", "p99", "MAX", "JITTER"
    ));
    s.push_str(&"-".repeat(74));
    s.push('\n');
    for x in summaries {
        s.push_str(&format!(
            "{:<12}  {:>4}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}  {:>6.1}x\n",
            x.phase,
            x.n,
            human_ms(x.min_ms),
            human_ms(x.p50_ms),
            human_ms(x.p90_ms),
            human_ms(x.p99_ms),
            human_ms(x.max_ms),
            x.jitter,
        ));
    }
    s.push_str("\nJITTER = p99/p50 (1.0 = even; a high value means occasional stalls).\n");
    s
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a size spec (`1G`, `512M`, `2048k`, or a raw byte count) into bytes,
/// 1024-based.
fn size_to_bytes(spec: &str) -> Result<u64> {
    let spec = spec.trim();
    let (num, mult) = match spec.chars().last() {
        Some('k') | Some('K') => (&spec[..spec.len() - 1], 1u64 << 10),
        Some('m') | Some('M') => (&spec[..spec.len() - 1], 1u64 << 20),
        Some('g') | Some('G') => (&spec[..spec.len() - 1], 1u64 << 30),
        Some('t') | Some('T') => (&spec[..spec.len() - 1], 1u64 << 40),
        _ => (spec, 1),
    };
    num.trim().parse::<u64>().map(|n| n * mult).map_err(|_| {
        anyhow!("bad size '{spec}' (try 1G, 512M, or a byte count)")
    })
}

/// Human-readable decimal size (GB = 1e9), matching the perftest framing.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}

/// Compact duration for the results table: `"45s"`, `"4m11s"`, `"1h02m"` (drops
/// seconds past an hour). Launches run minutes, so this reads better than a bare
/// second count.
fn human_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Capture a command's stdout (trimmed), or `None` on spawn failure / nonzero.
fn capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Capture `oxide <args>` stdout, or `None` on failure.
fn oxide_capture(args: &[&str]) -> Option<String> {
    let out = Command::new("oxide").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

fn oxide_capture_result(args: &[&str]) -> Result<String> {
    let output = Command::new("oxide")
        .args(args)
        .output()
        .with_context(|| format!("run `oxide {}`", args.join(" ")))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`oxide {}` failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).context("oxide output was not UTF-8")
}

fn oxide_run(args: &[&str]) -> Result<()> {
    oxide_capture_result(args).map(|_| ())
}

fn oxide_post_result(endpoint: &str, body: &str) -> Result<()> {
    let mut cmd = Command::new("oxide");
    cmd.args(["api", endpoint, "--method", "POST", "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        cmd.spawn().with_context(|| format!("spawn oxide POST {endpoint}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("oxide POST {endpoint} has no stdin"))?
        .write_all(body.as_bytes())
        .with_context(|| format!("write oxide POST {endpoint} body"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("wait for oxide POST {endpoint}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(anyhow!(
        "oxide POST {endpoint} failed with {}: {detail}",
        output.status
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::{Arc, Barrier, Mutex};

    #[derive(Parser)]
    struct PerftestCli {
        #[command(subcommand)]
        command: PerftestCmd,
    }

    fn planned_schema_v5_checkpoint() -> MatrixCheckpoint {
        MatrixCheckpoint {
            schema_version: 5,
            checkpoint_sequence: 0,
            status: RunStatus::Running,
            abort_error: None,
            name: "strict-checkpoint".into(),
            started: 100,
            updated: 100,
            ended: None,
            rated_tbw: None,
            workload: None,
            oxide_session: None,
            scope_proof: capability_unavailable(
                "matrix scope has not yet been sampled",
            ),
            report_evidence: None,
            rss_sleds: 3,
            repeat: 3,
            combos: vec![MatrixCheckpointCombo {
                label: "none".into(),
                levers: BTreeSet::new(),
                effective_config: VoxelConfig::default(),
                repeats: (0..3)
                    .map(|index| MatrixCheckpointRepeat {
                        index,
                        pre_boundary: BoundaryOutcome::Pending {},
                        launch: LaunchOutcome::Pending {},
                        preparation: PreparationOutcome::NotRequested {},
                        workload: WorkloadOutcome::NotRequested {},
                        post_boundary: BoundaryOutcome::Pending {},
                    })
                    .collect(),
            }],
        }
    }

    fn schema_v5_fixture(name: &str) -> MatrixCheckpoint {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/perftest")
            .join(name);
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn cookout_report_wrapper_publishes_multiple_checkpoint_states() {
        let root = tempfile::tempdir().unwrap();
        let inputs = [
            "matrix-complete-v5.json",
            "matrix-partial-v5.json",
            "matrix-failed-v5.json",
        ]
        .map(|name| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/perftest")
                .join(name)
        });
        let out = root.path().join("report");
        cookout_commands::run_report(&inputs, &out, true).unwrap();
        assert!(out.join("report.json").is_file());
        assert!(root.path().join("report.tar.gz").is_file());
        let report: Value = serde_json::from_slice(
            &std::fs::read(out.join("report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["schema"], "cookout.report.v2");
        assert!(
            report["issues"]
                .as_array()
                .is_some_and(|issues| !issues.is_empty())
        );
        let report_text = serde_json::to_string(&report).unwrap();
        assert!(report_text.contains("required repeat failed"));
        assert!(report_text.contains("required measurement missing"));
        for index in 0..inputs.len() {
            let archived =
                std::fs::read(out.join(format!("evidence-{index:04}.json")))
                    .unwrap();
            let envelope = cookout::validate_evidence(
                &archived,
                &cookout::Limits::default(),
            )
            .unwrap();
            assert_eq!(envelope.adapter.id, "oxide.voxel.perftest");
        }
        let retained: Value = serde_json::from_slice(
            &std::fs::read(out.join("evidence-0000.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(retained["schema"], "cookout.evidence.v1");
        assert_eq!(retained["adapter"]["id"], "oxide.voxel.perftest");
        assert_eq!(
            retained["source"]["value"]["source_schema"],
            "matrix_checkpoint"
        );
        assert!(
            !serde_json::to_string(&retained)
                .unwrap()
                .to_ascii_lowercase()
                .contains("password")
        );

        let error =
            cookout_commands::run_report(&inputs, &out, false).unwrap_err();
        assert!(format!("{error:#}").contains("destination"));
    }

    #[test]
    fn cookout_report_wrapper_rejects_malformed_and_unknown_schema_with_source()
    {
        let root = tempfile::tempdir().unwrap();
        for (name, contents) in [
            ("malformed.json", b"{".as_slice()),
            ("future.json", br#"{"schema_version":999}"#.as_slice()),
        ] {
            let input = root.path().join(name);
            std::fs::write(&input, contents).unwrap();
            let error = cookout_commands::run_report(
                std::slice::from_ref(&input),
                &root.path().join(format!("out-{name}")),
                false,
            )
            .unwrap_err();
            assert!(error.to_string().contains(&input.display().to_string()));
        }
    }

    #[test]
    fn cookout_report_wrapper_rejects_semantically_malformed_failed_matrix() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("failed-v4.json");
        let mut run = run_with("failed", &[("none", &[], &[1, 2])]);
        run.results[0].repeats.pop();
        run.results[0].repeats[0].peak_ram_bytes = Some(1);
        run.results[0].error = Some(String::new());
        std::fs::write(&input, serde_json::to_vec(&run).unwrap()).unwrap();

        let error = cookout_commands::run_report(
            std::slice::from_ref(&input),
            &root.path().join("report"),
            false,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&input.display().to_string()));
        assert!(message.contains("validate retained failed storage matrix"));
    }

    #[test]
    fn cookout_report_wrapper_rejects_semantically_malformed_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("checkpoint-v5.json");
        let mut checkpoint = planned_schema_v5_checkpoint();
        checkpoint.combos[0].label = "noncanonical".into();
        std::fs::write(&input, serde_json::to_vec(&checkpoint).unwrap())
            .unwrap();

        let error = cookout_commands::run_report(
            std::slice::from_ref(&input),
            &root.path().join("report"),
            false,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&input.display().to_string()));
        assert!(message.contains(
            "validate schema-v5 storage matrix checkpoint semantics"
        ));
    }

    #[test]
    fn cookout_superreport_wrapper_delegates_archive_deduplication() {
        let root = tempfile::tempdir().unwrap();
        let input = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/perftest/matrix-complete-v5.json");
        let first = root.path().join("first");
        let second = root.path().join("second");
        cookout_commands::run_report(
            std::slice::from_ref(&input),
            &first,
            true,
        )
        .unwrap();
        cookout_commands::run_report(&[input], &second, true).unwrap();
        let archives = [
            root.path().join("first.tar.gz"),
            root.path().join("second.tar.gz"),
        ];
        let out = root.path().join("aggregate");
        cookout_commands::run_superreport(&archives, &out, true).unwrap();
        let report: Value = serde_json::from_slice(
            &std::fs::read(out.join("report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["aggregation"]["unique_input_count"], 1);
        assert_eq!(report["aggregation"]["duplicate_count"], 1);
        assert_eq!(report["aggregation"]["normalization"], "adapter_replay");
        assert_eq!(report["schema"], "cookout.report.v2");
        let retained: Value = serde_json::from_slice(
            &std::fs::read(out.join("evidence-0000.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(retained["schema"], "cookout.evidence.v1");
        assert_eq!(retained["adapter"]["id"], "oxide.voxel.perftest");
        assert!(root.path().join("aggregate.tar.gz").is_file());
    }

    #[test]
    fn cookout_adapter_maps_complete_checkpoint_contract() {
        let source = Path::new("matrix-complete-v5.json");
        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &schema_v5_fixture("matrix-complete-v5.json"),
            source,
        )
        .unwrap();
        assert_eq!(document.variants.len(), 1);
        assert_eq!(document.runs.len(), 1);
        assert_eq!(
            document.variants[0].configuration,
            Some(cookout::model::VariantConfiguration::StorageLevers {
                levers: BTreeSet::new(),
            })
        );
        assert_eq!(
            document.runs[0].outcome,
            cookout::model::RunOutcome::Completed
        );
        assert!(document.runs[0]
            .phases
            .iter()
            .flat_map(|phase| &phase.observations)
            .any(|observation| observation.metric == "launch.bytes_written"));
    }

    #[test]
    fn cookout_adapter_retains_replayable_source_and_actionable_issues() {
        for source in [
            "matrix-complete-v5.json",
            "matrix-partial-v5.json",
            "matrix-failed-v5.json",
            "checkpoint-in-progress-v5.json",
        ] {
            let evidence = cookout_adapter::matrix_checkpoint_to_evidence(
                &schema_v5_fixture(source),
                Path::new(source),
            )
            .unwrap();
            let bytes = serde_json::to_vec(&evidence).unwrap();
            cookout::validate_evidence(&bytes, &cookout::Limits::default())
                .unwrap();
            let text = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
            assert!(!text.contains("password"), "{source}");
            assert!(
                evidence.issues.iter().all(|issue| issue
                    .code
                    .starts_with("oxide.voxel.perftest."))
            );

            if source == "matrix-failed-v5.json" {
                assert!(evidence.issues.iter().any(|issue| {
                    issue.code == "oxide.voxel.perftest.stage_failure"
                        && issue.description.contains("Candidate")
                        && issue.description.contains("repeat")
                        && matches!(
                            issue.scope,
                            cookout::IssueScope::Run { .. }
                        )
                }));
            }
            if source == "checkpoint-in-progress-v5.json" {
                assert!(
                    evidence.issues.iter().any(|issue| {
                        issue.code == "oxide.voxel.perftest.repeat_pending"
                            && issue.impact == cookout::IssueImpact::Blocking
                    }),
                    "{:#?}",
                    evidence.issues
                );
            }
        }

        let mut run = run_with("complete-v4", &[("none", &[], &[1, 2])]);
        for repeat in &mut run.results[0].repeats {
            repeat.peak_ram_bytes = Some(1);
        }
        let evidence = cookout_adapter::matrix_run_to_evidence(
            &run,
            Path::new("matrix-v4.json"),
        )
        .unwrap();
        assert_eq!(evidence.adapter.source_schema_version, 1);
        assert_eq!(evidence.source.value["source_schema"], "matrix_run");
    }

    fn checkpoint_with_successes(successful: usize) -> MatrixCheckpoint {
        let mut checkpoint = schema_v5_fixture("matrix-complete-v5.json");
        let completed = checkpoint.combos[0].repeats[0].clone();
        checkpoint.status = RunStatus::Completed;
        checkpoint.abort_error = None;
        checkpoint.ended = Some(checkpoint.updated);
        checkpoint.repeat = 5;
        checkpoint.combos[0].repeats = (0..5)
            .map(|index| {
                let mut repeat = completed.clone();
                repeat.index = index;
                if index >= successful {
                    repeat.workload = WorkloadOutcome::Failure {
                        error: "bounded workload failure".into(),
                    };
                }
                repeat
            })
            .collect();
        checkpoint
    }

    #[test]
    fn cookout_adapter_tolerates_one_bounded_failure_in_five() {
        let evidence = cookout_adapter::matrix_checkpoint_to_evidence(
            &checkpoint_with_successes(4),
            Path::new("four-of-five.json"),
        )
        .unwrap();
        let failure = evidence
            .issues
            .iter()
            .find(|issue| issue.code == "oxide.voxel.perftest.stage_failure")
            .unwrap();
        assert_eq!(failure.impact, cookout::IssueImpact::Diagnostic);
    }

    #[test]
    fn cookout_adapter_blocks_below_success_tolerance() {
        let evidence = cookout_adapter::matrix_checkpoint_to_evidence(
            &checkpoint_with_successes(3),
            Path::new("three-of-five.json"),
        )
        .unwrap();
        assert!(evidence.issues.iter().any(|issue| {
            issue.code == "oxide.voxel.perftest.stage_failure"
                && issue.impact == cookout::IssueImpact::Blocking
        }));
    }

    #[test]
    fn cookout_adapter_blocks_boundary_failure_at_success_tolerance() {
        let mut checkpoint = checkpoint_with_successes(4);
        checkpoint.status = RunStatus::Aborted;
        checkpoint.abort_error = Some("clean boundary failed".into());
        checkpoint.combos[0].repeats[4].post_boundary =
            BoundaryOutcome::Failure { error: "dirty boundary".into() };
        let evidence = cookout_adapter::matrix_checkpoint_to_evidence(
            &checkpoint,
            Path::new("dirty-four-of-five.json"),
        )
        .unwrap();
        assert!(evidence.issues.iter().any(|issue| {
            issue.code == "oxide.voxel.perftest.stage_failure"
                && issue.impact == cookout::IssueImpact::Blocking
        }));
    }

    #[test]
    fn voxel_recommendation_policy_uses_storage_pareto_objectives() {
        let policy = cookout_commands::voxel_analysis_policy();
        assert_eq!(policy.noise_multiplier, 2.0);
        let recommendation = policy.recommendation.unwrap();
        assert_eq!(
            recommendation.mode,
            cookout::policy::RecommendationMode::UsabilityDefault
        );
        assert_eq!(
            recommendation.selection,
            cookout::policy::SelectionMode::Pareto
        );
        assert_eq!(
            recommendation
                .objectives
                .iter()
                .map(|objective| objective.metric.metric.as_str())
                .collect::<Vec<_>>(),
            vec![
                "launch.bytes_written",
                "workload.bytes_written",
                "launch.duration",
                "launch.peak_ram_delta",
            ]
        );
        assert!(recommendation.objectives.iter().all(|objective| {
            objective.metric.direction
                == cookout::model::OptimizationDirection::LowerIsBetter
        }));
    }

    fn storage_cohort(checkpoint: &MatrixCheckpoint, source: &Path) -> String {
        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            checkpoint, source,
        )
        .unwrap();
        match document
            .target
            .dimensions
            .namespaced
            .get("oxide.voxel.storage_cohort")
            .unwrap()
        {
            cookout::model::DimensionValue::Text(value) => value.clone(),
            value => {
                panic!("storage cohort must be opaque text, got {value:?}")
            }
        }
    }

    fn checkpoint_with_complete_cohort_evidence() -> MatrixCheckpoint {
        let mut checkpoint = schema_v5_fixture("matrix-complete-v5.json");
        let plan = checkpoint
            .combos
            .iter()
            .map(|combo| (combo.label.clone(), combo.levers.clone()))
            .collect::<Vec<_>>();
        let mut evidence = build_report_evidence(
            &checkpoint.combos[0].effective_config,
            &plan,
            checkpoint.rss_sleds,
            checkpoint.workload.clone(),
            checkpoint.oxide_session.clone(),
            &[],
            checkpoint.repeat,
        );
        for (field, value) in [
            (&mut evidence.provenance.voxel_build, "voxel-build"),
            (&mut evidence.provenance.voxel_binary, "sha256:voxel-binary"),
            (&mut evidence.provenance.configured_image, "configured-image"),
            (&mut evidence.provenance.omicron_commit, "omicron-commit"),
            (&mut evidence.provenance.host, "host-id"),
        ] {
            *field = EvidenceValue::Available { value: value.into() };
        }
        checkpoint.report_evidence = Some(evidence);
        checkpoint
    }

    #[test]
    fn cookout_adapter_storage_cohort_ignores_results_status_and_metrics() {
        let checkpoint = checkpoint_with_complete_cohort_evidence();
        let expected = storage_cohort(&checkpoint, Path::new("same.json"));

        let mut changed = checkpoint.clone();
        let evidence = changed.report_evidence.as_mut().unwrap();
        evidence.capabilities.api_disk_lifecycle =
            CapabilityStatus::Fail { evidence: "irrelevant result".into() };
        if let LaunchOutcome::Success { metrics, .. } =
            &mut changed.combos[0].repeats[0].launch
        {
            metrics.launch_secs += 1;
            metrics.bringup_bytes += 1;
        }
        changed.status = RunStatus::Running;
        assert_eq!(storage_cohort(&changed, Path::new("same.json")), expected);
    }

    #[test]
    fn cookout_adapter_storage_cohort_splits_on_every_identity_fact() {
        let checkpoint = checkpoint_with_complete_cohort_evidence();
        let expected = storage_cohort(&checkpoint, Path::new("same.json"));
        let assert_split =
            |name: &str, mutate: &mut dyn FnMut(&mut MatrixCheckpoint)| {
                let mut changed = checkpoint.clone();
                mutate(&mut changed);
                assert_ne!(
                    storage_cohort(&changed, Path::new("same.json")),
                    expected,
                    "{name}"
                );
            };

        assert_split("RSS sled count", &mut |value| value.rss_sleds += 1);
        assert_split("ordered combination labels", &mut |value| {
            value.combos[0].label = "different".into()
        });
        assert_split("workload", &mut |value| {
            value.workload.as_mut().unwrap().count += 1
        });
        assert_split("full Oxide session", &mut |value| {
            value.oxide_session = Some(OxideSessionMetadata {
                profile: "profile".into(),
                host: "host".into(),
                provider: OxideAuthProviderMetadata::Helper {
                    path: PathBuf::from("/helper"),
                },
                oxide_cli_version: "cli-version".into(),
            })
        });
        assert_split("effective candidate configuration", &mut |value| {
            value.report_evidence.as_mut().unwrap().combos[0]
                .effective_config
                .disk_wear
                .host_sync_disabled = true
        });
        assert_split("capability contract version", &mut |value| {
            value.report_evidence.as_mut().unwrap().evidence_version += 1
        });
        for (name, field) in [
            ("Voxel build", 0),
            ("Voxel binary digest", 1),
            ("configured image", 2),
            ("Omicron commit", 3),
            ("host", 4),
        ] {
            assert_split(name, &mut |value| {
                let provenance =
                    &mut value.report_evidence.as_mut().unwrap().provenance;
                let target = match field {
                    0 => &mut provenance.voxel_build,
                    1 => &mut provenance.voxel_binary,
                    2 => &mut provenance.configured_image,
                    3 => &mut provenance.omicron_commit,
                    _ => &mut provenance.host,
                };
                *target = EvidenceValue::Available {
                    value: format!("different-{field}"),
                };
            });
        }
        assert!(expected.starts_with("v1:sha256:"));
        assert_eq!(expected.len(), "v1:sha256:".len() + 64);
    }

    #[test]
    fn cookout_adapter_partial_provenance_uses_source_and_run_identity() {
        let mut checkpoint = checkpoint_with_complete_cohort_evidence();
        checkpoint.report_evidence.as_mut().unwrap().provenance.host =
            EvidenceValue::Unavailable { reason: "not observable".into() };
        let first = storage_cohort(&checkpoint, Path::new("first/input.json"));
        assert_ne!(
            first,
            storage_cohort(&checkpoint, Path::new("second/input.json"))
        );
        checkpoint.name = "another-run".into();
        assert_ne!(
            first,
            storage_cohort(&checkpoint, Path::new("first/input.json"))
        );
    }

    #[test]
    fn cookout_adapter_preserves_completed_matrix_with_failed_repeats() {
        let mut checkpoint = schema_v5_fixture("matrix-partial-v5.json");
        checkpoint.status = RunStatus::Completed;
        checkpoint.ended = Some(checkpoint.updated);
        checkpoint.repeat = 1;
        checkpoint.combos[0].repeats.truncate(1);

        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &checkpoint,
            Path::new("completed-with-failures-v5.json"),
        )
        .unwrap();
        let context = document.report_context.as_ref().unwrap();
        assert_eq!(
            context.execution_state,
            cookout::model::ExecutionState::Completed
        );
        assert_eq!(context.abort_error, None);
        assert!(
            document
                .runs
                .iter()
                .any(|run| run.outcome == cookout::model::RunOutcome::Failed)
        );
    }

    fn cookout_golden(name: &str, document: &cookout::ExperimentDocument) {
        let mut target_dimensions = document.target.dimensions.clone();
        let cohort = target_dimensions
            .namespaced
            .remove("oxide.voxel.storage_cohort")
            .expect("storage cohort dimension");
        assert!(matches!(
            cohort,
            cookout::model::DimensionValue::Text(value)
                if value.starts_with("v1:sha256:")
                    && value.len() == "v1:sha256:".len() + 64
        ));
        let actual = json!({
            "variants": document.variants.iter().map(|variant| &variant.id).collect::<Vec<_>>(),
            "runs": document.runs.iter().map(|run| json!({
                "id": run.id,
                "outcome": run.outcome,
                "phases": run.phases.iter().map(|phase| json!({
                    "name": phase.name,
                    "status": phase.status,
                    "metrics": phase.observations.iter().map(|observation| &observation.metric).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "guardrail": run.guardrail,
            })).collect::<Vec<_>>(),
            // The opaque cohort is covered independently because its digest deliberately
            // changes whenever the cohort identity contract changes.
            "target_dimensions": target_dimensions,
            "constraints": document.target.constraints,
            "extensions": document.target.extensions,
        });
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/perftest")
            .join(name);
        let expected: Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn cookout_adapter_checkpoint_goldens_are_deterministic_and_truthful() {
        for (source, golden) in [
            ("matrix-complete-v5.json", "cookout-complete.json"),
            ("matrix-partial-v5.json", "cookout-partial.json"),
            ("matrix-failed-v5.json", "cookout-failed.json"),
            ("checkpoint-in-progress-v5.json", "cookout-checkpoint.json"),
        ] {
            let document = cookout_adapter::matrix_checkpoint_to_experiment(
                &schema_v5_fixture(source),
                Path::new(source),
            )
            .unwrap();
            cookout_golden(golden, &document);
            let serialized = serde_json::to_vec(&document).unwrap();
            let text =
                String::from_utf8(serialized).unwrap().to_ascii_lowercase();
            for forbidden in ["password", "bearer "] {
                assert!(
                    !text.contains(forbidden),
                    "Cookout evidence leaked {forbidden}"
                );
            }
            assert!(document.runs.iter().all(|run| {
                run.outcome != cookout::model::RunOutcome::GuardrailAborted
                    && run.guardrail.is_none()
            }));
        }
    }

    #[test]
    fn cookout_adapter_preserves_typed_report_context() {
        let cases = [
            (
                "matrix-complete-v5.json",
                cookout::model::ExecutionState::Completed,
                1,
                cookout::model::WorkloadDisposition::Succeeded,
                None,
            ),
            (
                "matrix-partial-v5.json",
                cookout::model::ExecutionState::Running,
                2,
                cookout::model::WorkloadDisposition::Failed,
                Some("synthetic workload request failed"),
            ),
            (
                "matrix-failed-v5.json",
                cookout::model::ExecutionState::Failed,
                2,
                cookout::model::WorkloadDisposition::Blocked,
                Some(
                    "synthetic launch attempt one failed; synthetic launch attempt two failed",
                ),
            ),
            (
                "checkpoint-in-progress-v5.json",
                cookout::model::ExecutionState::Running,
                1,
                cookout::model::WorkloadDisposition::NotRequested,
                Some("synthetic launch attempt is awaiting cleanup"),
            ),
        ];
        for (source, state, planned, disposition, failure) in cases {
            let document = cookout_adapter::matrix_checkpoint_to_experiment(
                &schema_v5_fixture(source),
                Path::new(source),
            )
            .unwrap();
            let context = document.report_context.as_ref().unwrap();
            assert_eq!(context.execution_state, state, "{source}");
            assert_eq!(context.planned_repeats, planned, "{source}");
            assert_eq!(
                context.source_display.as_deref(),
                Some(source),
                "{source}"
            );
            assert!(document.variants.iter().all(|variant| {
                variant
                    .report_context
                    .as_ref()
                    .is_some_and(|context| !context.conditions.is_empty())
            }));
            let run = &document.runs[0];
            let run_context = run.report_context.as_ref().unwrap();
            assert_eq!(
                run_context.workload_disposition, disposition,
                "{source}"
            );
            assert_eq!(
                run.failure.as_ref().map(|record| record.message.as_str()),
                failure
            );
            assert!(
                document
                    .variants
                    .iter()
                    .flat_map(|variant| &variant
                        .report_context
                        .as_ref()
                        .unwrap()
                        .conditions)
                    .all(|condition| !condition
                        .label
                        .to_ascii_lowercase()
                        .contains("password"))
            );
        }
    }

    #[test]
    fn cookout_adapter_maps_matrix_run_repeats() {
        let run = run_with(
            "ignored-source-name",
            &[("none", &[], &[10, 20]), ("1", &[1], &[8, 9])],
        );
        let document = cookout_adapter::matrix_run_to_experiment(
            &run,
            Path::new("matrix.json"),
        )
        .unwrap();
        assert_eq!(document.variants.len(), 2);
        assert_eq!(document.runs.len(), 4);
        assert!(document
            .runs
            .iter()
            .all(|run| run.outcome == cookout::model::RunOutcome::Completed));
        assert_eq!(document.identity.name, "Voxel");
        assert_eq!(
            document.report_context.as_ref().unwrap().source_display,
            Some("matrix.json".into())
        );
    }

    #[test]
    fn cookout_adapter_preserves_schema_v4_partial_aggregate_failure() {
        let mut run = run_with("partial", &[("none", &[], &[10, 20])]);
        run.repeat = 3;
        run.results[0].error = Some("third repeat failed".into());

        let document = cookout_adapter::matrix_run_to_experiment(
            &run,
            Path::new("matrix.json"),
        )
        .unwrap();
        let context = document.report_context.as_ref().unwrap();
        assert_eq!(
            context.execution_state,
            cookout::model::ExecutionState::Failed
        );
        assert_eq!(context.planned_repeats, 3);
        assert_eq!(
            document.variants[0]
                .report_context
                .as_ref()
                .unwrap()
                .expected_repeats,
            3
        );
        assert_eq!(document.runs.len(), 3);
        assert_eq!(document.runs[2].attempt, 3);
        assert_eq!(
            document.runs[2].outcome,
            cookout::model::RunOutcome::Failed
        );
        assert_eq!(
            document.runs[2].report_context.as_ref().unwrap().launch_failure,
            None
        );
        assert_eq!(
            document.runs[2].failure.as_ref().unwrap().message,
            "third repeat failed"
        );
    }

    #[test]
    fn cookout_adapter_does_not_invent_schema_v4_failed_slots() {
        let mut run = run_with("failed", &[("none", &[], &[])]);
        run.repeat = 3;
        run.results[0].error = Some("first attempt failed".into());

        let document = cookout_adapter::matrix_run_to_experiment(
            &run,
            Path::new("matrix.json"),
        )
        .unwrap();
        assert_eq!(
            document.report_context.as_ref().unwrap().planned_repeats,
            3
        );
        assert_eq!(document.runs.len(), 1);
        assert_eq!(document.runs[0].attempt, 1);
        assert_eq!(
            document.runs[0].outcome,
            cookout::model::RunOutcome::Failed
        );
    }

    #[test]
    fn cookout_adapter_preserves_partial_provenance_without_credentials() {
        let mut checkpoint = schema_v5_fixture("matrix-complete-v5.json");
        let plan = checkpoint
            .combos
            .iter()
            .map(|combo| (combo.label.clone(), combo.levers.clone()))
            .collect::<Vec<_>>();
        checkpoint.report_evidence = Some(build_report_evidence(
            &checkpoint.combos[0].effective_config,
            &plan,
            checkpoint.rss_sleds,
            checkpoint.workload.clone(),
            checkpoint.oxide_session.clone(),
            &[],
            checkpoint.repeat,
        ));
        let evidence = checkpoint.report_evidence.as_mut().unwrap();
        evidence.provenance.host =
            EvidenceValue::Unavailable { reason: "host capture failed".into() };
        checkpoint.oxide_session = Some(OxideSessionMetadata {
            profile: "synthetic-profile".into(),
            host: "distinctive-password-hash".into(),
            provider: OxideAuthProviderMetadata::Helper {
                path: PathBuf::from("/tmp/distinctive-secret-token"),
            },
            oxide_cli_version: "0.0.0-test".into(),
        });

        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &checkpoint,
            Path::new("matrix-complete-v5.json"),
        )
        .unwrap();
        let context = document.report_context.as_ref().unwrap();
        assert_eq!(
            context.provenance_state,
            cookout::model::ProvenanceState::Unavailable
        );
        assert!(
            context
                .conditions
                .iter()
                .any(|condition| condition.label == "Voxel build"
                    && condition.value != "unavailable")
        );
        assert!(
            context
                .conditions
                .iter()
                .any(|condition| condition.label == "Host"
                    && condition.value == "unavailable: host capture failed")
        );
        let serialized = serde_json::to_string(&document).unwrap();
        for forbidden in
            ["distinctive-password-hash", "distinctive-secret-token"]
        {
            assert!(
                !serialized.contains(forbidden),
                "Cookout evidence leaked {forbidden}"
            );
        }
    }

    #[test]
    fn cookout_adapter_keeps_failure_stages_and_measurement_semantics_distinct()
    {
        let mut preparation_failed =
            schema_v5_fixture("matrix-partial-v5.json");
        let repeat = &mut preparation_failed.combos[0].repeats[0];
        repeat.preparation =
            PreparationOutcome::Failure { error: "preparation failed".into() };
        repeat.workload =
            WorkloadOutcome::Failure { error: "workload was blocked".into() };
        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &preparation_failed,
            Path::new("matrix-partial-v5.json"),
        )
        .unwrap();
        let context = document.runs[0].report_context.as_ref().unwrap();
        assert_eq!(
            context.preparation_failure.as_deref(),
            Some("preparation failed")
        );
        assert_eq!(context.workload_failure, None);
        assert_eq!(
            context.workload_disposition,
            cookout::model::WorkloadDisposition::Blocked
        );

        let mut cleanup_failed = schema_v5_fixture("matrix-complete-v5.json");
        cleanup_failed.status = RunStatus::Aborted;
        cleanup_failed.abort_error = Some("cleanup failed".into());
        cleanup_failed.combos[0].repeats[0].post_boundary =
            BoundaryOutcome::Failure { error: "cleanup failed".into() };
        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &cleanup_failed,
            Path::new("matrix-complete-v5.json"),
        )
        .unwrap();
        let context = document.runs[0].report_context.as_ref().unwrap();
        assert_eq!(context.boundary_failure.as_deref(), Some("cleanup failed"));
        assert_eq!(
            context.launch_memory_semantics.as_deref(),
            Some("launch-baseline-delta")
        );
        assert_eq!(
            context.workload_memory_semantics.as_deref(),
            Some("workload-baseline-delta")
        );
    }

    #[test]
    fn cookout_adapter_accepts_independent_boundary_failures() {
        let mut launch_only = schema_v5_fixture("matrix-complete-v5.json");
        launch_only.status = RunStatus::Aborted;
        launch_only.abort_error = Some("cleanup failed".into());
        launch_only.workload = None;
        let repeat = &mut launch_only.combos[0].repeats[0];
        repeat.preparation = PreparationOutcome::NotRequested {};
        repeat.workload = WorkloadOutcome::NotRequested {};
        repeat.post_boundary =
            BoundaryOutcome::Failure { error: "cleanup failed".into() };
        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &launch_only,
            Path::new("launch-only.json"),
        )
        .unwrap();
        let context = document.runs[0].report_context.as_ref().unwrap();
        assert_eq!(
            context.workload_disposition,
            cookout::model::WorkloadDisposition::NotRequested
        );
        assert_eq!(context.boundary_failure.as_deref(), Some("cleanup failed"));

        let mut workload_failed = schema_v5_fixture("matrix-complete-v5.json");
        workload_failed.status = RunStatus::Aborted;
        workload_failed.abort_error =
            Some("workload and cleanup failed".into());
        let repeat = &mut workload_failed.combos[0].repeats[0];
        repeat.workload =
            WorkloadOutcome::Failure { error: "workload failed".into() };
        repeat.post_boundary =
            BoundaryOutcome::Failure { error: "cleanup failed".into() };
        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &workload_failed,
            Path::new("workload-failed.json"),
        )
        .unwrap();
        let context = document.runs[0].report_context.as_ref().unwrap();
        assert_eq!(
            context.workload_disposition,
            cookout::model::WorkloadDisposition::Failed
        );
        assert_eq!(
            context.workload_failure.as_deref(),
            Some("workload failed")
        );
        assert_eq!(context.boundary_failure.as_deref(), Some("cleanup failed"));
    }

    #[test]
    fn cookout_adapter_redacts_sensitive_diagnostics_without_losing_the_state()
    {
        let mut checkpoint = schema_v5_fixture("matrix-failed-v5.json");
        checkpoint.abort_error = Some("Bearer distinctive-secret-token".into());
        let LaunchOutcome::Failure { attempt_failures } =
            &mut checkpoint.combos[0].repeats[0].launch
        else {
            panic!("failed fixture must contain a failed launch")
        };
        attempt_failures[0].error =
            "$argon2id$distinctive-password-hash".into();

        let document = cookout_adapter::matrix_checkpoint_to_experiment(
            &checkpoint,
            Path::new("matrix-failed-v5.json"),
        )
        .unwrap();
        let serialized = serde_json::to_string(&document).unwrap();
        for forbidden in
            ["distinctive-secret-token", "distinctive-password-hash", "bearer "]
        {
            assert!(
                !serialized.to_ascii_lowercase().contains(forbidden),
                "Cookout evidence leaked {forbidden}"
            );
        }
        assert_eq!(
            document.report_context.as_ref().unwrap().execution_state,
            cookout::model::ExecutionState::Failed
        );
        assert!(
            document.runs[0]
                .report_context
                .as_ref()
                .unwrap()
                .launch_failure
                .as_deref()
                .is_some_and(|message| message.contains("redacted"))
        );
    }

    #[test]
    fn schema_v5_parity_fixtures_capture_durable_stage_semantics() {
        let complete = schema_v5_fixture("matrix-complete-v5.json");
        assert_eq!(complete.status, RunStatus::Completed);
        assert_eq!(complete.name, "synthetic-rack");
        assert!(matches!(
            complete.combos[0].repeats[0].launch,
            LaunchOutcome::Success {
                metrics: LaunchMetrics {
                    bringup_bytes: 42_000,
                    launch_secs: 120,
                    peak_ram_bytes: 8_388_608,
                },
                ..
            }
        ));
        assert!(matches!(
            complete.combos[0].repeats[0].workload,
            WorkloadOutcome::Success {
                metrics: WorkloadMetrics {
                    workload_bytes: 84_000,
                    workload_secs: 60,
                    workload_peak_delta_bytes: 4_194_304,
                }
            }
        ));

        let partial = schema_v5_fixture("matrix-partial-v5.json");
        assert_eq!(partial.status, RunStatus::Running);
        assert!(matches!(
            partial.combos[0].repeats[1].launch,
            LaunchOutcome::Pending {}
        ));

        let failed = schema_v5_fixture("matrix-failed-v5.json");
        assert_eq!(failed.status, RunStatus::Aborted);
        assert!(matches!(
            failed.combos[0].repeats[0].launch,
            LaunchOutcome::Failure { ref attempt_failures }
                if attempt_failures.len() == MATRIX_REPEAT_ATTEMPTS
        ));
        assert!(matches!(
            failed.combos[1].repeats[0].post_boundary,
            BoundaryOutcome::Failure { .. }
        ));

        let checkpoint = schema_v5_fixture("checkpoint-in-progress-v5.json");
        assert_eq!(checkpoint.status, RunStatus::Running);
        assert!(matches!(
            checkpoint.combos[0].repeats[0].launch,
            LaunchOutcome::Failure { ref attempt_failures }
                if matches!(attempt_failures[0].clean_boundary, BoundaryOutcome::Pending {})
        ));
    }

    #[test]
    fn schema_v5_parity_fixtures_are_synthetic_and_sanitized() {
        for name in [
            "matrix-complete-v5.json",
            "matrix-partial-v5.json",
            "matrix-failed-v5.json",
            "checkpoint-in-progress-v5.json",
        ] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/perftest")
                .join(name);
            let raw = std::fs::read_to_string(path).unwrap();
            let lowercase = raw.to_ascii_lowercase();
            assert!(raw.contains("synthetic-rack"));
            for forbidden in
                ["password_hash", "token", "secret", "recovery.sys"]
            {
                assert!(
                    !lowercase.contains(forbidden),
                    "{name} contains {forbidden}"
                );
            }
        }
    }

    #[test]
    fn schema_v5_checkpoint_serializes_strict_planned_snapshot() {
        let mut checkpoint = planned_schema_v5_checkpoint();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        CheckpointPublisher::new(&destination, None)
            .publish(&mut checkpoint)
            .unwrap();

        let value: Value =
            serde_json::from_slice(&std::fs::read(destination).unwrap())
                .unwrap();
        assert_eq!(value["schema_version"], 5);
        assert_eq!(value["checkpoint_sequence"], 1);
        assert_eq!(value["status"], "running");
        assert_eq!(value["ended"], Value::Null);
        assert_eq!(value["combos"][0]["repeats"].as_array().unwrap().len(), 3);
        assert_eq!(
            value["combos"][0]["repeats"][0]["pre_boundary"]["status"],
            "pending"
        );
        assert_eq!(
            value["combos"][0]["repeats"][0]["launch"]["status"],
            "pending"
        );
        assert_eq!(
            value["combos"][0]["repeats"][0]["workload"]["status"],
            "not_requested"
        );
        assert!(serde_json::from_value::<MatrixCheckpoint>(value).is_ok());
    }

    #[test]
    fn schema_v5_checkpoint_initial_publish_refuses_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        std::fs::write(&destination, b"someone else's output").unwrap();
        let mut checkpoint = planned_schema_v5_checkpoint();

        assert!(
            CheckpointPublisher::new(&destination, None)
                .publish(&mut checkpoint)
                .is_err()
        );
        assert_eq!(
            std::fs::read(destination).unwrap(),
            b"someone else's output"
        );
        assert_eq!(checkpoint.checkpoint_sequence, 0);
    }

    #[test]
    fn schema_v5_checkpoint_initial_pre_install_failure_leaves_destination_absent()
     {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        let mut publisher = CheckpointPublisher::new(&destination, None);
        let mut checkpoint = planned_schema_v5_checkpoint();

        publisher.fail_before_initial_install = true;
        assert!(publisher.publish(&mut checkpoint).is_err());

        assert!(!destination.exists());
        assert_eq!(checkpoint.checkpoint_sequence, 0);
    }

    #[test]
    fn schema_v5_checkpoint_parent_sync_failure_returns_error_after_visible_install()
     {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        let mut publisher = CheckpointPublisher::new(&destination, None);
        let mut checkpoint = planned_schema_v5_checkpoint();
        publisher.fail_parent_sync = true;

        let error = publisher.publish(&mut checkpoint).unwrap_err().to_string();

        assert!(
            error.contains("complete checkpoint was installed and is visible")
        );
        assert!(error.contains("parent sync failed"));
        assert!(error.contains("durability is uncertain"));
        assert!(error.contains("execution must stop"));
        let persisted: MatrixCheckpoint =
            serde_json::from_slice(&std::fs::read(destination).unwrap())
                .unwrap();
        assert_eq!(persisted.checkpoint_sequence, 1);
        assert_eq!(checkpoint.checkpoint_sequence, 0);
    }

    #[test]
    fn schema_v5_checkpoint_replacement_increments_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        let mut publisher = CheckpointPublisher::new(&destination, None);
        let mut checkpoint = planned_schema_v5_checkpoint();

        publisher.publish(&mut checkpoint).unwrap();
        publisher.publish(&mut checkpoint).unwrap();

        let persisted: MatrixCheckpoint =
            serde_json::from_slice(&std::fs::read(destination).unwrap())
                .unwrap();
        assert_eq!(persisted.checkpoint_sequence, 2);
        assert_eq!(checkpoint.checkpoint_sequence, 2);
    }

    #[test]
    fn schema_v5_checkpoint_hands_off_the_open_inode() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        let parent = std::fs::metadata(directory.path()).unwrap();
        let owner = FileOwner { uid: parent.uid(), gid: parent.gid() };
        let mut publisher = CheckpointPublisher::new(&destination, Some(owner));
        let mut checkpoint = planned_schema_v5_checkpoint();

        publisher.publish(&mut checkpoint).unwrap();
        let installed = std::fs::metadata(&destination).unwrap();
        assert_eq!(installed.uid(), owner.uid);
        assert_eq!(installed.gid(), owner.gid);
        assert_eq!(installed.mode() & 0o777, 0o600);

        publisher.publish(&mut checkpoint).unwrap();

        let installed = std::fs::metadata(destination).unwrap();
        assert_eq!(installed.uid(), owner.uid);
        assert_eq!(installed.gid(), owner.gid);
        assert_eq!(installed.mode() & 0o777, 0o600);
    }

    #[test]
    fn schema_v5_checkpoint_rejects_unknown_fields() {
        let mut value =
            serde_json::to_value(planned_schema_v5_checkpoint()).unwrap();
        value["unexpected"] = json!(true);
        assert!(serde_json::from_value::<MatrixCheckpoint>(value).is_err());

        let mut value =
            serde_json::to_value(planned_schema_v5_checkpoint()).unwrap();
        value["combos"][0]["repeats"][0]["launch"]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<MatrixCheckpoint>(value).is_err());

        macro_rules! assert_rejects_unknown_field {
            ($outcome:ty, $($value:expr),+ $(,)?) => {
                $(assert!(serde_json::from_value::<$outcome>($value).is_err());)+
            };
        }
        assert_rejects_unknown_field!(
            BoundaryOutcome,
            json!({"status":"pending","unexpected":true}),
            json!({"status":"clean","unexpected":true}),
        );
        assert_rejects_unknown_field!(
            LaunchOutcome,
            json!({"status":"pending","unexpected":true}),
        );
        assert_rejects_unknown_field!(
            WorkloadOutcome,
            json!({"status":"pending","unexpected":true}),
            json!({"status":"not_requested","unexpected":true}),
        );
        assert_rejects_unknown_field!(
            PreparationOutcome,
            json!({"status":"pending","unexpected":true}),
            json!({"status":"not_requested","unexpected":true}),
            json!({"status":"success","unexpected":true}),
        );

        let strict_variants = [
            serde_json::to_value(BoundaryOutcome::Pending {}).unwrap(),
            serde_json::to_value(BoundaryOutcome::Clean {}).unwrap(),
            serde_json::to_value(LaunchOutcome::Pending {}).unwrap(),
            serde_json::to_value(WorkloadOutcome::Pending {}).unwrap(),
            serde_json::to_value(WorkloadOutcome::NotRequested {}).unwrap(),
            serde_json::to_value(PreparationOutcome::Pending {}).unwrap(),
            serde_json::to_value(PreparationOutcome::NotRequested {}).unwrap(),
            serde_json::to_value(PreparationOutcome::Success {}).unwrap(),
        ];
        assert_eq!(
            strict_variants,
            [
                json!({"status":"pending"}),
                json!({"status":"clean"}),
                json!({"status":"pending"}),
                json!({"status":"pending"}),
                json!({"status":"not_requested"}),
                json!({"status":"pending"}),
                json!({"status":"not_requested"}),
                json!({"status":"success"}),
            ]
        );
    }

    #[test]
    fn schema_v5_checkpoint_pre_rename_failure_preserves_prior_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        let mut publisher = CheckpointPublisher::new(&destination, None);
        let mut checkpoint = planned_schema_v5_checkpoint();
        publisher.publish(&mut checkpoint).unwrap();
        let prior = std::fs::read(&destination).unwrap();

        publisher.fail_before_rename = true;
        assert!(publisher.publish(&mut checkpoint).is_err());

        assert_eq!(std::fs::read(destination).unwrap(), prior);
        assert_eq!(checkpoint.checkpoint_sequence, 1);
    }

    #[test]
    fn schema_v5_checkpoint_refuses_recreated_owned_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.json");
        let mut publisher = CheckpointPublisher::new(&destination, None);
        let mut checkpoint = planned_schema_v5_checkpoint();
        publisher.publish(&mut checkpoint).unwrap();
        std::fs::remove_file(&destination).unwrap();
        std::fs::write(&destination, b"replacement owned by someone else")
            .unwrap();

        assert!(publisher.publish(&mut checkpoint).is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"replacement owned by someone else"
        );
        assert_eq!(checkpoint.checkpoint_sequence, 1);
    }

    #[test]
    fn schema_v5_checkpoint_rejects_other_schema_versions() {
        for version in [0, 4, 6] {
            let mut value =
                serde_json::to_value(planned_schema_v5_checkpoint()).unwrap();
            value["schema_version"] = json!(version);
            assert!(serde_json::from_value::<MatrixCheckpoint>(value).is_err());
        }
    }

    #[test]
    fn schema_v5_checkpoint_success_requires_peak_memory_metrics() {
        let launch = json!({
            "status": "success",
            "metrics": { "bringup_bytes": 1, "launch_secs": 2 }
        });
        assert!(serde_json::from_value::<LaunchOutcome>(launch).is_err());

        let workload = json!({
            "status": "success",
            "metrics": { "workload_bytes": 1, "workload_secs": 2 }
        });
        assert!(serde_json::from_value::<WorkloadOutcome>(workload).is_err());
    }

    #[tokio::test]
    async fn checkpointed_repeat_publishes_launch_before_failed_workload_without_retry()
     {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        let events = RefCell::new(Vec::new());
        let launches = Cell::new(0);
        checkpointed_repeat_with::<(), (), _, _, _, _, _, _, _, _, _>(
            &mut repeat,
            true,
            |_| {
                events.borrow_mut().push("publish");
                Ok(())
            },
            || async {
                events.borrow_mut().push("boundary");
                Ok(())
            },
            || async {
                launches.set(launches.get() + 1);
                events.borrow_mut().push("launch");
                Ok((
                    LaunchMetrics {
                        bringup_bytes: 1,
                        launch_secs: 2,
                        peak_ram_bytes: 3,
                    },
                    (),
                ))
            },
            |value| async move { Ok(value) },
            |_| async {
                events.borrow_mut().push("workload");
                Err(anyhow!("workload boom"))
            },
        )
        .await
        .unwrap();

        assert_eq!(launches.get(), 1);
        assert!(matches!(repeat.launch, LaunchOutcome::Success { .. }));
        assert!(matches!(repeat.workload, WorkloadOutcome::Failure { .. }));
        assert!(matches!(repeat.post_boundary, BoundaryOutcome::Clean {}));
        assert_eq!(
            *events.borrow(),
            [
                "boundary", "publish", "launch", "publish", "publish",
                "workload", "publish", "boundary", "publish"
            ]
        );
    }

    #[tokio::test]
    async fn checkpointed_repeat_preparation_failure_blocks_workload_and_is_published()
     {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        repeat.preparation = PreparationOutcome::Pending {};
        repeat.workload = WorkloadOutcome::Pending {};
        let publications = RefCell::new(Vec::new());
        let workload_calls = Cell::new(0);
        checkpointed_repeat_with::<(), (), _, _, _, _, _, _, _, _, _>(
            &mut repeat,
            true,
            |repeat| {
                publications.borrow_mut().push((
                    repeat.preparation.clone(),
                    repeat.workload.clone(),
                    repeat.post_boundary.clone(),
                ));
                Ok(())
            },
            || async { Ok(()) },
            || async {
                Ok((
                    LaunchMetrics {
                        bringup_bytes: 1,
                        launch_secs: 2,
                        peak_ram_bytes: 3,
                    },
                    (),
                ))
            },
            |_| async { Err(anyhow!("zpool inventory missing")) },
            |_| async {
                workload_calls.set(workload_calls.get() + 1);
                unreachable!()
            },
        )
        .await
        .unwrap();

        assert_eq!(workload_calls.get(), 0);
        assert!(matches!(
            repeat.preparation,
            PreparationOutcome::Failure { .. }
        ));
        assert!(
            matches!(repeat.workload, WorkloadOutcome::Failure { ref error } if error.contains("blocked by simulated zpool preparation failure"))
        );
        let preparation_failure_publications = publications
            .borrow()
            .iter()
            .filter(|(preparation, _, _)| {
                matches!(preparation, PreparationOutcome::Failure { .. })
            })
            .cloned()
            .collect::<Vec<_>>();
        assert!(matches!(
            preparation_failure_publications.as_slice(),
            [
                (
                    PreparationOutcome::Failure { .. },
                    WorkloadOutcome::Pending {},
                    BoundaryOutcome::Pending {}
                ),
                (
                    PreparationOutcome::Failure { .. },
                    WorkloadOutcome::Failure { .. },
                    BoundaryOutcome::Pending {}
                ),
                (
                    PreparationOutcome::Failure { .. },
                    WorkloadOutcome::Failure { .. },
                    BoundaryOutcome::Clean {}
                ),
            ]
        ));
    }

    #[tokio::test]
    async fn checkpointed_repeat_retries_launch_only_after_clean_teardown() {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        let events = RefCell::new(Vec::new());
        let attempts = Cell::new(0);
        checkpointed_repeat_with::<(), (), _, _, _, _, _, _, _, _, _>(
            &mut repeat,
            false,
            |_| {
                events.borrow_mut().push("publish");
                Ok(())
            },
            || async {
                events.borrow_mut().push("boundary");
                Ok(())
            },
            || async {
                attempts.set(attempts.get() + 1);
                events.borrow_mut().push("launch");
                if attempts.get() == 1 {
                    Err(anyhow!("first"))
                } else {
                    Ok((
                        LaunchMetrics {
                            bringup_bytes: 1,
                            launch_secs: 2,
                            peak_ram_bytes: 3,
                        },
                        (),
                    ))
                }
            },
            |value| async move { Ok(value) },
            |_| async { unreachable!() },
        )
        .await
        .unwrap();
        assert_eq!(attempts.get(), 2);
        assert_eq!(
            *events.borrow(),
            [
                "boundary", "publish", "launch", "publish", "boundary",
                "publish", "launch", "publish", "publish", "boundary",
                "publish"
            ]
        );
        assert!(matches!(repeat.launch, LaunchOutcome::Success { .. }));
        let LaunchOutcome::Success { prior_attempt_failures, .. } =
            repeat.launch
        else {
            unreachable!()
        };
        assert_eq!(prior_attempt_failures.len(), 1);
        assert_eq!(prior_attempt_failures[0].error, "first");
        assert_eq!(
            prior_attempt_failures[0].clean_boundary,
            BoundaryOutcome::Clean {}
        );
        assert_eq!(repeat.post_boundary, BoundaryOutcome::Clean {});
    }

    #[tokio::test]
    async fn unavailable_external_dns_retains_launch_only_evidence() {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        let outcome =
            checkpointed_repeat_with::<bool, (), _, _, _, _, _, _, _, _, _>(
                &mut repeat,
                true,
                |_| Ok(()),
                || async { Ok(()) },
                || async {
                    Ok((
                        LaunchMetrics {
                            bringup_bytes: 1,
                            launch_secs: 2,
                            peak_ram_bytes: 3,
                        },
                        false,
                    ))
                },
                |external_reachable| async move { Ok(external_reachable) },
                |external_reachable| async move {
                    require_workload_external_dns(external_reachable)?;
                    unreachable!(
                        "DNS-unreachable repeat must not run the workload"
                    )
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome.launch_data, Some(false));
        assert!(matches!(repeat.launch, LaunchOutcome::Success { .. }));
        assert_eq!(repeat.preparation, PreparationOutcome::Success {});
        let WorkloadOutcome::Failure { error } = repeat.workload else {
            panic!("DNS-unreachable repeat must record a workload failure")
        };
        assert!(error.contains("retaining launch-only evidence"), "{error}");
        assert_eq!(repeat.post_boundary, BoundaryOutcome::Clean {});
    }

    #[tokio::test]
    async fn checkpointed_repeat_stops_immediately_after_publication_failure() {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        let publications = Cell::new(0);
        let boundaries = Cell::new(0);
        let launches = Cell::new(0);
        let error =
            checkpointed_repeat_with::<(), (), _, _, _, _, _, _, _, _, _>(
                &mut repeat,
                false,
                |_| {
                    publications.set(publications.get() + 1);
                    Err(anyhow::Error::new(PublicationError(anyhow!(
                        "disk full"
                    ))))
                },
                || async {
                    boundaries.set(boundaries.get() + 1);
                    Ok(())
                },
                || async {
                    launches.set(launches.get() + 1);
                    unreachable!()
                },
                |value| async move { Ok(value) },
                |_| async { unreachable!() },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disk full"));
        assert_eq!(publications.get(), 1);
        assert_eq!(boundaries.get(), 1);
        assert_eq!(launches.get(), 0);
    }

    #[tokio::test]
    async fn checkpointed_repeat_final_launch_exhaustion_has_terminal_clean_boundary()
     {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        let boundaries = Cell::new(0);
        let outcome =
            checkpointed_repeat_with::<(), (), _, _, _, _, _, _, _, _, _>(
                &mut repeat,
                false,
                |_| Ok(()),
                || async {
                    boundaries.set(boundaries.get() + 1);
                    Ok(())
                },
                || async { Err(anyhow!("launch failed")) },
                |value| async move { Ok(value) },
                |_| async { unreachable!() },
            )
            .await
            .unwrap();

        assert!(outcome.launch_data.is_none());
        assert_eq!(
            boundaries.get(),
            3,
            "pre-boundary plus one cleanup per attempt"
        );
        assert_eq!(repeat.post_boundary, BoundaryOutcome::Clean {});
    }

    #[test]
    fn aborted_publication_classification_preserves_publication_downcast() {
        assert!(may_publish_aborted(&anyhow!("operational failure")));
        let publication =
            anyhow::Error::new(PublicationError(anyhow!("disk full")))
                .context("repeat publication");
        assert!(!may_publish_aborted(&publication));
    }

    #[test]
    fn final_csv_failure_cannot_reclassify_completed_checkpoint_as_aborted() {
        let mut checkpoint = planned_schema_v5_checkpoint();
        checkpoint.status = RunStatus::Completed;
        checkpoint.ended = Some(101);

        assert!(
            publish_final_csv_with(&checkpoint, || Err(anyhow!(
                "CSV disk full"
            )))
            .is_err()
        );
        assert_eq!(checkpoint.status, RunStatus::Completed);
        assert_eq!(checkpoint.ended, Some(101));
    }

    #[test]
    fn runtime_evidence_update_preserves_initial_provenance_and_configs() {
        let checkpoint = planned_schema_v5_checkpoint();
        let base = checkpoint.combos[0].effective_config.clone();
        let plan = vec![(checkpoint.combos[0].label.clone(), BTreeSet::new())];
        let mut evidence = build_report_evidence(
            &base,
            &plan,
            3,
            Some(WorkloadSpec::api_disk_lifecycle()),
            None,
            &[],
            1,
        );
        let immutable = (
            evidence.provenance.clone(),
            evidence.base_config.clone(),
            evidence.combos.clone(),
        );
        update_report_evidence_runtime(
            &mut evidence,
            Some(&WorkloadSpec::api_disk_lifecycle()),
            None,
            &[],
            1,
        );
        assert_eq!(
            immutable,
            (evidence.provenance, evidence.base_config, evidence.combos)
        );
    }

    #[test]
    fn running_checkpoint_publication_refreshes_capabilities_from_stages() {
        let mut checkpoint = planned_schema_v5_checkpoint();
        let plan = vec![("none".to_string(), BTreeSet::new())];
        let mut evidence = build_report_evidence(
            &VoxelConfig::default(),
            &plan,
            3,
            None,
            None,
            &[],
            checkpoint.repeat,
        );
        checkpoint.combos[0].effective_config =
            evidence.combos[0].effective_config.clone();
        evidence.capabilities.matrix_host_storage_scope =
            capability_pass("fabricated");
        checkpoint.report_evidence = Some(evidence);

        publish_checkpoint(&mut None, &mut checkpoint).unwrap();

        let capabilities =
            &checkpoint.report_evidence.as_ref().unwrap().capabilities;
        assert_eq!(
            capabilities.matrix_host_storage_scope,
            capability_unavailable("matrix scope has not yet been sampled")
        );
        assert!(matches!(
            capabilities.clean_launch_teardown_boundaries,
            CapabilityStatus::Unavailable { .. }
        ));
    }

    #[tokio::test]
    async fn checkpointed_repeat_aborts_on_post_boundary_failure() {
        let mut repeat =
            planned_schema_v5_checkpoint().combos.remove(0).repeats.remove(0);
        let boundaries = Cell::new(0);
        let error =
            checkpointed_repeat_with::<(), (), _, _, _, _, _, _, _, _, _>(
                &mut repeat,
                false,
                |_| Ok(()),
                || async {
                    boundaries.set(boundaries.get() + 1);
                    if boundaries.get() == 2 {
                        Err(anyhow!("dirty"))
                    } else {
                        Ok(())
                    }
                },
                || async {
                    Ok((
                        LaunchMetrics {
                            bringup_bytes: 1,
                            launch_secs: 2,
                            peak_ram_bytes: 3,
                        },
                        (),
                    ))
                },
                |value| async move { Ok(value) },
                |_| async { unreachable!() },
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("dirty"));
        assert!(matches!(
            repeat.post_boundary,
            BoundaryOutcome::Failure { ref error } if error == "dirty"
        ));
    }

    #[test]
    fn cli_parses_sample_report_and_new_report() {
        assert!(matches!(
            PerftestCli::try_parse_from([
                "test",
                "sample-report",
                "before.json",
                "after.json"
            ])
            .unwrap()
            .command,
            PerftestCmd::SampleReport { .. }
        ));

        let parsed = PerftestCli::try_parse_from([
            "test",
            "report",
            "one.json",
            "two.json",
            "--out",
            "report-dir",
            "--archive",
        ])
        .unwrap();
        match parsed.command {
            PerftestCmd::Report { inputs, out, archive } => {
                assert_eq!(
                    inputs,
                    vec![PathBuf::from("one.json"), PathBuf::from("two.json")]
                );
                assert_eq!(out, PathBuf::from("report-dir"));
                assert!(archive);
            }
            _ => panic!("expected report command"),
        }
        assert!(
            PerftestCli::try_parse_from([
                "test",
                "report",
                "--out",
                "report-dir"
            ])
            .is_err()
        );
        assert!(
            PerftestCli::try_parse_from(["test", "report", "one.json"])
                .is_err()
        );

        let parsed = PerftestCli::try_parse_from([
            "test",
            "superreport",
            "one.tar.gz",
            "two.tar.gz",
            "--out",
            "aggregate",
            "--archive",
        ])
        .unwrap();
        match parsed.command {
            PerftestCmd::Superreport { reports, out, archive } => {
                assert_eq!(
                    reports,
                    vec![
                        PathBuf::from("one.tar.gz"),
                        PathBuf::from("two.tar.gz")
                    ]
                );
                assert_eq!(out, PathBuf::from("aggregate"));
                assert!(archive);
            }
            _ => panic!("expected superreport command"),
        }
    }

    #[test]
    fn cli_accepts_only_named_api_workload() {
        assert!(
            PerftestCli::try_parse_from([
                "test",
                "matrix",
                "--workload",
                "api-disk-lifecycle"
            ])
            .is_ok()
        );
        assert!(
            PerftestCli::try_parse_from([
                "test",
                "preflight",
                "--workload",
                "api-disk-lifecycle"
            ])
            .is_ok()
        );
        assert!(
            PerftestCli::try_parse_from(["test", "matrix", "--load"]).is_err()
        );
        assert!(PerftestCli::try_parse_from(["test", "load"]).is_err());
        assert!(
            PerftestCli::try_parse_from([
                "test",
                "matrix",
                "--oxide-auth-helper",
                "/tmp/auth"
            ])
            .is_err()
        );
    }

    #[test]
    fn schema_v4_rejects_older_absolute_peak_memory_schemas() {
        assert!(
            serde_json::from_value::<MatrixRun>(matrix_json(
                2,
                serde_json::json!({"load": false})
            ))
            .is_err()
        );
        assert!(
            serde_json::from_value::<MatrixRun>(matrix_json(
                3,
                serde_json::json!({"workload": null, "oxide_session": null})
            ))
            .is_err()
        );
    }

    fn matrix_json(version: u32, fields: Value) -> Value {
        let mut value = serde_json::json!({
            "schema_version": version,
            "name": "compat",
            "started": 1,
            "ended": 2,
            "rss_sleds": 3,
            "repeat": 1,
            "combos": [],
            "results": []
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        value
    }

    #[test]
    fn matrix_schema_rejects_unsupported_and_legacy_fields() {
        assert!(
            serde_json::from_value::<MatrixRun>(matrix_json(
                1,
                serde_json::json!({"load": false})
            ))
            .is_err()
        );
        assert!(serde_json::from_value::<MatrixRun>(matrix_json(
            4,
            serde_json::json!({"load": false, "workload": null, "oxide_session": null})
        ))
        .is_err());
    }

    #[test]
    fn schema_v4_rejects_unknown_fields_at_every_wire_boundary() {
        let base =
            serde_json::to_value(run_with("strict", &[("none", &[], &[1])]))
                .unwrap();
        for path in ["run", "combo", "repeat"] {
            let mut value = base.clone();
            match path {
                "run" => value["unexpected"] = serde_json::json!(true),
                "combo" => {
                    value["results"][0]["unexpected"] = serde_json::json!(true)
                }
                "repeat" => {
                    value["results"][0]["repeats"][0]["unexpected"] =
                        serde_json::json!(true)
                }
                _ => unreachable!(),
            }
            assert!(
                serde_json::from_value::<MatrixRun>(value).is_err(),
                "{path}"
            );
        }

        let mut value = matrix_json(
            4,
            serde_json::json!({
                "workload": WorkloadSpec::api_disk_lifecycle(),
                "oxide_session": test_session_metadata()
            }),
        );
        value["workload"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = matrix_json(
            4,
            serde_json::json!({
                "workload": WorkloadSpec::api_disk_lifecycle(),
                "oxide_session": test_session_metadata()
            }),
        );
        value["oxide_session"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());
    }

    #[test]
    fn compare_uses_fixed_workload_and_ignores_session_provenance() {
        let baseline: MatrixRun = serde_json::from_value(matrix_json(
            4,
            serde_json::json!({
                "workload": WorkloadSpec::api_disk_lifecycle(),
                "oxide_session": test_session_metadata()
            }),
        ))
        .unwrap();
        let mut current: MatrixRun = serde_json::from_value(matrix_json(
            4,
            serde_json::json!({
                "workload": WorkloadSpec::api_disk_lifecycle(),
                "oxide_session": test_session_metadata()
            }),
        ))
        .unwrap();
        assert!(validate_comparison_compatibility(&baseline, &current).is_ok());

        current.oxide_session = Some(test_session_metadata());
        assert!(validate_comparison_compatibility(&baseline, &current).is_ok());

        current.workload.as_mut().unwrap().parallel = 5;
        assert!(
            validate_comparison_compatibility(&baseline, &current).is_err()
        );
        current.workload = None;
        assert!(
            validate_comparison_compatibility(&baseline, &current).is_err()
        );
    }

    fn test_session_metadata() -> OxideSessionMetadata {
        OxideSessionMetadata {
            profile: "voxel-perftest".into(),
            host: "http://recovery.sys.oxide.test".into(),
            provider: OxideAuthProviderMetadata::Builtin,
            oxide_cli_version: "oxide 0.1".into(),
        }
    }

    #[test]
    fn schema_v4_workload_requires_session() {
        assert!(
            serde_json::from_value::<MatrixRun>(matrix_json(
                4,
                serde_json::json!({
                    "workload": WorkloadSpec::api_disk_lifecycle(),
                    "oxide_session": null
                })
            ))
            .is_err()
        );
    }

    #[test]
    fn successful_repeat_metadata_must_be_consistent() {
        let mut expected = OxideSessionAggregation::Unobserved;
        let metadata = test_session_metadata();
        expected.merge(Some(metadata.clone())).unwrap();
        expected.merge(Some(metadata)).unwrap();
        let mut changed = test_session_metadata();
        changed.host = "http://other.invalid".into();
        assert!(expected.merge(Some(changed)).is_err());
    }

    #[test]
    fn successful_repeat_metadata_availability_must_be_consistent() {
        let mut present_first = OxideSessionAggregation::Unobserved;
        present_first.merge(Some(test_session_metadata())).unwrap();
        assert!(present_first.merge(None).is_err());

        let mut missing_first = OxideSessionAggregation::Unobserved;
        missing_first.merge(None).unwrap();
        assert!(missing_first.merge(Some(test_session_metadata())).is_err());
    }

    #[test]
    fn simulated_zpool_count_is_five_per_rss_participant_in_each_rack() {
        let mut cfg = VoxelConfig::default();
        cfg.topology.sleds = 4;
        cfg.topology.racks = 2;
        assert_eq!(expected_simulated_zpool_count_per_rack(&cfg), 20);

        cfg.topology.rss_sleds = 3;
        assert_eq!(expected_simulated_zpool_count_per_rack(&cfg), 15);
    }

    #[test]
    fn omdb_zpool_list_requires_unique_canonical_uuids_and_terminal_sentinel() {
        let first = "00000000-0000-0000-0000-000000000001";
        let second = "00000000-0000-0000-0000-000000000002";
        let output = format!("{first}\n{second}\n{ZPOOL_LIST_SENTINEL}\n");
        assert_eq!(
            parse_omdb_zpool_ids(&output).unwrap(),
            vec![
                uuid::Uuid::parse_str(first).unwrap(),
                uuid::Uuid::parse_str(second).unwrap()
            ]
        );

        for malformed in [
            format!("not-a-uuid\n{ZPOOL_LIST_SENTINEL}\n"),
            format!("{first}\n{first}\n{ZPOOL_LIST_SENTINEL}\n"),
            format!("{first}\n"),
            format!("{first}\n{ZPOOL_LIST_SENTINEL}\ntrailing\n"),
            format!("{first}\n{ZPOOL_LIST_SENTINEL}\n{ZPOOL_LIST_SENTINEL}\n"),
        ] {
            assert!(parse_omdb_zpool_ids(&malformed).is_err(), "{malformed:?}");
        }
    }

    #[test]
    fn omdb_commands_probe_capability_and_use_parsed_uuid_without_shell_input()
    {
        let id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001")
            .unwrap();
        let capability = omdb_zpool_capability_command();
        assert!(capability.contains(OMDB));
        assert!(capability.contains("-w db zpool set-storage-buffer --help"));
        assert!(
            capability.contains("db region dry-run-region-allocation --help")
        );
        assert!(capability.ends_with(ZPOOL_CAPABILITY_SENTINEL));

        let list = omdb_zpool_list_command();
        assert!(list.contains("db zpool list -i"));
        assert!(list.ends_with(ZPOOL_LIST_SENTINEL));
        assert!(list.contains("2>/dev/null"));
        assert!(!list.contains("2>&1"));
        assert_eq!(
            omdb_switch_zone_command(&list),
            format!("zlogin oxz_switch '{list}'")
        );

        let set = omdb_zpool_set_buffer_command(id);
        assert!(set.contains(
            "set-storage-buffer 00000000-0000-0000-0000-000000000001 0"
        ));
        assert!(set.ends_with(ZPOOL_SET_SENTINEL));

        let dry_run = omdb_region_allocation_dry_run_command();
        assert!(dry_run.contains("db region dry-run-region-allocation"));
        assert!(dry_run.contains("--block-size 512"));
        assert!(dry_run.contains("--size 1073741824"));
        assert!(dry_run.contains("--num-regions-required 3"));
        assert!(dry_run.contains("--distinct-sleds"));
        assert!(dry_run.ends_with(REGION_ALLOCATION_SENTINEL));

        assert!(has_unique_terminal_sentinel(
            &format!("diagnostic\n{ZPOOL_SET_SENTINEL}\n"),
            ZPOOL_SET_SENTINEL
        ));
        assert!(!has_unique_terminal_sentinel(
            &format!("{ZPOOL_SET_SENTINEL}\ntrailing\n"),
            ZPOOL_SET_SENTINEL
        ));
        assert!(!has_unique_terminal_sentinel(
            &format!("{ZPOOL_SET_SENTINEL}\n{ZPOOL_SET_SENTINEL}\n"),
            ZPOOL_SET_SENTINEL
        ));
    }

    #[test]
    fn zpool_storage_buffer_update_retries_idempotently_with_longer_timeout() {
        let id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001")
            .unwrap();
        let mut outcomes = VecDeque::from([
            None,
            Some("transient database failure\n".to_string()),
            Some(format!("updated\n{ZPOOL_SET_SENTINEL}\n")),
        ]);
        let mut timeouts = Vec::new();

        set_zpool_storage_buffer_with(
            "rack 1 (g0)",
            id,
            Instant::now() + Duration::from_secs(180),
            Duration::ZERO,
            Instant::now,
            |_| {},
            |timeout| {
                timeouts.push(timeout);
                outcomes.pop_front().unwrap()
            },
        )
        .unwrap();

        assert_eq!(timeouts, vec![Duration::from_secs(60); 3]);
        assert!(outcomes.is_empty());
    }

    #[test]
    fn zpool_storage_buffer_update_stops_at_shared_deadline() {
        let id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001")
            .unwrap();
        let error = set_zpool_storage_buffer_with(
            "rack 1 (g0)",
            id,
            Instant::now(),
            Duration::ZERO,
            Instant::now,
            |_| {},
            |_| panic!("expired preparation must not issue another update"),
        )
        .unwrap_err();

        match error {
            ClassifiedFailure::Retryable(error) => {
                assert!(error.to_string().contains("shared deadline"));
            }
            ClassifiedFailure::Permanent(error) => {
                panic!("deadline exhaustion must remain retryable: {error:#}")
            }
        }
    }

    #[test]
    fn region_allocation_readiness_retries_until_real_query_succeeds() {
        let mut outcomes = VecDeque::from([
            Some("Error: InsufficientCapacity\n".to_string()),
            Some(format!(
                "REGION_ID DATASET_ID SIZE_USED\n{REGION_ALLOCATION_SENTINEL}\n"
            )),
        ]);
        let mut timeouts = Vec::new();

        wait_for_region_allocation_with(
            "rack 1 (g0)",
            Instant::now() + Duration::from_secs(180),
            Duration::ZERO,
            Instant::now,
            |_| {},
            |timeout| {
                timeouts.push(timeout);
                outcomes.pop_front().unwrap()
            },
        )
        .unwrap();

        assert_eq!(timeouts, vec![Duration::from_secs(60); 2]);
        assert!(outcomes.is_empty());
    }

    #[test]
    fn omdb_failure_summary_retains_safe_allocation_reason() {
        assert_eq!(
            omdb_failure_summary("Error: Not enough datasets"),
            "region allocation reports not enough provisionable datasets"
        );
        assert_eq!(
            omdb_failure_summary("Error: Not enough unique zpools selected"),
            "region allocation reports fewer than three unique zpools"
        );
        assert_eq!(
            omdb_failure_summary("Error: Not enough space"),
            "region allocation reports insufficient accounted pool space"
        );
        assert_eq!(
            omdb_failure_summary("Error: InsufficientCapacity"),
            "region allocation reports insufficient capacity"
        );
        assert_eq!(
            omdb_failure_summary("distinctive-secret"),
            "omdb returned a non-success status"
        );
    }

    #[test]
    fn zpool_storage_buffer_update_caps_to_remaining_budget_without_final_sleep()
     {
        let id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001")
            .unwrap();
        let start = Instant::now();
        let clock = Rc::new(Cell::new(start));
        let sleeps = Rc::new(RefCell::new(Vec::new()));
        let attempts = Rc::new(Cell::new(0));
        let now_clock = Rc::clone(&clock);
        let sleep_clock = Rc::clone(&clock);
        let observed_sleeps = Rc::clone(&sleeps);
        let run_clock = Rc::clone(&clock);
        let observed_attempts = Rc::clone(&attempts);

        let error = set_zpool_storage_buffer_with(
            "rack 1 (g0)",
            id,
            start + Duration::from_secs(20),
            Duration::from_secs(1),
            move || now_clock.get(),
            move |delay| {
                observed_sleeps.borrow_mut().push(delay);
                sleep_clock.set(sleep_clock.get() + delay);
            },
            move |timeout| {
                observed_attempts.set(observed_attempts.get() + 1);
                assert_eq!(timeout, Duration::from_secs(20));
                run_clock.set(start + Duration::from_secs(20));
                None
            },
        )
        .unwrap_err();

        assert!(matches!(error, ClassifiedFailure::Retryable(_)));
        assert_eq!(attempts.get(), 1);
        assert!(sleeps.borrow().is_empty());
    }

    const ZFS_OK: &str = "rpool\noxi_internal\noxp_external\n__VOXEL_POOLS_DONE__\nrpool\tsync\tdisabled\tlocal\nrpool\tcompression\tlz4\tlocal\noxi_internal\tsync\tdisabled\tlocal\noxi_internal\tcompression\tlz4\tlocal\noxp_external\tsync\tdisabled\tlocal\noxp_external\tcompression\tlz4\tlocal\n__VOXEL_ZFS_DONE__\n";

    #[test]
    fn guest_evidence_attempt_timeout_is_capped() {
        let now = Instant::now();
        assert_eq!(
            guest_evidence_attempt_timeout(now + Duration::from_secs(60), now),
            Some(Duration::from_secs(15))
        );
    }

    #[test]
    fn guest_evidence_attempt_timeout_uses_only_remaining_budget() {
        let now = Instant::now();
        assert_eq!(
            guest_evidence_attempt_timeout(now + Duration::from_secs(7), now),
            Some(Duration::from_secs(7))
        );
        assert_eq!(guest_evidence_attempt_timeout(now, now), None);
    }

    #[test]
    fn guest_evidence_final_failure_preserves_stage_error_or_uses_fallback() {
        let stage_failure = final_guest_evidence_failure(Some(anyhow!(
            "ZFS evidence stage failed"
        )));
        assert!(
            stage_failure.to_string().contains("ZFS evidence stage failed")
        );

        assert_eq!(
            final_guest_evidence_failure(None).to_string(),
            "observed evidence deadline with no successful ZFS verification"
        );
    }

    #[test]
    fn zfs_evidence_command_uses_a_single_printf_backslash() {
        assert!(ZFS_EVIDENCE_COMMAND.contains(r#"printf '%s\n' "$pools""#));
        assert!(!ZFS_EVIDENCE_COMMAND.contains(r#"printf '%s\\n' "$pools""#));
    }

    #[test]
    fn zfs_evidence_command_shell_frames_distinct_pools_and_properties() {
        use std::os::unix::fs::PermissionsExt;

        let bin = tempfile::tempdir().unwrap();
        let zpool = bin.path().join("zpool");
        let zfs = bin.path().join("zfs");
        std::fs::write(
            &zpool,
            "#!/bin/sh\nprintf 'rpool\\noxi_internal\\noxp_external\\n'\n",
        )
        .unwrap();
        std::fs::write(
            &zfs,
            "#!/bin/sh\npool=${6}\nprintf '%s\\tsync\\tdisabled\\tlocal\\n' \"$pool\"\nprintf '%s\\tcompression\\tlz4\\tlocal\\n' \"$pool\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &zpool,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(&zfs, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let output = Command::new("/bin/sh")
            .args(["-c", ZFS_EVIDENCE_COMMAND])
            .env("PATH", bin.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.starts_with(
            "rpool\noxi_internal\noxp_external\n__VOXEL_POOLS_DONE__\n"
        ));
        assert!(stdout.contains("oxi_internal\tsync\tdisabled\tlocal\n"));
        validate_zfs_evidence(&stdout, true).unwrap();
    }

    #[test]
    fn zfs_evidence_on_succeeds_and_off_allows_image_default_compression() {
        validate_zfs_evidence(ZFS_OK, true).unwrap();
        let compatible_compression =
            ZFS_OK.replace("compression\tlz4", "compression\ton");
        validate_zfs_evidence(&compatible_compression, true).unwrap();
        let internal_only = ZFS_OK
            .replace("oxp_external\n", "")
            .replace("oxp_external\tsync\tdisabled\tlocal\n", "")
            .replace("oxp_external\tcompression\tlz4\tlocal\n", "");
        validate_zfs_evidence(&internal_only, true).unwrap();
        assert!(validate_zfs_evidence(ZFS_OK, false).is_err());
        let off = ZFS_OK
            .replace("disabled\tlocal", "standard\tdefault")
            .replace("lz4\tlocal", "off\tdefault");
        validate_zfs_evidence(&off, false).unwrap();
        let stale_sync = off.replace("standard\tdefault", "disabled\tlocal");
        assert!(validate_zfs_evidence(&stale_sync, false).is_err());
        let image_default_compression =
            off.replace("off\tdefault", "on\tlocal");
        validate_zfs_evidence(&image_default_compression, false).unwrap();
    }

    #[test]
    fn zfs_evidence_rejects_missing_omicron_pool_duplicate_and_malformed_rows()
    {
        let no_omicron_pool = "rpool\n__VOXEL_POOLS_DONE__\nrpool\tsync\tdisabled\tlocal\nrpool\tcompression\tlz4\tlocal\n__VOXEL_ZFS_DONE__\n";
        assert!(validate_zfs_evidence(no_omicron_pool, true).is_err());
        assert!(
            validate_zfs_evidence(
                &ZFS_OK.replace(
                    "__VOXEL_ZFS_DONE__",
                    "rpool\tsync\tdisabled\tlocal\n__VOXEL_ZFS_DONE__"
                ),
                true
            )
            .is_err()
        );
        assert!(
            validate_zfs_evidence(
                &ZFS_OK.replace(
                    "rpool\tsync\tdisabled\tlocal",
                    "rpool sync disabled"
                ),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn zfs_evidence_rejects_missing_duplicate_and_trailing_sentinels() {
        assert!(
            validate_zfs_evidence(&ZFS_OK.replace(ZFS_SENTINEL, ""), true)
                .is_err()
        );
        assert!(
            validate_zfs_evidence(
                &ZFS_OK.replace(
                    ZFS_SENTINEL,
                    &format!("{ZFS_SENTINEL}\n{ZFS_SENTINEL}")
                ),
                true
            )
            .is_err()
        );
        assert!(
            validate_zfs_evidence(&format!("{ZFS_OK}trailing\n"), true)
                .is_err()
        );
        assert!(
            validate_zfs_evidence(&ZFS_OK.replace(POOLS_SENTINEL, ""), true)
                .is_err()
        );
        assert!(
            validate_zfs_evidence(
                &ZFS_OK.replace(
                    POOLS_SENTINEL,
                    &format!("{POOLS_SENTINEL}\n{POOLS_SENTINEL}")
                ),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn parses_nvme_controllers() {
        let list = "nvme0: model=WUS foo\nnvme10: model=bar\n  namespace stuff: x\nnvme0: dup\n";
        assert_eq!(parse_nvme_controllers(list), vec!["nvme0", "nvme10"]);
    }

    #[test]
    fn parses_raw_nvme_health_log() {
        let mut log = [0u8; 512];
        log[5] = 7;
        log[48..64].copy_from_slice(&1_234_567u128.to_le_bytes());
        log[128..144].copy_from_slice(&12_000u128.to_le_bytes());

        let health = parse_nvme_health_log(&log).unwrap();
        assert_eq!(health.data_units_written, 1_234_567);
        assert_eq!(health.percentage_used, 7);
        assert_eq!(health.power_on_hours, 12_000);

        assert!(parse_nvme_health_log(&log[..143]).is_err());
    }

    #[test]
    fn parses_zpool_alloc() {
        let out = "rpool\t123456\ntestbed/falcon\t987654321\nbad line\n";
        let pools = parse_zpool_alloc(out);
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0]["name"], "rpool");
        assert_eq!(pools[0]["alloc_bytes"], 123456);
        assert_eq!(pools[1]["alloc_bytes"], 987_654_321u64);
    }

    #[test]
    fn size_parsing() {
        assert_eq!(size_to_bytes("1G").unwrap(), 1 << 30);
        assert_eq!(size_to_bytes("512m").unwrap(), 512 << 20);
        assert_eq!(size_to_bytes("2048k").unwrap(), 2048 << 10);
        assert_eq!(size_to_bytes("4096").unwrap(), 4096);
        assert!(size_to_bytes("nope").is_err());
    }

    #[test]
    fn projection_math() {
        // 100 GB (decimal) written over 100s = 1 GB/s.
        let p = project_samples(
            &CounterSample { timestamp_seconds: 0, value: 0 },
            &CounterSample { timestamp_seconds: 100, value: 100_000_000_000 },
            &projection_policy(Some(1200.0)).unwrap(),
        )
        .unwrap();
        assert!((p.units_per_second - 1e9).abs() < 1.0);
        assert!((p.giga_units_per_day - 86_400.0).abs() < 1.0); // 1e9 B/s * 86400 / 1e9
        // 1e9 B/s -> 31.536 PB/yr = 31536 TB/yr; 1200 TBW / that ~= 0.038 yr.
        assert!((p.tera_units_per_year - 31_536.0).abs() < 1.0);
        let years = p.rated_years.unwrap();
        assert!((years - 1200.0 / 31_536.0).abs() < 1e-6);
    }

    #[test]
    fn projection_policy_rejects_invalid_and_unrepresentable_ratings() {
        for rating in
            [0.0, -1.0, f64::NAN, f64::INFINITY, u64::MAX as f64 / 1e12 + 1.0]
        {
            assert!(
                projection_policy(Some(rating)).is_err(),
                "accepted {rating}"
            );
        }
    }

    #[test]
    fn projection_without_rating_and_writes_has_zero_rate_and_no_lifetime() {
        let projection = project_samples(
            &CounterSample { timestamp_seconds: 10, value: 7 },
            &CounterSample { timestamp_seconds: 20, value: 7 },
            &projection_policy(None).unwrap(),
        )
        .unwrap();
        assert_eq!(projection.units_per_second, 0.0);
        assert_eq!(projection.rated_years, None);
    }

    #[test]
    fn projection_uses_one_second_for_equal_or_reversed_timestamps() {
        for (before, after) in [(10, 10), (11, 10)] {
            let projection = project_samples(
                &CounterSample { timestamp_seconds: before, value: 0 },
                &CounterSample { timestamp_seconds: after, value: 25 },
                &projection_policy(None).unwrap(),
            )
            .unwrap();
            assert_eq!(projection.units_per_second, 25.0);
        }
    }

    #[test]
    fn report_preserves_zero_delta_text() {
        let before = json!({
            "label": "before", "unix_time": 20,
            "devices": [{ "name": "nvme0", "data_units_written": 9 }],
            "pools": [],
        });
        let after = json!({
            "label": "after", "unix_time": 10,
            "devices": [{ "name": "nvme0", "data_units_written": 9 }],
            "pools": [],
        });
        let text = report(&before, &after, None).unwrap().join("\n");
        assert!(
            text.contains("window: 1s   [before] -> [after]"),
            "got:\n{text}"
        );
        assert!(
            text.contains("nvme0: wrote 0.00 B over window"),
            "got:\n{text}"
        );
        assert!(
            text.contains("rate 0.00 B/s   (0.00 GB/day, 0.00 TB/year)"),
            "got:\n{text}"
        );
    }

    #[test]
    fn human_bytes_decimal() {
        assert_eq!(human_bytes(0), "0.00 B");
        assert_eq!(human_bytes(1_500), "1.50 KB");
        assert_eq!(human_bytes(2_500_000_000), "2.50 GB");
    }

    #[test]
    fn peak_ram_delta_is_relative_to_baseline() {
        assert_eq!(peak_ram_delta(100, 145), 45);
    }

    #[test]
    fn peak_ram_delta_saturates_when_memory_falls() {
        assert_eq!(peak_ram_delta(145, 100), 0);
    }

    #[test]
    fn peak_ram_worker_does_not_probe_after_stop() {
        let stop = AtomicBool::new(true);
        let probes = std::cell::Cell::new(0);
        let delta = sample_peak_ram_until_stopped(
            100,
            &stop,
            || {
                probes.set(probes.get() + 1);
                Some(200)
            },
            || {},
        );
        assert_eq!(delta, None);
        assert_eq!(probes.get(), 0);
    }

    #[test]
    fn peak_ram_worker_discards_probe_that_finishes_after_stop() {
        let stop = AtomicBool::new(false);
        let probes = std::cell::Cell::new(0);
        let delta = sample_peak_ram_until_stopped(
            100,
            &stop,
            || {
                probes.set(probes.get() + 1);
                stop.store(true, Ordering::Relaxed);
                Some(150)
            },
            || panic!("worker must not wait after stop"),
        );
        assert_eq!(delta, None);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn peak_ram_worker_requires_an_in_window_sample() {
        let stop = AtomicBool::new(false);
        let attempts = std::cell::Cell::new(0);
        let delta = sample_peak_ram_until_stopped(
            100,
            &stop,
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
                None
            },
            || {},
        );
        assert_eq!(delta, None);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn peak_ram_worker_reports_largest_in_window_increase() {
        let stop = AtomicBool::new(false);
        let readings =
            std::cell::RefCell::new(VecDeque::from([Some(120), Some(150)]));
        let delta = sample_peak_ram_until_stopped(
            100,
            &stop,
            || readings.borrow_mut().pop_front().unwrap(),
            || {
                if readings.borrow().is_empty() {
                    stop.store(true, Ordering::Relaxed);
                }
            },
        );
        assert_eq!(delta, Some(50));
    }

    #[test]
    fn missing_required_ram_delta_is_retryable_execution_failure() {
        assert!(matches!(
            require_ram_delta(None, "launch"),
            Err(RepeatRunError::Execution(_))
        ));
        assert_eq!(require_ram_delta(Some(42), "launch").unwrap(), 42);
    }

    #[test]
    fn report_diffs_two_samples() {
        let before = json!({
            "label": "baseline", "unix_time": 1000,
            "devices": [{ "name": "nvme0", "data_units_written": 1000, "percentage_used": 5, "power_on_hours": 100 }],
            "pools": [{ "name": "rpool", "alloc_bytes": 1_000_000 }],
        });
        // +2000 data units over 100s = 2000*512000 = 1.024e9 B.
        let after = json!({
            "label": "lever3", "unix_time": 1100,
            "devices": [{ "name": "nvme0", "data_units_written": 3000, "percentage_used": 5, "power_on_hours": 100 }],
            "pools": [{ "name": "rpool", "alloc_bytes": 1_500_000 }],
        });
        let lines = report(&before, &after, Some(1200.0)).unwrap().join("\n");
        assert!(lines.contains("window: 100s"));
        assert!(lines.contains("[baseline] -> [lever3]"));
        assert!(lines.contains("nvme0: wrote 1.02 GB"), "got:\n{lines}");
        assert!(lines.contains("endurance: 1200 TBW"));
        // Pool alloc delta is +500000 B.
        assert!(lines.contains("rpool: +500.00 KB"), "got:\n{lines}");
    }

    #[test]
    fn report_handles_missing_before_device_without_false_spike() {
        let before =
            json!({ "label": "b", "unix_time": 0, "devices": [], "pools": [] });
        let after = json!({
            "label": "a", "unix_time": 60,
            "devices": [{ "name": "nvme9", "data_units_written": 500, "percentage_used": null, "power_on_hours": null }],
            "pools": [],
        });
        let lines = report(&before, &after, None).unwrap().join("\n");
        // Unmatched device -> zero delta, not a 500-unit spike.
        assert!(lines.contains("nvme9: wrote 0.00 B"), "got:\n{lines}");
    }

    // ---- matrix ----

    fn set(items: &[u8]) -> BTreeSet<u8> {
        items.iter().copied().collect()
    }

    #[test]
    fn default_ladder_is_cumulative() {
        let ladder: Vec<BTreeSet<u8>> =
            default_ladder().into_iter().map(|(_, s)| s).collect();
        assert_eq!(
            ladder,
            vec![
                set(&[]),
                set(&[1]),
                set(&[1, 2]),
                set(&[1, 2, 3]),
                set(&[1, 2, 3, 4])
            ]
        );
        // Labels read as "none", "1", "1+2", ...
        let labels: Vec<String> =
            default_ladder().into_iter().map(|(l, _)| l).collect();
        assert_eq!(labels, vec!["none", "1", "1+2", "1+2+3", "1+2+3+4"]);
    }

    #[test]
    fn parse_combos_spec_and_aliases() {
        let got: Vec<(String, BTreeSet<u8>)> =
            parse_combos(Some("none; 1 ;1+2; all ;;3+4")).unwrap();
        assert_eq!(
            got,
            vec![
                ("none".into(), set(&[])),
                ("1".into(), set(&[1])),
                ("1+2".into(), set(&[1, 2])),
                ("1+2+3+4".into(), set(&[1, 2, 3, 4])), // "all"
                ("3+4".into(), set(&[3, 4])),
            ]
        );
        // Absent -> the default ladder.
        assert_eq!(parse_combos(None).unwrap().len(), 5);
        // Out-of-range / non-numeric levers are rejected.
        assert!(parse_combos(Some("5")).is_err());
        assert!(parse_combos(Some("1+x")).is_err());
    }

    #[test]
    fn apply_combo_forces_absent_levers_off() {
        // Base explicitly has lever 3 on plus an rss_sleds reduction; a combo of
        // only {1,3} must retain guest tuning but clear the RSS reduction, so
        // each combo is exactly its set regardless of the base config.
        let base = VoxelConfig::from_toml(
            "[topology]\nsleds = 4\nrss_sleds = 3\n[disk_wear]\nguest_zfs_tuning = true\n",
        )
        .unwrap();
        assert!(base.disk_wear.guest_zfs_tuning);

        let c = apply_combo(&base, &set(&[1, 3]), 3);
        assert!(c.disk_wear.host_sync_disabled && c.disk_wear.guest_zfs_tuning);
        assert!(!c.disk_wear.host_compression);
        assert_eq!(
            c.topology.rss_sleds, 0,
            "lever 4 absent -> no reduction (all sleds)"
        );

        // Lever 4 present -> rss_sleds set to the matrix value.
        let c4 = apply_combo(&base, &set(&[4]), 3);
        assert_eq!(c4.topology.rss_sleds, 3);
        assert!(!c4.disk_wear.guest_zfs_tuning);
    }

    #[test]
    fn every_default_matrix_combo_applies_exactly_its_labeled_levers() {
        let base = VoxelConfig::from_toml("[topology]\nsleds = 4\n").unwrap();
        for (label, levers) in default_ladder() {
            let cfg = apply_combo(&base, &levers, 3);
            assert_eq!(
                cfg.disk_wear.host_sync_disabled,
                levers.contains(&1),
                "{label}"
            );
            assert_eq!(
                cfg.disk_wear.host_compression,
                levers.contains(&2),
                "{label}"
            );
            assert_eq!(
                cfg.disk_wear.guest_zfs_tuning,
                levers.contains(&3),
                "{label}"
            );
            assert_eq!(
                cfg.topology.rss_sleds,
                if levers.contains(&4) { 3 } else { 0 },
                "{label}"
            );
        }
        assert_eq!(describe_levers(&set(&[3, 4])), "guest,repl");
    }

    #[test]
    fn workload_failure_returns_before_measurement_fields_are_produced() {
        let error = measure_workload(
            &json!({}),
            || Err(ClassifiedFailure::Retryable(anyhow!("workload boom"))),
            || -> Result<Value> {
                panic!("sample must not run after workload failure")
            },
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("workload boom"), "{error}");
    }

    #[test]
    fn permanent_measured_workload_failure_is_not_retryable() {
        let error = measure_workload(
            &json!({}),
            || Err(ClassifiedFailure::Permanent(anyhow!("ownership mismatch"))),
            || -> Result<Value> {
                panic!("sample must not run after workload failure")
            },
        )
        .unwrap_err();
        assert!(matches!(error, RepeatRunError::Permanent(_)));
    }

    #[test]
    fn total_bytes_written_sums_devices() {
        let before = json!({ "devices": [
            { "name": "nvme0", "data_units_written": 100 },
            { "name": "nvme1", "data_units_written": 200 },
        ]});
        let after = json!({ "devices": [
            { "name": "nvme0", "data_units_written": 150 }, // +50
            { "name": "nvme1", "data_units_written": 400 }, // +200
            { "name": "nvme2", "data_units_written": 999 }, // no "before" -> 0
        ]});
        assert_eq!(total_bytes_written(&before, &after), 250 * DATA_UNIT_BYTES);
    }

    // ---- falcon-pool scoping ----

    #[test]
    fn parse_disk_name_accepts_disks_and_rejects_others() {
        // Bare, parenthesized, sliced, and /blkdev-suffixed forms all normalize
        // to the canonical cXtYdZ (slice + decoration dropped).
        assert_eq!(
            parse_disk_name("c5t00A0750152CC6D07d0").as_deref(),
            Some("c5t00A0750152CC6D07d0")
        );
        assert_eq!(
            parse_disk_name("(c1t00A0750152CC6DCEd0)").as_deref(),
            Some("c1t00A0750152CC6DCEd0")
        );
        assert_eq!(
            parse_disk_name("c2t0025384B51A026CFd0s0").as_deref(),
            Some("c2t0025384B51A026CFd0")
        );
        assert_eq!(
            parse_disk_name("c5t00A0750152CC6D07d0/blkdev").as_deref(),
            Some("c5t00A0750152CC6D07d0")
        );
        // Non-disk tokens.
        assert_eq!(parse_disk_name("voxel"), None);
        assert_eq!(parse_disk_name("nvme0"), None);
        assert_eq!(parse_disk_name("nvme0/1"), None);
        assert_eq!(parse_disk_name("mirror-0"), None);
        assert_eq!(parse_disk_name("ONLINE"), None);
        assert_eq!(parse_disk_name("CT1000P310SSD8"), None);
        assert_eq!(parse_disk_name("c5td0"), None); // empty target
    }

    #[test]
    fn pool_leaf_disks_extracts_vdev_devices() {
        // Real `zpool status voxel` shape.
        let status = "\
  pool: voxel
 state: ONLINE
  scan: none requested
config:

        NAME                     STATE     READ WRITE CKSUM
        voxel                    ONLINE       0     0     0
          c5t00A0750152CC6D07d0  ONLINE       0     0     0

errors: No known data errors
";
        let disks = pool_leaf_disks(status, "voxel").unwrap();
        assert_eq!(
            disks,
            ["c5t00A0750152CC6D07d0"].iter().map(|s| s.to_string()).collect()
        );
        // A mirror vdev: both leaves, but not the `mirror-0` keyword or pool name.
        let mirror = "\
config:
        voxel                    ONLINE
          mirror-0               ONLINE
            c5t00A0750152CC6D07d0  ONLINE
            c6t0025384B51A026CFd0  ONLINE
";
        let disks = pool_leaf_disks(mirror, "voxel").unwrap();
        assert_eq!(disks.len(), 2);
        assert!(disks.contains("c5t00A0750152CC6D07d0"));
        assert!(disks.contains("c6t0025384B51A026CFd0"));
    }

    #[test]
    fn pool_leaf_disks_rejects_unrecognized_active_leaf() {
        let status = "\
config:
        NAME                     STATE
        voxel                    ONLINE
          mirror-0               ONLINE
            c5t00A0750152CC6D07d0  ONLINE
            /var/tmp/file-vdev   ONLINE
";
        let error = pool_leaf_disks(status, "voxel").unwrap_err().to_string();
        assert!(error.contains("/var/tmp/file-vdev"), "{error}");
    }

    #[test]
    fn parse_nvme_disk_map_associates_disks_with_controllers() {
        // `nvmeadm list`: controller headers at col 0, namespace child lines
        // carry the parenthesized blkdev name.
        let list = "\
nvme0: model: CT1000P310SSD8, serial: AAAA, FW rev: 1
  nvme0/1 (c1t00A0750152CC6DCEd0): Size = 931 GiB
nvme1: model: Samsung SSD 990 PRO 4TB, serial: BBBB, FW rev: 2
  nvme1/1 (c2t0025384B51A026CFd0): Size = 3726 GiB
nvme2: model: CT1000P310SSD8, serial: CCCC, FW rev: 1
  nvme2/1 (c5t00A0750152CC6D07d0): Size = 931 GiB
";
        let map = parse_nvme_disk_map(list);
        assert_eq!(
            map.get("c1t00A0750152CC6DCEd0").map(String::as_str),
            Some("nvme0")
        );
        assert_eq!(
            map.get("c2t0025384B51A026CFd0").map(String::as_str),
            Some("nvme1")
        );
        assert_eq!(
            map.get("c5t00A0750152CC6D07d0").map(String::as_str),
            Some("nvme2")
        );
        // The controller header's model text must not be parsed as a disk.
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn pool_scope_rejects_partially_mapped_leaf_disks() {
        let disks = ["c1t00A0750152CC6DCEd0", "c5t00A0750152CC6D07d0"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let map = [("c1t00A0750152CC6DCEd0".to_string(), "nvme0".to_string())]
            .into_iter()
            .collect();

        let error =
            resolve_pool_controllers(&disks, &map).unwrap_err().to_string();
        assert!(error.contains("c5t00A0750152CC6D07d0"), "{error}");
    }

    #[test]
    fn total_bytes_written_scopes_to_falcon_controllers() {
        // Two drives changed, but only nvme2 backs the falcon pool -> count it
        // alone (the other drive is OS/other-pool noise).
        let before = json!({ "devices": [
            { "name": "nvme0", "data_units_written": 1000 },
            { "name": "nvme2", "data_units_written": 2000 },
        ]});
        let after = json!({
            "devices": [
                { "name": "nvme0", "data_units_written": 9000 }, // +8000, excluded
                { "name": "nvme2", "data_units_written": 2500 }, // +500,  counted
            ],
            "falcon_controllers": ["nvme2"],
        });
        assert_eq!(total_bytes_written(&before, &after), 500 * DATA_UNIT_BYTES);
    }

    #[test]
    fn total_bytes_written_falls_back_to_all_when_unscoped() {
        // No falcon_controllers (old sample / unresolved) -> sum every drive.
        let before = json!({ "devices": [
            { "name": "nvme0", "data_units_written": 0 },
            { "name": "nvme2", "data_units_written": 0 },
        ]});
        let after = json!({ "devices": [
            { "name": "nvme0", "data_units_written": 100 },
            { "name": "nvme2", "data_units_written": 200 },
        ]});
        assert_eq!(total_bytes_written(&before, &after), 300 * DATA_UNIT_BYTES);
        // Empty falcon_controllers is treated the same as absent.
        let after_empty = json!({
            "devices": [{ "name": "nvme0", "data_units_written": 100 }],
            "falcon_controllers": [],
        });
        assert_eq!(
            total_bytes_written(&before, &after_empty),
            100 * DATA_UNIT_BYTES
        );
    }

    #[test]
    fn strict_matrix_scope_rejects_empty_changed_and_missing_devices() {
        let sample = |controllers: &[&str], devices: &[&str]| {
            json!({
                "falcon_pool": "voxel",
                "falcon_controllers": controllers,
                "devices": devices.iter().map(|name| json!({
                    "name": name, "data_units_written": 1
                })).collect::<Vec<_>>(),
            })
        };
        assert!(validate_matrix_scope(&sample(&[], &["nvme0"]), None).is_err());
        let before = sample(&["nvme0"], &["nvme0"]);
        assert!(
            validate_matrix_scope(
                &sample(&["nvme1"], &["nvme1"]),
                Some(&before)
            )
            .is_err()
        );
        assert!(
            validate_matrix_scope(
                &sample(&["nvme0"], &["nvme1"]),
                Some(&before)
            )
            .is_err()
        );
        assert!(validate_matrix_scope(&before, None).is_ok());
    }

    #[test]
    fn operation_and_cleanup_error_retains_both() {
        let e = combine_operation_and_cleanup::<()>(
            Err(anyhow!("primary boom")),
            Err(anyhow!("cleanup boom")),
            "test",
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("primary boom") && e.contains("cleanup boom"),
            "{e}"
        );
    }

    #[test]
    fn workload_measurement_uses_post_auth_baseline() {
        let sample = |units, time| {
            json!({
                "unix_time": time,
                "falcon_pool": "voxel",
                "falcon_controllers": ["nvme0"],
                "devices": [{"name": "nvme0", "data_units_written": units}],
            })
        };
        let fresh = sample(100, 10);
        let after = sample(125, 15);
        let (bytes, secs) = workload_measurement(&fresh, &after).unwrap();
        assert_eq!(bytes, matrix_total_bytes_written(&fresh, &after).unwrap());
        assert_eq!(secs, 5);
    }

    #[test]
    fn permanent_repeat_outcome_is_not_retryable() {
        let error = finish_repeat_execution::<()>(
            Err(RepeatRunError::Permanent(anyhow!("auth incompatible"))),
            Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, RepeatRunError::Permanent(_)));
    }

    #[test]
    fn cleanup_always_attempts_rack_teardown_after_profile_close_failure() {
        let rack_attempted = std::cell::Cell::new(false);
        let profile = finish_repeat_execution::<()>(
            Ok(()),
            Err(anyhow!("profile close boom")),
        );
        let rack = {
            rack_attempted.set(true);
            Err(anyhow!("rack teardown boom"))
        };
        let error =
            finish_repeat_execution(profile, rack).unwrap_err().to_string();
        assert!(rack_attempted.get());
        assert!(error.contains("profile close boom"), "{error}");
        assert!(error.contains("rack teardown boom"), "{error}");
    }

    #[test]
    fn provisioning_profile_and_rack_failures_are_all_retained_as_boundary() {
        let provision = classify_provision(ProvisionError::Boundary(anyhow!(
            "authentication failed; additionally profile close failed"
        )));
        let error = finish_repeat_execution::<()>(
            Err(provision),
            Err(anyhow!("rack teardown failed")),
        )
        .unwrap_err();
        let RepeatRunError::Boundary(error) = error else {
            panic!(
                "profile or rack cleanup failure must be a boundary failure"
            );
        };
        let error = error.to_string();
        assert!(error.contains("authentication failed"), "{error}");
        assert!(error.contains("profile close failed"), "{error}");
        assert!(error.contains("rack teardown failed"), "{error}");
    }

    #[test]
    fn repeat_outcome_retries_only_execution_failure_with_clean_post_boundary()
    {
        let error = finish_repeat_execution::<()>(
            Err(RepeatRunError::Execution(anyhow!("body boom"))),
            Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, RepeatRunError::Execution(_)));

        let error =
            finish_repeat_execution(Ok(()), Err(anyhow!("cleanup boom")))
                .unwrap_err();
        assert!(matches!(error, RepeatRunError::Boundary(_)));

        let error = finish_repeat_execution::<()>(
            Err(RepeatRunError::Execution(anyhow!("body boom"))),
            Err(anyhow!("cleanup boom")),
        )
        .unwrap_err();
        let RepeatRunError::Boundary(error) = error else {
            panic!("body plus cleanup failure must be a boundary failure");
        };
        let error = error.to_string();
        assert!(error.contains("body boom"), "{error}");
        assert!(error.contains("cleanup boom"), "{error}");
    }

    #[test]
    fn matrix_repeat_failure_retries_once_then_retains_both_errors() {
        let mut errors = Vec::new();
        assert_eq!(
            record_repeat_failure(&mut errors, 1, &anyhow!("first boom")),
            RepeatFailureDisposition::Retry
        );
        let disposition =
            record_repeat_failure(&mut errors, 2, &anyhow!("second boom"));
        let RepeatFailureDisposition::Exhausted(error) = disposition else {
            panic!("second failure must exhaust the retry budget");
        };
        assert!(error.contains("attempt 1/2: first boom"), "{error}");
        assert!(error.contains("attempt 2/2: second boom"), "{error}");
    }

    #[test]
    fn project_items_and_disk_state_are_strict() {
        assert_eq!(
            item_names(r#"{"items":[{"name":"p"}]}"#).unwrap(),
            vec!["p"]
        );
        assert!(item_names(r#"{"items":[{}]}"#).is_err());
        assert!(item_names(r#"{}"#).is_err());
        assert!(matches!(
            classify_disk_state(r#"{"state":{"state":"detached"}}"#).unwrap(),
            DiskSettlement::Settled
        ));
        assert!(matches!(
            classify_disk_state(r#"{"state":{"state":"faulted"}}"#).unwrap(),
            DiskSettlement::Faulted
        ));
        assert!(classify_disk_state("not json").is_err());
    }

    #[test]
    fn disk_lifecycle_names_and_batches_are_nonce_scoped() {
        let nonce = uuid::Uuid::new_v4();
        let owner = DiskLifecycleOwner::new(nonce, "measured");
        assert!(owner.project_name.starts_with("voxel-perftest-"));
        assert!(owner.project_description.contains(&nonce.to_string()));
        let batches = owner.disk_batches();
        let compact = nonce.simple().to_string();
        assert!(batches.iter().flatten().all(|name| name.contains(&compact)));
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [4, 4, 4, 4, 4]
        );
    }

    #[test]
    fn project_ownership_requires_exact_name_and_description() {
        let owner = DiskLifecycleOwner::new(uuid::Uuid::new_v4(), "probe");
        let matching = json!({"items": [{
            "name": owner.project_name,
            "description": owner.project_description,
        }]});
        assert!(owner.reconcile_project(&matching.to_string()).unwrap());
        let foreign = json!({"items": [{
            "name": owner.project_name,
            "description": "wrong nonce",
        }]});
        assert!(owner.reconcile_project(&foreign.to_string()).is_err());
        assert!(owner.require_owned_disk("foreign-disk").is_err());
    }

    #[test]
    fn disk_lifecycle_supports_both_blank_disk_bodies() {
        for style in [DiskStyle::New, DiskStyle::Legacy] {
            let body: Value =
                serde_json::from_str(&disk_body("disk", 1 << 30, style))
                    .unwrap();
            let source = body
                .pointer("/disk_backend/disk_source")
                .or_else(|| body.get("disk_source"));
            assert_eq!(source.unwrap()["type"], "blank");
        }
    }

    #[test]
    fn lifecycle_api_is_sync() {
        fn require_sync<T: LifecycleApi + Sync>() {}
        require_sync::<OxideSession>();
    }

    #[test]
    fn disk_lifecycle_preparation_sets_recovery_silo_quota_before_shape_probe()
    {
        let spec = WorkloadSpec::api_disk_lifecycle();
        assert_eq!(
            DISK_LIFECYCLE_STORAGE_QUOTA_BYTES,
            spec.count as u64 * spec.size_bytes
        );

        struct QuotaApi {
            quota_set: AtomicBool,
        }

        impl LifecycleApi for QuotaApi {
            fn request(
                &self,
                endpoint: &str,
                method: &str,
                body: Option<&str>,
            ) -> std::result::Result<String, oxide_session::ApiCommandError>
            {
                if method == "PUT" {
                    assert_eq!(endpoint, "/v1/system/silos/recovery/quotas");
                    let body: Value =
                        serde_json::from_str(body.unwrap()).unwrap();
                    assert_eq!(body, json!({"storage": 20u64 << 30}));
                    self.quota_set.store(true, Ordering::SeqCst);
                    return Ok(json!({
                        "silo_id": "15560baa-b972-45d5-a9dd-4c05a12654d4",
                        "cpus": 0,
                        "memory": 0,
                        "storage": 20u64 << 30,
                    })
                    .to_string());
                }
                assert!(self.quota_set.load(Ordering::SeqCst));
                assert_eq!((method, endpoint), ("GET", "/v1/projects"));
                Err(oxide_session::ApiCommandError {
                    kind: oxide_session::ApiErrorKind::Permanent,
                    status: Some(403),
                    message: "stop after proving request order".into(),
                })
            }
        }

        let api = QuotaApi { quota_set: AtomicBool::new(false) };
        assert!(matches!(
            PreparedDiskLifecycle::prepare_with(
                &api,
                "recovery",
                Duration::ZERO
            ),
            Err(ClassifiedFailure::Permanent(_))
        ));
        assert!(api.quota_set.load(Ordering::SeqCst));
    }

    #[test]
    fn missing_recovery_silo_quota_api_is_permanent() {
        struct MissingQuotaApi;

        impl LifecycleApi for MissingQuotaApi {
            fn request(
                &self,
                _endpoint: &str,
                _method: &str,
                _body: Option<&str>,
            ) -> std::result::Result<String, oxide_session::ApiCommandError>
            {
                Err(oxide_session::ApiCommandError {
                    kind: oxide_session::ApiErrorKind::Retryable,
                    status: Some(404),
                    message: "quota API not found".into(),
                })
            }
        }

        assert!(matches!(
            set_disk_lifecycle_storage_quota(
                &MissingQuotaApi,
                "recovery",
                Duration::ZERO
            ),
            Err(ClassifiedFailure::Permanent(_))
        ));
    }

    fn fixed_owner(purpose: &str) -> DiskLifecycleOwner {
        DiskLifecycleOwner::new(
            uuid::Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef")
                .unwrap(),
            purpose,
        )
    }

    fn api_error(
        kind: oxide_session::ApiErrorKind,
        marker: &str,
    ) -> oxide_session::ApiCommandError {
        oxide_session::ApiCommandError {
            kind,
            status: None,
            message: marker.into(),
        }
    }

    struct ScriptStep {
        method: &'static str,
        endpoint: String,
        result: std::result::Result<String, oxide_session::ApiCommandError>,
    }

    struct ScriptedLifecycleApi {
        steps: Mutex<VecDeque<ScriptStep>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedLifecycleApi {
        fn new(steps: Vec<ScriptStep>) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn done(&self) {
            assert!(
                self.steps.lock().unwrap().is_empty(),
                "unused scripted requests"
            );
        }
    }

    impl LifecycleApi for ScriptedLifecycleApi {
        fn request(
            &self,
            endpoint: &str,
            method: &str,
            _body: Option<&str>,
        ) -> std::result::Result<String, oxide_session::ApiCommandError>
        {
            self.calls.lock().unwrap().push(format!("{method} {endpoint}"));
            if is_default_network_delete(method, endpoint) {
                return Ok("{}".into());
            }
            let step =
                self.steps.lock().unwrap().pop_front().unwrap_or_else(|| {
                    panic!("unexpected request {method} {endpoint}")
                });
            assert_eq!(method, step.method);
            assert_eq!(endpoint, step.endpoint);
            step.result
        }
    }

    fn ok(
        method: &'static str,
        endpoint: impl Into<String>,
        json: impl Into<String>,
    ) -> ScriptStep {
        ScriptStep {
            method,
            endpoint: endpoint.into(),
            result: Ok(json.into()),
        }
    }

    fn err(
        method: &'static str,
        endpoint: impl Into<String>,
        kind: oxide_session::ApiErrorKind,
        marker: &str,
    ) -> ScriptStep {
        ScriptStep {
            method,
            endpoint: endpoint.into(),
            result: Err(api_error(kind, marker)),
        }
    }

    fn err_status(
        method: &'static str,
        endpoint: impl Into<String>,
        kind: oxide_session::ApiErrorKind,
        status: u16,
        marker: &str,
    ) -> ScriptStep {
        ScriptStep {
            method,
            endpoint: endpoint.into(),
            result: Err(oxide_session::ApiCommandError {
                kind,
                status: Some(status),
                message: marker.into(),
            }),
        }
    }

    fn is_default_network_delete(method: &str, endpoint: &str) -> bool {
        method == "DELETE"
            && (endpoint.starts_with("/v1/internet-gateways/default?project=")
                && endpoint.ends_with("&vpc=default&cascade=true")
                || endpoint.starts_with("/v1/vpc-subnets/default?project=")
                    && endpoint.ends_with("&vpc=default")
                || endpoint.starts_with("/v1/vpcs/default?project="))
    }

    #[test]
    fn quota_update_retries_retryable_failure() {
        let endpoint = "/v1/system/silos/recovery/quotas";
        let api = ScriptedLifecycleApi::new(vec![
            err_status(
                "PUT",
                endpoint,
                oxide_session::ApiErrorKind::Retryable,
                500,
                "quota service failed",
            ),
            ok(
                "PUT",
                endpoint,
                json!({"storage": DISK_LIFECYCLE_STORAGE_QUOTA_BYTES})
                    .to_string(),
            ),
        ]);

        set_disk_lifecycle_storage_quota(&api, "recovery", Duration::ZERO)
            .unwrap();

        api.done();
        assert_eq!(api.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn quota_update_retry_exhaustion_remains_retryable() {
        let endpoint = "/v1/system/silos/recovery/quotas";
        let api = ScriptedLifecycleApi::new(
            (0..QUOTA_UPDATE_ATTEMPTS)
                .map(|_| {
                    err(
                        "PUT",
                        endpoint,
                        oxide_session::ApiErrorKind::Retryable,
                        "quota update timed out",
                    )
                })
                .collect(),
        );

        let error =
            set_disk_lifecycle_storage_quota(&api, "recovery", Duration::ZERO)
                .unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Retryable(_)));
        assert_eq!(
            api.calls.lock().unwrap().len(),
            QUOTA_UPDATE_ATTEMPTS as usize
        );
    }

    #[test]
    fn project_create_checks_exact_absence_then_reconciles_ambiguous_success() {
        let owner = fixed_owner("probe");
        let project =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "ambiguous",
            ),
            ok("GET", "/v1/projects", project),
        ]);
        create_owned_project(&api, &owner, Duration::ZERO).unwrap();
        api.done();
        assert_eq!(
            api.calls.lock().unwrap()[..2],
            ["GET /v1/projects", "POST /v1/projects"]
        );
    }

    #[test]
    fn explicit_project_create_server_failure_retries_after_absence_proof() {
        let owner = fixed_owner("probe");
        let created = json!({
            "name": owner.project_name,
            "description": owner.project_description,
        })
        .to_string();
        let mut steps = vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err_status(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                500,
                "first create failed before committing",
            ),
        ];
        steps.extend(
            (0..EXPLICIT_CREATE_ABSENCE_POLLS)
                .map(|_| ok("GET", "/v1/projects", r#"{"items":[]}"#)),
        );
        steps.push(ok("POST", "/v1/projects", created));
        let api = ScriptedLifecycleApi::new(steps);

        create_owned_project(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            2
        );
    }

    #[test]
    fn explicit_project_create_retry_collision_is_reconciled_without_third_post()
     {
        let owner = fixed_owner("probe");
        let project =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let mut steps = vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err_status(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                500,
                "first create failed before committing",
            ),
        ];
        steps.extend(
            (0..EXPLICIT_CREATE_ABSENCE_POLLS)
                .map(|_| ok("GET", "/v1/projects", r#"{"items":[]}"#)),
        );
        steps.extend([
            err_status(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                409,
                "first create committed before the retry",
            ),
            ok("GET", "/v1/projects", project),
        ]);
        let api = ScriptedLifecycleApi::new(steps);

        create_owned_project(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            2
        );
    }

    #[test]
    fn explicit_project_create_server_failure_does_not_adopt_foreign_collision()
    {
        let owner = fixed_owner("probe");
        let collision = json!({"items":[{
            "name": owner.project_name,
            "description": "not Voxel's ownership nonce",
        }]})
        .to_string();
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err_status(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                500,
                "explicit server failure",
            ),
            ok("GET", "/v1/projects", collision),
        ]);

        let error =
            create_owned_project(&api, &owner, Duration::ZERO).unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Permanent(_)));
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn explicit_project_create_server_failure_exhaustion_remains_retryable() {
        let owner = fixed_owner("probe");
        let mut steps = vec![ok("GET", "/v1/projects", r#"{"items":[]}"#)];
        for attempt in 1..=PROJECT_CREATE_POST_ATTEMPTS {
            steps.push(err_status(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                500,
                &format!("create server failure {attempt}"),
            ));
            let polls = if attempt < PROJECT_CREATE_POST_ATTEMPTS {
                EXPLICIT_CREATE_ABSENCE_POLLS
            } else {
                AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS
            };
            steps.extend(
                (0..polls)
                    .map(|_| ok("GET", "/v1/projects", r#"{"items":[]}"#)),
            );
        }
        let api = ScriptedLifecycleApi::new(steps);

        let error =
            create_owned_project(&api, &owner, Duration::ZERO).unwrap_err();
        let ClassifiedFailure::Retryable(error) = error else {
            panic!("explicit project create exhaustion must remain retryable");
        };

        let error = error.to_string();
        assert!(error.contains("create server failure 3"));
        assert!(error.contains(&owner.project_name));
        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            PROJECT_CREATE_POST_ATTEMPTS as usize
        );
    }

    #[test]
    fn uncertain_project_absence_does_not_resubmit_after_explicit_server_failure()
     {
        let owner = fixed_owner("probe");
        let project =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err_status(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                500,
                "explicit server failure",
            ),
            err(
                "GET",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "project absence could not be proven",
            ),
            ok("GET", "/v1/projects", project),
        ]);

        create_owned_project(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn ambiguous_project_reconciliation_polls_until_the_saga_record_is_visible()
    {
        let owner = fixed_owner("probe");
        let project =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "timed out after starting saga",
            ),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "GET",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "project list timed out",
            ),
            ok("GET", "/v1/projects", project),
        ]);

        create_owned_project(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn ambiguous_project_reconciliation_retains_the_initial_failure() {
        let owner = fixed_owner("probe");
        let mut steps = vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "HTTP 500; error_code Internal; request_id 3297dd70-d7b4-4270-8e4b-20d15072dcba",
            ),
        ];
        steps.extend(
            (0..AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS)
                .map(|_| ok("GET", "/v1/projects", r#"{"items":[]}"#)),
        );
        let api = ScriptedLifecycleApi::new(steps);

        let error =
            create_owned_project(&api, &owner, Duration::ZERO).unwrap_err();
        let ClassifiedFailure::Retryable(error) = error else {
            panic!("ambiguous project create exhaustion must remain retryable");
        };
        let error = error.to_string();

        assert!(error.contains(
            "ambiguous project create was not reconciled after bounded polling"
        ));
        assert!(error.contains("HTTP 500; error_code Internal"));
        assert!(error.contains("3297dd70-d7b4-4270-8e4b-20d15072dcba"));
        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn ambiguous_project_reconciliation_rejects_a_foreign_name_collision() {
        let owner = fixed_owner("probe");
        let collision = json!({"items":[{
            "name": owner.project_name,
            "description": "not Voxel's ownership nonce",
        }]})
        .to_string();
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "timed out after starting saga",
            ),
            ok("GET", "/v1/projects", collision),
        ]);

        let error =
            create_owned_project(&api, &owner, Duration::ZERO).unwrap_err();

        api.done();
        let ClassifiedFailure::Permanent(error) = error else {
            panic!("foreign project collision must remain permanent");
        };
        assert!(error.to_string().contains(&owner.project_name));
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn ambiguous_project_reconciliation_rejects_malformed_project_rows() {
        let owner = fixed_owner("probe");
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "timed out after starting saga",
            ),
            ok("GET", "/v1/projects", r#"{"items":[{}]}"#),
        ]);

        let error =
            create_owned_project(&api, &owner, Duration::ZERO).unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Permanent(_)));
    }

    #[test]
    fn ambiguous_project_reconciliation_stops_on_permanent_poll_failure() {
        let owner = fixed_owner("probe");
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "timed out after starting saga",
            ),
            err(
                "GET",
                "/v1/projects",
                oxide_session::ApiErrorKind::Authentication,
                "authentication rejected",
            ),
        ]);

        let error =
            create_owned_project(&api, &owner, Duration::ZERO).unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Permanent(_)));
    }

    #[test]
    fn ambiguous_disk_is_reconciled_before_any_second_create() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let disks = format!(r#"{{"items":[{{"name":"{name}"}}]}}"#);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            err(
                "POST",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                "ambiguous",
            ),
            ok("GET", &endpoint, disks),
        ]);
        create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap();
        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn explicit_disk_create_server_failure_retries_after_absence_proof() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let mut steps = vec![err_status(
            "POST",
            &endpoint,
            oxide_session::ApiErrorKind::Retryable,
            500,
            "first create failed before committing",
        )];
        steps.extend(
            (0..EXPLICIT_CREATE_ABSENCE_POLLS)
                .map(|_| ok("GET", &endpoint, r#"{"items":[]}"#)),
        );
        steps.push(ok("POST", &endpoint, json!({"name":name}).to_string()));
        let api = ScriptedLifecycleApi::new(steps);

        create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            2
        );
    }

    #[test]
    fn explicit_disk_create_retry_collision_is_reconciled_without_third_post() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let disks = format!(r#"{{"items":[{{"name":"{name}"}}]}}"#);
        let mut steps = vec![err_status(
            "POST",
            &endpoint,
            oxide_session::ApiErrorKind::Retryable,
            500,
            "first create failed before committing",
        )];
        steps.extend(
            (0..EXPLICIT_CREATE_ABSENCE_POLLS)
                .map(|_| ok("GET", &endpoint, r#"{"items":[]}"#)),
        );
        steps.extend([
            err_status(
                "POST",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                409,
                "first create committed before retry",
            ),
            ok("GET", &endpoint, disks),
        ]);
        let api = ScriptedLifecycleApi::new(steps);

        create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            2
        );
    }

    #[test]
    fn uncertain_disk_absence_does_not_resubmit_after_server_failure() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let disks = format!(r#"{{"items":[{{"name":"{name}"}}]}}"#);
        let api = ScriptedLifecycleApi::new(vec![
            err_status(
                "POST",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                500,
                "explicit server failure",
            ),
            err(
                "GET",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                "absence could not be proven",
            ),
            ok("GET", &endpoint, disks),
        ]);

        create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn explicit_disk_create_server_failure_exhaustion_remains_retryable() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let mut steps = Vec::new();
        for attempt in 1..=DISK_CREATE_POST_ATTEMPTS {
            steps.push(err_status(
                "POST",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                500,
                &format!("disk create server failure {attempt}"),
            ));
            let polls = if attempt < DISK_CREATE_POST_ATTEMPTS {
                EXPLICIT_CREATE_ABSENCE_POLLS
            } else {
                AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS
            };
            steps.extend(
                (0..polls).map(|_| ok("GET", &endpoint, r#"{"items":[]}"#)),
            );
        }
        let api = ScriptedLifecycleApi::new(steps);

        let error = create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap_err();
        let ClassifiedFailure::Retryable(error) = error else {
            panic!("explicit disk create exhaustion must remain retryable");
        };

        assert!(error.to_string().contains("disk create server failure 3"));
        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            DISK_CREATE_POST_ATTEMPTS as usize
        );
    }

    #[test]
    fn ambiguous_disk_reconciliation_polls_until_the_saga_record_is_visible() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let disks = format!(r#"{{"items":[{{"name":"{name}"}}]}}"#);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let mut steps = vec![err(
            "POST",
            &endpoint,
            oxide_session::ApiErrorKind::Retryable,
            "timed out after starting saga",
        )];
        steps.extend((0..39).map(|_| ok("GET", &endpoint, r#"{"items":[]}"#)));
        steps.push(ok("GET", &endpoint, disks));
        let api = ScriptedLifecycleApi::new(steps);

        create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("GET "))
                .count(),
            40
        );
    }

    #[test]
    fn ambiguous_disk_reconciliation_retains_the_initial_failure() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let mut steps = vec![err(
            "POST",
            &endpoint,
            oxide_session::ApiErrorKind::Retryable,
            "HTTP 500; error_code Internal; request_id 3297dd70-d7b4-4270-8e4b-20d15072dcba",
        )];
        steps.extend(
            (0..AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS)
                .map(|_| ok("GET", &endpoint, r#"{"items":[]}"#)),
        );
        let api = ScriptedLifecycleApi::new(steps);

        let error = create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap_err();
        let ClassifiedFailure::Retryable(error) = error else {
            panic!("ambiguous create exhaustion must remain retryable");
        };
        let error = error.to_string();

        api.done();
        assert!(error.contains("ambiguous disk create was not reconciled"));
        assert!(error.contains("HTTP 500; error_code Internal"));
        assert!(error.contains("3297dd70-d7b4-4270-8e4b-20d15072dcba"));
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("POST "))
                .count(),
            1
        );
    }

    #[test]
    fn only_explicit_shape_rejection_allows_legacy_fallback() {
        let owner = fixed_owner("probe");
        let name = owner.disk_name(0, 0);
        for kind in [
            oxide_session::ApiErrorKind::Retryable,
            oxide_session::ApiErrorKind::Authentication,
        ] {
            let endpoint = format!("/v1/disks?project={}", owner.project_name);
            let mut steps = vec![err("POST", &endpoint, kind, "first")];
            if kind == oxide_session::ApiErrorKind::Retryable {
                for _ in 0..AMBIGUOUS_CREATE_RECONCILE_ATTEMPTS {
                    steps.push(ok("GET", &endpoint, r#"{"items":[]}"#));
                }
            }
            let api = ScriptedLifecycleApi::new(steps);
            assert!(
                create_owned_disk(
                    &api,
                    &owner,
                    &name,
                    DiskStyle::New,
                    true,
                    Duration::ZERO,
                )
                .is_err()
            );
            api.done();
            assert_eq!(
                api.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.starts_with("POST "))
                    .count(),
                1
            );
        }
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            err(
                "POST",
                &endpoint,
                oxide_session::ApiErrorKind::ShapeRejected,
                "shape",
            ),
            ok("POST", &endpoint, json!({"name":name}).to_string()),
        ]);
        let first = create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::New,
            true,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(matches!(first, ClassifiedFailure::Permanent(_)));
        create_owned_disk(
            &api,
            &owner,
            &name,
            DiskStyle::Legacy,
            true,
            Duration::ZERO,
        )
        .unwrap();
        api.done();
    }

    #[test]
    fn preparation_cleans_up_after_project_and_disk_creation_failures() {
        let owner = fixed_owner("probe");
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            err(
                "POST",
                "/v1/projects",
                oxide_session::ApiErrorKind::Authentication,
                "project failure",
            ),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);
        assert!(prepare_owned_style(&api, &owner, Duration::ZERO).is_err());
        api.done();
        assert_eq!(
            api.calls.lock().unwrap().last().unwrap(),
            "GET /v1/projects"
        );

        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
            ok(
                "POST",
                "/v1/projects",
                json!({"name":owner.project_name,"description":owner.project_description})
                    .to_string(),
            ),
            err(
                "POST",
                &endpoint,
                oxide_session::ApiErrorKind::Authentication,
                "disk failure",
            ),
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &endpoint, r#"{"items":[]}"#),
            ok(
                "DELETE",
                format!("/v1/projects/{}", owner.project_name),
                "{}",
            ),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);
        assert!(prepare_owned_style(&api, &owner, Duration::ZERO).is_err());
        api.done();
        assert!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("DELETE /v1/projects/"))
        );
    }

    #[test]
    fn malformed_or_mismatched_create_successes_are_permanent() {
        let owner = fixed_owner("probe");
        for response in [
            "not json".to_string(),
            json!({"name":"wrong","description":owner.project_description})
                .to_string(),
        ] {
            let error =
                validate_project_success(&response, &owner).unwrap_err();
            assert!(matches!(error, ClassifiedFailure::Permanent(_)));
        }
        let name = owner.disk_name(0, 0);
        let endpoint = format!("/v1/disks?project={}", owner.project_name);
        for response in ["not json", r#"{"name":"wrong"}"#] {
            let api = ScriptedLifecycleApi::new(vec![ok(
                "POST", &endpoint, response,
            )]);
            assert!(matches!(
                create_owned_disk(
                    &api,
                    &owner,
                    &name,
                    DiskStyle::New,
                    false,
                    Duration::ZERO,
                ),
                Err(ClassifiedFailure::Permanent(_))
            ));
            api.done();
        }
    }

    #[test]
    fn retryable_network_delete_reconciles_subsequent_not_found() {
        let endpoint = "/test/default-network-resource";
        let api = ScriptedLifecycleApi::new(vec![
            err_status(
                "DELETE",
                endpoint,
                oxide_session::ApiErrorKind::Retryable,
                500,
                "internal error after delete may have committed",
            ),
            err_status(
                "DELETE",
                endpoint,
                oxide_session::ApiErrorKind::Retryable,
                404,
                "resource is now absent",
            ),
        ]);

        delete_owned_network_resource(&api, endpoint, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == format!("DELETE {endpoint}"))
                .count(),
            2
        );
    }

    #[test]
    fn network_delete_does_not_accept_non_retryable_not_found() {
        let endpoint = "/test/default-network-resource";
        let api = ScriptedLifecycleApi::new(vec![err_status(
            "DELETE",
            endpoint,
            oxide_session::ApiErrorKind::Authentication,
            404,
            "non-retryable not found",
        )]);

        let error =
            delete_owned_network_resource(&api, endpoint, Duration::ZERO)
                .unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Permanent(_)));
        assert_eq!(api.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn network_delete_retry_exhaustion_remains_retryable() {
        let endpoint = "/test/default-network-resource";
        let api = ScriptedLifecycleApi::new(
            (0..NETWORK_DELETE_ATTEMPTS)
                .map(|_| {
                    err_status(
                        "DELETE",
                        endpoint,
                        oxide_session::ApiErrorKind::Retryable,
                        500,
                        "persistent internal error",
                    )
                })
                .collect(),
        );

        let error =
            delete_owned_network_resource(&api, endpoint, Duration::ZERO)
                .unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Retryable(_)));
        assert_eq!(
            api.calls.lock().unwrap().len(),
            NETWORK_DELETE_ATTEMPTS as usize
        );
    }

    #[test]
    fn disk_delete_retries_retryable_failure_and_accepts_absence() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            err_status(
                "DELETE",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                500,
                "delete saga failed and unwound",
            ),
            err_status(
                "DELETE",
                &endpoint,
                oxide_session::ApiErrorKind::Retryable,
                404,
                "original disk name is absent",
            ),
        ]);

        delete_owned_disk(&api, &owner, &name, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(api.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn disk_delete_recovers_after_three_service_failures() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let mut steps = (0..3)
            .map(|_| {
                err_status(
                    "DELETE",
                    &endpoint,
                    oxide_session::ApiErrorKind::Retryable,
                    503,
                    "service unavailable",
                )
            })
            .collect::<Vec<_>>();
        steps.push(ok("DELETE", &endpoint, "{}"));
        let api = ScriptedLifecycleApi::new(steps);

        delete_owned_disk(&api, &owner, &name, Duration::ZERO).unwrap();

        api.done();
        assert_eq!(api.calls.lock().unwrap().len(), 4);
    }

    #[test]
    fn disk_delete_does_not_retry_permanent_failure() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![err_status(
            "DELETE",
            &endpoint,
            oxide_session::ApiErrorKind::Authentication,
            403,
            "delete forbidden",
        )]);

        assert!(matches!(
            delete_owned_disk(&api, &owner, &name, Duration::ZERO),
            Err(ClassifiedFailure::Permanent(_))
        ));

        api.done();
        assert_eq!(api.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn disk_delete_retry_exhaustion_remains_retryable() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(
            (0..DISK_DELETE_ATTEMPTS)
                .map(|_| {
                    err_status(
                        "DELETE",
                        &endpoint,
                        oxide_session::ApiErrorKind::Retryable,
                        500,
                        "delete saga repeatedly failed",
                    )
                })
                .collect(),
        );

        let error =
            delete_owned_disk(&api, &owner, &name, Duration::ZERO).unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Retryable(_)));
        assert_eq!(
            api.calls.lock().unwrap().len(),
            DISK_DELETE_ATTEMPTS as usize
        );
    }

    #[test]
    fn cleanup_wrong_project_or_foreign_resource_performs_no_delete() {
        let owner = fixed_owner("measured");
        let wrong =
            json!({"items":[{"name":owner.project_name,"description":"foreign"}]}).to_string();
        let api =
            ScriptedLifecycleApi::new(vec![ok("GET", "/v1/projects", wrong)]);
        assert!(cleanup_owned(&api, &owner, Duration::ZERO).is_err());
        assert!(
            !api.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("DELETE "))
        );
        api.done();

        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let snapshots =
            format!(r#"{{"items":[{{"name":"{}"}}]}}"#, owner.disk_name(0, 0));
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                snapshots,
            ),
            ok(
                "GET",
                format!("/v1/disks?project={}", owner.project_name),
                r#"{"items":[{"name":"foreign"}]}"#,
            ),
        ]);
        assert!(cleanup_owned(&api, &owner, Duration::ZERO).is_err());
        let calls = api.calls.lock().unwrap();
        assert!(!calls.iter().any(|call| call.starts_with("DELETE ")));
        drop(calls);
        api.done();
    }

    #[test]
    fn cleanup_deletes_exact_faulted_delete_saga_tombstone() {
        let owner = fixed_owner("measured");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let tombstone_id = "29b8affa-2687-4864-b2c9-ab8c599600a7";
        let tombstone_name = format!("deleted-{tombstone_id}");
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let tombstone_endpoint = format!(
            "/v1/disks/{tombstone_name}?project={}",
            owner.project_name
        );
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok(
                "GET",
                &disks_endpoint,
                json!({"items":[{
                    "id": tombstone_id,
                    "name": tombstone_name,
                    "state": {"state": "faulted"},
                }]})
                .to_string(),
            ),
            ok("GET", &tombstone_endpoint, r#"{"state":{"state":"faulted"}}"#),
            ok("DELETE", &tombstone_endpoint, "{}"),
            ok("DELETE", &project_endpoint, "{}"),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        assert!(api.calls.lock().unwrap().iter().any(
            |call| call.as_str() == format!("DELETE {tombstone_endpoint}")
        ));
    }

    #[test]
    fn cleanup_rejects_spoofed_delete_saga_tombstone() {
        let owner = fixed_owner("measured");
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        for item in [
            json!({
                "id": "29b8affa-2687-4864-b2c9-ab8c599600a7",
                "name": "deleted-29b8affa-2687-4864-b2c9-ab8c599600a7",
                "state": {"state": "detached"},
            }),
            json!({
                "id": "aaaaaaaa-2687-4864-b2c9-ab8c599600a7",
                "name": "deleted-bbbbbbbb-2687-4864-b2c9-ab8c599600a7",
                "state": {"state": "faulted"},
            }),
            json!({
                "id": "29b8affa-2687-4864-b2c9-ab8c599600a7",
                "name": "deleted-29b8affa26874864b2c9ab8c599600a7",
                "state": {"state": "faulted"},
            }),
        ] {
            let api = ScriptedLifecycleApi::new(vec![ok(
                "GET",
                &disks_endpoint,
                json!({"items":[item]}).to_string(),
            )]);

            assert!(matches!(
                owned_disk_names(&api, &owner),
                Err(ClassifiedFailure::Permanent(_))
            ));
            api.done();
            assert!(
                !api.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|call| call.starts_with("DELETE "))
            );
        }
    }

    #[test]
    fn cleanup_accepts_disk_absence_between_list_and_state_poll() {
        let owner = fixed_owner("measured");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let name = owner.disk_name(0, 0);
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let disk_endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok(
                "GET",
                &disks_endpoint,
                json!({"items":[{"name":name}]}).to_string(),
            ),
            err_status(
                "GET",
                &disk_endpoint,
                oxide_session::ApiErrorKind::Retryable,
                404,
                "disk disappeared after list",
            ),
            err_status(
                "DELETE",
                &disk_endpoint,
                oxide_session::ApiErrorKind::Retryable,
                404,
                "disk remains absent",
            ),
            ok("DELETE", &project_endpoint, "{}"),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
    }

    #[test]
    fn cleanup_deletes_snapshots_disks_default_network_then_project() {
        let owner = fixed_owner("measured");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let name = owner.disk_name(0, 0);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                format!(r#"{{"items":[{{"name":"{name}"}}]}}"#),
            ),
            ok(
                "GET",
                format!("/v1/disks?project={}", owner.project_name),
                format!(r#"{{"items":[{{"name":"{name}"}}]}}"#),
            ),
            ok(
                "DELETE",
                format!("/v1/snapshots/{name}?project={}", owner.project_name),
                "{}",
            ),
            ok(
                "GET",
                format!("/v1/disks/{name}?project={}", owner.project_name),
                r#"{"state":{"state":"detached"}}"#,
            ),
            ok(
                "DELETE",
                format!("/v1/disks/{name}?project={}", owner.project_name),
                "{}",
            ),
            ok("DELETE", format!("/v1/projects/{}", owner.project_name), "{}"),
            ok("GET", "/v1/projects", &owned),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);
        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();
        api.done();
        let deletes: Vec<_> = api
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("DELETE "))
            .cloned()
            .collect();
        assert_eq!(
            deletes,
            [
                format!(
                    "DELETE /v1/snapshots/{name}?project={}",
                    owner.project_name
                ),
                format!(
                    "DELETE /v1/disks/{name}?project={}",
                    owner.project_name
                ),
                format!(
                    "DELETE /v1/internet-gateways/default?project={}&vpc=default&cascade=true",
                    owner.project_name
                ),
                format!(
                    "DELETE /v1/vpc-subnets/default?project={}&vpc=default",
                    owner.project_name
                ),
                format!(
                    "DELETE /v1/vpcs/default?project={}",
                    owner.project_name
                ),
                format!("DELETE /v1/projects/{}", owner.project_name),
            ]
        );
    }

    #[test]
    fn cleanup_retries_project_delete_until_a_late_disk_can_be_removed() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let name = owner.disk_name(0, 0);
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::ShapeRejected,
                "project generation changed",
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::ShapeRejected,
                "project gained an in-progress disk",
            ),
            ok(
                "GET",
                &disks_endpoint,
                format!(r#"{{"items":[{{"name":"{name}"}}]}}"#),
            ),
            ok(
                "GET",
                format!("/v1/disks/{name}?project={}", owner.project_name),
                r#"{"state":{"state":"detached"}}"#,
            ),
            ok(
                "DELETE",
                format!("/v1/disks/{name}?project={}", owner.project_name),
                "{}",
            ),
            ok("DELETE", &project_endpoint, "{}"),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        let expected_project_delete = format!("DELETE {project_endpoint}");
        let calls = api.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == expected_project_delete)
                .count(),
            3
        );
        for endpoint in [
            format!(
                "/v1/internet-gateways/default?project={}&vpc=default&cascade=true",
                owner.project_name
            ),
            format!(
                "/v1/vpc-subnets/default?project={}&vpc=default",
                owner.project_name
            ),
            format!("/v1/vpcs/default?project={}", owner.project_name),
        ] {
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.as_str() == format!("DELETE {endpoint}"))
                    .count(),
                1
            );
        }
        assert!(!calls.iter().any(|call| call.starts_with("POST ")));
    }

    #[test]
    fn cleanup_reconciles_retryable_project_delete_as_success_when_absent() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::Retryable,
                "internal error after commit",
            ),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
    }

    #[test]
    fn cleanup_retries_project_absence_proof_after_retryable_delete() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::Retryable,
                "HTTP 500; error_code Internal",
            ),
            err(
                "GET",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "project absence proof timed out",
            ),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
    }

    #[test]
    fn cleanup_retries_final_project_absence_poll_errors() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            ok("DELETE", &project_endpoint, "{}"),
            err(
                "GET",
                "/v1/projects",
                oxide_session::ApiErrorKind::Retryable,
                "project absence poll timed out",
            ),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
    }

    #[test]
    fn cleanup_does_not_retry_permanent_project_delete_failure() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::Authentication,
                "authentication rejected",
            ),
        ]);

        let error = cleanup_owned(&api, &owner, Duration::ZERO).unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Permanent(_)));
        assert_eq!(
            api.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str()
                    == format!("DELETE {project_endpoint}"))
                .count(),
            1
        );
    }

    #[test]
    fn cleanup_retries_retryable_project_delete_when_project_remains() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let name = owner.disk_name(0, 0);
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
            ok("GET", &disks_endpoint, r#"{"items":[]}"#),
            err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::Retryable,
                "internal error before commit",
            ),
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                &disks_endpoint,
                format!(r#"{{"items":[{{"name":"{name}"}}]}}"#),
            ),
            ok(
                "GET",
                format!("/v1/disks/{name}?project={}", owner.project_name),
                r#"{"state":{"state":"detached"}}"#,
            ),
            ok(
                "DELETE",
                format!("/v1/disks/{name}?project={}", owner.project_name),
                "{}",
            ),
            ok("DELETE", &project_endpoint, "{}"),
            ok("GET", "/v1/projects", r#"{"items":[]}"#),
        ]);

        cleanup_owned(&api, &owner, Duration::ZERO).unwrap();

        api.done();
        assert!(
            !api.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("POST "))
        );
    }

    #[test]
    fn cleanup_project_delete_reconciliation_exhaustion_is_retryable() {
        let owner = fixed_owner("probe");
        let owned =
            json!({"items":[{"name":owner.project_name,"description":owner.project_description}]})
                .to_string();
        let disks_endpoint =
            format!("/v1/disks?project={}", owner.project_name);
        let project_endpoint = format!("/v1/projects/{}", owner.project_name);
        let mut steps = vec![
            ok("GET", "/v1/projects", &owned),
            ok(
                "GET",
                format!("/v1/snapshots?project={}", owner.project_name),
                r#"{"items":[]}"#,
            ),
        ];
        for _ in 0..PROJECT_DELETE_RECONCILE_ATTEMPTS {
            steps.push(ok("GET", &disks_endpoint, r#"{"items":[]}"#));
            steps.push(err(
                "DELETE",
                &project_endpoint,
                oxide_session::ApiErrorKind::ShapeRejected,
                "project generation changed",
            ));
        }
        let api = ScriptedLifecycleApi::new(steps);

        let error = cleanup_owned(&api, &owner, Duration::ZERO).unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Retryable(_)));
        assert!(
            !api.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("POST "))
        );
    }

    #[test]
    fn cleanup_waits_for_pending_disk_and_accepts_faulted_as_deletable() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(vec![
            ok("GET", &endpoint, r#"{"state":{"state":"creating"}}"#),
            ok("GET", &endpoint, r#"{"state":{"state":"faulted"}}"#),
        ]);

        wait_owned_disk(
            &api,
            &owner,
            &name,
            Duration::ZERO,
            DiskWaitMode::Cleanup,
        )
        .unwrap();
        api.done();

        let api = ScriptedLifecycleApi::new(vec![ok(
            "GET",
            &endpoint,
            r#"{"state":{"state":"faulted"}}"#,
        )]);
        assert!(matches!(
            wait_owned_disk(
                &api,
                &owner,
                &name,
                Duration::ZERO,
                DiskWaitMode::Measured,
            ),
            Err(ClassifiedFailure::Permanent(_))
        ));
        api.done();
    }

    #[test]
    fn disk_settlement_can_recover_after_two_minutes() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let mut steps = (0..60)
            .map(|_| ok("GET", &endpoint, r#"{"state":{"state":"creating"}}"#))
            .collect::<Vec<_>>();
        steps.push(ok("GET", &endpoint, r#"{"state":{"state":"detached"}}"#));
        let api = ScriptedLifecycleApi::new(steps);

        wait_owned_disk(
            &api,
            &owner,
            &name,
            Duration::ZERO,
            DiskWaitMode::Measured,
        )
        .unwrap();

        api.done();
        assert_eq!(api.calls.lock().unwrap().len(), 61);
    }

    #[test]
    fn disk_settlement_exhaustion_uses_five_minute_bound() {
        let owner = fixed_owner("measured");
        let name = owner.disk_name(0, 0);
        let endpoint =
            format!("/v1/disks/{name}?project={}", owner.project_name);
        let api = ScriptedLifecycleApi::new(
            (0..DISK_SETTLEMENT_ATTEMPTS)
                .map(|_| {
                    ok("GET", &endpoint, r#"{"state":{"state":"creating"}}"#)
                })
                .collect(),
        );

        let error = wait_owned_disk(
            &api,
            &owner,
            &name,
            Duration::ZERO,
            DiskWaitMode::Measured,
        )
        .unwrap_err();

        api.done();
        assert!(matches!(error, ClassifiedFailure::Retryable(_)));
        assert_eq!(
            api.calls.lock().unwrap().len(),
            DISK_SETTLEMENT_ATTEMPTS as usize
        );
    }

    #[derive(Default)]
    struct PhaseConcurrency {
        active: std::sync::atomic::AtomicUsize,
        max: std::sync::atomic::AtomicUsize,
    }

    impl PhaseConcurrency {
        fn synchronize(&self, barrier: &Barrier) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max.fetch_max(active, Ordering::SeqCst);
            barrier.wait();
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct LifecycleMock {
        project: std::sync::Mutex<Option<(String, String)>>,
        disks: std::sync::Mutex<BTreeSet<String>>,
        events: std::sync::Mutex<Vec<String>>,
        posts: PhaseConcurrency,
        gets: PhaseConcurrency,
        deletes: PhaseConcurrency,
        post_barrier: Arc<Barrier>,
        get_barrier: Arc<Barrier>,
        delete_barrier: Arc<Barrier>,
    }

    impl Default for LifecycleMock {
        fn default() -> Self {
            Self {
                project: Mutex::default(),
                disks: Mutex::default(),
                events: Mutex::default(),
                posts: PhaseConcurrency::default(),
                gets: PhaseConcurrency::default(),
                deletes: PhaseConcurrency::default(),
                post_barrier: Arc::new(Barrier::new(4)),
                get_barrier: Arc::new(Barrier::new(4)),
                delete_barrier: Arc::new(Barrier::new(4)),
            }
        }
    }

    impl LifecycleApi for LifecycleMock {
        fn request(
            &self,
            endpoint: &str,
            method: &str,
            body: Option<&str>,
        ) -> std::result::Result<String, oxide_session::ApiCommandError>
        {
            self.events.lock().unwrap().push(format!("{method} {endpoint}"));
            if is_default_network_delete(method, endpoint) {
                return Ok("{}".into());
            }
            if endpoint == "/v1/system/silos/recovery/quotas" && method == "PUT"
            {
                assert_eq!(
                    serde_json::from_str::<Value>(body.unwrap()).unwrap(),
                    json!({"storage": 20u64 << 30})
                );
                return Ok(json!({"storage": 20u64 << 30}).to_string());
            }
            if endpoint == "/v1/projects" && method == "GET" {
                let item =
                    self.project.lock().unwrap().clone().map(
                        |(name, description)| json!({"name": name, "description": description}),
                    );
                return Ok(
                    json!({"items": item.into_iter().collect::<Vec<_>>()})
                        .to_string(),
                );
            }
            if endpoint == "/v1/projects" && method == "POST" {
                let value: Value = serde_json::from_str(body.unwrap()).unwrap();
                let name = value["name"].as_str().unwrap().to_string();
                let description =
                    value["description"].as_str().unwrap().to_string();
                *self.project.lock().unwrap() =
                    Some((name.clone(), description.clone()));
                return Ok(json!({"name": name, "description": description})
                    .to_string());
            }
            if endpoint.starts_with("/v1/snapshots?") && method == "GET" {
                return Ok(r#"{"items":[]}"#.into());
            }
            if endpoint.starts_with("/v1/disks?") && method == "GET" {
                let items = self
                    .disks
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|name| json!({"name": name}))
                    .collect::<Vec<_>>();
                return Ok(json!({"items": items}).to_string());
            }
            if endpoint.starts_with("/v1/disks?") && method == "POST" {
                let value: Value = serde_json::from_str(body.unwrap()).unwrap();
                let name = value["name"].as_str().unwrap().to_string();
                let probe = self
                    .project
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(name, _)| name.contains("-probe-"));
                if !probe {
                    self.posts.synchronize(&self.post_barrier);
                }
                self.disks.lock().unwrap().insert(name.clone());
                return Ok(json!({"name": name}).to_string());
            }
            if endpoint.starts_with("/v1/disks/") && method == "GET" {
                let probe = self
                    .project
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(name, _)| name.contains("-probe-"));
                if !probe {
                    self.gets.synchronize(&self.get_barrier);
                }
                return Ok(r#"{"state":{"state":"detached"}}"#.into());
            }
            if endpoint.starts_with("/v1/disks/") && method == "DELETE" {
                let probe = self
                    .project
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(name, _)| name.contains("-probe-"));
                if !probe {
                    self.deletes.synchronize(&self.delete_barrier);
                }
                let name = endpoint
                    .trim_start_matches("/v1/disks/")
                    .split('?')
                    .next()
                    .unwrap();
                self.disks.lock().unwrap().remove(name);
                return Ok("{}".into());
            }
            if endpoint.starts_with("/v1/projects/") && method == "DELETE" {
                *self.project.lock().unwrap() = None;
                return Ok("{}".into());
            }
            panic!("unexpected request {method} {endpoint}");
        }
    }

    #[test]
    fn preflight_runs_shape_probe_and_complete_disk_lifecycle() {
        let api = LifecycleMock::default();

        run_disk_lifecycle_preflight(&api, "recovery", Duration::ZERO).unwrap();

        let events = api.events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "POST /v1/projects")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("POST /v1/disks?"))
                .count(),
            21
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str()
                    == "PUT /v1/system/silos/recovery/quotas")
                .count(),
            1
        );
        assert!(api.project.lock().unwrap().is_none());
        assert!(api.disks.lock().unwrap().is_empty());
    }

    #[test]
    fn disk_lifecycle_runs_five_strict_concurrent_batches() {
        let api = LifecycleMock::default();
        let prepared = PreparedDiskLifecycle {
            api: &api,
            style: DiskStyle::New,
            poll_delay: Duration::ZERO,
        };
        prepared.run(&WorkloadSpec::api_disk_lifecycle()).unwrap();
        let events = api.events.lock().unwrap();
        assert_eq!(
            events.iter().filter(|e| e.starts_with("POST /v1/disks?")).count(),
            20
        );
        assert_eq!(api.posts.max.load(Ordering::SeqCst), 4);
        assert_eq!(api.gets.max.load(Ordering::SeqCst), 4);
        assert_eq!(api.deletes.max.load(Ordering::SeqCst), 4);
        let phases: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if event.starts_with("POST /v1/disks?") {
                    Some("POST")
                } else if event.starts_with("GET /v1/disks/") {
                    Some("GET")
                } else if event.starts_with("DELETE /v1/disks/") {
                    Some("DELETE")
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(phases.len(), 60);
        for batch in phases.chunks_exact(12) {
            assert_eq!(
                batch,
                [
                    "POST", "POST", "POST", "POST", "GET", "GET", "GET", "GET",
                    "DELETE", "DELETE", "DELETE", "DELETE"
                ]
            );
        }
        assert!(events.first().unwrap().starts_with("GET /v1/projects"));
    }

    #[test]
    fn combine_classified_retains_both_errors() {
        let error = combine_classified(
            Err(ClassifiedFailure::Retryable(anyhow!("operation marker"))),
            Err(ClassifiedFailure::Permanent(anyhow!("cleanup marker"))),
        )
        .unwrap_err();
        let text = format!("{error:?}");
        assert!(
            text.contains("operation marker")
                && text.contains("cleanup marker")
        );
        assert!(matches!(error, ClassifiedFailure::Permanent(_)));
    }

    #[test]
    fn output_preflight_and_new_writer_refuse_overwrite() {
        let root =
            std::env::temp_dir().join(format!("voxel-perftest-{}", now_secs()));
        let a = root.with_extension("csv");
        let b = root.with_extension("json");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        assert!(preflight_output_paths(Some(&a), Some(&a)).is_err());
        std::fs::write(&a, "old").unwrap();
        assert!(preflight_output_paths(Some(&a), Some(&b)).is_err());
        assert!(write_new(&a, b"new", None).is_err());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "old");
        std::fs::remove_file(&a).unwrap();
    }

    #[test]
    fn matrix_csv_hands_off_the_open_inode() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("storage-levers.csv");
        let parent = std::fs::metadata(directory.path()).unwrap();
        let owner = FileOwner { uid: parent.uid(), gid: parent.gid() };

        write_new(&destination, b"csv", Some(owner)).unwrap();

        let installed = std::fs::metadata(destination).unwrap();
        assert_eq!(installed.uid(), owner.uid);
        assert_eq!(installed.gid(), owner.gid);
        assert_eq!(installed.mode() & 0o777, 0o600);
    }

    #[test]
    fn dual_output_publication_rolls_back_the_first_file_on_failure() {
        let root = std::env::temp_dir().join(format!(
            "voxel-perftest-publish-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let csv = root.with_extension("csv");
        let json = root.with_extension("json");
        let _ = std::fs::remove_file(&csv);
        let _ = std::fs::remove_file(&json);
        std::fs::write(&json, "occupied").unwrap();

        assert!(
            publish_matrix_outputs(
                Some((&csv, b"csv")),
                Some((&json, b"json")),
                None
            )
            .is_err()
        );
        assert!(!csv.exists(), "first output must be rolled back");
        assert_eq!(std::fs::read_to_string(&json).unwrap(), "occupied");
        std::fs::remove_file(&json).unwrap();
    }

    #[test]
    fn matrix_completeness_rejects_partial_and_accepts_complete_load() {
        let mut run = run_with("x", &[("none", &[], &[1, 2])]);
        for repeat in &mut run.results[0].repeats {
            repeat.peak_ram_bytes = Some(1);
        }
        assert!(validate_matrix_run(&run).is_ok());
        run.results[0].error = Some("boom".into());
        assert!(validate_matrix_run(&run).is_err());
        run.results[0].error = None;
        run.results[0].repeats.pop();
        assert!(validate_matrix_run(&run).is_err());
        run.results[0].repeats.push(RepeatSample {
            bringup_bytes: 1,
            launch_secs: 1,
            peak_ram_bytes: None,
            ..Default::default()
        });
        assert!(validate_matrix_run(&run).is_err());
        run.results[0].repeats[1].peak_ram_bytes = Some(1);
        run.workload = Some(WorkloadSpec::api_disk_lifecycle());
        assert!(validate_matrix_run(&run).is_err());
        for repeat in &mut run.results[0].repeats {
            repeat.workload_bytes = Some(1);
            repeat.workload_secs = Some(1);
        }
        assert!(validate_matrix_run(&run).is_err());
        for repeat in &mut run.results[0].repeats {
            repeat.workload_peak_delta_bytes = Some(1);
        }
        assert!(validate_matrix_run(&run).is_ok());
        run.results[0].repeats[0].workload_secs = None;
        assert!(validate_matrix_run(&run).is_err());
        run.results[0].repeats[0].workload_secs = Some(1);
        run.combos.push("extra".into());
        assert!(validate_matrix_run(&run).is_err());

        let mut ordered =
            run_with("x", &[("none", &[], &[1, 2]), ("1", &[1], &[1, 2])]);
        for combo in &mut ordered.results {
            for repeat in &mut combo.repeats {
                repeat.peak_ram_bytes = Some(1);
            }
        }
        assert!(validate_matrix_run(&ordered).is_ok());
        ordered.results.swap(0, 1);
        assert!(validate_matrix_run(&ordered).is_err());
        ordered.results.swap(0, 1);
        ordered.results.pop();
        assert!(validate_matrix_run(&ordered).is_err());
        ordered.results.push(ComboAggregate {
            label: "1".into(),
            levers: set(&[2]),
            repeats: vec![
                RepeatSample {
                    peak_ram_bytes: Some(1),
                    ..Default::default()
                };
                2
            ],
            error: None,
        });
        assert!(validate_matrix_run(&ordered).is_err());
    }

    #[test]
    fn matrix_publication_retains_a_keep_going_failure() {
        let mut run = run_with("x", &[("none", &[], &[1, 2])]);
        run.results[0].repeats.pop();
        run.results[0].repeats[0].peak_ram_bytes = Some(1);
        run.results[0].error = Some("both attempts failed".into());

        validate_publishable_matrix_run(&run).unwrap();

        run.results[0].error = None;
        assert!(validate_publishable_matrix_run(&run).is_err());
    }

    #[test]
    fn report_shows_scoped_falcon_pool_total() {
        let before = json!({
            "label": "before", "unix_time": 0,
            "devices": [
                { "name": "nvme0", "data_units_written": 0 },
                { "name": "nvme2", "data_units_written": 0 },
            ],
            "pools": [],
        });
        // nvme0 (OS) writes a lot; nvme2 (falcon pool) writes 1000 units.
        let after = json!({
            "label": "after", "unix_time": 100,
            "devices": [
                { "name": "nvme0", "data_units_written": 100_000 },
                { "name": "nvme2", "data_units_written": 1000 },
            ],
            "pools": [],
            "falcon_pool": "voxel",
            "falcon_controllers": ["nvme2"],
        });
        let lines = report(&before, &after, None).unwrap().join("\n");
        // The falcon-pool drive is tagged and totaled; the OS drive is not.
        assert!(lines.contains("nvme2: wrote"), "got:\n{lines}");
        assert!(lines.contains("<- falcon pool"), "got:\n{lines}");
        assert!(
            lines.contains("falcon pool 'voxel' total: 512.00 MB"),
            "got:\n{lines}"
        ); // 1000 * 512000 = 5.12e8 B = 512.00 MB
        assert!(lines.contains("1 drive(s): nvme2"), "got:\n{lines}");
    }

    #[test]
    fn report_warns_when_scope_unresolved() {
        let before =
            json!({ "label": "b", "unix_time": 0, "devices": [], "pools": [] });
        let after = json!({
            "label": "a", "unix_time": 60,
            "devices": [{ "name": "nvme0", "data_units_written": 500 }],
            "pools": [],
        });
        let lines = report(&before, &after, None).unwrap().join("\n");
        assert!(
            lines.contains("falcon pool drives unresolved"),
            "got:\n{lines}"
        );
    }

    #[test]
    fn render_csv_has_lever_columns_and_rows() {
        let results = vec![
            ComboAggregate {
                label: "none".into(),
                levers: set(&[]),
                repeats: vec![RepeatSample {
                    bringup_bytes: 1000,
                    launch_secs: 10,
                    ..Default::default()
                }],
                error: None,
            },
            ComboAggregate {
                label: "1+3".into(),
                levers: set(&[1, 3]),
                repeats: vec![RepeatSample {
                    bringup_bytes: 500,
                    launch_secs: 10,
                    workload_bytes: Some(2000),
                    workload_secs: Some(5),
                    workload_peak_delta_bytes: Some(3_000_000_000),
                    ..Default::default()
                }],
                error: None,
            },
            ComboAggregate {
                label: "4".into(),
                levers: set(&[4]),
                error: Some("launch: boom, timeout".into()),
                ..Default::default()
            },
        ];
        let csv = render_csv(&results);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "combo,sync,compression,guest_zfs,reduce_replication,bringup_bytes,bringup_secs,peak_ram_bytes,workload_bytes,workload_secs,workload_peak_delta_bytes,error"
        );
        // peak_ram_bytes column is empty when the sampler didn't measure it.
        assert_eq!(lines[1], "none,0,0,0,0,1000,10,,,,,");
        assert_eq!(lines[2], "1+3,1,0,1,0,500,10,,2000,5,3000000000,");
        // Errors have their commas sanitized so the CSV stays well-formed.
        assert_eq!(lines[3], "4,0,0,0,1,0,0,,,,,launch: boom; timeout");
    }

    #[test]
    fn render_table_adapts_columns() {
        let results = vec![ComboAggregate {
            label: "1".into(),
            levers: set(&[1]),
            repeats: vec![RepeatSample {
                bringup_bytes: 2_000_000_000,
                launch_secs: 100,
                ..Default::default()
            }],
            error: None,
        }];
        // No workload, no rating, no RAM sample -> those columns absent, but
        // BRING-UP, RATE/s, and LAUNCH always show.
        let plain = render_table(&results, None).unwrap();
        assert!(plain.contains("BRING-UP") && plain.contains("RATE/s"));
        assert!(plain.contains("LAUNCH"), "got:\n{plain}");
        assert!(plain.contains("1m40s"), "launch 100s -> 1m40s:\n{plain}"); // launch_secs 100
        assert!(!plain.contains("LAUNCH ΔRAM"), "no RAM sample:\n{plain}");
        assert!(!plain.contains("WORKLOAD") && !plain.contains("YEARS"));
        assert!(plain.contains("2.00 GB"), "got:\n{plain}");
        // 2 GB over 100s is 20 MB/s; 1200 TBW lasts about 1.90 years.
        let projected = render_table(&results, Some(1200.0)).unwrap();
        assert!(projected.contains("~YEARS"));
        assert!(projected.contains("      1.90"), "got:\n{projected}");

        // Once any combo measured launch RAM delta, the LAUNCH ΔRAM column appears.
        let with_ram = vec![ComboAggregate {
            label: "1".into(),
            levers: set(&[1]),
            repeats: vec![RepeatSample {
                bringup_bytes: 2_000_000_000,
                launch_secs: 100,
                peak_ram_bytes: Some(8_000_000_000),
                ..Default::default()
            }],
            error: None,
        }];
        let ram = render_table(&with_ram, None).unwrap();
        assert!(ram.contains("LAUNCH ΔRAM"), "got:\n{ram}");
        assert!(ram.contains("8.00 GB"), "got:\n{ram}");

        let with_workload = vec![ComboAggregate {
            label: "1".into(),
            levers: set(&[1]),
            repeats: vec![RepeatSample {
                bringup_bytes: 2_000_000_000,
                launch_secs: 100,
                peak_ram_bytes: Some(8_000_000_000),
                workload_bytes: Some(1_000_000_000),
                workload_secs: Some(20),
                workload_peak_delta_bytes: Some(3_000_000_000),
            }],
            error: None,
        }];
        let workload = render_table(&with_workload, None).unwrap();
        assert!(workload.contains("WORKLOAD ΔRAM"), "got:\n{workload}");
        assert!(workload.contains("3.00 GB"), "got:\n{workload}");
    }

    #[test]
    fn human_secs_formats() {
        assert_eq!(human_secs(0), "0s");
        assert_eq!(human_secs(45), "45s");
        assert_eq!(human_secs(100), "1m40s");
        assert_eq!(human_secs(3661), "1h01m");
    }

    // ---- smooth (latency) ----

    #[test]
    fn smooth_refuses_existing_project_without_cleanup_authority() {
        let error = smooth_project_absent(
            r#"{"items":[{"name":"requested"}]}"#,
            "requested",
        )
        .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        let ownership = SmoothOwnership::default();
        assert!(!ownership.may_cleanup_project());
        assert!(ownership.disks.is_empty());
        assert!(ownership.snapshots.is_empty());
    }

    #[test]
    fn smooth_failed_project_creation_has_no_cleanup_authority() {
        let ownership = SmoothOwnership::default();
        assert!(!ownership.may_cleanup_project());
        assert!(ownership.disks.is_empty());
        assert!(ownership.snapshots.is_empty());
    }

    #[test]
    fn smooth_successful_creation_owns_project_and_resources() {
        let mut ownership = SmoothOwnership::default();
        ownership.project_created();
        ownership.disk_created("sm-0");
        ownership.snapshot_created("sm-0-snap");
        assert!(ownership.may_cleanup_project());
        assert_eq!(ownership.snapshots, BTreeSet::from(["sm-0-snap".into()]));
        assert_eq!(ownership.disks, BTreeSet::from(["sm-0".into()]));
    }

    #[test]
    fn smooth_keep_suppresses_owned_project_cleanup() {
        let mut ownership = SmoothOwnership::default();
        ownership.project_created();
        assert!(smooth_should_cleanup(&ownership, false));
        assert!(!smooth_should_cleanup(&ownership, true));
    }

    #[test]
    fn smooth_json_refuses_existing_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("smooth.json");
        std::fs::write(&path, "original").unwrap();
        assert!(publish_smooth_json(&path, b"replacement").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }

    #[test]
    fn percentile_interpolates() {
        assert_eq!(percentile(&[], 50.0), 0.0);
        assert_eq!(percentile(&[42.0], 99.0), 42.0);
        // 1..=5: p50 is the middle (3), p0 the min, p100 the max.
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&xs, 0.0), 1.0);
        assert_eq!(percentile(&xs, 50.0), 3.0);
        assert_eq!(percentile(&xs, 100.0), 5.0);
        // p90 of 1..=10 (indices 0..9): rank 0.9*9 = 8.1 -> 9 + 0.1*(10-9) = 9.1.
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert!((percentile(&ten, 90.0) - 9.1).abs() < 1e-9);
    }

    #[test]
    fn summarize_latency_orders_and_computes_jitter() {
        // Unsorted input; typical ~10ms with one 200ms stall -> high jitter.
        let ms = [10.0, 12.0, 9.0, 11.0, 200.0];
        let s = summarize_latency("create", &ms);
        assert_eq!(s.phase, "create");
        assert_eq!(s.n, 5);
        assert_eq!(s.min_ms, 9.0);
        assert_eq!(s.max_ms, 200.0);
        assert_eq!(s.p50_ms, 11.0); // median of sorted [9,10,11,12,200]
        assert!((s.mean_ms - 48.4).abs() < 1e-9);
        // p99 rank 0.99*4 = 3.96 -> 12 + 0.96*(200-12) = 192.48; jitter = p99/p50.
        assert!((s.p99_ms - 192.48).abs() < 1e-9);
        assert!((s.jitter - 192.48 / 11.0).abs() < 1e-9);
        // Empty phase -> zeros, no divide-by-zero jitter.
        let empty = summarize_latency("settle", &[]);
        assert_eq!(empty.n, 0);
        assert_eq!(empty.jitter, 0.0);
    }

    #[test]
    fn human_ms_formats() {
        assert_eq!(human_ms(0.0), "0ms");
        assert_eq!(human_ms(12.4), "12ms");
        assert_eq!(human_ms(999.0), "999ms");
        assert_eq!(human_ms(1500.0), "1.50s");
    }

    #[test]
    fn render_smooth_table_has_phases_and_jitter() {
        let summaries = vec![
            summarize_latency("create", &[10.0, 12.0, 11.0]),
            summarize_latency("settle", &[1500.0, 1600.0, 5000.0]),
        ];
        let t = render_smooth_table(&summaries);
        assert!(
            t.contains("PHASE") && t.contains("p99") && t.contains("JITTER")
        );
        assert!(t.contains("create"), "got:\n{t}");
        assert!(t.contains("settle"), "got:\n{t}");
        assert!(t.contains("1.50s"), "settle p50 in seconds:\n{t}"); // 1500ms -> 1.50s
    }

    // ---- stats ----

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn stats_empty_is_zero_and_cv_none() {
        let s = stats(&[]);
        assert_eq!(s.n, 0);
        approx(s.mean, 0.0);
        approx(s.median, 0.0);
        approx(s.stddev, 0.0);
        assert_eq!(s.cv, None);
    }

    #[test]
    fn stats_single_sample_has_zero_stddev() {
        let s = stats(&[42.0]);
        assert_eq!(s.n, 1);
        approx(s.mean, 42.0);
        approx(s.median, 42.0);
        approx(s.stddev, 0.0);
        assert_eq!(s.cv, Some(0.0));
        // A single zero value: mean 0 -> CV undefined (None), no divide-by-zero.
        assert_eq!(stats(&[0.0]).cv, None);
    }

    #[test]
    fn stats_known_values() {
        // [2,4,4,4,5,5,7,9]: mean 5.0, sum of squared deviations 32. The code
        // uses the *sample* (n-1) stddev (per its docstring), sqrt(32/7) ≈ 2.138
        // — not the population stddev 2.0.
        let s = stats(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert_eq!(s.n, 8);
        approx(s.mean, 5.0);
        approx(s.median, 4.5); // even count -> mean of the two midders (4,5)
        let sample_stddev = (32.0_f64 / 7.0).sqrt();
        approx(s.stddev, sample_stddev);
        approx(s.cv.unwrap(), sample_stddev / 5.0);
    }

    #[test]
    fn stats_median_is_order_independent() {
        approx(stats(&[9.0, 1.0, 5.0]).median, 5.0);
        approx(stats(&[5.0, 9.0, 1.0]).median, 5.0);
    }

    #[test]
    fn combo_aggregate_stats_over_repeats() {
        let combo = ComboAggregate {
            label: "1".into(),
            levers: set(&[1]),
            repeats: vec![
                RepeatSample {
                    bringup_bytes: 100,
                    launch_secs: 10,
                    peak_ram_bytes: Some(4),
                    ..Default::default()
                },
                RepeatSample {
                    bringup_bytes: 300,
                    launch_secs: 20,
                    peak_ram_bytes: None,
                    ..Default::default()
                },
            ],
            error: None,
        };
        approx(combo.bringup_bytes().mean, 200.0);
        approx(combo.launch_secs().mean, 15.0);
        // Only one repeat measured peak RAM -> n == 1 over the measured samples.
        let ram = combo.peak_ram_bytes();
        assert_eq!(ram.n, 1);
        approx(ram.mean, 4.0);
        // No workload measured anywhere.
        assert!(!combo.has_workload());
        assert_eq!(combo.workload_bytes().n, 0);
    }

    // ---- compare / JSON ----

    /// Build a `MatrixRun` from `(label, levers, bringup-bytes-per-repeat)`.
    fn run_with(name: &str, combos: &[(&str, &[u8], &[u64])]) -> MatrixRun {
        let results = combos
            .iter()
            .map(|(label, levers, bytes)| ComboAggregate {
                label: (*label).to_string(),
                levers: set(levers),
                repeats: bytes
                    .iter()
                    .map(|&b| RepeatSample {
                        bringup_bytes: b,
                        launch_secs: 10,
                        ..Default::default()
                    })
                    .collect(),
                error: None,
            })
            .collect();
        MatrixRun {
            schema_version: MATRIX_SCHEMA_VERSION,
            name: name.into(),
            started: 0,
            ended: 1,
            rated_tbw: None,
            workload: None,
            oxide_session: None,
            report_evidence: None,
            rss_sleds: 0,
            repeat: 2,
            combos: combos.iter().map(|(l, _, _)| (*l).to_string()).collect(),
            results,
        }
    }

    #[test]
    fn matrix_run_json_round_trips() {
        let run = MatrixRun {
            schema_version: MATRIX_SCHEMA_VERSION,
            name: "a4x2".into(),
            started: 1000,
            ended: 1600,
            rated_tbw: Some(1200.0),
            workload: Some(WorkloadSpec::api_disk_lifecycle()),
            oxide_session: Some(test_session_metadata()),
            report_evidence: None,
            rss_sleds: 3,
            repeat: 2,
            combos: vec!["1+2".into()],
            results: vec![ComboAggregate {
                label: "1+2".into(),
                levers: set(&[1, 2]),
                repeats: vec![
                    RepeatSample {
                        bringup_bytes: 100,
                        launch_secs: 10,
                        peak_ram_bytes: Some(2048),
                        workload_bytes: Some(50),
                        workload_secs: Some(5),
                        workload_peak_delta_bytes: Some(1024),
                    },
                    RepeatSample {
                        bringup_bytes: 120,
                        launch_secs: 12,
                        peak_ram_bytes: Some(4096),
                        workload_bytes: Some(60),
                        workload_secs: Some(6),
                        workload_peak_delta_bytes: Some(2048),
                    },
                ],
                error: None,
            }],
        };
        let json = serde_json::to_string_pretty(&run).unwrap();
        assert!(json.contains("\"schema_version\": 4"));
        assert!(!json.contains("tmpfs"));
        let back: MatrixRun = serde_json::from_str(&json).unwrap();

        assert_eq!(back.schema_version, MATRIX_SCHEMA_VERSION);
        assert_eq!(back.name, "a4x2");
        assert_eq!(back.repeat, 2);
        assert_eq!(back.rated_tbw, Some(1200.0));
        assert_eq!(back.combos, vec!["1+2".to_string()]);
        assert_eq!(back.results.len(), 1);
        let combo = &back.results[0];
        assert_eq!(combo.label, "1+2");
        assert_eq!(combo.levers, set(&[1, 2]));
        assert_eq!(combo.repeats.len(), 2);
        approx(combo.bringup_bytes().mean, 110.0);
        assert_eq!(combo.peak_ram_bytes().n, 2);
        assert_eq!(combo.workload_peak_delta_bytes().n, 2);
        validate_matrix_run(&back).unwrap();
    }

    #[test]
    fn report_evidence_round_trips_with_exact_configs_and_redacts_secrets() {
        let mut base = VoxelConfig::default();
        base.recovery_silo.user_password_hash =
            "distinctive-password-hash".into();
        base.image.cp = Some("voxel-cp-deadbeef-perf".into());
        base.falcon.dataset = Some("tank/voxel-performance".into());
        let plan = vec![
            ("none".to_string(), set(&[])),
            ("1+4".to_string(), set(&[1, 4])),
        ];
        let mut run = run_with(
            "evidence",
            &[("none", &[], &[1, 2]), ("1+4", &[1, 4], &[3, 4])],
        );
        for combo in &mut run.results {
            for repeat in &mut combo.repeats {
                repeat.peak_ram_bytes = Some(1);
            }
        }
        let evidence = build_report_evidence(
            &base,
            &plan,
            3,
            None,
            None,
            &run.results,
            run.repeat,
        );
        run.rss_sleds = 3;
        run.report_evidence = Some(evidence.clone());

        validate_matrix_run(&run).unwrap();
        let json = serde_json::to_string(&run).unwrap();
        assert!(!json.contains("distinctive-password-hash"));
        assert!(json.contains(REDACTED_CREDENTIAL));
        assert!(json.contains("tank/voxel-performance"));
        let back: MatrixRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back.report_evidence, Some(evidence));
        let combos = &back.report_evidence.as_ref().unwrap().combos;
        assert_eq!(
            combos[0].effective_config,
            apply_combo(&combos[0].effective_config, &set(&[]), 3)
        );
        assert!(combos[1].effective_config.disk_wear.host_sync_disabled);
        assert_eq!(combos[1].effective_config.topology.rss_sleds, 3);
    }

    #[test]
    fn capability_ledger_is_exact_strict_and_complete() {
        let plan = vec![("none".to_string(), set(&[]))];
        let run = run_with("ledger", &[("none", &[], &[1, 2])]);
        let evidence = build_report_evidence(
            &VoxelConfig::default(),
            &plan,
            0,
            None,
            None,
            &run.results,
            run.repeat,
        );
        let value = serde_json::to_value(&evidence.capabilities).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 5);
        assert_eq!(value["ledger_version"], 1);

        let mut run = run;
        run.report_evidence = Some(evidence);
        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["capabilities"]["surprise"] = serde_json::json!({
            "status": "pass", "evidence": "invented"
        });
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["capabilities"]["matrix_host_storage_scope"]
            ["status"] = serde_json::json!("maybe");
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["capabilities"]
            .as_object_mut()
            .unwrap()
            .remove("matrix_host_storage_scope");
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());
    }

    #[test]
    fn future_capabilities_are_not_part_of_the_current_storage_contract() {
        let plan = vec![("none".to_string(), set(&[]))];
        let run = run_with("ledger", &[("none", &[], &[1, 2])]);
        let evidence = build_report_evidence(
            &VoxelConfig::default(),
            &plan,
            0,
            None,
            None,
            &run.results,
            run.repeat,
        );
        let capabilities =
            serde_json::to_value(&evidence.capabilities).unwrap();
        for future in [
            "fleet_api_fidelity",
            "silo_api_fidelity",
            "multirack_topology_fidelity",
        ] {
            assert!(capabilities.get(future).is_none());
        }

        let mut run = run;
        run.report_evidence = Some(evidence);
        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["capabilities"]["fleet_api_fidelity"] = serde_json::json!({"status":"unavailable", "reason":"future contract"});
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());
    }

    #[test]
    fn capability_aggregation_requires_every_repeat_and_preserves_failures() {
        let mut run = run_with("ledger", &[("none", &[], &[1, 2])]);
        run.workload = Some(WorkloadSpec::api_disk_lifecycle());
        for repeat in &mut run.results[0].repeats {
            repeat.peak_ram_bytes = Some(1);
            repeat.workload_bytes = Some(1);
            repeat.workload_secs = Some(1);
            repeat.workload_peak_delta_bytes = Some(1);
        }
        let capabilities = build_capability_ledger(
            run.workload.as_ref(),
            &run.results,
            run.repeat,
        );
        assert!(matches!(
            capabilities.api_disk_lifecycle,
            CapabilityStatus::Pass { .. }
        ));
        assert!(matches!(
            capabilities.simulated_zpool_preparation,
            CapabilityStatus::Pass { .. }
        ));

        run.results[0].repeats.pop();
        run.results[0].error = Some("workload proof failed".into());
        let capabilities = build_capability_ledger(
            run.workload.as_ref(),
            &run.results,
            run.repeat,
        );
        assert!(matches!(
            capabilities.api_disk_lifecycle,
            CapabilityStatus::Fail { .. }
        ));
        assert!(matches!(
            capabilities.clean_launch_teardown_boundaries,
            CapabilityStatus::Fail { .. }
        ));

        let capabilities =
            build_capability_ledger(None, &run.results, run.repeat);
        assert!(matches!(
            capabilities.api_disk_lifecycle,
            CapabilityStatus::Unavailable { .. }
        ));
    }

    #[test]
    fn pre_evidence_v4_is_readable_and_evidence_mismatches_are_rejected() {
        let old: MatrixRun = serde_json::from_value(matrix_json(
            4,
            serde_json::json!({"workload": null, "oxide_session": null}),
        ))
        .unwrap();
        assert!(old.report_evidence.is_none());

        let plan = vec![("none".to_string(), set(&[]))];
        let mut run = run_with("bad-evidence", &[("none", &[], &[1, 2])]);
        run.report_evidence = Some(build_report_evidence(
            &VoxelConfig::default(),
            &plan,
            0,
            None,
            None,
            &run.results,
            run.repeat,
        ));
        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["combos"][0]["label"] = serde_json::json!("1");
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["base_config"]["recovery_silo"]["user_password_hash"] =
            serde_json::json!("not-redacted");
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["session"]["workload"] =
            serde_json::to_value(WorkloadSpec::api_disk_lifecycle()).unwrap();
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["combos"] = serde_json::json!([]);
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["combos"][0] = serde_json::json!("5");
        value["results"][0]["label"] = serde_json::json!("5");
        value["results"][0]["levers"] = serde_json::json!([5]);
        value["report_evidence"]["combos"][0]["label"] = serde_json::json!("5");
        value["report_evidence"]["combos"][0]["levers"] =
            serde_json::json!([5]);
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        let mut value = serde_json::to_value(&run).unwrap();
        value["report_evidence"]["provenance"]["host"] =
            serde_json::json!({"availability":"available", "value":" "});
        assert!(serde_json::from_value::<MatrixRun>(value).is_err());

        for malformed in [
            serde_json::json!({
                "availability": "available",
                "value": "host-id",
                "unexpected": true
            }),
            serde_json::json!({
                "availability": "unavailable",
                "reason": "not observable",
                "unexpected": true
            }),
        ] {
            let mut value = serde_json::to_value(&run).unwrap();
            value["report_evidence"]["provenance"]["host"] = malformed;
            assert!(serde_json::from_value::<MatrixRun>(value).is_err());
        }
    }
}
