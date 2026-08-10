//! Typed input and normalized-model foundation for `perftest report`.
//!
//! Reports are published as real, independently movable directories. A sibling
//! lock serializes cooperating writers, and a final lstat immediately precedes
//! the atomic directory rename. Thus readers see either no report or the whole
//! report, and existing destinations are not overwritten, absent a
//! non-cooperating process mutating the namespace between that lstat and rename.
//! Helios has no portable rename-with-no-replace primitive; same-UID malicious
//! namespace races are outside this contract. Archives use tempfile's
//! hard-link-based no-clobber persistence and never replace an existing archive.

use super::{
    BoundaryOutcome, LaunchOutcome, MATRIX_REPEAT_ATTEMPTS, MatrixCheckpoint,
    MatrixRun, OxideSessionMetadata, REDACTED_CREDENTIAL, RunStatus, Stats,
    WorkloadOutcome, WorkloadSpec, canonical_combo_label,
    checkpoint_capability_ledger, combined_noise_threshold, stats,
    validate_matrix_run,
};
use anyhow::{Context, Result, bail};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use voxel_config::VoxelConfig;

pub(super) mod superreport;

#[cfg(all(test, unix))]
static COMPETING_DESTINATION_INODE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(super) const REPORT_GENERATOR: &str = "voxel-perftest-report";
pub(super) const MANIFEST_SCHEMA: &str = "voxel-perftest-manifest-v1";
pub(super) const MAX_REPORT_JSON: u64 = 64 * 1024 * 1024;
pub(super) const MAX_MANIFEST_JSON: u64 = 1024 * 1024;
pub(super) const MAX_REPORT_HTML: u64 = 32 * 1024 * 1024;
const MAX_NORMALIZED_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_NORMALIZED_REPEATS: usize = 4096;
const MAX_DIMENSION_COMBINATIONS: usize = 256;
const MAX_REPLAY_STRING_BYTES: usize = 4096;
const MAX_PLANNED_REPEAT_SLOTS: usize = 4096;

/// Input provenance needed by artifact publication. Aggregates carry only the
/// validated source digest, so publication never embeds archived raw inputs.
pub(super) struct PublicationInput<'a> {
    source_name: &'a str,
    sha256: String,
}

impl<'a> PublicationInput<'a> {
    pub(super) fn new(source_name: &'a str, raw_bytes: &'a [u8]) -> Self {
        Self { source_name, sha256: sha256_hex(raw_bytes) }
    }

    fn with_digest(source_name: &'a str, sha256: String) -> Self {
        Self { source_name, sha256 }
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Manifest {
    pub(super) report_generator: String,
    pub(super) schema: String,
    generated_at_unix_seconds: u64,
    pub(super) inputs: Vec<ManifestInput>,
    pub(super) report_html: ManifestArtifact,
    pub(super) report_json: ManifestArtifact,
    pub(super) manifest_filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) aggregation: Option<AggregationMetadata>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ManifestInput {
    pub(super) source_name: String,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ManifestArtifact {
    pub(super) filename: String,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct AggregationMetadata {
    pub(super) accepted_archives: Vec<String>,
    pub(super) rejected_archives: Vec<RejectedArchive>,
    pub(super) unique_input_count: usize,
    pub(super) duplicate_count: usize,
    pub(super) digest_order: Vec<String>,
    pub(super) origins: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct RejectedArchive {
    pub(super) path: String,
    pub(super) reason: String,
}

/// Checks the exact manifest representation used by publication. Artifact
/// digests have fixed width, and `u64::MAX` covers the longest timestamp.
pub(super) fn check_manifest_size(
    inputs: &[(&str, &str)],
    aggregation: Option<&AggregationMetadata>,
    limit: u64,
) -> Result<()> {
    let dummy_digest = "0".repeat(64);
    let manifest = Manifest {
        report_generator: REPORT_GENERATOR.to_string(),
        schema: MANIFEST_SCHEMA.to_string(),
        generated_at_unix_seconds: u64::MAX,
        inputs: inputs
            .iter()
            .map(|(source_name, sha256)| ManifestInput {
                source_name: (*source_name).to_string(),
                sha256: (*sha256).to_string(),
            })
            .collect(),
        report_html: ManifestArtifact {
            filename: "report.html".to_string(),
            sha256: dummy_digest.clone(),
        },
        report_json: ManifestArtifact {
            filename: "report.json".to_string(),
            sha256: dummy_digest,
        },
        manifest_filename: "manifest.json".to_string(),
        aggregation: aggregation.cloned(),
    };
    let size = serde_json::to_vec_pretty(&manifest)
        .context("serialize candidate manifest")?
        .len() as u64;
    if size > limit {
        bail!(
            "manifest.json would use {size} bytes, exceeding {limit} byte limit"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FailurePoint {
    AfterHtml,
    DuringDerivedWrite,
    DestinationBeforeDirectoryRename,
    DuringArchive,
    DestinationBeforeArchivePersist,
}

pub(super) fn publish_report(
    out: &Path,
    archive: bool,
    inputs: &[PublicationInput<'_>],
    report_html: &[u8],
    normalized_report: &Value,
) -> Result<()> {
    publish_report_impl(
        out,
        archive,
        inputs,
        report_html,
        normalized_report,
        &[],
        None,
    )
}

fn publish_report_with_derived(
    out: &Path,
    archive: bool,
    inputs: &[PublicationInput<'_>],
    report_html: &[u8],
    normalized_report: &Value,
    derived: &[SvgArtifact],
) -> Result<()> {
    publish_report_impl(
        out,
        archive,
        inputs,
        report_html,
        normalized_report,
        derived,
        None,
    )
}

fn publish_report_impl(
    out: &Path,
    archive: bool,
    inputs: &[PublicationInput<'_>],
    report_html: &[u8],
    normalized_report: &Value,
    derived: &[SvgArtifact],
    failure: Option<FailurePoint>,
) -> Result<()> {
    let lock_path = publication_lock_path(out)?;
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "acquire report publication lock {} (another cooperating writer may be active)",
                lock_path.display()
            )
        })?;
    let result = publish_report_locked(
        out,
        archive,
        inputs,
        report_html,
        normalized_report,
        derived,
        failure,
    );
    drop(lock);
    match fs::remove_file(&lock_path) {
        Ok(()) => result,
        Err(cleanup) => match result {
            Ok(()) => Err(anyhow::anyhow!(
                "report publication succeeded, but removing publication lock {} failed: {cleanup}",
                lock_path.display()
            )),
            Err(error) => Err(anyhow::anyhow!(
                "{error:#}; removing publication lock {} also failed: {cleanup}",
                lock_path.display()
            )),
        },
    }
}

fn publish_report_locked(
    out: &Path,
    archive: bool,
    inputs: &[PublicationInput<'_>],
    report_html: &[u8],
    normalized_report: &Value,
    derived: &[SvgArtifact],
    failure: Option<FailurePoint>,
) -> Result<()> {
    if report_html.len() as u64 > MAX_REPORT_HTML {
        bail!("report.html exceeds {MAX_REPORT_HTML} byte limit");
    }
    let report_json = serde_json::to_vec_pretty(normalized_report)
        .context("serialize normalized report.json")?;
    if report_json.len() as u64 > MAX_REPORT_JSON {
        bail!("report.json exceeds {MAX_REPORT_JSON} byte limit");
    }
    let archive_path = archive_path(out)?;
    refuse_existing(out, "output directory")?;
    if archive {
        refuse_existing(&archive_path, "archive")?;
    }
    let parent = usable_parent(out);
    let leaf = out.file_name().ok_or_else(|| {
        anyhow::anyhow!("output directory must have a final path component")
    })?;
    let temporary = tempfile::Builder::new()
        .prefix(&format!(".{}.tmp-", leaf.to_string_lossy()))
        .tempdir_in(parent)
        .with_context(|| {
            format!("create temporary report directory in {}", parent.display())
        })?;

    let mut artifacts = Vec::new();
    let generation = (|| -> Result<()> {
        write_complete(&temporary.path().join("report.html"), report_html)?;
        artifacts.push(("report.html", report_html.to_vec()));
        if failure == Some(FailurePoint::AfterHtml) {
            bail!("injected publication failure after report.html");
        }
        write_complete(&temporary.path().join("report.json"), &report_json)?;
        artifacts.push(("report.json", report_json.clone()));
        let manifest = Manifest {
            report_generator: REPORT_GENERATOR.to_string(),
            schema: MANIFEST_SCHEMA.to_string(),
            generated_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock precedes Unix epoch")?
                .as_secs(),
            inputs: inputs
                .iter()
                .map(|input| ManifestInput {
                    source_name: input.source_name.to_string(),
                    sha256: input.sha256.clone(),
                })
                .collect(),
            report_html: ManifestArtifact {
                filename: "report.html".to_string(),
                sha256: sha256_hex(report_html),
            },
            report_json: ManifestArtifact {
                filename: "report.json".to_string(),
                sha256: sha256_hex(&report_json),
            },
            manifest_filename: "manifest.json".to_string(),
            aggregation: normalized_report
                .get("aggregation")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .context("serialize aggregation manifest provenance")?,
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .context("serialize manifest")?;
        if manifest_json.len() as u64 > MAX_MANIFEST_JSON {
            bail!("manifest.json exceeds {MAX_MANIFEST_JSON} byte limit");
        }
        write_complete(
            &temporary.path().join("manifest.json"),
            &manifest_json,
        )?;
        artifacts.push(("manifest.json", manifest_json));
        if !derived.is_empty() {
            let images = temporary.path().join("images");
            fs::create_dir(&images)
                .context("create derived images directory")?;
            let mut names = BTreeSet::new();
            for artifact in derived {
                validate_svg_filename(&artifact.filename)?;
                if !names.insert(&artifact.filename) {
                    bail!(
                        "duplicate derived SVG filename '{}'",
                        artifact.filename
                    );
                }
                if failure == Some(FailurePoint::DuringDerivedWrite) {
                    bail!("injected derived artifact write failure");
                }
                write_complete(
                    &images.join(&artifact.filename),
                    &artifact.bytes,
                )?;
            }
            File::open(&images)
                .and_then(|directory| directory.sync_all())
                .context("flush derived images directory")?;
        }
        File::open(temporary.path())
            .and_then(|directory| directory.sync_all())
            .with_context(|| {
                format!(
                    "flush temporary report directory {}",
                    temporary.path().display()
                )
            })?;
        Ok(())
    })();
    if let Err(error) = generation {
        return cleanup_tempdir(temporary, error);
    }

    if failure == Some(FailurePoint::DestinationBeforeDirectoryRename) {
        if let Err(error) = fs::create_dir(out)
            .context("test hook: create competing destination")
        {
            return cleanup_tempdir(temporary, error);
        }
        #[cfg(all(test, unix))]
        {
            use std::os::unix::fs::MetadataExt;
            use std::sync::atomic::Ordering;
            let metadata = match fs::metadata(out)
                .context("test hook: stat competing destination")
            {
                Ok(metadata) => metadata,
                Err(error) => return cleanup_tempdir(temporary, error),
            };
            COMPETING_DESTINATION_INODE.store(metadata.ino(), Ordering::SeqCst);
        }
    }
    if let Err(error) = refuse_existing(out, "output directory") {
        return cleanup_tempdir(temporary, error);
    }
    let temporary_path = temporary.keep();
    if let Err(error) = fs::rename(&temporary_path, out) {
        return cleanup_path(
            &temporary_path,
            anyhow::Error::new(error)
                .context(format!("publish report directory {}", out.display())),
            true,
        );
    }
    if let Err(error) = sync_published_parent(parent, "report directory", out) {
        return cleanup_path(out, error, true);
    }

    if archive {
        match publish_archive(out, &archive_path, &artifacts, failure) {
            Ok(None) => {}
            Ok(Some(error)) => {
                // The archive was persisted before this durability/cleanup
                // error. Leave both complete requested artifacts in place:
                // removing either path without an identity-checked primitive
                // could delete a non-cooperating process's replacement.
                return Err(error);
            }
            Err(error) => return cleanup_path(out, error, true),
        }
    }
    Ok(())
}

fn validate_svg_filename(filename: &str) -> Result<()> {
    let path = Path::new(filename);
    if filename.is_empty()
        || filename.len() > MAX_SVG_FILENAME_BYTES
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
        || path.extension().and_then(|extension| extension.to_str())
            != Some("svg")
    {
        bail!("unsafe derived SVG filename '{filename}'");
    }
    Ok(())
}

fn publish_archive(
    out: &Path,
    destination: &Path,
    artifacts: &[(&str, Vec<u8>)],
    failure: Option<FailurePoint>,
) -> Result<Option<anyhow::Error>> {
    let parent = usable_parent(destination);
    let mut temporary = tempfile::Builder::new()
        .prefix(".report-archive.tmp-")
        .tempfile_in(parent)
        .with_context(|| {
            format!("create temporary archive in {}", parent.display())
        })?;
    let build = (|| -> Result<()> {
        if failure == Some(FailurePoint::DuringArchive) {
            bail!("injected archive failure");
        }
        {
            let encoder =
                GzEncoder::new(temporary.as_file_mut(), Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let top = out.file_name().expect("validated output leaf");
            for (filename, bytes) in artifacts {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_mtime(0);
                header.set_cksum();
                builder
                    .append_data(
                        &mut header,
                        Path::new(top).join(filename),
                        bytes.as_slice(),
                    )
                    .with_context(|| format!("add {filename} to archive"))?;
            }
            let encoder = builder.into_inner().context("finish tar archive")?;
            encoder.finish().context("finish gzip archive")?;
        }
        temporary.as_file().sync_all().context("flush temporary archive")?;
        Ok(())
    })();
    if let Err(error) = build {
        cleanup_named_tempfile(temporary, error)?;
        unreachable!(
            "failed archive cleanup always preserves the initiating error"
        );
    }
    if failure == Some(FailurePoint::DestinationBeforeArchivePersist) {
        fs::write(destination, b"existing archive")
            .context("test hook: create competing archive")?;
    }
    let temporary_path = temporary.path().to_path_buf();
    match temporary.persist_noclobber(destination) {
        Ok(file) => drop(file),
        Err(error) => {
            let initiating = anyhow::Error::new(error.error).context(format!(
                "publish archive {} (refusing overwrite)",
                destination.display()
            ));
            cleanup_named_tempfile(error.file, initiating)?;
            unreachable!(
                "failed archive cleanup always preserves the initiating error"
            );
        }
    }
    let cleanup = match fs::remove_file(&temporary_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    let sync = sync_published_parent(parent, "archive", destination);
    Ok(finish_archive_publication(destination, &temporary_path, cleanup, sync)
        .err())
}

fn finish_archive_publication(
    destination: &Path,
    temporary_path: &Path,
    cleanup: std::io::Result<()>,
    sync: Result<()>,
) -> Result<()> {
    match (cleanup, sync) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(cleanup), Ok(())) => bail!(
            "archive {} was published and durability was confirmed, but removing leftover temporary path {} failed: {cleanup}; the leftover remains",
            destination.display(),
            temporary_path.display()
        ),
        (Ok(()), Err(sync)) => Err(sync),
        (Err(cleanup), Err(sync)) => Err(anyhow::anyhow!(
            "{sync:#}; removing leftover temporary path {} also failed: {cleanup}; the leftover remains",
            temporary_path.display()
        )),
    }
}

fn write_complete(path: &Path, bytes: &[u8]) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create artifact {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .with_context(|| format!("write artifact {}", path.display()))?;
    writer
        .get_ref()
        .sync_all()
        .with_context(|| format!("flush artifact {}", path.display()))
}

fn refuse_existing(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "{description} {} already exists; refusing to overwrite",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("check {description} {}", path.display())
            });
        }
    }
    Ok(())
}

fn sync_published_parent(
    parent: &Path,
    description: &str,
    published: &Path,
) -> Result<()> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "{description} {} was published, but durability confirmation by syncing parent directory {} failed",
                published.display(),
                parent.display()
            )
        })
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn archive_path(out: &Path) -> Result<PathBuf> {
    let leaf = out.file_name().ok_or_else(|| {
        anyhow::anyhow!("output directory must have a final path component")
    })?;
    let mut name = leaf.to_os_string();
    name.push(".tar.gz");
    Ok(usable_parent(out).join(name))
}

fn publication_lock_path(out: &Path) -> Result<PathBuf> {
    let leaf = out.file_name().ok_or_else(|| {
        anyhow::anyhow!("output directory must have a final path component")
    })?;
    let mut name = std::ffi::OsString::from(".");
    name.push(leaf);
    name.push(".publish.lock");
    Ok(usable_parent(out).join(name))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn cleanup_tempdir(
    temporary: tempfile::TempDir,
    error: anyhow::Error,
) -> Result<()> {
    combine_cleanup(error, temporary.close())
}

fn cleanup_named_tempfile(
    temporary: tempfile::NamedTempFile,
    error: anyhow::Error,
) -> Result<()> {
    combine_cleanup(error, temporary.close())
}

fn combine_cleanup(
    error: anyhow::Error,
    cleanup: std::io::Result<()>,
) -> Result<()> {
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup) => {
            Err(anyhow::anyhow!("{error:#}; cleanup also failed: {cleanup}"))
        }
    }
}

fn cleanup_path(
    path: &Path,
    error: anyhow::Error,
    directory: bool,
) -> Result<()> {
    let cleanup = if directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup) => {
            Err(anyhow::anyhow!("{error:#}; cleanup also failed: {cleanup}"))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ExperimentKind {
    StorageLevers,
    MinimumHardware,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InputIdentity {
    source: PathBuf,
    kind: ExperimentKind,
    source_schema_version: u32,
    run_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "availability", content = "details")]
enum Provenance {
    Unavailable,
    Available(ProvenanceFields),
}

#[derive(
    Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
struct ProvenanceFields {
    #[serde(default)]
    voxel_revision: Option<String>,
    #[serde(default)]
    omicron_revision: Option<String>,
    #[serde(default)]
    image_id: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    voxel_build: Option<String>,
    #[serde(default)]
    voxel_binary: Option<String>,
    #[serde(default)]
    configured_image: Option<String>,
    #[serde(default)]
    omicron_commit: Option<String>,
    #[serde(default)]
    host: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "availability", content = "results")]
enum CapabilityEvidence {
    Unavailable,
    Available(Vec<CapabilityResult>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityResult {
    capability: Capability,
    status: CapabilityStatus,
    #[serde(default)]
    evidence: Option<BoundedEvidence>,
    #[serde(default)]
    elapsed_millis: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct BoundedEvidence(Value);

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
enum Capability {
    RackReadiness,
    Metrics,
    FleetApi,
    SiloApi,
    ProjectDiskLifecycle,
    TopologyFidelity,
    CleanTeardown,
    MatrixHostStorageScope,
    CleanLaunchTeardownBoundaries,
    ApiDiskLifecycle,
    SimulatedZpoolPreparation,
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
enum CapabilityStatus {
    Pass,
    Fail,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct NormalizedRepeat {
    candidate: String,
    outcome: RepeatOutcome,
    metrics: CommonMetrics,
    payload: RepeatPayload,
}

/// Meaning of a normalized RAM value.  These are deliberately part of the
/// serialized model and cohort identity: numerically similar values measured
/// from different baselines are not interchangeable samples.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
enum MemorySemantics {
    LegacyAbsoluteHostPeak,
    LaunchBaselineDelta,
    WorkloadBaselineDelta,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "error")]
enum RepeatOutcome {
    Success,
    Failure(String),
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CommonMetrics {
    launch_duration_secs: Option<u64>,
    peak_ram_bytes: Option<u64>,
    peak_ram_semantics: Option<MemorySemantics>,
    writes_bytes: Option<u64>,
    idle_writes_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "kebab-case")]
enum RepeatPayload {
    StorageLevers(StorageRepeatPayload),
    MinimumHardware,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StorageRepeatPayload {
    levers: std::collections::BTreeSet<u8>,
    #[serde(default)]
    workload_disposition: WorkloadDisposition,
    workload_bytes: Option<u64>,
    workload_duration_secs: Option<u64>,
    workload_peak_delta_bytes: Option<u64>,
    workload_peak_ram_semantics: Option<MemorySemantics>,
    launch_failure: Option<String>,
    prior_launch_attempt_failures: Option<String>,
    preparation_failure: Option<String>,
    workload_failure: Option<String>,
    boundary_failure: Option<String>,
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize,
)]
#[serde(rename_all = "kebab-case")]
enum WorkloadDisposition {
    #[default]
    #[serde(skip)]
    Legacy,
    NotRequested,
    Pending,
    Succeeded,
    Failed,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "dimensions", rename_all = "kebab-case")]
enum Dimensions {
    StorageLevers(StorageDimensions),
    MinimumHardware(MinimumHardwareDimensions),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StorageDimensions {
    rss_sleds: usize,
    combinations: Vec<String>,
}

#[derive(
    Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
struct MinimumHardwareDimensions {
    vdev_size_bytes: u64,
    vdev_count: usize,
    control_plane_storage_buffer_bytes: u64,
    cockroachdb_redundancy: usize,
    svcadm_autoclear: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "kebab-case")]
enum ExperimentPayload {
    StorageLevers(StoragePayload),
    MinimumHardware(MinimumHardwarePayload),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoragePayload {
    started: u64,
    ended: u64,
    requested_repeats: usize,
    rated_tbw: Option<f64>,
    workload: Option<WorkloadSpec>,
    oxide_session: Option<OxideSessionMetadata>,
    effective_candidate_configurations: Option<BTreeMap<String, VoxelConfig>>,
    effective_candidate_configurations_identity: Option<String>,
    launch_memory_semantics: MemorySemantics,
    workload_memory_semantics: Option<MemorySemantics>,
    run_status: Option<RunStatus>,
    abort_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMatrixRun {
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
    load: Option<bool>,
    rss_sleds: usize,
    repeat: usize,
    combos: Vec<String>,
    results: Vec<LegacyComboAggregate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyComboAggregate {
    label: String,
    levers: std::collections::BTreeSet<u8>,
    repeats: Vec<LegacyRepeatSample>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRepeatSample {
    bringup_bytes: u64,
    launch_secs: u64,
    peak_ram_bytes: Option<u64>,
    #[serde(default)]
    workload_bytes: Option<u64>,
    #[serde(default)]
    workload_secs: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MinimumHardwarePayload {
    expected_repeats: usize,
    host_storage_capacity_bytes: u64,
    fits_host_storage_envelope: bool,
    required_allocation_bytes: u64,
    peak_allocation_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct NormalizedInput {
    identity: InputIdentity,
    capability_contract_version: Option<u32>,
    provenance: Provenance,
    effective_configuration: EffectiveConfiguration,
    dimensions: Dimensions,
    repeats: Vec<NormalizedRepeat>,
    capabilities: CapabilityEvidence,
    payload: ExperimentPayload,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    rename_all = "snake_case",
    tag = "availability",
    content = "configuration"
)]
enum EffectiveConfiguration {
    Unavailable,
    Available(VoxelConfig),
}

#[derive(Deserialize)]
struct MinimumHardwareWire {
    schema_version: u32,
    #[serde(default)]
    contract_version: Option<u32>,
    #[serde(default)]
    contract_name: Option<String>,
    identity: MinimumHardwareIdentity,
    provenance: Option<ProvenanceFields>,
    effective_configuration: VoxelConfig,
    dimensions: MinimumHardwareDimensions,
    repeats: Vec<MinimumHardwareRepeat>,
    capabilities: Option<Vec<CapabilityResult>>,
    payload: MinimumHardwarePayload,
}

#[derive(Deserialize)]
struct MinimumHardwareIdentity {
    run_id: String,
}

#[derive(Deserialize)]
struct MinimumHardwareRepeat {
    candidate: String,
    outcome: RepeatOutcome,
    #[serde(default)]
    launch_duration_secs: Option<u64>,
    #[serde(default)]
    peak_ram_bytes: Option<u64>,
    #[serde(default)]
    launch_writes_bytes: Option<u64>,
    #[serde(default)]
    idle_writes_bytes: Option<u64>,
}

#[derive(Serialize)]
struct ReportGeneratorIdentity<'a> {
    name: &'a str,
    version: &'a str,
}

#[derive(Serialize)]
struct ReportContractIdentity<'a> {
    name: &'a str,
    version: u32,
}

#[derive(Serialize)]
struct NormalizedInputMetadata<'a> {
    source: String,
    sha256: String,
    identity: &'a InputIdentity,
}

#[derive(Serialize)]
struct ReportDocument<'a> {
    schema: &'a str,
    generator: ReportGeneratorIdentity<'a>,
    contract: ReportContractIdentity<'a>,
    inputs: Vec<NormalizedInputMetadata<'a>>,
    normalized_inputs: &'a [NormalizedInput],
    analysis: &'a Analysis,
    view: &'a ReportView,
    warnings: Vec<String>,
    aggregate_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    aggregation: Option<&'a AggregationMetadata>,
}

#[derive(Debug, Deserialize)]
struct ReplayReportDocument {
    schema: String,
    generator: ReplayGeneratorIdentity,
    contract: ReplayContractIdentity,
    inputs: Vec<ReplayInputMetadata>,
    normalized_inputs: Vec<NormalizedInput>,
    #[serde(default)]
    aggregation: Option<AggregationMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayGeneratorIdentity {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayContractIdentity {
    name: String,
    version: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayInputMetadata {
    source: String,
    sha256: String,
    identity: InputIdentity,
}

#[derive(Clone, Debug)]
pub(super) struct ReplayInput {
    metadata: ReplayInputMetadata,
    normalized: NormalizedInput,
}

pub(super) struct ParsedReplayEvidence {
    pub(super) inputs: Vec<ReplayInput>,
    pub(super) aggregation: Option<AggregationMetadata>,
}

#[derive(Clone)]
pub(super) struct PreparedInput {
    source: String,
    sha256: String,
    raw_bytes: Option<Vec<u8>>,
    normalized: NormalizedInput,
}

impl PreparedInput {
    pub(super) fn digest(&self) -> &str {
        &self.sha256
    }

    pub(super) fn source(&self) -> &str {
        &self.source
    }
}

impl ReplayInput {
    pub(super) fn digest(&self) -> &str {
        &self.metadata.sha256
    }

    pub(super) fn source(&self) -> &str {
        &self.metadata.source
    }

    pub(super) fn normalized_size(&self) -> Result<usize> {
        serde_json::to_vec(&self.normalized)
            .map(|bytes| bytes.len())
            .context("serialize normalized evidence")
    }

    pub(super) fn normalized_fingerprint(&self) -> Result<String> {
        serde_json::to_vec(&self.normalized)
            .map(|bytes| sha256_hex(&bytes))
            .context("serialize normalized evidence fingerprint")
    }

    pub(super) fn into_prepared(self) -> PreparedInput {
        PreparedInput {
            source: self.metadata.source,
            sha256: self.metadata.sha256,
            raw_bytes: None,
            normalized: self.normalized,
        }
    }
}

pub(super) fn parse_normalized_report_evidence(
    bytes: &[u8],
) -> Result<Vec<ReplayInput>> {
    Ok(parse_normalized_report_document(bytes)?.inputs)
}

pub(super) fn parse_normalized_report_document(
    bytes: &[u8],
) -> Result<ParsedReplayEvidence> {
    let mut document: ReplayReportDocument = serde_json::from_slice(bytes)
        .context("deserialize normalized report evidence")?;
    for input in &mut document.normalized_inputs {
        upgrade_legacy_workload_dispositions(input)?;
    }
    if document.schema != "voxel-perftest-report-v1" {
        bail!("unsupported normalized report schema '{}'", document.schema);
    }
    if document.generator.name != REPORT_GENERATOR
        || document.generator.version.is_empty()
    {
        bail!("normalized report generator identity is incompatible");
    }
    if document.contract.name != CAPABILITY_CONTRACT_NAME
        || document.contract.version != CAPABILITY_CONTRACT_VERSION
    {
        bail!("normalized report capability contract is incompatible");
    }
    if document.inputs.len() != document.normalized_inputs.len() {
        bail!("normalized report metadata and evidence counts differ");
    }

    let inputs = document
        .inputs
        .into_iter()
        .zip(document.normalized_inputs)
        .map(|(metadata, normalized)| {
            if metadata.sha256.len() != 64
                || !metadata
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("input digest is not a 64-lowercase-hex SHA-256 value");
            }
            if metadata.source != normalized.identity.source.display().to_string()
                || metadata.identity != normalized.identity
            {
                bail!("input metadata identity does not agree with normalized evidence");
            }
            validate_normalized_input(&normalized)?;
            Ok(ReplayInput {
                metadata,
                normalized,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(aggregation) = &document.aggregation {
        let digests = inputs
            .iter()
            .map(|input| input.digest().to_string())
            .collect::<Vec<_>>();
        let retained =
            digests.iter().cloned().collect::<std::collections::BTreeSet<_>>();
        if retained.len() != inputs.len() {
            bail!("normalized report aggregation input digests are not unique");
        }
        let origin_keys = aggregation
            .origins
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let occurrence_count = aggregation
            .origins
            .values()
            .try_fold(0usize, |count, origins| count.checked_add(origins.len()))
            .context(
                "normalized report aggregation occurrence count overflow",
            )?;
        let duplicate_count =
            occurrence_count.checked_sub(inputs.len()).context(
                "normalized report aggregation has fewer origins than inputs",
            )?;
        if aggregation.unique_input_count != inputs.len()
            || aggregation.digest_order != digests
            || origin_keys != retained
            || aggregation.origins.values().any(Vec::is_empty)
            || aggregation.duplicate_count != duplicate_count
        {
            bail!("normalized report aggregation provenance is inconsistent");
        }
    }
    Ok(ParsedReplayEvidence { inputs, aggregation: document.aggregation })
}

fn upgrade_legacy_workload_dispositions(
    input: &mut NormalizedInput,
) -> Result<()> {
    let workload_requested = matches!(
        &input.payload,
        ExperimentPayload::StorageLevers(StoragePayload {
            workload: Some(_),
            ..
        })
    );
    for repeat in &mut input.repeats {
        let RepeatPayload::StorageLevers(payload) = &mut repeat.payload else {
            continue;
        };
        if payload.workload_disposition != WorkloadDisposition::Legacy {
            continue;
        }
        if payload.launch_failure.is_some()
            || payload.prior_launch_attempt_failures.is_some()
            || payload.preparation_failure.is_some()
            || payload.workload_failure.is_some()
            || payload.boundary_failure.is_some()
        {
            bail!(
                "missing workload disposition alongside checkpoint diagnostics"
            );
        }
        payload.workload_disposition = if !workload_requested {
            WorkloadDisposition::NotRequested
        } else if payload.workload_bytes.is_some()
            && payload.workload_duration_secs.is_some()
        {
            WorkloadDisposition::Succeeded
        } else {
            WorkloadDisposition::Pending
        };
    }
    Ok(())
}

fn validate_normalized_input(input: &NormalizedInput) -> Result<()> {
    let serialized_size = serde_json::to_vec(input)
        .context("serialize normalized input for resource validation")?
        .len();
    if serialized_size > MAX_NORMALIZED_INPUT_BYTES {
        bail!(
            "normalized input exceeds {MAX_NORMALIZED_INPUT_BYTES} byte limit"
        );
    }
    if input.repeats.len() > MAX_NORMALIZED_REPEATS {
        bail!("normalized input exceeds {MAX_NORMALIZED_REPEATS} repeat limit");
    }
    let check_string = |name: &str, value: &str| -> Result<()> {
        if value.len() > MAX_REPLAY_STRING_BYTES {
            bail!("{name} exceeds {MAX_REPLAY_STRING_BYTES} byte limit");
        }
        Ok(())
    };
    check_string("run_id", &input.identity.run_id)?;
    check_string("source", &input.identity.source.display().to_string())?;
    if let Provenance::Available(provenance) = &input.provenance {
        for value in [
            &provenance.voxel_revision,
            &provenance.omicron_revision,
            &provenance.image_id,
            &provenance.host_id,
            &provenance.voxel_build,
            &provenance.voxel_binary,
            &provenance.configured_image,
            &provenance.omicron_commit,
            &provenance.host,
        ]
        .into_iter()
        .flatten()
        {
            check_string("provenance string", value)?;
        }
    }
    let storage = match (&input.dimensions, &input.payload) {
        (
            Dimensions::StorageLevers(dimensions),
            ExperimentPayload::StorageLevers(payload),
        ) if input.identity.kind == ExperimentKind::StorageLevers => {
            if dimensions.combinations.is_empty()
                || dimensions.combinations.len() > MAX_DIMENSION_COMBINATIONS
                || dimensions
                    .combinations
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != dimensions.combinations.len()
            {
                bail!(
                    "storage dimension candidate labels must be nonempty and unique"
                );
            }
            for combination in &dimensions.combinations {
                check_string("storage combination", combination)?;
            }
            if payload.requested_repeats == 0
                || payload.requested_repeats > MAX_PLANNED_REPEAT_SLOTS
            {
                bail!(
                    "storage requested_repeats must be greater than zero and at most {MAX_PLANNED_REPEAT_SLOTS}"
                );
            }
            let planned = payload
                .requested_repeats
                .checked_mul(dimensions.combinations.len())
                .context("storage planned repeat count overflow")?;
            if planned > MAX_PLANNED_REPEAT_SLOTS
                || input.repeats.len() > planned
            {
                bail!(
                    "storage normalized repeats exceed bounded planned slots"
                );
            }
            Some((dimensions, payload))
        }
        (
            Dimensions::MinimumHardware(_),
            ExperimentPayload::MinimumHardware(_),
        ) if input.identity.kind == ExperimentKind::MinimumHardware => {
            let ExperimentPayload::MinimumHardware(payload) = &input.payload
            else {
                unreachable!()
            };
            if !matches!(
                input.effective_configuration,
                EffectiveConfiguration::Available(_)
            ) {
                bail!(
                    "minimum-hardware effective configuration must be available"
                );
            }
            if payload.expected_repeats == 0
                || payload.expected_repeats > MAX_PLANNED_REPEAT_SLOTS
            {
                bail!(
                    "minimum-hardware expected_repeats must be greater than zero and at most {MAX_PLANNED_REPEAT_SLOTS}"
                );
            }
            if input.repeats.len() > payload.expected_repeats {
                bail!(
                    "minimum-hardware normalized repeats exceed planned slots"
                );
            }
            None
        }
        _ => bail!(
            "normalized input kind, dimensions, and payload are inconsistent"
        ),
    };

    match (&input.capabilities, input.capability_contract_version) {
        (CapabilityEvidence::Unavailable, None) => {}
        (CapabilityEvidence::Available(results), Some(version))
            if input.identity.kind == ExperimentKind::MinimumHardware =>
        {
            validate_capabilities(
                Some(CAPABILITY_CONTRACT_NAME),
                Some(version),
                Some(results),
            )?
        }
        (
            CapabilityEvidence::Available(results),
            Some(CAPABILITY_CONTRACT_VERSION),
        ) => {
            let required = [
                Capability::MatrixHostStorageScope,
                Capability::CleanLaunchTeardownBoundaries,
                Capability::ApiDiskLifecycle,
                Capability::SimulatedZpoolPreparation,
            ];
            if results.len() != required.len()
                || required.iter().any(|capability| {
                    results
                        .iter()
                        .filter(|result| result.capability == *capability)
                        .count()
                        != 1
                })
            {
                bail!(
                    "storage capability contract requires one result for each capability"
                );
            }
            validate_capability_result_shapes(results)?;
        }
        _ => bail!(
            "normalized capability evidence and contract version are incompatible"
        ),
    }
    if let CapabilityEvidence::Available(results) = &input.capabilities {
        for error in results.iter().filter_map(|result| result.error.as_deref())
        {
            check_string("capability error", error)?;
        }
    }

    for repeat in &input.repeats {
        check_string("candidate", &repeat.candidate)?;
        if let RepeatOutcome::Failure(error) = &repeat.outcome {
            check_string("repeat failure", error)?;
        }
        match (storage, &repeat.payload) {
            (
                Some((dimensions, payload)),
                RepeatPayload::StorageLevers(details),
            ) => {
                if details.workload_disposition == WorkloadDisposition::Legacy {
                    bail!("legacy workload disposition was not upgraded");
                }
                for value in [
                    &details.launch_failure,
                    &details.prior_launch_attempt_failures,
                    &details.preparation_failure,
                    &details.workload_failure,
                    &details.boundary_failure,
                ]
                .into_iter()
                .flatten()
                {
                    check_string("storage diagnostic", value)?;
                }
                if repeat.metrics.peak_ram_bytes.is_some()
                    != repeat.metrics.peak_ram_semantics.is_some()
                {
                    bail!(
                        "peak RAM value and memory semantics must be present together"
                    );
                }
                let canonical = canonical_combo_label(&details.levers);
                if repeat.candidate != canonical
                    || !dimensions.combinations.contains(&canonical)
                {
                    bail!(
                        "storage candidate label and lever set are inconsistent"
                    );
                }
                if repeat.metrics.peak_ram_bytes.is_some()
                    && repeat.metrics.peak_ram_semantics
                        != Some(payload.launch_memory_semantics)
                {
                    bail!("launch memory semantics are inconsistent");
                }
                if details.workload_peak_delta_bytes.is_some()
                    != details.workload_peak_ram_semantics.is_some()
                    || (details.workload_peak_delta_bytes.is_some()
                        && details.workload_peak_ram_semantics
                            != payload.workload_memory_semantics)
                {
                    bail!(
                        "workload value and memory semantics are inconsistent"
                    );
                }
            }
            (None, RepeatPayload::MinimumHardware) => {
                if repeat.candidate != input.identity.run_id {
                    bail!(
                        "minimum-hardware candidate does not match run identity"
                    );
                }
                if repeat.metrics.peak_ram_semantics.is_some() {
                    bail!(
                        "minimum-hardware memory semantics must remain unavailable"
                    );
                }
            }
            _ => bail!("normalized repeat payload kind is inconsistent"),
        }
    }
    if let ExperimentPayload::StorageLevers(payload) = &input.payload {
        if let Some(error) = &payload.abort_error {
            check_string("payload abort error", error)?;
        }
    }
    Ok(())
}

pub(super) fn run(inputs: &[PathBuf], out: &Path, archive: bool) -> Result<()> {
    if inputs.is_empty() {
        bail!("at least one report input is required");
    }

    // Refuse collisions before touching inputs, both for predictable errors and
    // so an existing destination is never affected by malformed input.
    refuse_existing(out, "output directory")?;
    let requested_archive = archive_path(out)?;
    if archive {
        refuse_existing(&requested_archive, "archive")?;
    }

    let mut prepared = Vec::with_capacity(inputs.len());
    for path in inputs {
        let bytes = fs::read(path)
            .with_context(|| format!("read report input {}", path.display()))?;
        let value: Value =
            serde_json::from_slice(&bytes).with_context(|| {
                format!("parse report input {}", path.display())
            })?;
        let input = normalize(path, value).with_context(|| {
            format!("invalid report input {}", path.display())
        })?;
        prepared.push(PreparedInput {
            source: path.display().to_string(),
            sha256: sha256_hex(&bytes),
            raw_bytes: Some(bytes),
            normalized: input,
        });
    }

    generate_and_publish_report(
        &prepared,
        out,
        archive,
        &requested_archive,
        None,
    )
}

pub(super) fn generate_and_publish_report(
    prepared: &[PreparedInput],
    out: &Path,
    archive: bool,
    requested_archive: &Path,
    aggregation: Option<&AggregationMetadata>,
) -> Result<()> {
    let candidate = build_candidate_report(prepared, aggregation)?;
    let replay = parse_normalized_report_evidence(
        &serde_json::to_vec(&candidate.document)
            .context("serialize replay validation model")?,
    )
    .context("validate normalized report replay boundary")?;
    if replay.len() != prepared.len()
        || replay.iter().zip(prepared).any(|(replay, prepared)| {
            replay.metadata.source != prepared.source
                || replay.metadata.sha256 != prepared.sha256
                || replay.normalized != prepared.normalized
        })
    {
        bail!("normalized report replay changed prepared input evidence");
    }
    let publication_inputs = prepared
        .iter()
        .map(|input| match &input.raw_bytes {
            Some(bytes) => PublicationInput::new(&input.source, bytes),
            None => PublicationInput::with_digest(
                &input.source,
                input.sha256.clone(),
            ),
        })
        .collect::<Vec<_>>();
    if aggregation.is_some() {
        let derived = render_report_svgs(&candidate.view)?;
        publish_report_with_derived(
            out,
            archive,
            &publication_inputs,
            &candidate.html,
            &candidate.document,
            &derived,
        )?;
    } else {
        publish_report(
            out,
            archive,
            &publication_inputs,
            &candidate.html,
            &candidate.document,
        )?;
    }

    print!(
        "{}",
        format_run_summary(
            &candidate.normalized,
            &candidate.analysis,
            candidate.eligible
        )
    );
    println!("report: {}", out.display());
    if archive {
        println!("archive: {}", requested_archive.display());
    }
    Ok(())
}

struct CandidateReport {
    normalized: Vec<NormalizedInput>,
    analysis: Analysis,
    eligible: usize,
    html: Vec<u8>,
    document: Value,
    view: ReportView,
}

fn build_candidate_report(
    prepared: &[PreparedInput],
    aggregation: Option<&AggregationMetadata>,
) -> Result<CandidateReport> {
    let normalized = prepared
        .iter()
        .map(|input| input.normalized.clone())
        .collect::<Vec<_>>();
    let digests = prepared
        .iter()
        .map(|input| InputDigestView {
            source: input.source.clone(),
            sha256: Some(input.sha256.clone()),
            run_status: None,
            evidence_state: None,
            abort_error: None,
        })
        .collect::<Vec<_>>();
    let analysis = analyze(&normalized);
    let mut view = build_report_view(&normalized, &analysis, &digests)?;
    view.aggregation = aggregation.cloned();
    let html = render_report_html(&view)
        .context("render offline report HTML")?
        .into_bytes();
    let mut warnings = view
        .sections
        .iter()
        .flat_map(|section| {
            section.warnings.iter().cloned().chain(
                section
                    .cohorts
                    .iter()
                    .filter_map(|cohort| cohort.warning.clone()),
            )
        })
        .collect::<Vec<_>>();
    if let Some(aggregation) = aggregation {
        warnings.extend(aggregation.rejected_archives.iter().map(|rejected| {
            format!("Rejected archive {}: {}", rejected.path, rejected.reason)
        }));
    }
    let eligible = analysis
        .cohorts
        .iter()
        .flat_map(|cohort| &cohort.candidates)
        .filter(|candidate| candidate.ineligibility.is_empty())
        .count();
    let document = ReportDocument {
        schema: "voxel-perftest-report-v1",
        generator: ReportGeneratorIdentity {
            name: REPORT_GENERATOR,
            version: env!("CARGO_PKG_VERSION"),
        },
        contract: ReportContractIdentity {
            name: CAPABILITY_CONTRACT_NAME,
            version: CAPABILITY_CONTRACT_VERSION,
        },
        inputs: normalized
            .iter()
            .zip(prepared)
            .map(|(input, prepared)| NormalizedInputMetadata {
                source: input.identity.source.display().to_string(),
                sha256: prepared.sha256.clone(),
                identity: &input.identity,
            })
            .collect(),
        normalized_inputs: &normalized,
        analysis: &analysis,
        view: &view,
        warnings,
        aggregate_status: format!(
            "{} input(s), {} cohort(s), {} eligible candidate(s)",
            normalized.len(),
            analysis.cohorts.len(),
            eligible
        ),
        aggregation,
    };
    let document = serde_json::to_value(document)
        .context("serialize normalized report model")?;
    Ok(CandidateReport { normalized, analysis, eligible, html, document, view })
}

pub(super) fn check_candidate_report_size(
    prepared: &[PreparedInput],
    aggregation: &AggregationMetadata,
    json_limit: u64,
    html_limit: u64,
) -> Result<()> {
    let candidate = build_candidate_report(prepared, Some(aggregation))?;
    let json_size = serde_json::to_vec_pretty(&candidate.document)
        .context("serialize candidate report.json")?
        .len() as u64;
    if json_size > json_limit {
        bail!(
            "report.json would use {json_size} bytes, exceeding {json_limit} byte limit"
        );
    }
    let html_size = candidate.html.len() as u64;
    if html_size > html_limit {
        bail!(
            "report.html would use {html_size} bytes, exceeding {html_limit} byte limit"
        );
    }
    Ok(())
}

fn format_run_summary(
    inputs: &[NormalizedInput],
    analysis: &Analysis,
    eligible: usize,
) -> String {
    let storage = inputs
        .iter()
        .filter(|input| input.identity.kind == ExperimentKind::StorageLevers)
        .count();
    let hardware = inputs.len() - storage;
    let mut summary = format!(
        "inputs: {} accepted, 0 rejected\nexperiment kinds: storage-levers={storage}, minimum-hardware={hardware}\ncohorts: {}; eligible candidates: {eligible}\n",
        inputs.len(),
        analysis.cohorts.len()
    );
    for cohort in &analysis.cohorts {
        let recommendation = cohort
            .recommendation
            .as_ref()
            .map(|recommendation| recommendation.display.as_str())
            .unwrap_or("none");
        let kind = match &cohort.key {
            CohortKey::Storage(_) => "storage-levers",
            CohortKey::MinimumHardware(_) => "minimum-hardware",
        };
        let serialized =
            serde_json::to_vec(&cohort.key).expect("cohort key serializes");
        let stable_id = &sha256_hex(&serialized)[..12];
        summary.push_str(&format!(
            "cohort {kind}/{stable_id} recommendation: {recommendation}\n"
        ));
    }
    summary
}

#[cfg(test)]
fn load(path: &Path) -> Result<NormalizedInput> {
    let bytes = fs::read(path)
        .with_context(|| format!("read report input {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse report input {}", path.display()))?;
    normalize(path, value)
        .with_context(|| format!("invalid report input {}", path.display()))
}

fn normalize(path: &Path, value: Value) -> Result<NormalizedInput> {
    let object = value.as_object().ok_or_else(|| {
        anyhow::anyhow!("unknown perftest input shape: expected a JSON object")
    })?;
    match object.get("kind") {
        Some(Value::String(kind)) if kind == "minimum-hardware" => {
            normalize_minimum_hardware(path, value)
        }
        Some(Value::String(kind)) => {
            bail!(
                "unsupported perftest input kind '{kind}'; supported kind is 'minimum-hardware'"
            )
        }
        Some(_) => bail!("perftest input kind must be a string"),
        None => {
            let source_version = object
                .get("schema_version")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "matrix schema_version must be an unsigned integer"
                    )
                })?;
            if matches!(source_version, 2 | 3) {
                let legacy: LegacyMatrixRun = serde_json::from_value(value)
                    .context("deserialize historical storage matrix")?;
                validate_legacy_matrix(&legacy)
                    .context("validate historical storage matrix semantics")?;
                return Ok(normalize_legacy_matrix(path, legacy));
            }
            if source_version == 5 {
                let checkpoint: MatrixCheckpoint = serde_json::from_value(
                    value,
                )
                .context("deserialize schema-v5 storage matrix checkpoint")?;
                validate_matrix_checkpoint(&checkpoint).context(
                    "validate schema-v5 storage matrix checkpoint semantics",
                )?;
                return Ok(normalize_matrix_checkpoint(path, checkpoint));
            }
            let matrix: MatrixRun = serde_json::from_value(value).context(
                "deserialize authoritative schema-v4 storage matrix",
            )?;
            if let Err(error) = validate_matrix_run(&matrix) {
                if matrix.results.iter().any(|result| result.error.is_some()) {
                    validate_report_failed_matrix(&matrix)
                        .context("validate retained failed storage matrix")?;
                } else {
                    return Err(error)
                        .context("validate storage matrix semantics");
                }
            }
            Ok(normalize_matrix(path, source_version, matrix))
        }
    }
}

/// Reporting may retain a matrix aggregate that stopped after an execution
/// failure. Compare and normal matrix readers intentionally remain strict.
pub(super) fn validate_report_failed_matrix(matrix: &MatrixRun) -> Result<()> {
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

fn validate_legacy_matrix(matrix: &LegacyMatrixRun) -> Result<()> {
    if !matches!(matrix.schema_version, 2 | 3) {
        bail!("historical matrix adapter only accepts schema v2 or v3");
    }
    if matrix.schema_version == 2 {
        if matrix.load != Some(false)
            || matrix.workload.is_some()
            || matrix.oxide_session.is_some()
        {
            bail!(
                "schema v2 requires load=false and cannot contain v3 workload metadata"
            );
        }
    } else if matrix.load.is_some()
        || matrix.workload.is_some() != matrix.oxide_session.is_some()
    {
        bail!(
            "schema v3 workload and oxide_session must agree and cannot contain load"
        );
    }
    if matrix.repeat == 0 || matrix.combos.len() != matrix.results.len() {
        bail!("historical matrix has invalid repeat or result count");
    }
    for (planned, result) in matrix.combos.iter().zip(&matrix.results) {
        if result.label != *planned
            || result.label != canonical_combo_label(&result.levers)
        {
            bail!("historical matrix combo identity mismatch");
        }
        if result.error.is_some() || result.repeats.len() != matrix.repeat {
            bail!(
                "historical matrix must contain all requested successful repeats"
            );
        }
        for repeat in &result.repeats {
            if repeat.peak_ram_bytes.is_none() {
                bail!(
                    "historical matrix repeat is missing absolute peak_ram_bytes"
                );
            }
            if repeat.workload_bytes.is_some() != repeat.workload_secs.is_some()
                || matrix.workload.is_some() != repeat.workload_bytes.is_some()
            {
                bail!(
                    "historical matrix repeat has incomplete workload bytes/time fields"
                );
            }
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
                matches!(repeat.pre_boundary, BoundaryOutcome::Pending)
                    || matches!(repeat.launch, LaunchOutcome::Pending)
                    || (matches!(
                        repeat.preparation,
                        super::PreparationOutcome::Pending
                    ) && !matches!(
                        repeat.launch,
                        LaunchOutcome::Failure { .. }
                    ))
                    || (matches!(repeat.workload, WorkloadOutcome::Pending)
                        && !matches!(
                            repeat.launch,
                            LaunchOutcome::Failure { .. }
                        ))
                    || matches!(repeat.post_boundary, BoundaryOutcome::Pending)
                    || match &repeat.launch {
                        LaunchOutcome::Success {
                            prior_attempt_failures,
                            ..
                        } => prior_attempt_failures.iter().any(|failure| {
                            matches!(
                                failure.clean_boundary,
                                BoundaryOutcome::Pending
                            )
                        }),
                        LaunchOutcome::Failure { attempt_failures } => {
                            attempt_failures.iter().any(|failure| {
                                matches!(
                                    failure.clean_boundary,
                                    BoundaryOutcome::Pending
                                )
                            })
                        }
                        LaunchOutcome::Pending => false,
                    };
            if completed && has_pending {
                bail!("completed matrix contains a pending repeat stage");
            }
            let boundary_has_empty_failure = |boundary: &BoundaryOutcome| matches!(boundary, BoundaryOutcome::Failure { error } if error.is_empty());
            let workload_is_expected_pre_launch =
                |workload: &WorkloadOutcome| {
                    matches!(
                        (&matrix.workload, workload),
                        (Some(_), WorkloadOutcome::Pending)
                            | (None, WorkloadOutcome::NotRequested)
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
                    super::PreparationOutcome::NotRequested,
                    WorkloadOutcome::NotRequested,
                    _,
                ) => {}
                (
                    Some(_),
                    super::PreparationOutcome::Pending,
                    WorkloadOutcome::Pending,
                    LaunchOutcome::Pending | LaunchOutcome::Success { .. },
                ) => {}
                (
                    Some(_),
                    super::PreparationOutcome::Success,
                    WorkloadOutcome::Pending
                    | WorkloadOutcome::Success { .. }
                    | WorkloadOutcome::Failure { .. },
                    LaunchOutcome::Success { .. },
                ) => {}
                (
                    Some(_),
                    super::PreparationOutcome::Failure { error },
                    WorkloadOutcome::Pending,
                    LaunchOutcome::Success { .. },
                ) if !completed
                    && !error.is_empty()
                    && matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending
                    ) => {}
                (
                    Some(_),
                    super::PreparationOutcome::Failure { error },
                    WorkloadOutcome::Failure { error: workload_error },
                    LaunchOutcome::Success { .. },
                ) if !error.is_empty()
                    && workload_error.contains(
                        "blocked by simulated zpool preparation failure",
                    ) => {}
                (
                    Some(_),
                    super::PreparationOutcome::Pending,
                    WorkloadOutcome::Pending,
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
            if matches!(repeat.pre_boundary, BoundaryOutcome::Pending)
                && (!matches!(repeat.launch, LaunchOutcome::Pending)
                    || !workload_is_expected_pre_launch(&repeat.workload)
                    || !matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending
                    ))
            {
                bail!("pending pre-boundary cannot have later stage evidence");
            }
            if matches!(repeat.pre_boundary, BoundaryOutcome::Failure { .. })
                && (!matches!(repeat.launch, LaunchOutcome::Pending)
                    || !workload_is_expected_pre_launch(&repeat.workload)
                    || !matches!(
                        repeat.post_boundary,
                        BoundaryOutcome::Pending
                    ))
            {
                bail!(
                    "failed pre-boundary cannot have launch or workload evidence"
                );
            }
            match &repeat.launch {
                LaunchOutcome::Pending
                    if !workload_is_expected_pre_launch(&repeat.workload)
                        || !matches!(
                            repeat.post_boundary,
                            BoundaryOutcome::Pending
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
                                BoundaryOutcome::Clean
                            )
                        })
                    {
                        bail!(
                            "failed launch has a dirty non-final attempt boundary"
                        );
                    }
                    match &attempt_failures.last().unwrap().clean_boundary {
                        BoundaryOutcome::Pending
                            if !completed
                                && matches!(
                                    repeat.post_boundary,
                                    BoundaryOutcome::Pending
                                ) => {}
                        BoundaryOutcome::Failure { error }
                            if matches!(
                                &repeat.post_boundary,
                                BoundaryOutcome::Failure { error: post_error }
                                    if post_error == error
                            ) => {}
                        BoundaryOutcome::Clean
                            if attempt_failures.len()
                                == MATRIX_REPEAT_ATTEMPTS
                                && matches!(
                                    repeat.post_boundary,
                                    BoundaryOutcome::Clean
                                ) => {}
                        BoundaryOutcome::Clean
                            if !completed
                                && attempt_failures.len()
                                    < MATRIX_REPEAT_ATTEMPTS
                                && matches!(
                                    repeat.post_boundary,
                                    BoundaryOutcome::Pending
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
                                BoundaryOutcome::Clean
                            )
                    }) || prior_attempt_failures.len()
                        >= MATRIX_REPEAT_ATTEMPTS
                    {
                        bail!(
                            "successful launch has invalid prior-attempt failure evidence"
                        );
                    }
                    match (&matrix.workload, &repeat.workload) {
                        (None, WorkloadOutcome::NotRequested)
                        | (Some(_), WorkloadOutcome::Pending)
                        | (Some(_), WorkloadOutcome::Success { .. }) => {}
                        (Some(_), WorkloadOutcome::Failure { error })
                            if !error.is_empty() => {}
                        _ => bail!(
                            "workload outcome disagrees with requested workload"
                        ),
                    }
                    if matrix.workload.is_some()
                        && matches!(repeat.workload, WorkloadOutcome::Pending)
                        && !matches!(
                            repeat.post_boundary,
                            BoundaryOutcome::Pending
                        )
                    {
                        bail!(
                            "pending requested workload requires a pending post-boundary"
                        );
                    }
                }
                LaunchOutcome::Pending => {}
            }
            if completed
                && (!matches!(repeat.pre_boundary, BoundaryOutcome::Clean)
                    || matches!(repeat.launch, LaunchOutcome::Pending)
                    || !matches!(repeat.post_boundary, BoundaryOutcome::Clean))
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

fn normalize_matrix_checkpoint(
    path: &Path,
    matrix: MatrixCheckpoint,
) -> NormalizedInput {
    let workload_memory_semantics = matrix
        .workload
        .as_ref()
        .map(|_| MemorySemantics::WorkloadBaselineDelta);
    let effective_candidate_configurations = Some(
        matrix
            .combos
            .iter()
            .map(|combo| (combo.label.clone(), combo.effective_config.clone()))
            .collect::<BTreeMap<_, _>>(),
    );
    let effective_candidate_configurations_identity =
        effective_candidate_configurations.as_ref().map(|configs| {
            serde_json::to_string(configs).expect("effective configs serialize")
        });
    let mut repeats = Vec::new();
    for combo in &matrix.combos {
        for repeat in &combo.repeats {
            let launch_attempts = match &repeat.launch {
                LaunchOutcome::Success { prior_attempt_failures, .. } => {
                    prior_attempt_failures.as_slice()
                }
                LaunchOutcome::Failure { attempt_failures } => {
                    attempt_failures.as_slice()
                }
                LaunchOutcome::Pending => &[],
            };
            let nested_boundary_errors = launch_attempts
                .iter()
                .filter_map(|attempt| match &attempt.clean_boundary {
                    BoundaryOutcome::Failure { error } => Some(error.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut boundary_failure = launch_attempts
                .iter()
                .enumerate()
                .filter_map(|(index, attempt)| match &attempt.clean_boundary {
                    BoundaryOutcome::Failure { error } => Some(format!(
                        "launch attempt {} clean boundary: {error}",
                        index + 1
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            boundary_failure.extend(
                [&repeat.pre_boundary, &repeat.post_boundary]
                    .into_iter()
                    .filter_map(|outcome| match outcome {
                        BoundaryOutcome::Failure { error }
                            if !nested_boundary_errors
                                .contains(&error.as_str()) =>
                        {
                            Some(error.clone())
                        }
                        _ => None,
                    }),
            );
            let boundary_failure = (!boundary_failure.is_empty())
                .then(|| boundary_failure.join("; "));
            let nested_boundaries_clean = match &repeat.launch {
                LaunchOutcome::Success { prior_attempt_failures, .. } => {
                    prior_attempt_failures.iter().all(|failure| {
                        matches!(failure.clean_boundary, BoundaryOutcome::Clean)
                    })
                }
                LaunchOutcome::Failure { attempt_failures } => {
                    attempt_failures.iter().all(|failure| {
                        matches!(failure.clean_boundary, BoundaryOutcome::Clean)
                    })
                }
                LaunchOutcome::Pending => true,
            };
            let boundaries_clean =
                matches!(repeat.pre_boundary, BoundaryOutcome::Clean)
                    && matches!(repeat.post_boundary, BoundaryOutcome::Clean)
                    && nested_boundaries_clean;
            let prior_launch_attempt_failures =
                match &repeat.launch {
                    LaunchOutcome::Success {
                        prior_attempt_failures, ..
                    } if !prior_attempt_failures.is_empty() => Some(
                        prior_attempt_failures
                            .iter()
                            .map(|failure| failure.error.as_str())
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                    _ => None,
                };
            let (launch_failure, metrics) = match &repeat.launch {
                LaunchOutcome::Success { metrics, prior_attempt_failures }
                    if boundaries_clean =>
                {
                    (
                        None,
                        CommonMetrics {
                            launch_duration_secs: Some(metrics.launch_secs),
                            peak_ram_bytes: Some(metrics.peak_ram_bytes),
                            peak_ram_semantics: Some(
                                MemorySemantics::LaunchBaselineDelta,
                            ),
                            writes_bytes: Some(metrics.bringup_bytes),
                            idle_writes_bytes: None,
                        },
                    )
                }
                LaunchOutcome::Success { .. } => {
                    (None, CommonMetrics::default())
                }
                LaunchOutcome::Failure { attempt_failures } => (
                    Some(
                        attempt_failures
                            .iter()
                            .map(|failure| failure.error.as_str())
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                    CommonMetrics::default(),
                ),
                LaunchOutcome::Pending => (None, CommonMetrics::default()),
            };
            let (
                workload_bytes,
                workload_duration_secs,
                workload_peak_delta_bytes,
                workload_failure,
            ) = match &repeat.workload {
                WorkloadOutcome::Success { metrics } if boundaries_clean => (
                    Some(metrics.workload_bytes),
                    Some(metrics.workload_secs),
                    Some(metrics.workload_peak_delta_bytes),
                    None,
                ),
                WorkloadOutcome::Failure { error } => {
                    (None, None, None, Some(error.clone()))
                }
                _ => (None, None, None, None),
            };
            let preparation_failure = match &repeat.preparation {
                super::PreparationOutcome::Failure { error } => {
                    Some(error.clone())
                }
                _ => None,
            };
            let workload_disposition =
                match (&matrix.workload, &repeat.workload) {
                    (None, WorkloadOutcome::NotRequested) => {
                        WorkloadDisposition::NotRequested
                    }
                    (Some(_), WorkloadOutcome::Success { .. }) => {
                        WorkloadDisposition::Succeeded
                    }
                    (Some(_), WorkloadOutcome::Failure { .. })
                        if preparation_failure.is_some() =>
                    {
                        WorkloadDisposition::Blocked
                    }
                    (Some(_), WorkloadOutcome::Failure { .. }) => {
                        WorkloadDisposition::Failed
                    }
                    (Some(_), WorkloadOutcome::Pending)
                        if launch_failure.is_some()
                            || boundary_failure.is_some()
                            || preparation_failure.is_some() =>
                    {
                        WorkloadDisposition::Blocked
                    }
                    (Some(_), WorkloadOutcome::Pending) => {
                        WorkloadDisposition::Pending
                    }
                    _ => unreachable!("validated workload state"),
                };
            let failure = boundary_failure
                .clone()
                .or_else(|| preparation_failure.clone())
                .or_else(|| workload_failure.clone())
                .or_else(|| launch_failure.clone());
            repeats.push(NormalizedRepeat {
                candidate: combo.label.clone(),
                outcome: failure.map(RepeatOutcome::Failure).unwrap_or_else(
                    || {
                        if matches!(
                            repeat.launch,
                            LaunchOutcome::Success { .. }
                        ) && matches!(
                            repeat.workload,
                            WorkloadOutcome::Success { .. }
                                | WorkloadOutcome::NotRequested
                        ) && boundaries_clean
                        {
                            RepeatOutcome::Success
                        } else {
                            RepeatOutcome::Failure("repeat is pending".into())
                        }
                    },
                ),
                metrics,
                payload: RepeatPayload::StorageLevers(StorageRepeatPayload {
                    levers: combo.levers.clone(),
                    workload_disposition,
                    workload_bytes,
                    workload_duration_secs,
                    workload_peak_delta_bytes,
                    workload_peak_ram_semantics: workload_peak_delta_bytes
                        .map(|_| MemorySemantics::WorkloadBaselineDelta),
                    launch_failure,
                    prior_launch_attempt_failures,
                    preparation_failure,
                    workload_failure,
                    boundary_failure,
                }),
            });
        }
    }
    let (contract_version, provenance, effective_configuration, capabilities) =
        matrix.report_evidence.as_ref().map(matrix_report_evidence).unwrap_or((
            None,
            Provenance::Unavailable,
            EffectiveConfiguration::Unavailable,
            CapabilityEvidence::Unavailable,
        ));
    NormalizedInput {
        identity: InputIdentity {
            source: path.to_path_buf(),
            kind: ExperimentKind::StorageLevers,
            source_schema_version: 5,
            run_id: matrix.name,
        },
        capability_contract_version: contract_version,
        provenance,
        effective_configuration,
        dimensions: Dimensions::StorageLevers(StorageDimensions {
            rss_sleds: matrix.rss_sleds,
            combinations: matrix
                .combos
                .iter()
                .map(|combo| combo.label.clone())
                .collect(),
        }),
        repeats,
        capabilities,
        payload: ExperimentPayload::StorageLevers(StoragePayload {
            started: matrix.started,
            ended: matrix.ended.unwrap_or(matrix.updated),
            requested_repeats: matrix.repeat,
            rated_tbw: matrix.rated_tbw,
            workload: matrix.workload,
            oxide_session: matrix.oxide_session,
            effective_candidate_configurations,
            effective_candidate_configurations_identity,
            launch_memory_semantics: MemorySemantics::LaunchBaselineDelta,
            workload_memory_semantics,
            run_status: Some(matrix.status),
            abort_error: matrix.abort_error,
        }),
    }
}

fn normalize_legacy_matrix(
    path: &Path,
    matrix: LegacyMatrixRun,
) -> NormalizedInput {
    let repeats = matrix
        .results
        .iter()
        .flat_map(|combo| {
            combo.repeats.iter().map(|repeat| NormalizedRepeat {
                candidate: combo.label.clone(),
                outcome: RepeatOutcome::Success,
                metrics: CommonMetrics {
                    launch_duration_secs: Some(repeat.launch_secs),
                    peak_ram_bytes: repeat.peak_ram_bytes,
                    peak_ram_semantics: Some(
                        MemorySemantics::LegacyAbsoluteHostPeak,
                    ),
                    writes_bytes: Some(repeat.bringup_bytes),
                    idle_writes_bytes: None,
                },
                payload: RepeatPayload::StorageLevers(StorageRepeatPayload {
                    levers: combo.levers.clone(),
                    workload_disposition: if repeat.workload_bytes.is_some()
                        && repeat.workload_secs.is_some()
                    {
                        WorkloadDisposition::Succeeded
                    } else if matrix.workload.is_some() {
                        WorkloadDisposition::Pending
                    } else {
                        WorkloadDisposition::NotRequested
                    },
                    workload_bytes: repeat.workload_bytes,
                    workload_duration_secs: repeat.workload_secs,
                    workload_peak_delta_bytes: None,
                    workload_peak_ram_semantics: None,
                    launch_failure: None,
                    prior_launch_attempt_failures: None,
                    preparation_failure: None,
                    workload_failure: None,
                    boundary_failure: None,
                }),
            })
        })
        .collect();
    NormalizedInput {
        identity: InputIdentity {
            source: path.to_path_buf(),
            kind: ExperimentKind::StorageLevers,
            source_schema_version: matrix.schema_version,
            run_id: matrix.name,
        },
        capability_contract_version: None,
        provenance: Provenance::Unavailable,
        effective_configuration: EffectiveConfiguration::Unavailable,
        dimensions: Dimensions::StorageLevers(StorageDimensions {
            rss_sleds: matrix.rss_sleds,
            combinations: matrix.combos,
        }),
        repeats,
        capabilities: CapabilityEvidence::Unavailable,
        payload: ExperimentPayload::StorageLevers(StoragePayload {
            started: matrix.started,
            ended: matrix.ended,
            requested_repeats: matrix.repeat,
            rated_tbw: matrix.rated_tbw,
            workload: matrix.workload,
            oxide_session: matrix.oxide_session,
            effective_candidate_configurations: None,
            effective_candidate_configurations_identity: None,
            launch_memory_semantics: MemorySemantics::LegacyAbsoluteHostPeak,
            workload_memory_semantics: None,
            run_status: None,
            abort_error: None,
        }),
    }
}

fn normalize_matrix(
    path: &Path,
    source_version: u32,
    matrix: MatrixRun,
) -> NormalizedInput {
    let workload_memory_semantics = matrix
        .workload
        .as_ref()
        .map(|_| MemorySemantics::WorkloadBaselineDelta);
    let effective_candidate_configurations =
        matrix.report_evidence.as_ref().map(|evidence| {
            evidence
                .combos
                .iter()
                .map(|combo| {
                    (combo.label.clone(), combo.effective_config.clone())
                })
                .collect::<BTreeMap<_, _>>()
        });
    let effective_candidate_configurations_identity =
        effective_candidate_configurations.as_ref().map(|configurations| {
            serde_json::to_string(configurations).expect(
                "validated effective candidate configurations serialize",
            )
        });
    let mut repeats = matrix
        .results
        .iter()
        .flat_map(|combo| {
            combo.repeats.iter().map(|repeat| NormalizedRepeat {
                candidate: combo.label.clone(),
                outcome: RepeatOutcome::Success,
                metrics: CommonMetrics {
                    launch_duration_secs: Some(repeat.launch_secs),
                    peak_ram_bytes: repeat.peak_ram_bytes,
                    peak_ram_semantics: Some(
                        MemorySemantics::LaunchBaselineDelta,
                    ),
                    writes_bytes: Some(repeat.bringup_bytes),
                    idle_writes_bytes: None,
                },
                payload: RepeatPayload::StorageLevers(StorageRepeatPayload {
                    levers: combo.levers.clone(),
                    workload_disposition: if repeat.workload_bytes.is_some()
                        && repeat.workload_secs.is_some()
                    {
                        WorkloadDisposition::Succeeded
                    } else if matrix.workload.is_some() {
                        WorkloadDisposition::Pending
                    } else {
                        WorkloadDisposition::NotRequested
                    },
                    workload_bytes: repeat.workload_bytes,
                    workload_duration_secs: repeat.workload_secs,
                    workload_peak_delta_bytes: repeat.workload_peak_delta_bytes,
                    workload_peak_ram_semantics: repeat
                        .workload_peak_delta_bytes
                        .map(|_| MemorySemantics::WorkloadBaselineDelta),
                    launch_failure: None,
                    prior_launch_attempt_failures: None,
                    preparation_failure: None,
                    workload_failure: None,
                    boundary_failure: None,
                }),
            })
        })
        .collect::<Vec<_>>();
    for combo in &matrix.results {
        if let Some(error) = &combo.error {
            repeats.push(NormalizedRepeat {
                candidate: combo.label.clone(),
                outcome: RepeatOutcome::Failure(error.clone()),
                metrics: CommonMetrics::default(),
                payload: RepeatPayload::StorageLevers(StorageRepeatPayload {
                    levers: combo.levers.clone(),
                    workload_disposition: WorkloadDisposition::Pending,
                    workload_bytes: None,
                    workload_duration_secs: None,
                    workload_peak_delta_bytes: None,
                    workload_peak_ram_semantics: None,
                    launch_failure: Some(error.clone()),
                    prior_launch_attempt_failures: None,
                    preparation_failure: None,
                    workload_failure: None,
                    boundary_failure: None,
                }),
            });
        }
    }
    let (contract_version, provenance, effective_configuration, capabilities) =
        matrix.report_evidence.as_ref().map(matrix_report_evidence).unwrap_or((
            None,
            Provenance::Unavailable,
            EffectiveConfiguration::Unavailable,
            CapabilityEvidence::Unavailable,
        ));
    NormalizedInput {
        identity: InputIdentity {
            source: path.to_path_buf(),
            kind: ExperimentKind::StorageLevers,
            source_schema_version: source_version,
            run_id: matrix.name,
        },
        capability_contract_version: contract_version,
        provenance,
        effective_configuration,
        dimensions: Dimensions::StorageLevers(StorageDimensions {
            rss_sleds: matrix.rss_sleds,
            combinations: matrix.combos,
        }),
        repeats,
        capabilities,
        payload: ExperimentPayload::StorageLevers(StoragePayload {
            started: matrix.started,
            ended: matrix.ended,
            requested_repeats: matrix.repeat,
            rated_tbw: matrix.rated_tbw,
            workload: matrix.workload,
            oxide_session: matrix.oxide_session,
            effective_candidate_configurations,
            effective_candidate_configurations_identity,
            launch_memory_semantics: MemorySemantics::LaunchBaselineDelta,
            workload_memory_semantics,
            run_status: None,
            abort_error: None,
        }),
    }
}

fn matrix_report_evidence(
    evidence: &super::MatrixReportEvidence,
) -> (Option<u32>, Provenance, EffectiveConfiguration, CapabilityEvidence) {
    let available = |value: &super::EvidenceValue<String>| match value {
        super::EvidenceValue::Available { value } => Some(value.clone()),
        super::EvidenceValue::Unavailable { .. } => None,
    };
    let provenance = [
        available(&evidence.provenance.voxel_build),
        available(&evidence.provenance.voxel_binary),
        available(&evidence.provenance.configured_image),
        available(&evidence.provenance.omicron_commit),
        available(&evidence.provenance.host),
    ];
    let provenance = if provenance.iter().all(Option::is_some) {
        let fields = ProvenanceFields {
            voxel_revision: None,
            omicron_revision: None,
            image_id: None,
            host_id: None,
            voxel_build: provenance[0].clone(),
            voxel_binary: provenance[1].clone(),
            configured_image: provenance[2].clone(),
            omicron_commit: provenance[3].clone(),
            host: provenance[4].clone(),
        };
        if valid_provenance(&fields) {
            Provenance::Available(fields)
        } else {
            Provenance::Unavailable
        }
    } else {
        Provenance::Unavailable
    };
    let status = |capability, value: &super::CapabilityStatus| {
        let (status, evidence_value, error) = match value {
            super::CapabilityStatus::Pass { evidence } => (
                CapabilityStatus::Pass,
                Some(BoundedEvidence(Value::String(evidence.clone()))),
                None,
            ),
            super::CapabilityStatus::Fail { evidence } => {
                (CapabilityStatus::Fail, None, Some(evidence.clone()))
            }
            super::CapabilityStatus::Unavailable { reason } => {
                (CapabilityStatus::Unavailable, None, Some(reason.clone()))
            }
        };
        CapabilityResult {
            capability,
            status,
            evidence: evidence_value,
            elapsed_millis: None,
            error,
        }
    };
    let ledger = &evidence.capabilities;
    let capabilities = vec![
        status(
            Capability::MatrixHostStorageScope,
            &ledger.matrix_host_storage_scope,
        ),
        status(
            Capability::CleanLaunchTeardownBoundaries,
            &ledger.clean_launch_teardown_boundaries,
        ),
        status(Capability::ApiDiskLifecycle, &ledger.api_disk_lifecycle),
        status(
            Capability::SimulatedZpoolPreparation,
            &ledger.simulated_zpool_preparation,
        ),
    ];
    (
        Some(evidence.evidence_version),
        provenance,
        // Storage candidates carry their exact effective configurations in
        // StoragePayload. The common field is for single-configuration
        // experiment kinds and must not misattribute the matrix base config to
        // each storage candidate.
        EffectiveConfiguration::Unavailable,
        CapabilityEvidence::Available(capabilities),
    )
}

fn normalize_minimum_hardware(
    path: &Path,
    value: Value,
) -> Result<NormalizedInput> {
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "minimum-hardware schema_version must be an unsigned integer"
            )
        })?;
    if version != 1 {
        bail!(
            "unsupported minimum-hardware schema version {version}; supported version is 1"
        );
    }
    validate_contract_shape(&value)?;
    let wire: MinimumHardwareWire = serde_json::from_value(value)
        .context("deserialize minimum-hardware schema v1")?;
    if wire.payload.expected_repeats == 0 {
        bail!("minimum-hardware expected_repeats must be greater than zero");
    }
    if wire.repeats.len() > wire.payload.expected_repeats {
        bail!(
            "minimum-hardware completed repeats must not exceed expected_repeats ({} > {})",
            wire.repeats.len(),
            wire.payload.expected_repeats
        );
    }
    serde_json::to_vec(&wire.effective_configuration)
        .context("serialize minimum-hardware effective configuration")?;
    validate_capabilities(
        wire.contract_name.as_deref(),
        wire.contract_version,
        wire.capabilities.as_deref(),
    )?;
    let candidate_names = wire
        .repeats
        .iter()
        .map(|repeat| repeat.candidate.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if candidate_names.len() > 1 {
        bail!(
            "a minimum-hardware document must describe exactly one candidate"
        );
    }
    let capability_contract_version = wire
        .capabilities
        .as_ref()
        .filter(|results| !results.is_empty())
        .and(wire.contract_version);
    let repeats = wire
        .repeats
        .into_iter()
        .map(|repeat| NormalizedRepeat {
            candidate: repeat.candidate,
            outcome: repeat.outcome,
            metrics: CommonMetrics {
                launch_duration_secs: repeat.launch_duration_secs,
                peak_ram_bytes: repeat.peak_ram_bytes,
                peak_ram_semantics: None,
                writes_bytes: repeat.launch_writes_bytes,
                idle_writes_bytes: repeat.idle_writes_bytes,
            },
            payload: RepeatPayload::MinimumHardware,
        })
        .collect();
    Ok(NormalizedInput {
        identity: InputIdentity {
            source: path.to_path_buf(),
            kind: ExperimentKind::MinimumHardware,
            source_schema_version: wire.schema_version,
            run_id: wire.identity.run_id,
        },
        capability_contract_version,
        provenance: wire
            .provenance
            .filter(valid_provenance)
            .map_or(Provenance::Unavailable, Provenance::Available),
        effective_configuration: EffectiveConfiguration::Available(
            wire.effective_configuration,
        ),
        dimensions: Dimensions::MinimumHardware(wire.dimensions),
        repeats,
        capabilities: wire
            .capabilities
            .filter(|results| !results.is_empty())
            .map_or(
                CapabilityEvidence::Unavailable,
                CapabilityEvidence::Available,
            ),
        payload: ExperimentPayload::MinimumHardware(wire.payload),
    })
}

const CAPABILITY_CONTRACT_VERSION: u32 = 1;
const CAPABILITY_CONTRACT_NAME: &str = "oxide-internal-faux-rack";
const MAX_CAPABILITY_TEXT_BYTES: usize = 1024;
const MAX_CAPABILITY_EVIDENCE_BYTES: usize = 4096;

fn validate_contract_shape(value: &Value) -> Result<()> {
    let object = value.as_object().expect("minimum hardware is an object");
    let supplied = ["contract_name", "contract_version", "capabilities"]
        .iter()
        .any(|field| object.contains_key(*field));
    if !supplied {
        return Ok(());
    }
    for field in ["contract_name", "contract_version", "capabilities"] {
        if !object.contains_key(field) || object[field].is_null() {
            bail!(
                "capability contract field '{field}' is required and must not be null"
            );
        }
    }
    Ok(())
}

fn validate_capabilities(
    name: Option<&str>,
    version: Option<u32>,
    results: Option<&[CapabilityResult]>,
) -> Result<()> {
    let Some(results) = results else {
        return Ok(());
    };
    if name != Some(CAPABILITY_CONTRACT_NAME) {
        bail!(
            "capability contract name must be exactly '{CAPABILITY_CONTRACT_NAME}'"
        );
    }
    let version = version.ok_or_else(|| {
        anyhow::anyhow!("capability evidence requires contract_version")
    })?;
    if version != CAPABILITY_CONTRACT_VERSION {
        bail!(
            "unsupported capability contract version {version}; supported version is 1"
        );
    }
    let required = [
        Capability::RackReadiness,
        Capability::Metrics,
        Capability::FleetApi,
        Capability::SiloApi,
        Capability::ProjectDiskLifecycle,
        Capability::TopologyFidelity,
        Capability::CleanTeardown,
    ];
    for capability in required {
        if results.iter().filter(|r| r.capability == capability).count() != 1 {
            bail!("contract v1 requires exactly one result for {capability:?}");
        }
    }
    validate_capability_result_shapes(results)
}

fn validate_capability_result_shapes(
    results: &[CapabilityResult],
) -> Result<()> {
    for result in results {
        if let Some(evidence) = &result.evidence {
            let bytes = serde_json::to_vec(&evidence.0)
                .context("serialize capability evidence")?;
            if bytes.is_empty() || bytes.len() > MAX_CAPABILITY_EVIDENCE_BYTES {
                bail!(
                    "structured capability evidence exceeds {MAX_CAPABILITY_EVIDENCE_BYTES} bytes"
                );
            }
        }
        if let Some(text) = result.error.as_deref() {
            if text.is_empty() || text.len() > MAX_CAPABILITY_TEXT_BYTES {
                bail!(
                    "capability errors must contain 1..={MAX_CAPABILITY_TEXT_BYTES} bytes"
                );
            }
        }
        match result.status {
            CapabilityStatus::Pass if result.error.is_some() => {
                bail!("passing capability must not have an error")
            }
            CapabilityStatus::Pass if result.evidence.is_none() => {
                bail!("passing capability requires evidence")
            }
            CapabilityStatus::Fail | CapabilityStatus::Unavailable
                if result.error.is_none() =>
            {
                bail!("non-passing capability requires an actionable error")
            }
            CapabilityStatus::Fail | CapabilityStatus::Unavailable
                if result.evidence.is_some() =>
            {
                bail!(
                    "non-passing capability must not include success evidence"
                )
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "identity", rename_all = "kebab-case")]
enum CohortKey {
    Storage(StorageCohortKey),
    MinimumHardware(MinimumHardwareCohortKey),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct StorageCohortKey {
    rss_sleds: usize,
    combinations: Vec<String>,
    workload: Option<WorkloadSpec>,
    oxide_session_identity: Option<String>,
    effective_configuration_identity: Option<String>,
    capability_contract_version: Option<u32>,
    launch_memory_semantics: MemorySemantics,
    workload_memory_semantics: Option<MemorySemantics>,
    provenance: ComparableProvenance,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct MinimumHardwareCohortKey {
    provenance: ComparableProvenance,
    effective_configuration_identity: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum ComparableProvenance {
    Available(ProvenanceFields),
    Unknown { source: PathBuf, run_id: String },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "configuration", rename_all = "kebab-case")]
enum CandidateKey {
    Storage(std::collections::BTreeSet<u8>),
    MinimumHardware(MinimumHardwareDimensions),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum Objective {
    RequiredAllocation,
    PeakAllocation,
    PeakRam,
    WorkloadPeakRam,
    LaunchDuration,
    LaunchWrites,
    WorkloadDuration,
    WorkloadWrites,
    IdleWrites,
    Simplicity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SelectionPolicy {
    expected_repeats: usize,
    objectives: Vec<Objective>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum SimplicityKey {
    Storage {
        lever_count: usize,
        levers: std::collections::BTreeSet<u8>,
    },
    MinimumHardware {
        vdev_count: usize,
        cockroachdb_redundancy: usize,
        svcadm_autoclear: bool,
        vdev_size_bytes: u64,
        control_plane_storage_buffer_bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum IneligibilityReason {
    SchemaNotRecommendationEligible,
    ApiWorkloadRequired,
    EffectiveConfigurationUnavailable,
    CapabilityEvidenceUnavailable,
    CapabilityFailed,
    ProvenanceUnavailable,
    RequiredRepeatFailed,
    RequiredRepeatMissing,
    RequiredMeasurementMissing,
    HostStorageEnvelopeExceeded,
    ConflictingPooledSources,
    CapabilityStatus { capability: Capability, status: CapabilityStatus },
}

#[derive(Clone, Debug, Serialize)]
struct CandidateSummary {
    expected_repeats: usize,
    completed_repeats: usize,
    successful_repeats: usize,
    fits_host_storage_envelope: Option<bool>,
    host_storage_capacity_bytes: Option<u64>,
    launch_duration: Option<Stats>,
    peak_ram: Option<Stats>,
    workload_peak_ram: Option<Stats>,
    launch_writes: Option<Stats>,
    workload_duration: Option<Stats>,
    workload_writes: Option<Stats>,
    idle_writes: Option<Stats>,
    required_allocation_bytes: Option<u64>,
    peak_allocation_bytes: Option<u64>,
    success_rate: f64,
}

#[derive(Clone, Debug, Serialize)]
struct AnalyzedCandidate {
    key: CandidateKey,
    candidate: String,
    policy: SelectionPolicy,
    repeats: Vec<NormalizedRepeat>,
    summary: CandidateSummary,
    ineligibility: Vec<IneligibilityReason>,
    dominated: bool,
    elimination_reasons: Vec<String>,
    simplicity: SimplicityKey,
    decision: DecisionTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum MetricComparison {
    Better,
    Worse,
    WithinNoise,
    NoiseUnknown,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
enum DecisionTrace {
    Ineligible(Vec<IneligibilityReason>),
    ParetoDominated { by: CandidateKey, objectives: Vec<Objective> },
    LexicographicLoss { criterion: Objective },
    SimplicityLoss,
    TieOrNoiseUnknown,
    Selected(SelectionRationale),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum SelectionRationale {
    SoleEligible,
    SelectedAt(Objective),
}

#[derive(Clone, Debug, Serialize)]
struct Recommendation {
    key: CandidateKey,
    display: String,
    rationale: SelectionRationale,
}

#[derive(Clone, Debug, Serialize)]
struct AnalyzedCohort {
    key: CohortKey,
    candidates: Vec<AnalyzedCandidate>,
    recommendation: Option<Recommendation>,
    tie: Vec<CandidateKey>,
    no_recommendation: Option<NoRecommendationReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum NoRecommendationReason {
    NoEligibleCandidates,
    TradeoffOrNoiseTie,
}

#[derive(Clone, Debug, Serialize)]
struct Analysis {
    cohorts: Vec<AnalyzedCohort>,
    global_recommendation: Option<Recommendation>,
}

fn analyze(inputs: &[NormalizedInput]) -> Analysis {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<
        CohortKey,
        BTreeMap<CandidateKey, Vec<&NormalizedInput>>,
    > = BTreeMap::new();
    for input in inputs {
        let key = cohort_key(input);
        let keys = input
            .repeats
            .iter()
            .map(|r| candidate_key(input, Some(r)))
            .collect::<std::collections::BTreeSet<_>>();
        for candidate in keys.into_iter().chain(
            (input.repeats.is_empty()).then(|| candidate_key(input, None)),
        ) {
            grouped
                .entry(key.clone())
                .or_default()
                .entry(candidate)
                .or_default()
                .push(input);
        }
    }
    let mut cohorts = grouped
        .into_iter()
        .map(|(key, candidates)| {
            let mut candidates = candidates
                .into_iter()
                .map(|(key, sources)| analyze_candidate(key, &sources))
                .collect::<Vec<_>>();
            for index in 0..candidates.len() {
                let dominator = (0..candidates.len()).find(|&other| {
                    other != index
                        && dominates(&candidates[other], &candidates[index])
                });
                candidates[index].dominated = dominator.is_some();
                if candidates[index].dominated {
                    candidates[index].elimination_reasons.push(
                        "Pareto dominated by another eligible candidate".into(),
                    );
                    let other = dominator.unwrap();
                    candidates[index].decision =
                        DecisionTrace::ParetoDominated {
                            by: candidates[other].key.clone(),
                            objectives: better_objectives(
                                &candidates[other],
                                &candidates[index],
                            ),
                        };
                }
            }
            let frontier = candidates
                .iter()
                .filter(|c| c.ineligibility.is_empty() && !c.dominated)
                .collect::<Vec<_>>();
            let (recommendation, tie) = rank(frontier);
            if let Some(selected) = &recommendation {
                let selected_index = candidates
                    .iter_mut()
                    .position(|c| c.key == selected.key)
                    .expect("recommendation names a candidate");
                candidates[selected_index].decision =
                    DecisionTrace::Selected(selected.rationale);
                let selected_candidate = candidates[selected_index].clone();
                for candidate in candidates.iter_mut().filter(|candidate| {
                    candidate.ineligibility.is_empty()
                        && !candidate.dominated
                        && candidate.key != selected.key
                }) {
                    candidate.decision =
                        first_ranking_loss(candidate, &selected_candidate);
                }
            }
            for candidate in candidates.iter_mut().filter(|c| {
                c.ineligibility.is_empty()
                    && !c.dominated
                    && matches!(c.decision, DecisionTrace::TieOrNoiseUnknown)
            }) {
                candidate.decision = DecisionTrace::TieOrNoiseUnknown;
            }
            let no_recommendation = recommendation.is_none().then_some(
                if candidates.iter().any(|c| c.ineligibility.is_empty()) {
                    NoRecommendationReason::TradeoffOrNoiseTie
                } else {
                    NoRecommendationReason::NoEligibleCandidates
                },
            );
            AnalyzedCohort {
                key,
                candidates,
                recommendation,
                tie,
                no_recommendation,
            }
        })
        .collect::<Vec<_>>();
    cohorts.sort_by(|a, b| a.key.cmp(&b.key));
    Analysis { cohorts, global_recommendation: None }
}

fn cohort_key(input: &NormalizedInput) -> CohortKey {
    let provenance = match &input.provenance {
        Provenance::Available(p) => ComparableProvenance::Available(p.clone()),
        Provenance::Unavailable => ComparableProvenance::Unknown {
            source: input.identity.source.clone(),
            run_id: input.identity.run_id.clone(),
        },
    };
    match (&input.dimensions, &input.payload) {
        (Dimensions::StorageLevers(d), ExperimentPayload::StorageLevers(p)) => {
            CohortKey::Storage(StorageCohortKey {
                rss_sleds: d.rss_sleds,
                combinations: d.combinations.clone(),
                workload: p.workload.clone(),
                oxide_session_identity: p.oxide_session.as_ref().map(
                    |session| {
                        serde_json::to_string(session)
                            .expect("session serializes")
                    },
                ),
                effective_configuration_identity: p
                    .effective_candidate_configurations_identity
                    .clone(),
                capability_contract_version: input.capability_contract_version,
                launch_memory_semantics: p.launch_memory_semantics,
                workload_memory_semantics: p.workload_memory_semantics,
                provenance,
            })
        }
        (Dimensions::MinimumHardware(_), _) => {
            let EffectiveConfiguration::Available(configuration) =
                &input.effective_configuration
            else {
                unreachable!(
                    "minimum-hardware normalization supplies effective configuration"
                )
            };
            CohortKey::MinimumHardware(MinimumHardwareCohortKey {
                provenance,
                effective_configuration_identity: serde_json::to_string(configuration).expect(
                    "effective configuration serialization was validated during normalization",
                ),
            })
        }
        _ => unreachable!("normalized dimensions and payload kind agree"),
    }
}

fn candidate_key(
    input: &NormalizedInput,
    repeat: Option<&NormalizedRepeat>,
) -> CandidateKey {
    match &input.dimensions {
        Dimensions::StorageLevers(_) => CandidateKey::Storage(
            repeat
                .and_then(|r| match &r.payload {
                    RepeatPayload::StorageLevers(p) => Some(p.levers.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
        ),
        Dimensions::MinimumHardware(d) => {
            CandidateKey::MinimumHardware(d.clone())
        }
    }
}

fn policy(input: &NormalizedInput) -> SelectionPolicy {
    match &input.payload {
        ExperimentPayload::StorageLevers(p) => SelectionPolicy {
            expected_repeats: p.requested_repeats,
            objectives: vec![
                Objective::PeakRam,
                Objective::WorkloadPeakRam,
                Objective::LaunchDuration,
                Objective::LaunchWrites,
                Objective::WorkloadDuration,
                Objective::WorkloadWrites,
                Objective::Simplicity,
            ],
        },
        ExperimentPayload::MinimumHardware(p) => SelectionPolicy {
            expected_repeats: p.expected_repeats,
            objectives: vec![
                Objective::RequiredAllocation,
                Objective::PeakAllocation,
                Objective::PeakRam,
                Objective::LaunchDuration,
                Objective::IdleWrites,
                Objective::Simplicity,
            ],
        },
    }
}

// Schema-v4 capability statuses summarize whole-matrix completion, so repeat
// evidence must attribute their failures to candidates. Schema v5 makes the
// host-storage scope an independent pre-run proof; that one remains a veto.
fn capability_gates_candidate(
    input: &NormalizedInput,
    capability: Capability,
) -> bool {
    input.identity.kind != ExperimentKind::StorageLevers
        || (input.identity.source_schema_version == 5
            && capability == Capability::MatrixHostStorageScope)
}

fn analyze_candidate(
    key: CandidateKey,
    sources: &[&NormalizedInput],
) -> AnalyzedCandidate {
    let observed_name = sources
        .iter()
        .flat_map(|i| i.repeats.iter())
        .find(|r| candidate_key(sources[0], Some(r)) == key)
        .map(|r| r.candidate.clone())
        .unwrap_or_else(|| sources[0].identity.run_id.clone());
    let mut repeats = sources
        .iter()
        .flat_map(|i| {
            i.repeats
                .iter()
                .filter(|r| candidate_key(i, Some(r)) == key)
                .cloned()
        })
        .collect::<Vec<_>>();
    repeats.sort_by_key(|repeat| {
        serde_json::to_string(repeat).unwrap_or_default()
    });
    let successful = repeats
        .iter()
        .filter(|r| r.outcome == RepeatOutcome::Success)
        .collect::<Vec<_>>();
    let policies = sources.iter().map(|i| policy(i)).collect::<Vec<_>>();
    // Replay validation caps every input at MAX_PLANNED_REPEAT_SLOTS and
    // superreport caps inputs at 4096, so this sum is at most 2^24 and cannot
    // overflow even on Rust's minimum supported usize width.
    let expected_repeats = policies.iter().map(|p| p.expected_repeats).sum();
    let selection_policy = SelectionPolicy {
        expected_repeats,
        objectives: policies[0].objectives.clone(),
    };
    let objectives = &selection_policy.objectives;
    let metric = |f: fn(&CommonMetrics) -> Option<u64>| {
        let values = repeats
            .iter()
            .filter_map(|r| f(&r.metrics).map(|x| x as f64))
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| stats(&values))
    };
    let mut ineligibility = Vec::new();
    let first = sources[0];
    if sources.iter().any(|input| {
        input.identity.kind == ExperimentKind::StorageLevers
            && !matches!(input.identity.source_schema_version, 4 | 5)
    }) {
        ineligibility
            .push(IneligibilityReason::SchemaNotRecommendationEligible);
    }
    if sources.iter().any(|input| {
        matches!(&input.payload, ExperimentPayload::StorageLevers(payload) if payload.workload.is_none())
    }) {
        ineligibility.push(IneligibilityReason::ApiWorkloadRequired);
    }
    let candidate_configuration_identity =
        |input: &NormalizedInput| match (&key, &input.payload) {
            (
                CandidateKey::Storage(_),
                ExperimentPayload::StorageLevers(payload),
            ) => payload
                .effective_candidate_configurations
                .as_ref()
                .and_then(|configurations| configurations.get(&observed_name))
                .map(|configuration| {
                    serde_json::to_string(configuration)
                        .expect("effective configuration serializes")
                }),
            (CandidateKey::MinimumHardware(_), _) => {
                match &input.effective_configuration {
                    EffectiveConfiguration::Available(configuration) => Some(
                        serde_json::to_string(configuration)
                            .expect("effective configuration serializes"),
                    ),
                    EffectiveConfiguration::Unavailable => None,
                }
            }
            _ => None,
        };
    if sources
        .iter()
        .any(|input| candidate_configuration_identity(input).is_none())
    {
        ineligibility
            .push(IneligibilityReason::EffectiveConfigurationUnavailable);
    }
    let first_candidate_configuration = candidate_configuration_identity(first);
    let pooled_conflict = sources.iter().skip(1).any(|source| {
        source.dimensions != first.dimensions
            || source.capability_contract_version
                != first.capability_contract_version
            || source.provenance != first.provenance
            || policy(source).objectives != policies[0].objectives
            || candidate_configuration_identity(source)
                != first_candidate_configuration
            || match (&source.payload, &first.payload) {
                (
                    ExperimentPayload::StorageLevers(source),
                    ExperimentPayload::StorageLevers(first),
                ) => {
                    source.workload != first.workload
                        || source.oxide_session != first.oxide_session
                        || source.run_status != first.run_status
                        || source.effective_candidate_configurations_identity
                            != first.effective_candidate_configurations_identity
                        || source.launch_memory_semantics
                            != first.launch_memory_semantics
                        || source.workload_memory_semantics
                            != first.workload_memory_semantics
                }
                (source, first) => source != first,
            }
            || source.repeats.iter().any(|repeat| {
                candidate_key(source, Some(repeat)) == key
                    && repeat.candidate != observed_name
            })
    });
    if pooled_conflict {
        ineligibility.push(IneligibilityReason::ConflictingPooledSources);
    }
    if sources.iter().any(|i| matches!(i.provenance, Provenance::Unavailable)) {
        ineligibility.push(IneligibilityReason::ProvenanceUnavailable);
    }
    if sources
        .iter()
        .any(|i| matches!(i.capabilities, CapabilityEvidence::Unavailable))
    {
        ineligibility.push(IneligibilityReason::CapabilityEvidenceUnavailable);
    }
    let failed_capabilities = sources
        .iter()
        .flat_map(|input| {
            let results = match &input.capabilities {
                CapabilityEvidence::Available(results) => results.as_slice(),
                CapabilityEvidence::Unavailable => &[],
            };
            results.iter().filter(move |result| {
                result.status != CapabilityStatus::Pass
                    && capability_gates_candidate(input, result.capability)
            })
        })
        .collect::<Vec<_>>();
    if !failed_capabilities.is_empty() {
        ineligibility.push(IneligibilityReason::CapabilityFailed);
    }
    for result in failed_capabilities {
        ineligibility.push(IneligibilityReason::CapabilityStatus {
            capability: result.capability,
            status: result.status,
        });
    }
    if repeats.iter().any(|r| matches!(r.outcome, RepeatOutcome::Failure(_))) {
        ineligibility.push(IneligibilityReason::RequiredRepeatFailed);
    }
    if sources.iter().any(|input| {
        matches!(&input.payload,
        ExperimentPayload::StorageLevers(payload)
            if input.identity.source_schema_version == 5
                && payload.run_status != Some(RunStatus::Completed))
    }) {
        ineligibility.push(IneligibilityReason::RequiredRepeatMissing);
    }
    if repeats.len() != expected_repeats {
        ineligibility.push(IneligibilityReason::RequiredRepeatMissing);
    }
    let launch_duration = metric(|m| m.launch_duration_secs);
    let peak_ram = metric(|m| m.peak_ram_bytes);
    let storage_metric = |f: fn(&StorageRepeatPayload) -> Option<u64>| {
        let values = repeats
            .iter()
            .filter_map(|repeat| match &repeat.payload {
                RepeatPayload::StorageLevers(payload) => {
                    f(payload).map(|x| x as f64)
                }
                RepeatPayload::MinimumHardware => None,
            })
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| stats(&values))
    };
    let missing_required = successful.iter().any(|r| {
        objectives.iter().any(|objective| match objective {
            Objective::PeakRam => r.metrics.peak_ram_bytes.is_none(),
            Objective::WorkloadPeakRam => match &r.payload {
                RepeatPayload::StorageLevers(p) => {
                    p.workload_peak_delta_bytes.is_none()
                }
                _ => false,
            },
            Objective::LaunchDuration => {
                r.metrics.launch_duration_secs.is_none()
            }
            Objective::LaunchWrites => r.metrics.writes_bytes.is_none(),
            Objective::WorkloadDuration => match &r.payload {
                RepeatPayload::StorageLevers(p) => {
                    p.workload_duration_secs.is_none()
                }
                _ => false,
            },
            Objective::WorkloadWrites => match &r.payload {
                RepeatPayload::StorageLevers(p) => p.workload_bytes.is_none(),
                _ => false,
            },
            Objective::IdleWrites => r.metrics.idle_writes_bytes.is_none(),
            _ => false,
        })
    });
    if missing_required || successful.len() < expected_repeats {
        ineligibility.push(IneligibilityReason::RequiredMeasurementMissing);
    }
    let hardware = sources.iter().find_map(|i| match &i.payload {
        ExperimentPayload::MinimumHardware(p) => Some(p),
        _ => None,
    });
    if sources.iter().any(|source| {
        matches!(&source.payload, ExperimentPayload::MinimumHardware(p)
            if !p.fits_host_storage_envelope
                || p.required_allocation_bytes > p.host_storage_capacity_bytes
                || p.peak_allocation_bytes > p.host_storage_capacity_bytes)
    }) {
        ineligibility.push(IneligibilityReason::HostStorageEnvelopeExceeded);
    }
    let required_allocation_bytes =
        hardware.map(|p| p.required_allocation_bytes);
    let simplicity = match &key {
        CandidateKey::Storage(levers) => SimplicityKey::Storage {
            lever_count: levers.len(),
            levers: levers.clone(),
        },
        CandidateKey::MinimumHardware(d) => SimplicityKey::MinimumHardware {
            vdev_count: d.vdev_count,
            cockroachdb_redundancy: d.cockroachdb_redundancy,
            svcadm_autoclear: d.svcadm_autoclear,
            vdev_size_bytes: d.vdev_size_bytes,
            control_plane_storage_buffer_bytes: d
                .control_plane_storage_buffer_bytes,
        },
    };
    let name =
        if pooled_conflict { candidate_display(&key) } else { observed_name };
    let summary = CandidateSummary {
        expected_repeats,
        completed_repeats: repeats.len(),
        successful_repeats: successful.len(),
        fits_host_storage_envelope: (!pooled_conflict)
            .then(|| hardware.map(|p| p.fits_host_storage_envelope))
            .flatten(),
        host_storage_capacity_bytes: (!pooled_conflict)
            .then(|| hardware.map(|p| p.host_storage_capacity_bytes))
            .flatten(),
        launch_duration: (!pooled_conflict)
            .then_some(launch_duration)
            .flatten(),
        peak_ram: (!pooled_conflict).then_some(peak_ram).flatten(),
        workload_peak_ram: (!pooled_conflict)
            .then(|| storage_metric(|p| p.workload_peak_delta_bytes))
            .flatten(),
        launch_writes: (!pooled_conflict)
            .then(|| metric(|m| m.writes_bytes))
            .flatten(),
        workload_duration: (!pooled_conflict)
            .then(|| storage_metric(|p| p.workload_duration_secs))
            .flatten(),
        workload_writes: (!pooled_conflict)
            .then(|| storage_metric(|p| p.workload_bytes))
            .flatten(),
        idle_writes: (!pooled_conflict)
            .then(|| metric(|m| m.idle_writes_bytes))
            .flatten(),
        required_allocation_bytes: (!pooled_conflict)
            .then_some(required_allocation_bytes)
            .flatten(),
        peak_allocation_bytes: (!pooled_conflict)
            .then(|| hardware.map(|p| p.peak_allocation_bytes))
            .flatten(),
        success_rate: if repeats.is_empty() {
            0.0
        } else {
            successful.len() as f64 / repeats.len() as f64
        },
    };
    ineligibility.sort_unstable();
    ineligibility.dedup();
    let decision = if ineligibility.is_empty() {
        DecisionTrace::TieOrNoiseUnknown
    } else {
        DecisionTrace::Ineligible(ineligibility.clone())
    };
    AnalyzedCandidate {
        key,
        candidate: name,
        policy: selection_policy,
        repeats,
        summary,
        ineligibility,
        dominated: false,
        elimination_reasons: Vec::new(),
        simplicity,
        decision,
    }
}

fn candidate_display(key: &CandidateKey) -> String {
    match key {
        CandidateKey::Storage(levers) => canonical_combo_label(levers),
        CandidateKey::MinimumHardware(dimensions) => {
            serde_json::to_string(dimensions)
                .expect("candidate dimensions serialize")
        }
    }
}

fn candidate_label(display: &str, key: &CandidateKey) -> String {
    format!("{display} — {}", candidate_display(key))
}

fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::RackReadiness => "rack-readiness",
        Capability::Metrics => "metrics",
        Capability::FleetApi => "fleet-api",
        Capability::SiloApi => "silo-api",
        Capability::ProjectDiskLifecycle => "project-disk-lifecycle",
        Capability::TopologyFidelity => "topology-fidelity",
        Capability::CleanTeardown => "clean-teardown",
        Capability::MatrixHostStorageScope => "matrix-host-storage-scope",
        Capability::CleanLaunchTeardownBoundaries => {
            "clean-launch-teardown-boundaries"
        }
        Capability::ApiDiskLifecycle => "api-disk-lifecycle",
        Capability::SimulatedZpoolPreparation => "simulated-zpool-preparation",
    }
}

fn compare_stat(a: Option<Stats>, b: Option<Stats>) -> MetricComparison {
    let (Some(a), Some(b)) = (a, b) else {
        return MetricComparison::Missing;
    };
    let Some(noise) = combined_noise_threshold(a, b) else {
        return MetricComparison::NoiseUnknown;
    };
    if (a.mean - b.mean).abs() <= noise {
        MetricComparison::WithinNoise
    } else if a.mean < b.mean {
        MetricComparison::Better
    } else {
        MetricComparison::Worse
    }
}

fn compare_value(a: Option<u64>, b: Option<u64>) -> MetricComparison {
    match (a, b) {
        (Some(a), Some(b)) if a < b => MetricComparison::Better,
        (Some(a), Some(b)) if a > b => MetricComparison::Worse,
        (Some(_), Some(_)) => MetricComparison::WithinNoise,
        _ => MetricComparison::Missing,
    }
}

fn compare_objective(
    a: &AnalyzedCandidate,
    b: &AnalyzedCandidate,
    objective: Objective,
) -> MetricComparison {
    match objective {
        Objective::RequiredAllocation => compare_value(
            a.summary.required_allocation_bytes,
            b.summary.required_allocation_bytes,
        ),
        Objective::PeakAllocation => compare_value(
            a.summary.peak_allocation_bytes,
            b.summary.peak_allocation_bytes,
        ),
        Objective::PeakRam => {
            compare_stat(a.summary.peak_ram, b.summary.peak_ram)
        }
        Objective::WorkloadPeakRam => compare_stat(
            a.summary.workload_peak_ram,
            b.summary.workload_peak_ram,
        ),
        Objective::LaunchDuration => {
            compare_stat(a.summary.launch_duration, b.summary.launch_duration)
        }
        Objective::LaunchWrites => {
            compare_stat(a.summary.launch_writes, b.summary.launch_writes)
        }
        Objective::WorkloadDuration => compare_stat(
            a.summary.workload_duration,
            b.summary.workload_duration,
        ),
        Objective::WorkloadWrites => {
            compare_stat(a.summary.workload_writes, b.summary.workload_writes)
        }
        Objective::IdleWrites => {
            compare_stat(a.summary.idle_writes, b.summary.idle_writes)
        }
        Objective::Simplicity => match a.simplicity.cmp(&b.simplicity) {
            std::cmp::Ordering::Less => MetricComparison::Better,
            std::cmp::Ordering::Greater => MetricComparison::Worse,
            std::cmp::Ordering::Equal => MetricComparison::WithinNoise,
        },
    }
}

fn dominates(a: &AnalyzedCandidate, b: &AnalyzedCandidate) -> bool {
    if !a.ineligibility.is_empty() || !b.ineligibility.is_empty() {
        return false;
    }
    a.policy.objectives == b.policy.objectives
        && a.policy.objectives.iter().all(|&objective| {
            matches!(
                compare_objective(a, b, objective),
                MetricComparison::Better | MetricComparison::WithinNoise
            )
        })
        && a.policy.objectives.iter().any(|&objective| {
            compare_objective(a, b, objective) == MetricComparison::Better
        })
}

fn first_ranking_loss(
    candidate: &AnalyzedCandidate,
    selected: &AnalyzedCandidate,
) -> DecisionTrace {
    for &criterion in &candidate.policy.objectives {
        let comparison = compare_objective(candidate, selected, criterion);
        if comparison == MetricComparison::Worse {
            return DecisionTrace::LexicographicLoss { criterion };
        }
        if comparison == MetricComparison::Better {
            break;
        }
    }
    DecisionTrace::SimplicityLoss
}

fn better_objectives(
    a: &AnalyzedCandidate,
    b: &AnalyzedCandidate,
) -> Vec<Objective> {
    a.policy
        .objectives
        .iter()
        .copied()
        .filter(|&objective| {
            compare_objective(a, b, objective) == MetricComparison::Better
        })
        .collect()
}

fn rank(
    frontier: Vec<&AnalyzedCandidate>,
) -> (Option<Recommendation>, Vec<CandidateKey>) {
    if frontier.is_empty() {
        return (None, Vec::new());
    }
    if frontier.len() == 1 {
        let candidate = frontier[0];
        return (
            Some(Recommendation {
                key: candidate.key.clone(),
                display: candidate.candidate.clone(),
                rationale: SelectionRationale::SoleEligible,
            }),
            Vec::new(),
        );
    }
    let objectives = frontier[0].policy.objectives.clone();
    let mut remaining = frontier;
    for objective in objectives {
        let before = remaining.clone();
        if before.iter().enumerate().any(|(index, candidate)| {
            before.iter().skip(index + 1).any(|other| {
                matches!(
                    compare_objective(candidate, other, objective),
                    MetricComparison::NoiseUnknown | MetricComparison::Missing
                )
            })
        }) {
            return (None, before.iter().map(|c| c.key.clone()).collect());
        }
        remaining.retain(|candidate| {
            !before.iter().any(|other| {
                compare_objective(candidate, other, objective)
                    == MetricComparison::Worse
            })
        });
        if remaining.len() == 1 {
            return (
                Some(Recommendation {
                    key: remaining[0].key.clone(),
                    display: remaining[0].candidate.clone(),
                    rationale: SelectionRationale::SelectedAt(objective),
                }),
                Vec::new(),
            );
        }
    }
    (None, remaining.into_iter().map(|c| c.key.clone()).collect())
}

fn valid_provenance(fields: &ProvenanceFields) -> bool {
    const MAX_IDENTITY_BYTES: usize = 1024;
    let valid_family = |family: &[&Option<String>]| {
        family.iter().all(|field| {
            field.as_deref().is_some_and(|text| {
                !text.trim().is_empty() && text.len() <= MAX_IDENTITY_BYTES
            })
        })
    };
    let absent_family =
        |family: &[&Option<String>]| family.iter().all(|field| field.is_none());
    let legacy = [
        &fields.voxel_revision,
        &fields.omicron_revision,
        &fields.image_id,
        &fields.host_id,
    ];
    let matrix = [
        &fields.voxel_build,
        &fields.voxel_binary,
        &fields.configured_image,
        &fields.omicron_commit,
        &fields.host,
    ];
    (valid_family(&legacy) && absent_family(&matrix))
        || (absent_family(&legacy) && valid_family(&matrix))
}

#[derive(Clone, Debug, Serialize)]
struct InputDigestView {
    source: String,
    sha256: Option<String>,
    run_status: Option<RunStatus>,
    evidence_state: Option<String>,
    abort_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ViewChartKind {
    GrossWrites,
    Waterfall,
    LaunchDuration,
    PeakRam,
    WorkloadRam,
    WorkloadWear,
    WorkloadDuration,
    Capabilities,
    Allocation,
}

#[derive(Clone, Debug, Serialize)]
struct ChartView {
    kind: ViewChartKind,
    title: String,
    unit: String,
    option: Value,
    fallback_rows: Vec<ChartFallbackRow>,
}

#[derive(Clone, Debug, Serialize)]
struct ChartFallbackRow {
    category: String,
    series: String,
    value: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct StorageComboView {
    label: String,
    key: CandidateKey,
    rows: Vec<SampleRow>,
    writes_decimal_gb: Vec<f64>,
    writes: Option<Stats>,
    launch_seconds: Vec<u64>,
    peak_ram_decimal_gb: Vec<f64>,
    workload_ram_delta_decimal_gb: Vec<f64>,
    workload_bytes: Vec<u64>,
    workload_seconds: Vec<u64>,
    failed_repeats: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SampleRow {
    source: String,
    run_id: String,
    repeat_ordinal: usize,
    outcome: RepeatOutcome,
    metrics: CommonMetrics,
    workload_disposition: Option<WorkloadDisposition>,
    workload_bytes: Option<u64>,
    workload_duration_secs: Option<u64>,
    workload_peak_delta_bytes: Option<u64>,
    workload_peak_ram_semantics: Option<MemorySemantics>,
    launch_failure: Option<String>,
    prior_launch_attempt_failures: Option<String>,
    preparation_failure: Option<String>,
    workload_failure: Option<String>,
    boundary_failure: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct CandidateView {
    key: CandidateKey,
    stable_id: String,
    label: String,
    dimensions: Option<MinimumHardwareDimensions>,
    host_storage_capacity_bytes: Option<u64>,
    eligible: bool,
    feasible: Option<bool>,
    recommended: bool,
    decision: String,
    ineligibility: Vec<String>,
    #[serde(skip)]
    renderer_ineligibility: Vec<String>,
    required_allocation_bytes: Option<u64>,
    peak_allocation_bytes: Option<u64>,
    success_rate: f64,
    expected_repeats: usize,
    completed_repeats: usize,
    successful_repeats: usize,
    launch_duration: Option<Stats>,
    peak_ram: Option<Stats>,
    launch_writes: Option<Stats>,
    idle_writes: Option<Stats>,
    rows: Vec<SampleRow>,
    capabilities: Vec<CapabilityResult>,
    capabilities_available: bool,
    launch_samples_seconds: Vec<u64>,
    peak_ram_samples_bytes: Vec<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct CoverageView {
    planned_slots: usize,
    launch_samples: usize,
    workload_requested: bool,
    workload_succeeded: usize,
    workload_failed: usize,
    workload_blocked: usize,
    unresolved: usize,
}

impl CoverageView {
    fn accounted_workload_slots(&self) -> usize {
        self.workload_succeeded
            + self.workload_failed
            + self.workload_blocked
            + self.unresolved
    }
}

#[derive(Clone, Debug, Serialize)]
struct BestSupportedRecommendationView {
    candidate: String,
    basis: String,
    missing_candidates: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ConditionRow {
    label: String,
    value: String,
    code: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VaryingConditionRow {
    label: String,
    values: Vec<Option<ConditionRow>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FactoredConditions {
    shared: Vec<ConditionRow>,
    varying: Vec<VaryingConditionRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CandidateSettingProjection {
    candidates: Vec<String>,
    shared: Vec<ConditionRow>,
    varying: Vec<VaryingConditionRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FailureGroup {
    message: String,
    count: usize,
}

#[derive(Clone, Debug, Serialize)]
struct MatrixCapabilityView {
    source: String,
    run_id: String,
    results: Option<Vec<CapabilityResult>>,
}

#[derive(Clone, Debug, Serialize)]
struct CohortView {
    #[serde(skip)]
    key: CohortKey,
    label: String,
    conditions: Vec<ConditionRow>,
    conclusion: String,
    warning: Option<String>,
    best_supported: Option<BestSupportedRecommendationView>,
    coverage: CoverageView,
    matrix_capabilities: Vec<MatrixCapabilityView>,
    candidates: Vec<CandidateView>,
    storage_summary: Vec<StorageComboView>,
    charts: Vec<ChartView>,
}

#[derive(Clone, Debug, Serialize)]
struct DescriptiveAggregateView {
    label: String,
    disclaimer: String,
    inputs: Vec<String>,
    storage_summary: Vec<StorageComboView>,
    charts: Vec<ChartView>,
}

#[derive(Clone, Debug, Serialize)]
struct ReportSectionView {
    kind: ExperimentKind,
    title: String,
    conclusion: String,
    warnings: Vec<String>,
    cohorts: Vec<CohortView>,
    descriptive_aggregate: Option<DescriptiveAggregateView>,
}

#[derive(Clone, Debug, Serialize)]
struct ReportView {
    title: String,
    executive_conclusion: String,
    inputs: Vec<InputDigestView>,
    sections: Vec<ReportSectionView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aggregation: Option<AggregationMetadata>,
}

fn input_digest_view(input: &NormalizedInput) -> InputDigestView {
    let run_status = match &input.payload {
        ExperimentPayload::StorageLevers(payload) => payload.run_status,
        ExperimentPayload::MinimumHardware(_) => None,
    };
    let abort_error = match &input.payload {
        ExperimentPayload::StorageLevers(payload) => {
            payload.abort_error.clone()
        }
        ExperimentPayload::MinimumHardware(_) => None,
    };
    let has_stage_failure =
        input.repeats.iter().any(|repeat| match &repeat.payload {
            RepeatPayload::StorageLevers(payload) => {
                payload.launch_failure.is_some()
                    || payload.workload_failure.is_some()
                    || payload.boundary_failure.is_some()
            }
            RepeatPayload::MinimumHardware => false,
        });
    let evidence_state = match run_status {
        Some(RunStatus::Running) => Some("interrupted-current-snapshot".into()),
        Some(RunStatus::Aborted) => Some("aborted".into()),
        Some(RunStatus::Completed) if has_stage_failure => {
            Some("partial-evidence".into())
        }
        Some(RunStatus::Completed) => Some("completed".into()),
        None => None,
    };
    InputDigestView {
        source: input.identity.source.display().to_string(),
        sha256: None,
        run_status,
        evidence_state,
        abort_error,
    }
}

fn chart_value(chart: charming::Chart) -> Result<Value> {
    serde_json::to_value(chart).context("serialize Charming chart option")
}

fn sample_fallback_rows(
    labels: &[String],
    samples: &[Vec<f64>],
) -> Vec<ChartFallbackRow> {
    labels
        .iter()
        .zip(samples)
        .flat_map(|(label, values)| {
            let mut rows = values
                .iter()
                .enumerate()
                .map(|(index, value)| ChartFallbackRow {
                    category: label.clone(),
                    series: format!("Sample {}", index + 1),
                    value: Some(*value),
                })
                .collect::<Vec<_>>();
            rows.push(ChartFallbackRow {
                category: label.clone(),
                series: "Mean".into(),
                value: (!values.is_empty())
                    .then(|| values.iter().sum::<f64>() / values.len() as f64),
            });
            rows
        })
        .collect()
}

fn series_fallback_rows(
    labels: &[String],
    series: &str,
    values: impl IntoIterator<Item = Option<f64>>,
) -> Vec<ChartFallbackRow> {
    labels
        .iter()
        .zip(values)
        .map(|(label, value)| ChartFallbackRow {
            category: label.clone(),
            series: series.into(),
            value,
        })
        .collect()
}

fn sample_chart(
    title: &str,
    unit: &str,
    labels: &[String],
    samples: &[Vec<f64>],
) -> Result<Value> {
    use charming::{
        Chart,
        component::{Axis, Legend},
        datatype::{CompositeValue, DataPoint},
        element::{AxisType, Tooltip},
        series::{Line, Scatter},
    };
    let mut chart = Chart::new()
        .legend(Legend::new())
        .tooltip(Tooltip::new())
        .x_axis(
            Axis::new()
                .type_(AxisType::Category)
                .name("configuration")
                .data(labels.to_vec()),
        )
        .y_axis(Axis::new().type_(AxisType::Value).name(unit));
    let mut means = Vec::with_capacity(samples.len());
    for (index, values) in samples.iter().enumerate() {
        let points = values
            .iter()
            .map(|value| {
                DataPoint::from(vec![
                    CompositeValue::String(labels[index].clone()),
                    CompositeValue::from(*value),
                ])
            })
            .collect::<Vec<_>>();
        chart = chart
            .series(Scatter::new().name(labels[index].clone()).data(points));
        means.push(
            (!values.is_empty())
                .then(|| values.iter().sum::<f64>() / values.len() as f64),
        );
    }
    chart = chart.series(Line::new().name("Mean").data(means));
    let _ = title; // The semantic HTML heading is authoritative and escaped separately.
    chart_value(chart)
}

fn bar_chart(labels: &[String], unit: &str, values: Vec<f64>) -> Result<Value> {
    use charming::{
        Chart,
        component::{Axis, Legend},
        element::{AxisType, Tooltip},
        series::Bar,
    };
    chart_value(
        Chart::new()
            .legend(Legend::new())
            .tooltip(Tooltip::new())
            .x_axis(
                Axis::new()
                    .type_(AxisType::Category)
                    .name("configuration")
                    .data(labels.to_vec()),
            )
            .y_axis(Axis::new().type_(AxisType::Value).name(unit))
            .series(Bar::new().name(unit).data(values)),
    )
}

fn allocation_chart(
    labels: &[String],
    required: &[Option<u64>],
    peak: &[Option<u64>],
) -> Result<Value> {
    use charming::{
        Chart,
        component::{Axis, Legend},
        element::{AxisType, Tooltip},
        series::Bar,
    };
    chart_value(
        Chart::new()
            .legend(Legend::new())
            .tooltip(Tooltip::new())
            .x_axis(
                Axis::new()
                    .type_(AxisType::Category)
                    .name("candidate")
                    .data(labels.to_vec()),
            )
            .y_axis(Axis::new().type_(AxisType::Value).name("decimal GB"))
            .series(
                Bar::new().name("Required allocation").data(
                    required
                        .iter()
                        .map(|v| v.map(|x| x as f64 / 1e9))
                        .collect::<Vec<_>>(),
                ),
            )
            .series(
                Bar::new().name("Peak allocation").data(
                    peak.iter()
                        .map(|v| v.map(|x| x as f64 / 1e9))
                        .collect::<Vec<_>>(),
                ),
            ),
    )
}

fn capability_chart(candidates: &[CandidateView]) -> Result<Value> {
    use charming::{
        Chart,
        component::{Axis, Legend},
        element::{AxisType, Tooltip},
        series::Bar,
    };
    let mut labels = Vec::new();
    let mut statuses = Vec::new();
    for candidate in candidates {
        if candidate.capabilities_available {
            for result in &candidate.capabilities {
                labels.push(format!(
                    "{} / {}",
                    candidate.label,
                    capability_label(result.capability)
                ));
                statuses.push(match result.status {
                    CapabilityStatus::Pass => 1,
                    CapabilityStatus::Fail => 0,
                    CapabilityStatus::Unavailable => -1,
                });
            }
        } else {
            for capability in [
                Capability::RackReadiness,
                Capability::Metrics,
                Capability::FleetApi,
                Capability::SiloApi,
                Capability::ProjectDiskLifecycle,
                Capability::TopologyFidelity,
                Capability::CleanTeardown,
            ] {
                labels.push(format!(
                    "{} / {}",
                    candidate.label,
                    capability_label(capability)
                ));
                statuses.push(-1);
            }
        }
    }
    chart_value(
        Chart::new()
            .legend(Legend::new())
            .tooltip(Tooltip::new())
            .x_axis(
                Axis::new()
                    .type_(AxisType::Category)
                    .name("candidate / capability")
                    .data(labels),
            )
            .y_axis(
                Axis::new()
                    .type_(AxisType::Value)
                    .name("status (-1 unavailable, 0 fail, 1 pass)"),
            )
            .series(Bar::new().name("Capability status").data(statuses)),
    )
}

fn capability_fallback_rows(
    candidates: &[CandidateView],
) -> Vec<ChartFallbackRow> {
    const LEGACY_CAPABILITIES: [Capability; 7] = [
        Capability::RackReadiness,
        Capability::Metrics,
        Capability::FleetApi,
        Capability::SiloApi,
        Capability::ProjectDiskLifecycle,
        Capability::TopologyFidelity,
        Capability::CleanTeardown,
    ];
    candidates
        .iter()
        .flat_map(|candidate| {
            if candidate.capabilities_available {
                candidate
                    .capabilities
                    .iter()
                    .map(|result| {
                        (
                            result.capability,
                            Some(match result.status {
                                CapabilityStatus::Pass => 1.0,
                                CapabilityStatus::Fail => 0.0,
                                CapabilityStatus::Unavailable => -1.0,
                            }),
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                LEGACY_CAPABILITIES
                    .into_iter()
                    .map(|capability| (capability, None))
                    .collect()
            }
            .into_iter()
            .map(|(capability, value)| ChartFallbackRow {
                category: format!(
                    "{} / {}",
                    candidate.label,
                    capability_label(capability)
                ),
                series: "Capability status".into(),
                value,
            })
        })
        .collect()
}

fn sample_rows(input: &NormalizedInput, key: &CandidateKey) -> Vec<SampleRow> {
    let mut ordinal = 0;
    input
        .repeats
        .iter()
        .filter_map(|repeat| {
            if candidate_key(input, Some(repeat)) != *key {
                return None;
            }
            ordinal += 1;
            let (
                workload_disposition,
                workload_bytes,
                workload_duration_secs,
                workload_peak_delta_bytes,
                workload_peak_ram_semantics,
                launch_failure,
                prior_launch_attempt_failures,
                preparation_failure,
                workload_failure,
                boundary_failure,
            ) = match &repeat.payload {
                RepeatPayload::StorageLevers(p) => (
                    Some(p.workload_disposition),
                    p.workload_bytes,
                    p.workload_duration_secs,
                    p.workload_peak_delta_bytes,
                    p.workload_peak_ram_semantics,
                    p.launch_failure.clone(),
                    p.prior_launch_attempt_failures.clone(),
                    p.preparation_failure.clone(),
                    p.workload_failure.clone(),
                    p.boundary_failure.clone(),
                ),
                RepeatPayload::MinimumHardware => {
                    (None, None, None, None, None, None, None, None, None, None)
                }
            };
            Some(SampleRow {
                source: input.identity.source.display().to_string(),
                run_id: input.identity.run_id.clone(),
                repeat_ordinal: ordinal,
                outcome: repeat.outcome.clone(),
                metrics: repeat.metrics.clone(),
                workload_disposition,
                workload_bytes,
                workload_duration_secs,
                workload_peak_delta_bytes,
                workload_peak_ram_semantics,
                launch_failure,
                prior_launch_attempt_failures,
                preparation_failure,
                workload_failure,
                boundary_failure,
            })
        })
        .collect()
}

fn decision_label(decision: &DecisionTrace) -> String {
    match decision {
        DecisionTrace::Ineligible(_) => {
            "Ineligible because required evidence is incomplete or failed"
                .into()
        }
        DecisionTrace::ParetoDominated { .. } => {
            "Eligible but dominated on controlled objectives".into()
        }
        DecisionTrace::LexicographicLoss { .. } => {
            "Eligible but lost at the first differing objective".into()
        }
        DecisionTrace::SimplicityLoss => {
            "Eligible but a simpler tied candidate was preferred".into()
        }
        DecisionTrace::TieOrNoiseUnknown => {
            "No defensible distinction within available evidence".into()
        }
        DecisionTrace::Selected(_) => "Selected within this cohort".into(),
    }
}

fn recommendation_label(recommendation: &Recommendation) -> String {
    format!(
        "Advisory recommendation: {} ({}).",
        candidate_label(&recommendation.display, &recommendation.key),
        match recommendation.rationale {
            SelectionRationale::SoleEligible => "sole eligible candidate",
            SelectionRationale::SelectedAt(_) =>
                "selected by the declared lexicographic policy",
        }
    )
}

fn no_recommendation_label(reason: Option<NoRecommendationReason>) -> String {
    match reason {
        Some(NoRecommendationReason::NoEligibleCandidates) => {
            "No recommendation: no candidate has complete eligible evidence.".into()
        }
        Some(NoRecommendationReason::TradeoffOrNoiseTie) => {
            "No recommendation: the evidence leaves a tradeoff or noise-level tie.".into()
        }
        None => "No recommendation was produced.".into(),
    }
}

fn condition(
    label: impl Into<String>,
    value: impl Into<String>,
    code: bool,
) -> ConditionRow {
    ConditionRow { label: label.into(), value: value.into(), code }
}

fn human_key(key: &str) -> String {
    let mut chars = key.replace('_', " ").chars().collect::<Vec<_>>();
    if let Some(first) = chars.first_mut() {
        first.make_ascii_uppercase();
    }
    chars.into_iter().collect()
}

fn flatten_typed_value(
    label: &str,
    value: &Value,
    rows: &mut Vec<ConditionRow>,
) {
    match value {
        Value::Object(object) => {
            if object.is_empty() {
                rows.push(condition(label, "{}", true));
            }
            for (key, value) in object {
                flatten_typed_value(
                    &format!("{label} / {}", human_key(key)),
                    value,
                    rows,
                );
            }
        }
        Value::Array(values) => {
            if values.is_empty() {
                rows.push(condition(label, "[]", true));
            }
            for (index, value) in values.iter().enumerate() {
                flatten_typed_value(
                    &format!("{label} / {}", index + 1),
                    value,
                    rows,
                );
            }
        }
        Value::String(value) => rows.push(condition(label, value, true)),
        Value::Null => rows.push(condition(label, "not supplied", false)),
        value => rows.push(condition(label, value.to_string(), false)),
    }
}

fn typed_identity_conditions(
    label: &str,
    identity: Option<&str>,
) -> Vec<ConditionRow> {
    let Some(identity) = identity else {
        return vec![condition(label, "not supplied", false)];
    };
    match serde_json::from_str(identity) {
        Ok(value) => {
            let mut rows = Vec::new();
            flatten_typed_value(label, &value, &mut rows);
            rows
        }
        Err(_) => vec![condition(label, identity, true)],
    }
}

fn workload_label(workload: &WorkloadSpec) -> String {
    let size = if workload.size_bytes % (1 << 30) == 0 {
        format!("{} GiB", workload.size_bytes / (1 << 30))
    } else if workload.size_bytes % (1 << 20) == 0 {
        format!("{} MiB", workload.size_bytes / (1 << 20))
    } else {
        format!("{} bytes", workload.size_bytes)
    };
    format!(
        "API disk lifecycle — {} disks, parallelism {}, {} each, snapshots {}",
        workload.count,
        workload.parallel,
        size,
        if workload.snapshot { "enabled" } else { "disabled" }
    )
}

fn provenance_conditions(
    provenance: &ComparableProvenance,
) -> Vec<ConditionRow> {
    match provenance {
        ComparableProvenance::Available(provenance) => {
            let matrix_fields = [
                ("Voxel build", &provenance.voxel_build),
                ("Voxel binary", &provenance.voxel_binary),
                ("Configured image", &provenance.configured_image),
                ("Omicron commit", &provenance.omicron_commit),
                ("Host", &provenance.host),
            ];
            if matrix_fields.iter().any(|(_, value)| value.is_some()) {
                matrix_fields
                    .into_iter()
                    .map(|(label, value)| {
                        condition(
                            label,
                            value.as_deref().unwrap_or("missing"),
                            true,
                        )
                    })
                    .collect()
            } else {
                [
                    ("Voxel revision", &provenance.voxel_revision),
                    ("Omicron revision", &provenance.omicron_revision),
                    ("Image ID", &provenance.image_id),
                    ("Host ID", &provenance.host_id),
                ]
                .into_iter()
                .map(|(label, value)| {
                    condition(
                        label,
                        value.as_deref().unwrap_or("missing"),
                        true,
                    )
                })
                .collect()
            }
        }
        ComparableProvenance::Unknown { source, run_id } => vec![
            condition("Provenance", "unavailable", false),
            condition("Source", source.display().to_string(), true),
            condition("Run ID", run_id, true),
        ],
    }
}

fn memory_semantics_label(semantics: Option<MemorySemantics>) -> &'static str {
    match semantics {
        Some(MemorySemantics::LegacyAbsoluteHostPeak) => {
            "Legacy absolute host peak"
        }
        Some(MemorySemantics::LaunchBaselineDelta) => {
            "Launch baseline-adjusted delta"
        }
        Some(MemorySemantics::WorkloadBaselineDelta) => {
            "Workload baseline-adjusted delta"
        }
        None => "Not applicable",
    }
}

fn cohort_conditions(key: &CohortKey) -> Vec<ConditionRow> {
    match key {
        CohortKey::Storage(storage) => {
            let mut rows = vec![
                condition("RSS sleds", storage.rss_sleds.to_string(), false),
                condition(
                    "Combinations",
                    if storage.combinations.is_empty() {
                        "none".into()
                    } else {
                        storage.combinations.join(" → ")
                    },
                    false,
                ),
                condition(
                    "Workload",
                    storage
                        .workload
                        .as_ref()
                        .map(workload_label)
                        .unwrap_or_else(|| "Not requested".into()),
                    false,
                ),
            ];
            rows.extend(typed_identity_conditions(
                "Oxide session",
                storage.oxide_session_identity.as_deref(),
            ));
            rows.extend(typed_identity_conditions(
                "Effective candidate configuration",
                storage.effective_configuration_identity.as_deref(),
            ));
            rows.push(condition(
                "Capability contract version",
                storage
                    .capability_contract_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "not supplied".into()),
                false,
            ));
            rows.push(condition(
                "Launch memory semantics",
                memory_semantics_label(Some(storage.launch_memory_semantics)),
                false,
            ));
            rows.push(condition(
                "Workload memory semantics",
                memory_semantics_label(storage.workload_memory_semantics),
                false,
            ));
            rows.extend(provenance_conditions(&storage.provenance));
            rows
        }
        CohortKey::MinimumHardware(hardware) => {
            let mut rows = typed_identity_conditions(
                "Effective configuration",
                Some(&hardware.effective_configuration_identity),
            );
            rows.extend(provenance_conditions(&hardware.provenance));
            rows
        }
    }
}

fn factor_condition_rows(rows: &[Vec<ConditionRow>]) -> FactoredConditions {
    let mut labels = Vec::new();
    for row in rows.iter().flatten() {
        if !labels.contains(&row.label) {
            labels.push(row.label.clone());
        }
    }
    let mut shared = Vec::new();
    let mut varying = Vec::new();
    for label in labels {
        let values = rows
            .iter()
            .map(|conditions| {
                conditions
                    .iter()
                    .find(|condition| condition.label == label)
                    .cloned()
            })
            .collect::<Vec<_>>();
        if let Some(first) = values.first().and_then(Option::as_ref) {
            if values.iter().all(|value| value.as_ref() == Some(first)) {
                shared.push(first.clone());
                continue;
            }
        }
        varying.push(VaryingConditionRow { label, values });
    }
    FactoredConditions { shared, varying }
}

fn flatten_candidate_setting(
    label: &str,
    value: &Value,
    rows: &mut Vec<ConditionRow>,
) {
    match value {
        Value::Object(object) => {
            if object.is_empty() {
                rows.push(condition(label, "{}", true));
            }
            for (key, value) in object {
                let child = if label.is_empty() {
                    human_key(key)
                } else {
                    format!("{label} / {}", human_key(key))
                };
                flatten_candidate_setting(&child, value, rows);
            }
        }
        Value::Array(values) => {
            if values.is_empty() {
                rows.push(condition(label, "[]", true));
            }
            for (index, value) in values.iter().enumerate() {
                let child = if label.is_empty() {
                    (index + 1).to_string()
                } else {
                    format!("{label} / {}", index + 1)
                };
                flatten_candidate_setting(&child, value, rows);
            }
        }
        Value::String(value) => rows.push(condition(label, value, true)),
        Value::Null => rows.push(condition(label, "not supplied", false)),
        value => rows.push(condition(label, value.to_string(), false)),
    }
}

fn candidate_setting_projection(
    cohort: &CohortView,
) -> Option<CandidateSettingProjection> {
    let (candidates, rows) = match &cohort.key {
        CohortKey::Storage(storage) => {
            let exact = storage
                .effective_configuration_identity
                .as_deref()
                .and_then(|identity| serde_json::from_str(identity).ok())
                .and_then(|value: Value| value.as_object().cloned());
            let candidates = storage.combinations.clone();
            let mut rows = Vec::with_capacity(candidates.len());
            for name in &candidates {
                let mut settings = Vec::new();
                if let Some(configurations) = &exact {
                    if let Some(configuration) = configurations.get(name) {
                        flatten_candidate_setting(
                            "",
                            configuration,
                            &mut settings,
                        );
                    } else {
                        settings.push(condition("Storage levers", name, true));
                    }
                } else {
                    let observed = cohort.candidates.iter().find(|candidate| {
                        renderer_candidate_label(candidate) == *name
                    });
                    let value = observed
                        .and_then(|candidate| match &candidate.key {
                            CandidateKey::Storage(levers) => {
                                Some(canonical_combo_label(levers))
                            }
                            CandidateKey::MinimumHardware(_) => None,
                        })
                        .unwrap_or_else(|| name.clone());
                    settings.push(condition("Storage levers", value, true));
                }
                rows.push(settings);
            }
            (candidates, rows)
        }
        CohortKey::MinimumHardware(_) => {
            let mut rows = Vec::with_capacity(cohort.candidates.len());
            for candidate in &cohort.candidates {
                let dimensions =
                    serde_json::to_value(candidate.dimensions.as_ref()?)
                        .ok()?;
                let mut settings = Vec::new();
                flatten_candidate_setting("", &dimensions, &mut settings);
                rows.push(settings);
            }
            (
                cohort
                    .candidates
                    .iter()
                    .map(|candidate| candidate.label.clone())
                    .collect(),
                rows,
            )
        }
    };
    let factored = factor_condition_rows(&rows);
    Some(CandidateSettingProjection {
        candidates,
        shared: factored.shared,
        varying: factored.varying,
    })
}

fn verified_storage_ladder(cohort: &CohortView) -> bool {
    let CohortKey::Storage(storage) = &cohort.key else {
        return false;
    };
    let rendered = cohort
        .candidates
        .iter()
        .map(renderer_candidate_label)
        .collect::<Vec<_>>();
    storage.combinations.len() > 1
        && rendered == storage.combinations
        && storage.combinations.windows(2).all(|pair| {
            let Some(previous_candidate) =
                cohort.candidates.iter().find(|candidate| {
                    renderer_candidate_label(candidate) == pair[0]
                })
            else {
                return false;
            };
            let Some(next_candidate) =
                cohort.candidates.iter().find(|candidate| {
                    renderer_candidate_label(candidate) == pair[1]
                })
            else {
                return false;
            };
            let (CandidateKey::Storage(previous), CandidateKey::Storage(next)) =
                (&previous_candidate.key, &next_candidate.key)
            else {
                return false;
            };
            previous_candidate.rows.iter().any(sample_row_has_measurement)
                && next_candidate.rows.iter().any(sample_row_has_measurement)
                && previous.is_subset(next)
                && next.len() == previous.len() + 1
        })
}

fn sample_row_has_measurement(row: &SampleRow) -> bool {
    row.metrics.launch_duration_secs.is_some()
        || row.metrics.peak_ram_bytes.is_some()
        || row.metrics.writes_bytes.is_some()
        || row.metrics.idle_writes_bytes.is_some()
        || row.workload_bytes.is_some()
        || row.workload_duration_secs.is_some()
        || row.workload_peak_delta_bytes.is_some()
}

fn cohort_has_no_measurements(cohort: &CohortView) -> bool {
    !cohort
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.rows)
        .any(sample_row_has_measurement)
}

fn renderer_candidate_label(candidate: &CandidateView) -> String {
    match &candidate.key {
        CandidateKey::Storage(_) => renderer_storage_label(&candidate.label),
        CandidateKey::MinimumHardware(_) => candidate.label.clone(),
    }
}

fn renderer_storage_label(label: &str) -> String {
    label.split_once(" — ").map_or_else(
        || label.to_string(),
        |(display, canonical)| {
            if display == canonical {
                display.to_string()
            } else {
                label.to_string()
            }
        },
    )
}

fn experiment_kind_id(kind: ExperimentKind) -> &'static str {
    match kind {
        ExperimentKind::StorageLevers => "storage-levers",
        ExperimentKind::MinimumHardware => "minimum-hardware",
    }
}

fn cohort_anchor(key: &CohortKey) -> String {
    let typed = serde_json::to_vec(key).expect("cohort key serializes");
    format!("cohort-{}", &sha256_hex(&typed)[..16])
}

fn group_exact_failures(failures: &[String]) -> Vec<FailureGroup> {
    let mut groups: Vec<FailureGroup> = Vec::new();
    for failure in failures {
        if let Some(group) =
            groups.iter_mut().find(|group| group.message == *failure)
        {
            group.count += 1;
        } else {
            groups.push(FailureGroup { message: failure.clone(), count: 1 });
        }
    }
    groups
}

fn cohort_label(kind: ExperimentKind, index: usize, key: &CohortKey) -> String {
    match (kind, key) {
        (ExperimentKind::StorageLevers, CohortKey::Storage(storage)) => {
            format!(
                "Storage cohort {} — {} RSS sleds",
                index + 1,
                storage.rss_sleds
            )
        }
        (ExperimentKind::MinimumHardware, _) => {
            format!("Minimum hardware cohort {}", index + 1)
        }
        _ => unreachable!("cohort kind agrees with key"),
    }
}

fn analysis_cohorts(
    inputs: &[NormalizedInput],
    analysis: &Analysis,
    kind: ExperimentKind,
) -> Vec<CohortView> {
    analysis
        .cohorts
        .iter()
        .filter(|cohort| matches!((&cohort.key, kind),
            (CohortKey::Storage(_), ExperimentKind::StorageLevers)
            | (CohortKey::MinimumHardware(_), ExperimentKind::MinimumHardware)))
        .enumerate()
        .map(|(index, cohort)| {
            let cohort_inputs = inputs.iter().filter(|input| cohort_key(input) == cohort.key).collect::<Vec<_>>();
            let conclusion = cohort.recommendation.as_ref().map(recommendation_label)
                .unwrap_or_else(|| no_recommendation_label(cohort.no_recommendation));
            let matrix_capabilities = if kind == ExperimentKind::StorageLevers {
                cohort_inputs
                    .iter()
                    .map(|input| MatrixCapabilityView {
                        source: input.identity.source.display().to_string(),
                        run_id: input.identity.run_id.clone(),
                        results: match &input.capabilities {
                            CapabilityEvidence::Available(results) => Some(results.clone()),
                            CapabilityEvidence::Unavailable => None,
                        },
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let candidates = cohort.candidates.iter().map(|candidate| CandidateView {
                key: candidate.key.clone(),
                stable_id: sha256_hex(serde_json::to_string(&candidate.key).expect("candidate key serializes").as_bytes()),
                label: candidate_label(&candidate.candidate, &candidate.key),
                dimensions: match &candidate.key { CandidateKey::MinimumHardware(d) => Some(d.clone()), _ => None },
                host_storage_capacity_bytes: candidate.summary.host_storage_capacity_bytes,
                eligible: candidate.ineligibility.is_empty(),
                feasible: candidate.summary.host_storage_capacity_bytes
                    .zip(candidate.summary.required_allocation_bytes)
                    .zip(candidate.summary.peak_allocation_bytes)
                    .map(|((capacity, required), peak)| required <= capacity && peak <= capacity
                        && candidate.summary.fits_host_storage_envelope == Some(true)),
                recommended: cohort.recommendation.as_ref().is_some_and(|r| r.key == candidate.key),
                decision: decision_label(&candidate.decision),
                ineligibility: candidate.ineligibility.iter().map(|r| match r { IneligibilityReason::SchemaNotRecommendationEligible => "schema is descriptive-only", IneligibilityReason::ApiWorkloadRequired => "API workload is required", IneligibilityReason::EffectiveConfigurationUnavailable => "exact effective configuration unavailable", IneligibilityReason::CapabilityEvidenceUnavailable => "capability evidence unavailable", IneligibilityReason::CapabilityFailed => "one or more capabilities failed", IneligibilityReason::ProvenanceUnavailable => "provenance unavailable", IneligibilityReason::RequiredRepeatFailed => "required repeat failed", IneligibilityReason::RequiredRepeatMissing => "required repeat missing", IneligibilityReason::RequiredMeasurementMissing => "required measurement missing", IneligibilityReason::HostStorageEnvelopeExceeded => "host storage envelope exceeded", IneligibilityReason::ConflictingPooledSources => "pooled sources conflict", IneligibilityReason::CapabilityStatus { .. } => "individual capability did not pass" }.into()).collect(),
                renderer_ineligibility: candidate.ineligibility.iter().map(|reason| match reason {
                    IneligibilityReason::CapabilityStatus { capability, status } => format!(
                        "capability {} has exact status {}",
                        serde_json::to_value(capability).expect("capability serializes"),
                        serde_json::to_value(status).expect("status serializes")
                    ),
                    other => match other { IneligibilityReason::SchemaNotRecommendationEligible => "schema is descriptive-only", IneligibilityReason::ApiWorkloadRequired => "API workload is required", IneligibilityReason::EffectiveConfigurationUnavailable => "exact effective configuration unavailable", IneligibilityReason::CapabilityEvidenceUnavailable => "capability evidence unavailable", IneligibilityReason::CapabilityFailed => "one or more capabilities failed", IneligibilityReason::ProvenanceUnavailable => "provenance unavailable", IneligibilityReason::RequiredRepeatFailed => "required repeat failed", IneligibilityReason::RequiredRepeatMissing => "required repeat missing", IneligibilityReason::RequiredMeasurementMissing => "required measurement missing", IneligibilityReason::HostStorageEnvelopeExceeded => "host storage envelope exceeded", IneligibilityReason::ConflictingPooledSources => "pooled sources conflict", IneligibilityReason::CapabilityStatus { .. } => unreachable!() }.into(),
                }).collect(),
                required_allocation_bytes: candidate.summary.required_allocation_bytes,
                peak_allocation_bytes: candidate.summary.peak_allocation_bytes,
                success_rate: candidate.summary.success_rate,
                expected_repeats: candidate.summary.expected_repeats,
                completed_repeats: candidate.summary.completed_repeats,
                successful_repeats: candidate.summary.successful_repeats,
                launch_duration: candidate.summary.launch_duration,
                peak_ram: candidate.summary.peak_ram,
                launch_writes: candidate.summary.launch_writes,
                idle_writes: candidate.summary.idle_writes,
                rows: cohort_inputs.iter().flat_map(|input| sample_rows(input, &candidate.key)).collect(),
                capabilities: if kind == ExperimentKind::StorageLevers {
                    Vec::new()
                } else {
                    cohort_inputs.iter().filter(|input| input.repeats.iter().any(|repeat| candidate_key(input, Some(repeat)) == candidate.key)).flat_map(|input| match &input.capabilities { CapabilityEvidence::Available(v) => v.clone(), CapabilityEvidence::Unavailable => Vec::new() }).collect()
                },
                capabilities_available: kind != ExperimentKind::StorageLevers && {
                    let sources = cohort_inputs.iter().filter(|input| input.repeats.iter().any(|repeat| candidate_key(input, Some(repeat)) == candidate.key)).collect::<Vec<_>>();
                    !sources.is_empty() && sources.iter().all(|input| matches!(input.capabilities, CapabilityEvidence::Available(_)))
                },
                launch_samples_seconds: candidate
                    .repeats
                    .iter()
                    .filter_map(|repeat| repeat.metrics.launch_duration_secs)
                    .collect(),
                peak_ram_samples_bytes: candidate
                    .repeats
                    .iter()
                    .filter_map(|repeat| repeat.metrics.peak_ram_bytes)
                    .collect(),
            }).collect();
            CohortView {
                key: cohort.key.clone(),
                label: cohort_label(kind, index, &cohort.key),
                conditions: cohort_conditions(&cohort.key),
                conclusion,
                warning: matches!(&cohort.key,
                    CohortKey::Storage(StorageCohortKey { provenance: ComparableProvenance::Unknown { .. }, .. })
                    | CohortKey::MinimumHardware(MinimumHardwareCohortKey { provenance: ComparableProvenance::Unknown { .. }, .. }))
                    .then(|| "Legacy or incomplete provenance: this cohort is not comparable to other cohorts.".into()),
                best_supported: None,
                coverage: CoverageView::default(),
                matrix_capabilities,
                candidates,
                storage_summary: Vec::new(),
                charts: Vec::new(),
            }
        })
        .collect()
}

fn build_report_view(
    inputs: &[NormalizedInput],
    analysis: &Analysis,
    supplied_digests: &[InputDigestView],
) -> Result<ReportView> {
    if !supplied_digests.is_empty() && supplied_digests.len() != inputs.len() {
        bail!(
            "input digest count {} does not match normalized input count {}",
            supplied_digests.len(),
            inputs.len()
        );
    }
    let inputs_view = if supplied_digests.is_empty() {
        inputs.iter().map(input_digest_view).collect()
    } else {
        inputs
            .iter()
            .zip(supplied_digests)
            .map(|(input, digest)| {
                let source = input.identity.source.display().to_string();
                if digest.source != source {
                    bail!(
                        "input digest source '{}' does not match normalized input source '{}'",
                        digest.source,
                        source
                    );
                }
                let mut digest = digest.clone();
                let state = input_digest_view(input);
                digest.run_status = state.run_status;
                digest.evidence_state = state.evidence_state;
                digest.abort_error = state.abort_error;
                Ok(digest)
            })
            .collect::<Result<Vec<_>>>()?
    };
    let mut sections = Vec::new();
    let storage_inputs = inputs
        .iter()
        .filter(|i| i.identity.kind == ExperimentKind::StorageLevers)
        .collect::<Vec<_>>();
    if !storage_inputs.is_empty() {
        use std::collections::BTreeMap;
        let mut grouped: BTreeMap<
            std::collections::BTreeSet<u8>,
            (String, Vec<&NormalizedRepeat>),
        > = BTreeMap::new();
        for input in &storage_inputs {
            for repeat in &input.repeats {
                if let RepeatPayload::StorageLevers(payload) = &repeat.payload {
                    let entry =
                        grouped.entry(payload.levers.clone()).or_insert_with(
                            || (repeat.candidate.clone(), Vec::new()),
                        );
                    entry.1.push(repeat);
                }
            }
        }
        let declared = match &storage_inputs[0].dimensions {
            Dimensions::StorageLevers(d) => d.combinations.clone(),
            _ => Vec::new(),
        };
        let mut summaries = Vec::new();
        for (levers, (label, repeats)) in &grouped {
            let writes_decimal_gb = repeats
                .iter()
                .filter_map(|r| {
                    r.metrics.writes_bytes.map(|v| v as f64 / 1_000_000_000.0)
                })
                .collect::<Vec<_>>();
            summaries.push(StorageComboView {
                label: candidate_label(
                    label,
                    &CandidateKey::Storage(levers.clone()),
                ),
                key: CandidateKey::Storage(levers.clone()),
                rows: storage_inputs
                    .iter()
                    .flat_map(|input| {
                        sample_rows(
                            input,
                            &CandidateKey::Storage(levers.clone()),
                        )
                    })
                    .collect(),
                writes: (!writes_decimal_gb.is_empty())
                    .then(|| stats(&writes_decimal_gb)),
                writes_decimal_gb,
                launch_seconds: repeats
                    .iter()
                    .filter_map(|r| r.metrics.launch_duration_secs)
                    .collect(),
                peak_ram_decimal_gb: repeats
                    .iter()
                    .filter_map(|r| {
                        r.metrics
                            .peak_ram_bytes
                            .map(|v| v as f64 / 1_000_000_000.0)
                    })
                    .collect(),
                workload_ram_delta_decimal_gb: repeats
                    .iter()
                    .filter_map(|r| match &r.payload {
                        RepeatPayload::StorageLevers(p) => p
                            .workload_peak_delta_bytes
                            .map(|v| v as f64 / 1_000_000_000.0),
                        _ => None,
                    })
                    .collect(),
                workload_bytes: repeats
                    .iter()
                    .filter_map(|r| match &r.payload {
                        RepeatPayload::StorageLevers(p) => p.workload_bytes,
                        _ => None,
                    })
                    .collect(),
                workload_seconds: repeats
                    .iter()
                    .filter_map(|r| match &r.payload {
                        RepeatPayload::StorageLevers(p) => {
                            p.workload_duration_secs
                        }
                        _ => None,
                    })
                    .collect(),
                failed_repeats: repeats
                    .iter()
                    .filter_map(|r| match &r.outcome {
                        RepeatOutcome::Failure(e) => Some(e.clone()),
                        _ => None,
                    })
                    .collect(),
            });
        }
        summaries.sort_by_key(|summary| {
            declared
                .iter()
                .position(|label| label == &candidate_display(&summary.key))
                .unwrap_or(usize::MAX)
        });
        let labels = summaries
            .iter()
            .map(|summary| renderer_storage_label(&summary.label))
            .collect::<Vec<_>>();
        let mut charts = vec![
            ChartView {
                kind: ViewChartKind::GrossWrites,
                title: "Gross bring-up writes: individual samples".into(),
                unit: "decimal GB".into(),
                option: sample_chart(
                    "writes",
                    "decimal GB",
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| s.writes_decimal_gb.clone())
                        .collect::<Vec<_>>(),
                )?,
                fallback_rows: sample_fallback_rows(
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| s.writes_decimal_gb.clone())
                        .collect::<Vec<_>>(),
                ),
            },
            ChartView {
                kind: ViewChartKind::LaunchDuration,
                title: "Launch duration samples".into(),
                unit: "seconds".into(),
                option: sample_chart(
                    "launch",
                    "seconds",
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| {
                            s.launch_seconds.iter().map(|v| *v as f64).collect()
                        })
                        .collect::<Vec<_>>(),
                )?,
                fallback_rows: sample_fallback_rows(
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| {
                            s.launch_seconds.iter().map(|v| *v as f64).collect()
                        })
                        .collect::<Vec<_>>(),
                ),
            },
            ChartView {
                kind: ViewChartKind::PeakRam,
                title:
                    "Legacy absolute host peak RAM samples (descriptive only)"
                        .into(),
                unit: "decimal GB".into(),
                option: sample_chart(
                    "RAM",
                    "decimal GB",
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| s.peak_ram_decimal_gb.clone())
                        .collect::<Vec<_>>(),
                )?,
                fallback_rows: sample_fallback_rows(
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| s.peak_ram_decimal_gb.clone())
                        .collect::<Vec<_>>(),
                ),
            },
        ];
        let ladder = summaries.windows(2).all(|pair| {
            match (&pair[0].key, &pair[1].key) {
                (CandidateKey::Storage(a), CandidateKey::Storage(b)) => {
                    a.is_subset(b) && b.len() == a.len() + 1
                }
                _ => false,
            }
        });
        if summaries.len() > 1
            && ladder
            && summaries.iter().all(|s| s.writes.is_some())
        {
            let means = summaries
                .iter()
                .filter_map(|s| s.writes.map(|x| x.mean))
                .collect::<Vec<_>>();
            let deltas = means.windows(2).map(|w| w[1] - w[0]).collect();
            charts.push(ChartView {
                kind: ViewChartKind::Waterfall,
                title: "Incremental cumulative-ladder write changes".into(),
                unit: "change in decimal GB from previous rung".into(),
                option: bar_chart(
                    &labels[1..],
                    "change in decimal GB from previous rung",
                    deltas,
                )?,
                fallback_rows: series_fallback_rows(
                    &labels[1..],
                    "change in decimal GB from previous rung",
                    means.windows(2).map(|pair| Some(pair[1] - pair[0])),
                ),
            });
        }
        if summaries.iter().any(|s| !s.workload_bytes.is_empty()) {
            charts.push(ChartView {
                kind: ViewChartKind::WorkloadWear,
                title: "Workload writes".into(),
                unit: "decimal GB".into(),
                option: sample_chart(
                    "workload writes",
                    "decimal GB",
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| {
                            s.workload_bytes
                                .iter()
                                .map(|v| *v as f64 / 1_000_000_000.0)
                                .collect()
                        })
                        .collect::<Vec<_>>(),
                )?,
                fallback_rows: sample_fallback_rows(
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| {
                            s.workload_bytes
                                .iter()
                                .map(|v| *v as f64 / 1_000_000_000.0)
                                .collect()
                        })
                        .collect::<Vec<_>>(),
                ),
            });
            charts.push(ChartView {
                kind: ViewChartKind::WorkloadDuration,
                title: "Workload duration".into(),
                unit: "seconds".into(),
                option: sample_chart(
                    "workload duration",
                    "seconds",
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| {
                            s.workload_seconds
                                .iter()
                                .map(|v| *v as f64)
                                .collect()
                        })
                        .collect::<Vec<_>>(),
                )?,
                fallback_rows: sample_fallback_rows(
                    &labels,
                    &summaries
                        .iter()
                        .map(|s| {
                            s.workload_seconds
                                .iter()
                                .map(|v| *v as f64)
                                .collect()
                        })
                        .collect::<Vec<_>>(),
                ),
            });
        }
        let descriptive_ok = storage_inputs.len() > 1
            && storage_inputs.iter().all(|input| {
                input.identity.source_schema_version < 4
                    && matches!(input.provenance, Provenance::Unavailable)
            })
            && storage_inputs.windows(2).all(|pair| {
                match (
                    &pair[0].dimensions,
                    &pair[1].dimensions,
                    &pair[0].payload,
                    &pair[1].payload,
                ) {
                    (
                        Dimensions::StorageLevers(a),
                        Dimensions::StorageLevers(b),
                        ExperimentPayload::StorageLevers(ap),
                        ExperimentPayload::StorageLevers(bp),
                    ) => {
                        a.combinations == b.combinations
                            && a.rss_sleds == b.rss_sleds
                            && ap.workload == bp.workload
                    }
                    _ => false,
                }
            })
            && ladder
            && summaries
                .iter()
                .all(|summary| !summary.writes_decimal_gb.is_empty());
        let descriptive_aggregate = descriptive_ok.then(|| DescriptiveAggregateView {
            label: "Descriptive aggregate of same-shaped historical storage inputs".into(),
            disclaimer: "These selected historical inputs lack provenance. This pooled result is descriptive only, is not a controlled cohort, and is not a default recommendation.".into(),
            inputs: storage_inputs.iter().map(|input| input.identity.source.display().to_string()).collect(),
            storage_summary: summaries.clone(),
            charts: charts.clone(),
        });
        let mut cohorts =
            analysis_cohorts(inputs, analysis, ExperimentKind::StorageLevers);
        for cohort in &mut cohorts {
            cohort.storage_summary = cohort
                .candidates
                .iter()
                .map(|candidate| {
                    let writes_decimal_gb = candidate
                        .rows
                        .iter()
                        .filter_map(|row| {
                            row.metrics.writes_bytes.map(|v| v as f64 / 1e9)
                        })
                        .collect::<Vec<_>>();
                    StorageComboView {
                        label: candidate.label.clone(),
                        key: candidate.key.clone(),
                        rows: candidate.rows.clone(),
                        writes: (!writes_decimal_gb.is_empty())
                            .then(|| stats(&writes_decimal_gb)),
                        writes_decimal_gb,
                        launch_seconds: candidate
                            .rows
                            .iter()
                            .filter_map(|row| row.metrics.launch_duration_secs)
                            .collect(),
                        peak_ram_decimal_gb: candidate
                            .rows
                            .iter()
                            .filter_map(|row| {
                                row.metrics
                                    .peak_ram_bytes
                                    .map(|v| v as f64 / 1e9)
                            })
                            .collect(),
                        workload_ram_delta_decimal_gb: candidate
                            .rows
                            .iter()
                            .filter_map(|row| {
                                row.workload_peak_delta_bytes
                                    .map(|v| v as f64 / 1e9)
                            })
                            .collect(),
                        workload_bytes: candidate
                            .rows
                            .iter()
                            .filter_map(|row| row.workload_bytes)
                            .collect(),
                        workload_seconds: candidate
                            .rows
                            .iter()
                            .filter_map(|row| row.workload_duration_secs)
                            .collect(),
                        failed_repeats: candidate
                            .rows
                            .iter()
                            .filter_map(|row| match &row.outcome {
                                RepeatOutcome::Failure(error) => {
                                    Some(error.clone())
                                }
                                RepeatOutcome::Success => None,
                            })
                            .collect(),
                    }
                })
                .collect();
            let cohort_inputs = storage_inputs
                .iter()
                .filter(|input| cohort_key(input) == cohort.key)
                .copied()
                .collect::<Vec<_>>();
            let declared_keys = cohort_inputs
                .first()
                .and_then(|input| match &input.dimensions {
                    Dimensions::StorageLevers(d) => Some(
                        d.combinations
                            .iter()
                            .filter_map(|name| {
                                input
                                    .repeats
                                    .iter()
                                    .find(|repeat| &repeat.candidate == name)
                                    .map(|repeat| {
                                        candidate_key(input, Some(repeat))
                                    })
                            })
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            cohort.storage_summary.sort_by_key(|summary| {
                declared_keys
                    .iter()
                    .position(|key| key == &summary.key)
                    .unwrap_or(usize::MAX)
            });
            let labels = cohort
                .storage_summary
                .iter()
                .map(|row| renderer_storage_label(&row.label))
                .collect::<Vec<_>>();
            let expose_chart_sample_counts = cohort_inputs
                .iter()
                .any(|input| input.identity.source_schema_version == 5);
            let chart_from = |kind,
                              title: &str,
                              unit: &str,
                              samples: Vec<Vec<f64>>|
             -> Result<ChartView> {
                let mut option = sample_chart(title, unit, &labels, &samples)?;
                if expose_chart_sample_counts {
                    for (index, sample) in samples.iter().enumerate() {
                        option["series"][index]["name"] = Value::String(
                            format!("{} (n={})", labels[index], sample.len()),
                        );
                    }
                }
                Ok(ChartView {
                    kind,
                    title: title.into(),
                    unit: unit.into(),
                    option,
                    fallback_rows: sample_fallback_rows(&labels, &samples),
                })
            };
            let current_ram =
                cohort.storage_summary.iter().flat_map(|row| &row.rows).any(
                    |row| {
                        row.metrics.peak_ram_semantics
                            == Some(MemorySemantics::LaunchBaselineDelta)
                    },
                );
            cohort.charts = vec![
                chart_from(
                    ViewChartKind::GrossWrites,
                    "Gross bring-up writes: individual samples and mean",
                    "decimal GB",
                    cohort
                        .storage_summary
                        .iter()
                        .map(|r| r.writes_decimal_gb.clone())
                        .collect(),
                )?,
                chart_from(
                    ViewChartKind::LaunchDuration,
                    "Launch duration samples",
                    "seconds",
                    cohort
                        .storage_summary
                        .iter()
                        .map(|r| {
                            r.launch_seconds.iter().map(|v| *v as f64).collect()
                        })
                        .collect(),
                )?,
                chart_from(
                    ViewChartKind::PeakRam,
                    if current_ram {
                        "Launch RAM baseline-adjusted delta samples"
                    } else {
                        "Legacy absolute host peak RAM samples (descriptive only)"
                    },
                    "decimal GB",
                    cohort
                        .storage_summary
                        .iter()
                        .map(|r| r.peak_ram_decimal_gb.clone())
                        .collect(),
                )?,
            ];
            if cohort
                .storage_summary
                .iter()
                .any(|row| !row.workload_ram_delta_decimal_gb.is_empty())
            {
                cohort.charts.push(chart_from(
                    ViewChartKind::WorkloadRam,
                    "API workload RAM baseline-adjusted delta samples",
                    "decimal GB",
                    cohort
                        .storage_summary
                        .iter()
                        .map(|row| row.workload_ram_delta_decimal_gb.clone())
                        .collect(),
                )?);
            }
            let valid_ladder = declared_keys.len() > 1
                && declared_keys.len() == cohort.storage_summary.len()
                && declared_keys
                    .iter()
                    .zip(&cohort.storage_summary)
                    .all(|(key, row)| key == &row.key)
                && verified_storage_ladder(cohort);
            if valid_ladder
                && cohort.storage_summary.iter().all(|row| row.writes.is_some())
            {
                let means = cohort
                    .storage_summary
                    .iter()
                    .map(|row| row.writes.unwrap().mean)
                    .collect::<Vec<_>>();
                cohort.charts.push(ChartView {
                    kind: ViewChartKind::Waterfall,
                    title: "Incremental cumulative-ladder write changes".into(),
                    unit: "change in decimal GB from previous rung".into(),
                    option: bar_chart(
                        &labels[1..],
                        "change in decimal GB from previous rung",
                        means
                            .windows(2)
                            .map(|pair| pair[1] - pair[0])
                            .collect(),
                    )?,
                    fallback_rows: series_fallback_rows(
                        &labels[1..],
                        "change in decimal GB from previous rung",
                        means.windows(2).map(|pair| Some(pair[1] - pair[0])),
                    ),
                });
            }
            if cohort
                .storage_summary
                .iter()
                .any(|row| !row.workload_bytes.is_empty())
            {
                cohort.charts.push(chart_from(
                    ViewChartKind::WorkloadWear,
                    "Workload writes",
                    "decimal GB",
                    cohort
                        .storage_summary
                        .iter()
                        .map(|r| {
                            r.workload_bytes
                                .iter()
                                .map(|v| *v as f64 / 1e9)
                                .collect()
                        })
                        .collect(),
                )?);
                cohort.charts.push(chart_from(
                    ViewChartKind::WorkloadDuration,
                    "Workload duration",
                    "seconds",
                    cohort
                        .storage_summary
                        .iter()
                        .map(|r| {
                            r.workload_seconds
                                .iter()
                                .map(|v| *v as f64)
                                .collect()
                        })
                        .collect(),
                )?);
            }
            populate_storage_findings(cohort);
        }
        let warnings = storage_inputs
            .iter()
            .any(|input| matches!(input.capabilities, CapabilityEvidence::Unavailable))
            .then(|| "Historical storage inputs without capability evidence remain evidence but are ineligible for a default recommendation.".into())
            .into_iter()
            .collect();
        sections.push(ReportSectionView {
            kind: ExperimentKind::StorageLevers, title: "Storage levers".into(),
            conclusion: "Controlled conclusions and recommendations appear only inside their exact typed cohorts.".into(),
            warnings,
            cohorts, descriptive_aggregate,
        });
    }
    let minimum_inputs = inputs
        .iter()
        .filter(|i| i.identity.kind == ExperimentKind::MinimumHardware)
        .collect::<Vec<_>>();
    if !minimum_inputs.is_empty() {
        let mut cohorts =
            analysis_cohorts(inputs, analysis, ExperimentKind::MinimumHardware);
        for cohort in &mut cohorts {
            let labels = cohort
                .candidates
                .iter()
                .map(|c| c.label.clone())
                .collect::<Vec<_>>();
            cohort.charts = vec![
                ChartView {
                    kind: ViewChartKind::Allocation,
                    title: "Required and peak allocation".into(),
                    unit: "decimal GB".into(),
                    option: allocation_chart(
                        &labels,
                        &cohort
                            .candidates
                            .iter()
                            .map(|c| c.required_allocation_bytes)
                            .collect::<Vec<_>>(),
                        &cohort
                            .candidates
                            .iter()
                            .map(|c| c.peak_allocation_bytes)
                            .collect::<Vec<_>>(),
                    )?,
                    fallback_rows: {
                        let mut rows = series_fallback_rows(
                            &labels,
                            "Required allocation",
                            cohort.candidates.iter().map(|candidate| {
                                candidate
                                    .required_allocation_bytes
                                    .map(|value| value as f64 / 1e9)
                            }),
                        );
                        rows.extend(series_fallback_rows(
                            &labels,
                            "Peak allocation",
                            cohort.candidates.iter().map(|candidate| {
                                candidate
                                    .peak_allocation_bytes
                                    .map(|value| value as f64 / 1e9)
                            }),
                        ));
                        rows
                    },
                },
                ChartView {
                    kind: ViewChartKind::LaunchDuration,
                    title: "Launch duration samples".into(),
                    unit: "seconds".into(),
                    option: sample_chart(
                        "launch",
                        "seconds",
                        &labels,
                        &cohort
                            .candidates
                            .iter()
                            .map(|c| {
                                c.launch_samples_seconds
                                    .iter()
                                    .map(|v| *v as f64)
                                    .collect()
                            })
                            .collect::<Vec<_>>(),
                    )?,
                    fallback_rows: sample_fallback_rows(
                        &labels,
                        &cohort
                            .candidates
                            .iter()
                            .map(|candidate| {
                                candidate
                                    .launch_samples_seconds
                                    .iter()
                                    .map(|value| *value as f64)
                                    .collect()
                            })
                            .collect::<Vec<_>>(),
                    ),
                },
                ChartView {
                    kind: ViewChartKind::PeakRam,
                    title: "Peak RAM samples".into(),
                    unit: "decimal GB".into(),
                    option: sample_chart(
                        "RAM",
                        "decimal GB",
                        &labels,
                        &cohort
                            .candidates
                            .iter()
                            .map(|c| {
                                c.peak_ram_samples_bytes
                                    .iter()
                                    .map(|v| *v as f64 / 1e9)
                                    .collect()
                            })
                            .collect::<Vec<_>>(),
                    )?,
                    fallback_rows: sample_fallback_rows(
                        &labels,
                        &cohort
                            .candidates
                            .iter()
                            .map(|candidate| {
                                candidate
                                    .peak_ram_samples_bytes
                                    .iter()
                                    .map(|value| *value as f64 / 1e9)
                                    .collect()
                            })
                            .collect::<Vec<_>>(),
                    ),
                },
                ChartView {
                    kind: ViewChartKind::Capabilities,
                    title: "Individual capability status matrix".into(),
                    unit: "status".into(),
                    option: capability_chart(&cohort.candidates)?,
                    fallback_rows: capability_fallback_rows(&cohort.candidates),
                },
            ];
        }
        sections.push(ReportSectionView { kind: ExperimentKind::MinimumHardware, title: "Minimum hardware fixture evidence".into(), conclusion: "Capability, feasibility, allocation, RAM, launch, eligibility, and advisory decisions are grouped by cohort.".into(), warnings: Vec::new(), cohorts, descriptive_aggregate: None });
    }
    Ok(ReportView { title: "Voxel performance report".into(), executive_conclusion: "Conclusions and recommendations are cohort-local; charts supplement the complete tables below.".into(), inputs: inputs_view, sections, aggregation: None })
}

fn html_escape(text: &str) -> String {
    text.chars().fold(String::new(), |mut out, c| {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        };
        out
    })
}

const SVG_WIDTH: usize = 1200;
const SVG_HEIGHT: usize = 800;
// SVGs are derived from already-bounded report inputs. Keep independent finite
// fail-closed limits, but do not make this optional publication layer narrower
// than the 4096-input/repeat and 64 MiB JSON report contracts.
const MAX_SVG_VISIBLE_ROWS: usize = 1_000_000;
const MAX_SVG_VISIBLE_CATEGORIES: usize = 4096;
const MAX_SVG_LABEL_BYTES: usize = 160;
const MAX_SVG_ROW_LABEL_BYTES: usize = 120;
const MAX_SVG_BYTES: usize = 64 * 1024 * 1024;
const MAX_SVG_ARTIFACTS: usize = 32_768;
const MAX_SVG_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_SVG_FILENAME_BYTES: usize = 96;

#[derive(Clone, Copy, Eq, PartialEq)]
struct SvgLimits {
    rows: usize,
    categories: usize,
    artifacts: usize,
    individual_bytes: usize,
    total_bytes: usize,
}

const SVG_LIMITS: SvgLimits = SvgLimits {
    rows: MAX_SVG_VISIBLE_ROWS,
    categories: MAX_SVG_VISIBLE_CATEGORIES,
    artifacts: MAX_SVG_ARTIFACTS,
    individual_bytes: MAX_SVG_BYTES,
    total_bytes: MAX_SVG_TOTAL_BYTES,
};

/// Structural identity supplied by the report-view traversal. Indices, rather
/// than presentation labels, keep filenames safe and collision-free.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SvgChartIdentity {
    Cohort { section: usize, cohort: usize, chart: usize },
    Aggregate { section: usize, chart: usize },
}

impl SvgChartIdentity {
    fn cohort(section: usize, cohort: usize, chart: usize) -> Self {
        Self::Cohort { section, cohort, chart }
    }

    fn aggregate(section: usize, chart: usize) -> Self {
        Self::Aggregate { section, chart }
    }

    fn filename(self) -> Result<String> {
        let indices = match self {
            Self::Cohort { section, cohort, chart } => {
                vec![section, cohort, chart]
            }
            Self::Aggregate { section, chart } => vec![section, chart],
        };
        if indices.iter().any(|index| *index > 999_999) {
            bail!("SVG structural index exceeds 999999");
        }
        let filename = match self {
            Self::Cohort { section, cohort, chart } => format!(
                "section-{section:03}-cohort-{cohort:03}-chart-{chart:03}.svg"
            ),
            Self::Aggregate { section, chart } => {
                format!("section-{section:03}-aggregate-chart-{chart:03}.svg")
            }
        };
        if filename.len() > MAX_SVG_FILENAME_BYTES {
            bail!("SVG filename exceeds {MAX_SVG_FILENAME_BYTES} byte limit");
        }
        Ok(filename)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SvgArtifact {
    filename: String,
    bytes: Vec<u8>,
}

/// Renders and validates a deterministic collection for later publication
/// under `images/`. Charts without any plotted value intentionally produce no
/// artifact, matching the report's definition of a non-empty chart.
fn render_chart_svgs(
    charts: &[(SvgChartIdentity, &ChartView)],
) -> Result<Vec<SvgArtifact>> {
    render_chart_svgs_with_limits(charts, SVG_LIMITS)
}

fn render_chart_svgs_with_limits(
    charts: &[(SvgChartIdentity, &ChartView)],
    limits: SvgLimits,
) -> Result<Vec<SvgArtifact>> {
    let mut artifacts = Vec::new();
    let mut names = BTreeSet::new();
    let mut total_bytes = 0usize;
    for (identity, chart) in charts {
        if !chart.fallback_rows.iter().any(|row| row.value.is_some()) {
            continue;
        }
        if artifacts.len() == limits.artifacts {
            bail!("SVG artifact count exceeds {}", limits.artifacts);
        }
        let filename = identity.filename()?;
        if !names.insert(filename.clone()) {
            bail!("duplicate SVG structural identity for {filename}");
        }
        let bytes = if limits == SVG_LIMITS {
            render_chart_svg(chart)?
        } else {
            render_chart_svg_with_limits(chart, limits)?
        }
        .into_bytes();
        total_bytes = total_bytes
            .checked_add(bytes.len())
            .context("SVG total byte count overflow")?;
        if total_bytes > limits.total_bytes {
            bail!(
                "SVG artifacts exceed {} total byte limit",
                limits.total_bytes
            );
        }
        artifacts.push(SvgArtifact { filename, bytes });
    }
    artifacts.sort_by(|left, right| left.filename.cmp(&right.filename));
    Ok(artifacts)
}

fn report_svg_charts(view: &ReportView) -> Vec<(SvgChartIdentity, &ChartView)> {
    let mut charts = Vec::new();
    for (section_index, section) in view.sections.iter().enumerate() {
        if let Some(aggregate) = &section.descriptive_aggregate {
            charts.extend(aggregate.charts.iter().enumerate().map(
                |(chart, view)| {
                    (SvgChartIdentity::aggregate(section_index, chart), view)
                },
            ));
        }
        for (cohort_index, cohort) in section.cohorts.iter().enumerate() {
            charts.extend(cohort.charts.iter().enumerate().map(
                |(chart, view)| {
                    (
                        SvgChartIdentity::cohort(
                            section_index,
                            cohort_index,
                            chart,
                        ),
                        view,
                    )
                },
            ));
        }
    }
    charts
}

fn render_report_svgs(view: &ReportView) -> Result<Vec<SvgArtifact>> {
    render_chart_svgs(&report_svg_charts(view))
}

fn svg_text(x: f64, y: f64, class: &str, text: &str) -> String {
    let (width, height, baseline) = if class == "heading" {
        (1120.0, 28.0, 22.0)
    } else if class == "unit" {
        (1120.0, 18.0, 13.0)
    } else {
        (880.0, 18.0, 13.0)
    };
    format!(
        "<svg x=\"{x:.1}\" y=\"{:.1}\" width=\"{width:.1}\" height=\"{height:.1}\" overflow=\"hidden\"><text x=\"0\" y=\"{baseline:.1}\" class=\"{class}\">{}</text></svg>",
        y - baseline,
        html_escape(text)
    )
}

fn svg_value(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude != 0.0 && !(0.0001..1_000_000_000.0).contains(&magnitude) {
        format!("{value:.6e}")
    } else {
        format!("{value:.6}")
    }
}

fn valid_xml_text(text: &str) -> bool {
    text.chars().all(|character| {
        matches!(character, '\u{9}' | '\u{a}' | '\u{d}')
            || ('\u{20}'..='\u{d7ff}').contains(&character)
            || ('\u{e000}'..='\u{fffd}').contains(&character)
            || ('\u{10000}'..='\u{10ffff}').contains(&character)
    })
}

fn check_svg_size(size: usize, limit: usize) -> Result<()> {
    if size > limit {
        bail!("SVG exceeds {limit} byte limit");
    }
    Ok(())
}

fn render_chart_svg(chart: &ChartView) -> Result<String> {
    render_chart_svg_with_limits(chart, SVG_LIMITS)
}

fn render_chart_svg_with_limits(
    chart: &ChartView,
    limits: SvgLimits,
) -> Result<String> {
    if chart.fallback_rows.len() > limits.rows {
        bail!("SVG chart has more than {} visible fallback rows", limits.rows);
    }
    for label in std::iter::once(chart.title.as_str())
        .chain(std::iter::once(chart.unit.as_str()))
        .chain(
            chart
                .fallback_rows
                .iter()
                .flat_map(|row| [row.category.as_str(), row.series.as_str()]),
        )
    {
        if label.len() > MAX_SVG_LABEL_BYTES {
            bail!("SVG label exceeds {MAX_SVG_LABEL_BYTES} byte limit");
        }
        if !valid_xml_text(label) {
            bail!("SVG label contains a character forbidden by XML 1.0");
        }
    }
    if chart.fallback_rows.iter().any(|row| {
        row.category
            .len()
            .checked_add(row.series.len())
            .is_none_or(|size| size > MAX_SVG_ROW_LABEL_BYTES)
    }) {
        bail!(
            "SVG combined row label exceeds {MAX_SVG_ROW_LABEL_BYTES} byte limit"
        );
    }
    if chart
        .fallback_rows
        .iter()
        .filter_map(|row| row.value)
        .any(|value| !value.is_finite())
    {
        bail!("SVG chart contains a non-finite fallback value");
    }
    let mut categories = Vec::<&str>::new();
    let mut series = Vec::<&str>::new();
    for row in &chart.fallback_rows {
        if !categories.contains(&row.category.as_str()) {
            categories.push(&row.category);
        }
        if !series.contains(&row.series.as_str()) {
            series.push(&row.series);
        }
    }
    if categories.len() > limits.categories {
        bail!(
            "SVG chart has more than {} visible categories",
            limits.categories
        );
    }

    let layout = match chart.kind {
        ViewChartKind::Waterfall => "ladder",
        ViewChartKind::Allocation => "grouped-bars",
        ViewChartKind::Capabilities => "status-matrix",
        _ => "samples-and-mean",
    };
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{SVG_WIDTH}\" height=\"{SVG_HEIGHT}\" viewBox=\"0 0 {SVG_WIDTH} {SVG_HEIGHT}\" role=\"img\" aria-labelledby=\"title desc\"><title id=\"title\">{}</title><desc id=\"desc\">{} chart. Values are plotted from the report fallback rows; missing values are explicitly unavailable. Unit: {}.</desc><style>text{{font-family:monospace;font-size:12px;fill:#111}}.heading{{font-size:20px;font-weight:bold}}.axis,.zero-line{{stroke:#222;stroke-width:1}}.sample{{fill:#1769aa}}.mean{{fill:none;stroke:#111;stroke-width:3}}.positive{{fill:#397d49}}.negative{{fill:#b23a48}}.required{{fill:#356cb6}}.peak{{fill:#d17a22}}.missing{{fill:#fff;stroke:#555;stroke-dasharray:4 3}}.status{{stroke:#111;stroke-width:2}}</style><rect width=\"100%\" height=\"100%\" fill=\"white\"/><g data-layout=\"{layout}\">",
        html_escape(&chart.title),
        html_escape(layout),
        html_escape(&chart.unit)
    );
    svg.push_str(&svg_text(40.0, 35.0, "heading", &chart.title));
    svg.push_str(&svg_text(
        40.0,
        58.0,
        "unit",
        &format!("Unit: {}", chart.unit),
    ));
    svg.push_str("<path class=\"axis\" d=\"M220 90V750H1160\"/>");

    let values = chart
        .fallback_rows
        .iter()
        .filter_map(|row| row.value)
        .collect::<Vec<_>>();
    let minimum = values.iter().copied().fold(0.0_f64, f64::min);
    let maximum = values.iter().copied().fold(0.0_f64, f64::max);
    let raw_span = maximum - minimum;
    if !minimum.is_finite() || !maximum.is_finite() || !raw_span.is_finite() {
        bail!("SVG chart has a non-finite derived range");
    }
    let span = if raw_span == 0.0 { 1.0 } else { raw_span };
    let value_y = |value: f64| 730.0 - ((value - minimum) / span * 610.0);
    let count = chart.fallback_rows.len().max(1) as f64;
    let mut mean_segments = Vec::<Vec<String>>::new();
    let mut mean_points = Vec::new();

    if chart.kind == ViewChartKind::Waterfall {
        let zero_x = 690.0 + (0.0 - minimum) / span * 420.0;
        if !zero_x.is_finite() || !(690.0..=1110.0).contains(&zero_x) {
            bail!("SVG waterfall zero coordinate is outside the plot");
        }
        svg.push_str(&format!(
            "<path class=\"zero-line\" d=\"M{zero_x:.1} 90V750\"/>"
        ));
    }

    let ordered_rows = if chart.kind == ViewChartKind::Allocation {
        categories
            .iter()
            .flat_map(|category| {
                series.iter().filter_map(move |series_name| {
                    chart.fallback_rows.iter().find(|row| {
                        row.category == **category
                            && row.series == **series_name
                    })
                })
            })
            .collect::<Vec<_>>()
    } else {
        chart.fallback_rows.iter().collect::<Vec<_>>()
    };

    for (index, row) in ordered_rows.iter().enumerate() {
        let category_index = categories
            .iter()
            .position(|category| *category == row.category)
            .expect("projected category exists");
        let x = if categories.len() <= 1 {
            685.0
        } else {
            235.0
                + category_index as f64
                    * (900.0 / (categories.len() - 1) as f64)
        };
        let y = 105.0 + index as f64 * (620.0 / count);
        let value_text =
            row.value.map(svg_value).unwrap_or_else(|| "unavailable".into());
        match chart.kind {
            ViewChartKind::Capabilities => {
                let (symbol, status, class, shape) = match row.value {
                    Some(1.0) => (
                        "●",
                        "Pass",
                        "positive status",
                        format!(
                            "<circle cx=\"250\" cy=\"{y:.1}\" r=\"7\" class=\"status\"/>"
                        ),
                    ),
                    Some(0.0) => (
                        "×",
                        "Fail",
                        "negative status",
                        format!(
                            "<path d=\"M243 {:.1}L257 {:.1}M257 {:.1}L243 {:.1}\" class=\"status\"/>",
                            y - 7.0,
                            y + 7.0,
                            y - 7.0,
                            y + 7.0
                        ),
                    ),
                    Some(_) => (
                        "◇",
                        "Unavailable",
                        "missing status",
                        format!(
                            "<path d=\"M250 {:.1}L257 {y:.1}L250 {:.1}L243 {y:.1}Z\" class=\"status\"/>",
                            y - 8.0,
                            y + 8.0
                        ),
                    ),
                    None => (
                        "?",
                        "unavailable",
                        "missing status",
                        format!(
                            "<rect x=\"243\" y=\"{:.1}\" width=\"14\" height=\"14\" class=\"status missing\"/>",
                            y - 7.0
                        ),
                    ),
                };
                svg.push_str(&format!(
                    "<rect x=\"225\" y=\"{:.1}\" width=\"900\" height=\"{:.1}\" class=\"{}\"/>",
                    y - 15.0,
                    (600.0 / count).max(2.0),
                    class
                ));
                svg.push_str(&shape);
                svg.push_str(&svg_text(
                    240.0,
                    y,
                    "status-label",
                    &format!(
                        "{symbol} {status} — {} — {}: {value_text}",
                        row.category, row.series
                    ),
                ));
            }
            ViewChartKind::Waterfall => {
                if let Some(value) = row.value {
                    let zero_x = 690.0 + (0.0 - minimum) / span * 420.0;
                    let value_x = 690.0 + (value - minimum) / span * 420.0;
                    let left = zero_x.min(value_x);
                    let width = (value_x - zero_x).abs().max(1.0);
                    let class =
                        if value >= 0.0 { "positive" } else { "negative" };
                    svg.push_str(&format!("<rect x=\"{left:.1}\" y=\"{:.1}\" width=\"{width:.1}\" height=\"14\" class=\"{class}\"/>", y - 12.0));
                } else {
                    svg.push_str(&format!("<rect x=\"690\" y=\"{:.1}\" width=\"420\" height=\"14\" class=\"missing\"/>", y - 12.0));
                }
                svg.push_str(&svg_text(
                    240.0,
                    y,
                    "value",
                    &format!(
                        "{} — {}: {value_text} {}",
                        row.category, row.series, chart.unit
                    ),
                ));
            }
            ViewChartKind::Allocation => {
                let class =
                    if row.series.to_ascii_lowercase().contains("required") {
                        "required"
                    } else {
                        "peak"
                    };
                let width = row.value.map_or(0.0, |value| {
                    if maximum > 0.0 {
                        value.max(0.0) / maximum * 420.0
                    } else {
                        0.0
                    }
                });
                if row.value.is_some() {
                    svg.push_str(&format!("<rect x=\"690\" y=\"{:.1}\" width=\"{width:.1}\" height=\"14\" class=\"{class}\"/>", y - 12.0));
                } else {
                    svg.push_str(&format!("<rect x=\"690\" y=\"{:.1}\" width=\"14\" height=\"14\" class=\"missing\"/>", y - 12.0));
                }
                svg.push_str(&svg_text(
                    240.0,
                    y,
                    "value",
                    &format!(
                        "{} — {}: {value_text} {}",
                        row.category, row.series, chart.unit
                    ),
                ));
            }
            _ => {
                if let Some(value) = row.value {
                    let cy = value_y(value);
                    if !cy.is_finite() || !(120.0..=730.0).contains(&cy) {
                        bail!("SVG sample coordinate is outside the plot");
                    }
                    if row.series == "Mean" {
                        mean_points.push(format!("{x:.1},{cy:.1}"));
                    }
                    svg.push_str(&format!(
                        "<circle data-category-x=\"{x:.1}\" cx=\"{x:.1}\" cy=\"{cy:.1}\" r=\"{}\" class=\"{}\"/>",
                        if row.series == "Mean" { 6 } else { 4 },
                        if row.series == "Mean" {
                            "mean"
                        } else {
                            "sample"
                        }
                    ));
                } else {
                    if row.series == "Mean" && !mean_points.is_empty() {
                        mean_segments.push(std::mem::take(&mut mean_points));
                    }
                    svg.push_str(&format!("<rect x=\"{:.1}\" y=\"710\" width=\"12\" height=\"12\" class=\"missing\"/>", x - 6.0));
                }
                svg.push_str(&svg_text(
                    240.0,
                    y,
                    "value",
                    &format!(
                        "{} — {}: {value_text} {}",
                        row.category, row.series, chart.unit
                    ),
                ));
            }
        }
    }
    if !mean_points.is_empty() {
        mean_segments.push(mean_points);
    }
    for mean_points in mean_segments {
        svg.push_str(&format!(
            "<polyline class=\"mean\" points=\"{}\"/>",
            mean_points.join(" ")
        ));
    }
    svg.push_str("</g></svg>");
    check_svg_size(svg.len(), limits.individual_bytes)?;
    Ok(svg)
}

fn script_json(value: &Value) -> Result<String> {
    let json = serde_json::to_string(value)
        .context("serialize embedded chart option")?;
    Ok(json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

fn stable_html_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map(|json| html_escape(&json))
        .context("serialize HTML evidence")
}

fn populate_storage_findings(cohort: &mut CohortView) {
    let workload_requested = matches!(
        &cohort.key,
        CohortKey::Storage(StorageCohortKey { workload: Some(_), .. })
    );
    let rows = cohort
        .storage_summary
        .iter()
        .flat_map(|summary| &summary.rows)
        .collect::<Vec<_>>();
    let planned_slots = cohort
        .candidates
        .iter()
        .map(|candidate| candidate.expected_repeats)
        .sum();
    let mut coverage = CoverageView {
        planned_slots,
        launch_samples: rows
            .iter()
            .filter(|row| row.metrics.launch_duration_secs.is_some())
            .count(),
        workload_requested,
        ..CoverageView::default()
    };
    if workload_requested {
        for row in rows {
            match row.workload_disposition {
                Some(WorkloadDisposition::Succeeded) => {
                    coverage.workload_succeeded += 1
                }
                Some(WorkloadDisposition::Failed) => {
                    coverage.workload_failed += 1
                }
                Some(WorkloadDisposition::Blocked) => {
                    coverage.workload_blocked += 1
                }
                Some(
                    WorkloadDisposition::Legacy
                    | WorkloadDisposition::Pending
                    | WorkloadDisposition::NotRequested,
                )
                | None => {
                    coverage.unresolved += 1;
                }
            }
        }
        coverage.unresolved += coverage
            .planned_slots
            .saturating_sub(coverage.accounted_workload_slots());
        debug_assert_eq!(
            coverage.accounted_workload_slots(),
            coverage.planned_slots
        );
    }
    cohort.coverage = coverage;

    let sample_stats = |values: &[u64]| {
        (!values.is_empty()).then(|| {
            stats(&values.iter().map(|value| *value as f64).collect::<Vec<_>>())
        })
    };
    let objectives = cohort
        .storage_summary
        .iter()
        .filter_map(|summary| {
            let (first, second) = if workload_requested {
                (
                    sample_stats(&summary.workload_bytes),
                    sample_stats(&summary.workload_seconds),
                )
            } else {
                (summary.writes, sample_stats(&summary.launch_seconds))
            };
            first.zip(second).map(|objectives| (summary, objectives))
        })
        .collect::<Vec<_>>();
    let winner = (objectives.len() > 1)
        .then(|| {
            objectives.iter().find(|(candidate, candidate_stats)| {
                objectives.iter().all(|(other, other_stats)| {
                    candidate.key == other.key
                        || (compare_stat(
                            Some(candidate_stats.0),
                            Some(other_stats.0),
                        ) == MetricComparison::Better
                            && compare_stat(
                                Some(candidate_stats.1),
                                Some(other_stats.1),
                            ) == MetricComparison::Better)
                })
            })
        })
        .flatten()
        .map(|(summary, _)| *summary);
    cohort.best_supported = winner.map(|winner| {
        let first = if workload_requested {
            sample_stats(&winner.workload_bytes)
                .expect("winner has workload writes")
                .mean
                / 1e9
        } else {
            winner.writes.expect("winner has launch writes").mean
        };
        let second = sample_stats(if workload_requested {
            &winner.workload_seconds
        } else {
            &winner.launch_seconds
        })
        .expect("winner has duration samples")
        .mean;
        let sample_count = if workload_requested {
            winner.workload_bytes.len()
        } else {
            winner.launch_seconds.len()
        };
        BestSupportedRecommendationView {
            candidate: renderer_storage_label(&winner.label),
            basis: if workload_requested {
                format!(
                    "lowest observed mean workload writes ({first:.6} decimal GB) and workload duration ({second:.1} seconds), based on {sample_count} retained workload samples"
                )
            } else {
                format!(
                    "lowest observed mean launch writes ({first:.6} decimal GB) and launch duration ({second:.1} seconds), based on {sample_count} retained launch samples"
                )
            },
            missing_candidates: cohort
                .storage_summary
                .iter()
                .filter(|summary| {
                    if workload_requested {
                        summary.workload_bytes.is_empty() || summary.workload_seconds.is_empty()
                    } else {
                        summary.writes.is_none() || summary.launch_seconds.is_empty()
                    }
                })
                .map(|summary| renderer_storage_label(&summary.label))
                .collect(),
        }
    });
}

fn chart_fallback_html(chart: &ChartView) -> Result<String> {
    let mut html = format!(
        "<table data-chart-fallback=\"{}\"><caption>{} ({})</caption><thead><tr><th>Category</th><th>Series</th><th>Value</th></tr></thead><tbody>",
        html_escape(&format!("{:?}", chart.kind)),
        html_escape(&chart.title),
        html_escape(&chart.unit)
    );
    for row in &chart.fallback_rows {
        html.push_str(&format!(
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td></tr>",
            html_escape(&row.category),
            html_escape(&row.series),
            row.value
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|| "unavailable".into())
        ));
    }
    html.push_str("</tbody></table>");
    Ok(html)
}

fn condition_value_html(row: Option<&ConditionRow>) -> String {
    let Some(row) = row else {
        return "<span class=\"state missing\">absent</span>".into();
    };
    let value = html_escape(&row.value);
    if row.code { format!("<code>{value}</code>") } else { value }
}

fn practical_stats(stats: Option<Stats>) -> String {
    stats.map_or_else(
        || "<span class=\"state unavailable\">unavailable</span>".into(),
        |stats| {
            format!(
                "{:.3} ± {:.3} <small>(n={})</small>",
                stats.mean, stats.stddev, stats.n
            )
        },
    )
}

fn practical_values(values: impl IntoIterator<Item = f64>) -> String {
    let values = values.into_iter().collect::<Vec<_>>();
    practical_stats((!values.is_empty()).then(|| stats(&values)))
}

fn complete_stats(stats: Option<Stats>, unit: &str) -> String {
    stats.map_or_else(
        || "unavailable".into(),
        |stats| {
            format!(
                "{} / {:.6} / {:.6} / {:.6} / {} ({unit})",
                stats.n,
                stats.mean,
                stats.median,
                stats.stddev,
                stats
                    .cv
                    .map(|value| format!("{value:.6}"))
                    .unwrap_or_else(|| "unavailable".into())
            )
        },
    )
}

fn complete_values(values: &[u64], unit: &str) -> String {
    complete_stats(
        (!values.is_empty()).then(|| {
            stats(&values.iter().map(|value| *value as f64).collect::<Vec<_>>())
        }),
        unit,
    )
}

fn stage_diagnostics_html(row: &SampleRow) -> String {
    [
        ("Boundary failure", row.boundary_failure.as_deref()),
        ("Launch failure", row.launch_failure.as_deref()),
        (
            "Prior launch attempt failures",
            row.prior_launch_attempt_failures.as_deref(),
        ),
        ("Preparation failure", row.preparation_failure.as_deref()),
        ("Workload failure", row.workload_failure.as_deref()),
    ]
    .into_iter()
    .filter_map(|(label, error)| {
        error.map(|error| {
            format!(
                "<div><strong>{label}:</strong> {}</div>",
                html_escape(error)
            )
        })
    })
    .collect()
}

fn render_ergonomic_report_html(report: &ReportView) -> Result<String> {
    const ECHARTS: &str = include_str!("../../assets/echarts-5.5.1.min.js");
    let mut options = Vec::new();
    let mut body = format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'none'; connect-src 'none'; font-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{0}</title><style>
:root{{--ink:#172033;--muted:#526075;--line:#cbd5e1;--paper:#fff;--accent:#075985}}*{{box-sizing:border-box}}html{{scroll-behavior:smooth}}body{{font:15px/1.45 system-ui;max-width:1280px;margin:auto;padding:1.5rem;color:var(--ink);background:var(--paper)}}nav{{position:sticky;top:0;z-index:2;background:#fffffff2;border-bottom:1px solid var(--line);padding:.65rem 0;display:flex;gap:1rem;overflow:auto}}nav a{{color:var(--accent);white-space:nowrap}}nav a:focus-visible,summary:focus-visible{{outline:3px solid #38bdf8;outline-offset:2px}}section,article{{scroll-margin-top:4rem}}article{{border-top:2px solid var(--line);margin-top:2rem;padding-top:1rem}}.cards{{display:grid;grid-template-columns:repeat(auto-fit,minmax(17rem,1fr));gap:.8rem;margin:1rem 0}}.card{{border:1px solid var(--line);border-radius:.4rem;padding:.8rem;background:#f8fafc}}.warning,.failed{{border-left:4px solid #b45309;padding:.6rem;background:#fff7ed}}.pending{{color:#854d0e}}.blocked{{color:#9a3412}}.unavailable,.missing{{color:var(--muted);font-style:italic}}table{{border-collapse:collapse;width:100%;margin:.7rem 0}}th,td{{border:1px solid var(--line);padding:.38rem;text-align:left;vertical-align:top}}thead th{{position:sticky;top:3rem;background:#e2e8f0;z-index:1}}.table-scroll{{overflow:auto}}.chart{{height:360px;min-width:280px}}code{{overflow-wrap:anywhere}}details{{margin:.7rem 0}}summary{{cursor:pointer;font-weight:600}}.ladder tbody th::before{{content:'↳ ';color:var(--accent)}}small{{color:var(--muted)}}@media(max-width:650px){{body{{padding:.7rem}}.chart{{height:300px}}th,td{{font-size:.85rem}}}}@media print{{nav{{display:none}}body{{max-width:none;padding:0}}.chart{{break-inside:avoid}}details{{break-inside:avoid}}thead th{{position:static}}}}
</style></head><body><header id="top"><h1>{0}</h1><p>{1}</p></header><nav aria-label="Report sections"><a href="#findings">Findings</a><a href="#experiments">Experiments</a><a href="#appendix">Provenance appendix</a></nav>"##,
        html_escape(&report.title),
        html_escape(&report.executive_conclusion)
    );

    for input in &report.inputs {
        let warning = match input.evidence_state.as_deref() {
            Some("interrupted-current-snapshot") => Some(
                "Interrupted/current snapshot — this is not a completed run.",
            ),
            Some("aborted") => Some(
                "Aborted run — retained measurements are incomplete evidence.",
            ),
            Some("partial-evidence") => Some(
                "Partial evidence — the completed run contains stage failures.",
            ),
            _ => None,
        };
        if let Some(warning) = warning {
            body.push_str(&format!(
                "<p class=\"warning\"><strong>{}</strong> Input: <code>{}</code>{}</p>",
                warning,
                html_escape(&input.source),
                input
                    .abort_error
                    .as_ref()
                    .map(|error| format!(
                        "<br><strong>Abort reason:</strong> {}",
                        html_escape(error)
                    ))
                    .unwrap_or_default()
            ));
        }
    }

    body.push_str("<main id=\"findings\"><div id=\"experiments\">");
    for section in &report.sections {
        let section_id =
            format!("section-{}", experiment_kind_id(section.kind));
        let factored = factor_condition_rows(
            &section
                .cohorts
                .iter()
                .map(|cohort| cohort.conditions.clone())
                .collect::<Vec<_>>(),
        );
        body.push_str(&format!(
            "<section id=\"{}\"><h2>{}</h2><p>{}</p>",
            section_id,
            html_escape(&section.title),
            html_escape(&section.conclusion)
        ));
        for warning in &section.warnings {
            body.push_str(&format!(
                "<p class=\"warning\"><strong>Warning:</strong> {}</p>",
                html_escape(warning)
            ));
        }
        if !factored.shared.is_empty() {
            body.push_str(
                "<details><summary>Settings shared by all cohorts</summary><table><tbody>",
            );
            for row in &factored.shared {
                body.push_str(&format!(
                    "<tr><th>{}</th><td>{}</td></tr>",
                    html_escape(&row.label),
                    condition_value_html(Some(row))
                ));
            }
            body.push_str("</tbody></table></details>");
        }
        if let Some(aggregate) = &section.descriptive_aggregate {
            body.push_str(&format!(
                "<aside><h3>{}</h3><p class=\"warning\">{}</p><p><strong>Contributing inputs:</strong> {}</p><details><summary>Concise aggregate summary</summary><table><thead><tr><th>Candidate</th><th>Launch writes (decimal GB)</th><th>Launch duration (seconds)</th><th>Peak RAM (decimal GB)</th><th>Retained samples</th></tr></thead><tbody>",
                html_escape(&aggregate.label), html_escape(&aggregate.disclaimer),
                html_escape(&aggregate.inputs.join(", "))
            ));
            for summary in &aggregate.storage_summary {
                body.push_str(&format!(
                    "<tr><th>{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    html_escape(&renderer_storage_label(&summary.label)),
                    practical_stats(summary.writes),
                    practical_values(summary.launch_seconds.iter().map(|v| *v as f64)),
                    practical_values(summary.peak_ram_decimal_gb.iter().copied()),
                    summary.rows.len()
                ));
            }
            body.push_str("</tbody></table></details>");
            body.push_str("<details><summary>Complete aggregate evidence</summary><table><thead><tr><th>Candidate</th><th>Launch writes n / mean / median / stddev / CV (decimal GB)</th><th>Launch samples (seconds)</th><th>Peak RAM samples (decimal GB)</th><th>Workload bytes</th><th>Workload duration (seconds)</th><th>Source / run / repeat / outcome</th></tr></thead><tbody>");
            for summary in &aggregate.storage_summary {
                let attribution = summary
                    .rows
                    .iter()
                    .map(|row| {
                        format!(
                            "{} / {} / {} / {}",
                            html_escape(&row.source),
                            html_escape(&row.run_id),
                            row.repeat_ordinal,
                            stable_html_json(&row.outcome)
                                .unwrap_or_else(|_| "unavailable".into())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("<br>");
                body.push_str(&format!(
                    "<tr><th>{}</th><td>{}</td><td>{:?}</td><td>{:?}</td><td>{:?}</td><td>{:?}</td><td>{}</td></tr>",
                    html_escape(&renderer_storage_label(&summary.label)),
                    complete_stats(summary.writes, "decimal GB"),
                    summary.launch_seconds,
                    summary.peak_ram_decimal_gb,
                    summary.workload_bytes,
                    summary.workload_seconds,
                    attribution
                ));
            }
            body.push_str("</tbody></table></details>");
            for chart in aggregate.charts.iter().filter(|chart| {
                chart.fallback_rows.iter().any(|row| row.value.is_some())
            }) {
                let id = format!("chart-{}", options.len());
                body.push_str(&format!("<section><h4>{}</h4><div id=\"{}\" class=\"chart\" role=\"img\" aria-label=\"{}\"></div><details><summary>Tabulated results</summary>{}</details></section>", html_escape(&chart.title), id, html_escape(&chart.title), chart_fallback_html(chart)?));
                options.push((id, script_json(&chart.option)?));
            }
            body.push_str("</aside>");
        }
        for (cohort_index, cohort) in section.cohorts.iter().enumerate() {
            let cohort_id = cohort_anchor(&cohort.key);
            let empty = cohort_has_no_measurements(cohort);
            body.push_str(&format!("<article id=\"{}\"><h3>{}</h3><div class=\"cards\"><div class=\"card\"><h4>Verdict</h4><p>{}</p>", cohort_id, html_escape(&cohort.label), html_escape(&cohort.conclusion)));
            if let Some(best) = &cohort.best_supported {
                body.push_str(&format!(
                    "<h5>Best-supported recommendation</h5><p><strong>{}</strong>: {}</p>",
                    html_escape(&best.candidate),
                    html_escape(&best.basis)
                ));
                if !best.missing_candidates.is_empty() {
                    body.push_str(&format!(
                        "<p class=\"warning\">Missing comparable measurements: {}</p>",
                        html_escape(&best.missing_candidates.join(", "))
                    ));
                }
            } else {
                body.push_str("<h5>Best-supported recommendation</h5><p>No unique best-supported configuration can be identified from the available aggregate evidence.</p>");
            }
            body.push_str(
                "</div><div class=\"card\"><h4>Evidence coverage</h4>",
            );
            body.push_str(&format!(
                "<p>{} of {} planned launch samples retained.</p>",
                cohort.coverage.launch_samples, cohort.coverage.planned_slots
            ));
            if cohort.coverage.workload_requested {
                body.push_str(&format!("<p><span class=\"success\">{} succeeded</span>; <span class=\"failed\">{} failed</span>; <span class=\"blocked\">{} blocked</span>; <span class=\"pending\">{} pending or unresolved</span>.</p>", cohort.coverage.workload_succeeded, cohort.coverage.workload_failed, cohort.coverage.workload_blocked, cohort.coverage.unresolved));
            } else {
                body.push_str("<p>No API workload was requested.</p>");
            }
            body.push_str("</div></div>");
            if let Some(warning) = &cohort.warning {
                body.push_str(&format!(
                    "<p class=\"warning\">{}</p>",
                    html_escape(warning)
                ));
            }

            if empty {
                body.push_str("<p class=\"unavailable\"><strong>No repeat measurements are available for this cohort.</strong> Content-free repeat charts and the placeholder sample table are omitted.</p>");
                for input in &report.inputs {
                    let associated = cohort
                        .candidates
                        .iter()
                        .flat_map(|candidate| &candidate.rows)
                        .any(|row| row.source == input.source)
                        || cohort
                            .matrix_capabilities
                            .iter()
                            .any(|ledger| ledger.source == input.source);
                    if associated {
                        if let Some(reason) = &input.abort_error {
                            body.push_str(&format!("<p class=\"warning\"><strong>Associated abort reason:</strong> {}</p>", html_escape(reason)));
                        }
                    }
                }
                body.push_str(
                    "<details><summary>Formal retained evidence</summary>",
                );
            }

            if let Some(settings) = candidate_setting_projection(cohort) {
                body.push_str(if verified_storage_ladder(cohort) { "<h4>Verified storage lever ladder</h4><div class=\"table-scroll\"><table class=\"ladder\">" } else { "<h4>Candidate setting matrix</h4><div class=\"table-scroll\"><table>" });
                body.push_str("<thead><tr><th>Candidate</th>");
                for row in &settings.varying {
                    body.push_str(&format!(
                        "<th>{}</th>",
                        html_escape(&row.label)
                    ));
                }
                body.push_str("</tr></thead><tbody>");
                for (index, candidate) in settings.candidates.iter().enumerate()
                {
                    body.push_str(&format!(
                        "<tr><th>{}</th>",
                        html_escape(candidate)
                    ));
                    for row in &settings.varying {
                        body.push_str(&format!(
                            "<td>{}</td>",
                            condition_value_html(row.values[index].as_ref())
                        ));
                    }
                    body.push_str("</tr>");
                }
                body.push_str("</tbody></table></div>");
                if !settings.shared.is_empty() {
                    body.push_str("<details><summary>Settings shared by all candidates</summary><table><tbody>");
                    for row in &settings.shared {
                        body.push_str(&format!(
                            "<tr><th>{}</th><td>{}</td></tr>",
                            html_escape(&row.label),
                            condition_value_html(Some(row))
                        ));
                    }
                    body.push_str("</tbody></table></details>");
                }
            }

            let visible_charts = cohort
                .charts
                .iter()
                .filter(|chart| {
                    chart.fallback_rows.iter().any(|row| row.value.is_some())
                })
                .collect::<Vec<_>>();
            if !visible_charts.is_empty() {
                body.push_str("<h4>Charts</h4><noscript><p class=\"warning\">Interactive charts require JavaScript; use Tabulated results.</p></noscript>");
            }
            for chart in visible_charts {
                let id = format!("chart-{}", options.len());
                body.push_str(&format!("<section><h5>{}</h5><div id=\"{}\" class=\"chart\" role=\"img\" aria-label=\"{}\"></div><details><summary>Tabulated results</summary>{}</details></section>", html_escape(&chart.title), id, html_escape(&chart.title), chart_fallback_html(chart)?));
                options.push((id, script_json(&chart.option)?));
            }

            body.push_str("<h4>Formal recommendation eligibility</h4><div class=\"table-scroll\"><table><thead><tr><th>Candidate</th><th>Eligible</th><th>Feasible</th><th>Recommended</th><th>Successful repeats</th><th>Launch duration (seconds)</th><th>Peak RAM (bytes)</th><th>Launch writes (bytes)</th><th>Decision and exact ineligibility</th></tr></thead><tbody>");
            for candidate in &cohort.candidates {
                body.push_str(&format!("<tr><th>{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}/{} ({:.1}%)</td><td>{}</td><td>{}</td><td>{}</td><td>{}{}</td></tr>", html_escape(&renderer_candidate_label(candidate)), candidate.eligible, candidate.feasible.map_or("—".into(), |v| v.to_string()), candidate.recommended, candidate.successful_repeats, candidate.expected_repeats, candidate.success_rate * 100.0, practical_stats(candidate.launch_duration), practical_stats(candidate.peak_ram), practical_stats(candidate.launch_writes), html_escape(&candidate.decision), if candidate.renderer_ineligibility.is_empty() { String::new() } else { format!("<ul>{}</ul>", candidate.renderer_ineligibility.iter().map(|reason| format!("<li>{}</li>", html_escape(reason))).collect::<String>()) }));
            }
            body.push_str("</tbody></table></div>");

            body.push_str("<details><summary>Complete evidence</summary><table><thead><tr><th>Candidate</th><th>Host capacity (bytes)</th><th>Required allocation (bytes)</th><th>Peak allocation (bytes)</th><th>Completed repeats</th><th>Launch duration (seconds) n / mean / median / stddev / CV</th><th>Peak RAM (bytes) n / mean / median / stddev / CV</th><th>Launch writes (bytes) n / mean / median / stddev / CV</th><th>Idle writes (bytes) n / mean / median / stddev / CV</th><th>Workload writes (bytes) n / mean / median / stddev / CV</th><th>Workload duration (seconds) n / mean / median / stddev / CV</th><th>Workload peak RAM delta (bytes) n / mean / median / stddev / CV</th><th>Workload peak RAM semantics</th></tr></thead><tbody>");
            for candidate in &cohort.candidates {
                let workload_bytes = candidate
                    .rows
                    .iter()
                    .filter_map(|row| row.workload_bytes)
                    .collect::<Vec<_>>();
                let workload_seconds = candidate
                    .rows
                    .iter()
                    .filter_map(|row| row.workload_duration_secs)
                    .collect::<Vec<_>>();
                let workload_peak_delta = candidate
                    .rows
                    .iter()
                    .filter_map(|row| row.workload_peak_delta_bytes)
                    .collect::<Vec<_>>();
                let workload_semantics = candidate
                    .rows
                    .iter()
                    .filter_map(|row| {
                        row.workload_peak_ram_semantics
                            .map(|semantics| format!("{semantics:?}"))
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!("<tr><th>{}</th><td>{}</td><td>{}</td><td>{}</td><td>{} of {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", html_escape(&renderer_candidate_label(candidate)), candidate.host_storage_capacity_bytes.map_or("unavailable".into(), |value| value.to_string()), candidate.required_allocation_bytes.map_or("unavailable".into(), |value| value.to_string()), candidate.peak_allocation_bytes.map_or("unavailable".into(), |value| value.to_string()), candidate.completed_repeats, candidate.expected_repeats, complete_stats(candidate.launch_duration, "seconds"), complete_stats(candidate.peak_ram, "bytes"), complete_stats(candidate.launch_writes, "bytes"), complete_stats(candidate.idle_writes, "bytes"), complete_values(&workload_bytes, "bytes"), complete_values(&workload_seconds, "seconds"), complete_values(&workload_peak_delta, "bytes"), if workload_semantics.is_empty() { "unavailable" } else { &workload_semantics }));
            }
            body.push_str("</tbody></table>");

            if !cohort.storage_summary.is_empty() {
                body.push_str("<details><summary>Metric and failure details</summary><table><thead><tr><th>Candidate</th><th>Launch writes (decimal GB)</th><th>Launch duration (seconds)</th><th>Peak RAM (decimal GB)</th><th>Failures</th></tr></thead><tbody>");
                for row in &cohort.storage_summary {
                    let failures = group_exact_failures(&row.failed_repeats);
                    body.push_str(&format!(
                        "<tr><th>{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        html_escape(&renderer_storage_label(&row.label)),
                        practical_stats(row.writes),
                        practical_values(row.launch_seconds.iter().map(|value| *value as f64)),
                        practical_values(row.peak_ram_decimal_gb.iter().copied()),
                        failures
                            .iter()
                            .map(|failure| format!(
                                "<div class=\"failed\"><span title=\"{}\">{}</span> ×{}</div>",
                                html_escape(&failure.message),
                                html_escape(&failure.message),
                                failure.count
                            ))
                            .collect::<String>()
                    ));
                }
                body.push_str("</tbody></table></details>");
            }
            body.push_str("<h5>Capabilities and sample details</h5>");
            if matches!(cohort.key, CohortKey::MinimumHardware(_)) {
                for candidate in &cohort.candidates {
                    body.push_str(&format!(
                        "<h5>{}</h5><p>Capability evidence: {}</p>",
                        html_escape(&candidate.label),
                        if candidate.capabilities_available {
                            "available"
                        } else {
                            "<span class=\"unavailable\">unavailable</span>"
                        }
                    ));
                    if !candidate.capabilities.is_empty() {
                        body.push_str("<table><thead><tr><th>Capability</th><th>Status</th><th>Evidence</th><th>Error</th><th>Elapsed ms</th></tr></thead><tbody>");
                        for capability in &candidate.capabilities {
                            body.push_str(&format!("<tr><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>", stable_html_json(&capability.capability)?, stable_html_json(&capability.status)?, stable_html_json(&capability.evidence)?, html_escape(capability.error.as_deref().unwrap_or("—")), capability.elapsed_millis.map_or("—".into(), |v| v.to_string())));
                        }
                        body.push_str("</tbody></table>");
                    }
                }
            }
            let include_sample = |row: &SampleRow| {
                !empty
                    || row.launch_failure.is_some()
                    || row.prior_launch_attempt_failures.is_some()
                    || row.preparation_failure.is_some()
                    || row.workload_failure.is_some()
                    || row.boundary_failure.is_some()
                    || matches!(row.outcome, RepeatOutcome::Failure(_))
            };
            if cohort
                .candidates
                .iter()
                .flat_map(|candidate| &candidate.rows)
                .any(include_sample)
            {
                body.push_str("<table><thead><tr><th>Candidate</th><th>Source / run / repeat</th><th>Outcome</th><th>Workload disposition</th><th>Common metrics (explicit byte and second fields)</th><th>Workload bytes / Workload duration (seconds) / peak delta (bytes) / semantics</th><th>Stage diagnostics</th></tr></thead><tbody>");
                for candidate in &cohort.candidates {
                    for row in
                        candidate.rows.iter().filter(|row| include_sample(row))
                    {
                        body.push_str(&format!("<tr><th>{}</th><td><code>{}</code><br><code>{}</code><br>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>", html_escape(&renderer_candidate_label(candidate)), html_escape(&row.source), html_escape(&row.run_id), row.repeat_ordinal, stable_html_json(&row.outcome)?, stable_html_json(&row.workload_disposition)?, stable_html_json(&row.metrics)?, stable_html_json(&serde_json::json!({"bytes": row.workload_bytes, "duration_secs": row.workload_duration_secs, "peak_delta_bytes": row.workload_peak_delta_bytes, "peak_ram_semantics": row.workload_peak_ram_semantics}))?, stage_diagnostics_html(row)));
                    }
                }
                body.push_str("</tbody></table>");
            }
            if !cohort.matrix_capabilities.is_empty() {
                body.push_str("<h5>Matrix-wide capability evidence</h5><p>A failed status records failure of that matrix-wide proof. Repeat-derived failures mean one or more applicable repeats failed; they do not imply every workload failed.</p>");
            }
            for ledger in &cohort.matrix_capabilities {
                body.push_str(&format!(
                    "<p>{} / <code>{}</code>: {}</p>",
                    html_escape(&ledger.source),
                    html_escape(&ledger.run_id),
                    if ledger.results.is_some() {
                        "capability ledger available"
                    } else {
                        "<span class=\"unavailable\">unavailable</span>"
                    }
                ));
                if let Some(results) = &ledger.results {
                    body.push_str("<table><thead><tr><th>Capability</th><th>Status</th><th>Evidence</th><th>Error</th><th>Elapsed ms</th></tr></thead><tbody>");
                    for result in results {
                        body.push_str(&format!("<tr><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>", stable_html_json(&result.capability)?, stable_html_json(&result.status)?, stable_html_json(&result.evidence)?, html_escape(result.error.as_deref().unwrap_or("—")), result.elapsed_millis.map_or("—".into(), |v| v.to_string())));
                    }
                    body.push_str("</tbody></table>");
                }
            }
            body.push_str("</details>");
            if empty {
                body.push_str("</details>");
            }
            if !factored.varying.is_empty() {
                body.push_str(
                    "<details><summary>Cohort-specific conditions</summary><table><tbody>",
                );
                for row in &factored.varying {
                    body.push_str(&format!(
                        "<tr><th>{}</th><td>{}</td></tr>",
                        html_escape(&row.label),
                        condition_value_html(row.values[cohort_index].as_ref())
                    ));
                }
                body.push_str("</tbody></table></details>");
            }
            body.push_str("</article>");
        }
        body.push_str("</section>");
    }
    body.push_str("</div></main><section id=\"appendix\"><h2>Provenance and complete conditions</h2><table><thead><tr><th>Source</th><th>SHA-256</th><th>Run status</th><th>Evidence state</th></tr></thead><tbody>");
    for input in &report.inputs {
        body.push_str(&format!(
            "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td>{}{}</td></tr>",
            html_escape(&input.source),
            html_escape(input.sha256.as_deref().unwrap_or("not supplied")),
            stable_html_json(&input.run_status)?,
            html_escape(input.evidence_state.as_deref().unwrap_or("not supplied")),
            input
                .abort_error
                .as_ref()
                .map(|e| format!("<br><strong>Abort reason:</strong> {}", html_escape(e)))
                .unwrap_or_default()
        ));
    }
    body.push_str("</tbody></table>");
    if let Some(aggregation) = &report.aggregation {
        body.push_str(&format!("<h3>Aggregation provenance</h3><p>{} accepted; {} rejected; {} unique inputs; {} duplicates.</p>", aggregation.accepted_archives.len(), aggregation.rejected_archives.len(), aggregation.unique_input_count, aggregation.duplicate_count));
        for path in &aggregation.accepted_archives {
            body.push_str(&format!(
                "<p>Accepted: <code>{}</code></p>",
                html_escape(path)
            ));
        }
        for rejected in &aggregation.rejected_archives {
            body.push_str(&format!(
                "<p class=\"warning\">Rejected: <code>{}</code> — {}</p>",
                html_escape(&rejected.path),
                html_escape(&rejected.reason)
            ));
        }
        for digest in &aggregation.digest_order {
            body.push_str(&format!(
                "<details><summary><code>{}</code></summary>{}</details>",
                html_escape(digest),
                aggregation
                    .origins
                    .get(digest)
                    .into_iter()
                    .flatten()
                    .map(|origin| format!(
                        "<p><code>{}</code></p>",
                        html_escape(origin)
                    ))
                    .collect::<String>()
            ));
        }
    }
    for section in &report.sections {
        for cohort in &section.cohorts {
            body.push_str(&format!(
                "<details><summary>Complete conditions — {}</summary><table><tbody>",
                html_escape(&cohort.label)
            ));
            for row in &cohort.conditions {
                body.push_str(&format!(
                    "<tr><th>{}</th><td>{}</td></tr>",
                    html_escape(&row.label),
                    condition_value_html(Some(row))
                ));
            }
            body.push_str("</tbody></table></details>");
        }
    }
    body.push_str(
        "</section><!-- Embedded Apache ECharts v5.5.1; no network resources. --><script>",
    );
    body.push_str(ECHARTS);
    body.push_str("</script><script>\n");
    for (id, option) in options {
        body.push_str(&format!(
            "echarts.init(document.getElementById('{}')).setOption({});\n",
            id, option
        ));
    }
    body.push_str("</script></body></html>");
    Ok(body)
}

fn render_report_html(report: &ReportView) -> Result<String> {
    render_ergonomic_report_html(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    fn matrix(version: u32) -> serde_json::Value {
        let version_fields = if version == 2 {
            json!({"load": false})
        } else {
            json!({
                "rated_tbw": 1200.0,
                "workload": {
                    "kind": "api-disk-lifecycle", "count": 20, "parallel": 4,
                    "size_bytes": 1073741824, "snapshot": false
                },
                "oxide_session": {
                    "profile": "voxel-perftest",
                    "host": "http://recovery.sys.oxide.test",
                    "provider": {"kind": "builtin"},
                    "oxide_cli_version": "oxide 0.1"
                }
            })
        };
        let mut value = json!({
            "schema_version": version, "name": "fixture", "started": 1, "ended": 2,
            "rss_sleds": 3, "repeat": 1, "combos": ["none"],
            "results": [{"label": "none", "levers": [], "repeats": [
                {"bringup_bytes": 42, "launch_secs": 7, "peak_ram_bytes": 1024}
            ]}]
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(version_fields.as_object().unwrap().clone());
        if version == 3 {
            value["results"][0]["repeats"][0]["workload_bytes"] = json!(2048);
            value["results"][0]["repeats"][0]["workload_secs"] = json!(9);
        }
        value
    }

    fn load_value(
        name: &str,
        value: serde_json::Value,
    ) -> anyhow::Result<NormalizedInput> {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        load(&path)
    }

    #[test]
    fn detects_and_normalizes_matrix_v2_and_v3_without_inventing_evidence() {
        for version in [2, 3] {
            let input =
                load_value(&format!("matrix-v{version}.json"), matrix(version))
                    .unwrap();
            assert_eq!(input.identity.kind, ExperimentKind::StorageLevers);
            assert_eq!(input.identity.source_schema_version, version);
            assert_eq!(input.repeats.len(), 1);
            assert_eq!(input.repeats[0].metrics.launch_duration_secs, Some(7));
            assert_eq!(input.repeats[0].metrics.peak_ram_bytes, Some(1024));
            assert_eq!(input.capabilities, CapabilityEvidence::Unavailable);
            assert_eq!(input.provenance, Provenance::Unavailable);
            let ExperimentPayload::StorageLevers(payload) = &input.payload
            else {
                panic!("expected storage payload")
            };
            if version == 3 {
                assert_eq!(payload.rated_tbw, Some(1200.0));
                assert_eq!(
                    payload.workload,
                    Some(WorkloadSpec::api_disk_lifecycle())
                );
                let session = payload.oxide_session.as_ref().unwrap();
                assert_eq!(session.profile, "voxel-perftest");
                assert_eq!(session.host, "http://recovery.sys.oxide.test");
                assert_eq!(
                    session.provider,
                    super::super::OxideAuthProviderMetadata::Builtin
                );
                assert_eq!(session.oxide_cli_version, "oxide 0.1");
                assert_eq!(input.repeats[0].candidate, "none");
                let RepeatPayload::StorageLevers(repeat) =
                    &input.repeats[0].payload
                else {
                    panic!("expected storage repeat")
                };
                assert_eq!(repeat.workload_bytes, Some(2048));
                assert_eq!(repeat.workload_duration_secs, Some(9));
            }
        }
    }

    #[test]
    fn strict_matrix_rejects_legacy_while_report_adapter_retains_samples() {
        let legacy = matrix(3);
        assert!(
            serde_json::from_value::<super::super::MatrixRun>(legacy.clone())
                .is_err()
        );
        let normalized = load_value("legacy-report.json", legacy).unwrap();
        assert_eq!(normalized.repeats.len(), 1);
        assert_eq!(normalized.repeats[0].metrics.peak_ram_bytes, Some(1024));
        assert_eq!(
            normalized.repeats[0].metrics.peak_ram_semantics,
            Some(MemorySemantics::LegacyAbsoluteHostPeak)
        );
        assert!(analyze(&[normalized]).cohorts[0].recommendation.is_none());
    }

    #[test]
    fn legacy_adapter_rejects_unknown_and_v4_only_fields() {
        for (field, value) in [
            ("workload_peak_delta_bytes", json!(4096)),
            ("workload_secondz", json!(9)),
        ] {
            let mut legacy = matrix(3);
            legacy["results"][0]["repeats"][0][field] = value;
            let error = load_value(&format!("legacy-{field}.json"), legacy)
                .unwrap_err();
            assert!(format!("{error:#}").contains("unknown field"));
        }

        let mut legacy = matrix(3);
        legacy["report_evidence"] = json!({});
        let error =
            load_value("legacy-report-evidence.json", legacy).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field"));
    }

    #[test]
    fn memory_semantics_are_serialized_and_split_cohort_identity() {
        let legacy = load_value("same.json", matrix(3)).unwrap();
        let mut delta = legacy.clone();
        let ExperimentPayload::StorageLevers(payload) = &mut delta.payload
        else {
            unreachable!()
        };
        payload.launch_memory_semantics = MemorySemantics::LaunchBaselineDelta;
        delta.repeats[0].metrics.peak_ram_semantics =
            Some(MemorySemantics::LaunchBaselineDelta);
        assert_ne!(cohort_key(&legacy), cohort_key(&delta));
        assert_eq!(
            serde_json::to_value(&delta).unwrap()["payload"]["data"]["launch_memory_semantics"],
            "launch-baseline-delta"
        );
    }

    #[test]
    fn schema_v4_normalizes_workload_peak_delta_and_rejects_malformed_triples()
    {
        let mut value = matrix(3);
        value["schema_version"] = json!(4);
        value["results"][0]["repeats"][0]["workload_peak_delta_bytes"] =
            json!(4096);
        let normalized = load_value("v4.json", value.clone()).unwrap();
        let RepeatPayload::StorageLevers(payload) =
            &normalized.repeats[0].payload
        else {
            unreachable!()
        };
        assert_eq!(payload.workload_peak_delta_bytes, Some(4096));
        assert_eq!(
            payload.workload_peak_ram_semantics,
            Some(MemorySemantics::WorkloadBaselineDelta)
        );

        value["results"][0]["error"] = json!("retained failure");
        value["repeat"] = json!(2);
        value["results"][0]["repeats"][0]
            .as_object_mut()
            .unwrap()
            .remove("workload_peak_delta_bytes");
        let error =
            load_value("malformed-retained-v4.json", value).unwrap_err();
        assert!(format!("{error:#}").contains("invalid workload metrics"));
    }

    fn schema_v5_report(status: super::super::RunStatus) -> serde_json::Value {
        use super::super::{
            BoundaryOutcome, LaunchMetrics, LaunchOutcome, MatrixCheckpoint,
            MatrixCheckpointCombo, MatrixCheckpointRepeat, WorkloadOutcome,
        };
        let ended = (status != super::super::RunStatus::Running).then_some(2);
        serde_json::to_value(MatrixCheckpoint {
            schema_version: 5,
            checkpoint_sequence: 7,
            status,
            abort_error: (status == super::super::RunStatus::Aborted)
                .then(|| "fixture abort".into()),
            name: "v5-fixture".into(),
            started: 1,
            updated: 2,
            ended,
            rated_tbw: None,
            workload: Some(WorkloadSpec::api_disk_lifecycle()),
            oxide_session: None,
            scope_proof: super::super::capability_unavailable(
                "matrix scope has not yet been sampled",
            ),
            report_evidence: None,
            rss_sleds: 3,
            repeat: 1,
            combos: vec![MatrixCheckpointCombo {
                label: "none".into(),
                levers: Default::default(),
                effective_config: VoxelConfig::default(),
                repeats: vec![MatrixCheckpointRepeat {
                    index: 0,
                    pre_boundary: BoundaryOutcome::Clean,
                    launch: LaunchOutcome::Success {
                        metrics: LaunchMetrics {
                            bringup_bytes: 42,
                            launch_secs: 7,
                            peak_ram_bytes: 1024,
                        },
                        prior_attempt_failures: Vec::new(),
                    },
                    preparation: super::super::PreparationOutcome::Success,
                    workload: WorkloadOutcome::Failure {
                        error: "workload exploded".into(),
                    },
                    post_boundary: BoundaryOutcome::Clean,
                }],
            }],
        })
        .unwrap()
    }

    fn partial_matrix_report() -> NormalizedInput {
        let mut value = schema_v5_report(super::super::RunStatus::Completed);
        let template = value["combos"][0].clone();
        let combinations = [
            ("none", Vec::new(), 80_u64, 20_u64, 600_u64),
            ("1", vec![1], 70, 18, 550),
            ("1+2", vec![1, 2], 60, 12, 500),
            ("1+2+3", vec![1, 2, 3], 50, 4, 350),
            ("1+2+3+4", vec![1, 2, 3, 4], 45, 0, 0),
        ];
        value["repeat"] = json!(3);
        value["combos"] = Value::Array(
            combinations
                .into_iter()
                .map(|(label, levers, launch_bytes, workload_bytes, workload_secs)| {
                    let mut combo = template.clone();
                    combo["label"] = json!(label);
                    combo["levers"] = json!(levers);
                    let repeat_template = combo["repeats"][0].clone();
                    combo["repeats"] = Value::Array(
                        (0..3)
                            .map(|index| {
                                let mut repeat = repeat_template.clone();
                                repeat["index"] = json!(index);
                                repeat["launch"]["metrics"]["bringup_bytes"] =
                                    json!(launch_bytes + index as u64);
                                if label == "1+2+3+4" {
                                    repeat["preparation"] = json!({
                                        "status": "failure",
                                        "error": "zpool preparation failed"
                                    });
                                    repeat["workload"] = json!({
                                        "status": "failure",
                                        "error": "blocked by simulated zpool preparation failure: zpool preparation failed"
                                    });
                                } else if index < 2 {
                                    repeat["workload"] = json!({
                                        "status": "success",
                                        "metrics": {
                                            "workload_bytes": workload_bytes + index as u64,
                                            "workload_secs": workload_secs + index as u64,
                                            "workload_peak_delta_bytes": 1024 + index as u64
                                        }
                                    });
                                }
                                repeat
                            })
                            .collect(),
                    );
                    combo
                })
                .collect(),
        );
        load_value("partial-matrix.json", value).unwrap()
    }

    #[test]
    fn partial_evidence_surfaces_descriptive_findings_and_coverage() {
        let input = partial_matrix_report();
        let analysis = analyze(std::slice::from_ref(&input));
        let view = build_report_view(&[input], &analysis, &[]).unwrap();
        let cohort = &view.sections[0].cohorts[0];

        assert_eq!(cohort.coverage.planned_slots, 15);
        assert_eq!(cohort.coverage.launch_samples, 15);
        assert_eq!(cohort.coverage.workload_succeeded, 8);
        assert_eq!(cohort.coverage.workload_failed, 4);
        assert_eq!(cohort.coverage.workload_blocked, 3);
        assert_eq!(cohort.coverage.unresolved, 0);
        assert_eq!(cohort.coverage.accounted_workload_slots(), 15);
        let recommendation = cohort.best_supported.as_ref().unwrap();
        assert_eq!(recommendation.candidate, "1+2+3 — 1+2+3");
        assert!(recommendation.basis.contains("lowest observed mean workload"));
        assert_eq!(recommendation.missing_candidates, ["1+2+3+4 — 1+2+3+4"]);
        assert!(
            cohort
                .candidates
                .iter()
                .all(|candidate| candidate.capabilities.is_empty())
        );
        assert!(analysis.cohorts[0].recommendation.is_none());
    }

    #[test]
    fn candidate_local_capability_failures_do_not_veto_successful_candidates() {
        let mut input = partial_matrix_report();
        input.identity.source_schema_version = 4;
        input.provenance = Provenance::Available(ProvenanceFields {
            voxel_revision: None,
            omicron_revision: None,
            image_id: None,
            host_id: None,
            voxel_build: Some("test-build".into()),
            voxel_binary: Some("sha256:test".into()),
            configured_image: Some("test-image".into()),
            omicron_commit: Some("test-commit".into()),
            host: Some("test-host".into()),
        });
        input.capability_contract_version = Some(CAPABILITY_CONTRACT_VERSION);
        input.capabilities = CapabilityEvidence::Available(vec![
            CapabilityResult {
                capability: Capability::MatrixHostStorageScope,
                status: CapabilityStatus::Fail,
                evidence: None,
                elapsed_millis: None,
                error: Some(
                    "another schema-v4 candidate was incomplete".into(),
                ),
            },
            CapabilityResult {
                capability: Capability::CleanLaunchTeardownBoundaries,
                status: CapabilityStatus::Fail,
                evidence: None,
                elapsed_millis: None,
                error: Some("another candidate had a dirty boundary".into()),
            },
            CapabilityResult {
                capability: Capability::ApiDiskLifecycle,
                status: CapabilityStatus::Fail,
                evidence: None,
                elapsed_millis: None,
                error: Some("another candidate workload failed".into()),
            },
            CapabilityResult {
                capability: Capability::SimulatedZpoolPreparation,
                status: CapabilityStatus::Fail,
                evidence: None,
                elapsed_millis: None,
                error: Some("another candidate preparation failed".into()),
            },
        ]);
        let labels = match &input.dimensions {
            Dimensions::StorageLevers(dimensions) => &dimensions.combinations,
            Dimensions::MinimumHardware(_) => unreachable!(),
        };
        let configurations = labels
            .iter()
            .map(|label| (label.clone(), VoxelConfig::default()))
            .collect::<BTreeMap<_, _>>();
        let ExperimentPayload::StorageLevers(experiment) = &mut input.payload
        else {
            unreachable!()
        };
        experiment.run_status = None;
        experiment.effective_candidate_configurations_identity = Some(
            serde_json::to_string(&configurations)
                .expect("test configurations serialize"),
        );
        experiment.effective_candidate_configurations = Some(configurations);
        for repeat in input
            .repeats
            .iter_mut()
            .filter(|repeat| repeat.candidate == "1+2+3")
        {
            repeat.outcome = RepeatOutcome::Success;
            let RepeatPayload::StorageLevers(payload) = &mut repeat.payload
            else {
                unreachable!()
            };
            payload.workload_disposition = WorkloadDisposition::Succeeded;
            payload.workload_bytes = Some(4);
            payload.workload_duration_secs = Some(350);
            payload.workload_peak_delta_bytes = Some(1024);
            payload.workload_peak_ram_semantics =
                Some(MemorySemantics::WorkloadBaselineDelta);
            payload.workload_failure = None;
        }

        let analysis = analyze(std::slice::from_ref(&input));
        let candidate = analysis.cohorts[0]
            .candidates
            .iter()
            .find(|candidate| candidate.candidate == "1+2+3")
            .unwrap();
        assert!(candidate.ineligibility.is_empty());

        input.identity.source_schema_version = 5;
        let ExperimentPayload::StorageLevers(experiment) = &mut input.payload
        else {
            unreachable!()
        };
        experiment.run_status = Some(RunStatus::Aborted);
        let analysis = analyze(&[input]);
        let candidate = analysis.cohorts[0]
            .candidates
            .iter()
            .find(|candidate| candidate.candidate == "1+2+3")
            .unwrap();
        assert!(
            candidate
                .ineligibility
                .contains(&IneligibilityReason::CapabilityFailed)
        );
        assert!(candidate.ineligibility.contains(
            &IneligibilityReason::CapabilityStatus {
                capability: Capability::MatrixHostStorageScope,
                status: CapabilityStatus::Fail,
            }
        ));
    }

    #[test]
    fn best_supported_recommendation_does_not_break_noise_ties() {
        let mut input = partial_matrix_report();
        for repeat in &mut input.repeats {
            let RepeatPayload::StorageLevers(payload) = &mut repeat.payload
            else {
                unreachable!()
            };
            if payload.workload_bytes.is_some() {
                payload.workload_bytes = Some(100);
                payload.workload_duration_secs = Some(10);
            }
        }
        let view = build_report_view(
            std::slice::from_ref(&input),
            &analyze(std::slice::from_ref(&input)),
            &[],
        )
        .unwrap();
        assert!(view.sections[0].cohorts[0].best_supported.is_none());
    }

    #[test]
    fn partial_evidence_html_prioritizes_findings_and_fallbacks() {
        let input = partial_matrix_report();
        let view = build_report_view(
            std::slice::from_ref(&input),
            &analyze(std::slice::from_ref(&input)),
            &[],
        )
        .unwrap();
        let cohort = &view.sections[0].cohorts[0];
        let html = render_report_html(&view).unwrap();

        assert!(
            html.find("Best-supported recommendation").unwrap()
                < html.find("Formal recommendation eligibility").unwrap()
        );
        assert!(
            html.find("Tabulated results").unwrap()
                < html.find("Capabilities and sample details").unwrap()
        );
        assert!(html.contains("15 of 15 planned launch samples retained"));
        assert!(html.contains("8 succeeded"));
        assert!(html.contains("4 failed"));
        assert!(html.contains("3 blocked"));
        assert!(html.contains("Storage cohort 1 — 3 RSS sleds"));
        assert!(html.contains(
            "API disk lifecycle — 20 disks, parallelism 4, 1 GiB each, snapshots disabled"
        ));
        assert!(
            !html.contains("{&quot;kind&quot;:&quot;api-disk-lifecycle&quot;")
        );
        assert!(!html.contains("<strong>Conditions:</strong>"));
        assert!(html.contains("Interactive charts require JavaScript"));
        assert!(html.contains("<noscript>"));
        assert_eq!(
            html.matches("data-chart-fallback").count(),
            cohort.charts.len()
        );
        assert_eq!(html.matches("Source / run / repeat").count(), 1);
        assert_eq!(html.matches("Matrix-wide capability evidence").count(), 1);
        assert!(!html.contains("Capability evidence by candidate"));
    }

    #[test]
    fn coverage_accounts_for_launch_blocked_and_unresolved_slots() {
        let mut input = partial_matrix_report();
        input.repeats[0].metrics = CommonMetrics::default();
        input.repeats[0].outcome =
            RepeatOutcome::Failure("launch failed".into());
        let RepeatPayload::StorageLevers(first) = &mut input.repeats[0].payload
        else {
            unreachable!()
        };
        first.workload_bytes = None;
        first.workload_duration_secs = None;
        first.workload_disposition = WorkloadDisposition::Blocked;
        first.launch_failure = Some("launch failed".into());

        input.repeats[1].metrics = CommonMetrics::default();
        input.repeats[1].outcome =
            RepeatOutcome::Failure("repeat is pending".into());
        let RepeatPayload::StorageLevers(second) =
            &mut input.repeats[1].payload
        else {
            unreachable!()
        };
        second.workload_bytes = None;
        second.workload_duration_secs = None;
        second.workload_disposition = WorkloadDisposition::Pending;

        let view = build_report_view(
            std::slice::from_ref(&input),
            &analyze(std::slice::from_ref(&input)),
            &[],
        )
        .unwrap();
        let coverage = &view.sections[0].cohorts[0].coverage;
        assert_eq!(coverage.workload_succeeded, 6);
        assert_eq!(coverage.workload_failed, 4);
        assert_eq!(coverage.workload_blocked, 4);
        assert_eq!(coverage.unresolved, 1);
        assert_eq!(coverage.accounted_workload_slots(), coverage.planned_slots);
    }

    #[test]
    fn report_suppresses_workload_counts_when_none_was_requested() {
        let mut input = partial_matrix_report();
        let ExperimentPayload::StorageLevers(payload) = &mut input.payload
        else {
            unreachable!()
        };
        payload.workload = None;
        for repeat in &mut input.repeats {
            let RepeatPayload::StorageLevers(payload) = &mut repeat.payload
            else {
                unreachable!()
            };
            payload.workload_bytes = None;
            payload.workload_duration_secs = None;
            payload.workload_peak_delta_bytes = None;
            payload.workload_peak_ram_semantics = None;
            payload.workload_failure = None;
            payload.workload_disposition = WorkloadDisposition::NotRequested;
        }
        let view = build_report_view(
            std::slice::from_ref(&input),
            &analyze(std::slice::from_ref(&input)),
            &[],
        )
        .unwrap();
        let cohort = &view.sections[0].cohorts[0];
        let html = render_report_html(&view).unwrap();
        assert!(!cohort.coverage.workload_requested);
        assert!(html.contains("No API workload was requested"));
        assert!(!html.contains("planned workload slots"));
    }

    #[test]
    fn successful_workload_with_dirty_post_boundary_is_not_called_blocked() {
        let mut value = schema_v5_report(super::super::RunStatus::Aborted);
        value["combos"][0]["repeats"][0]["post_boundary"] = json!({
            "status": "failure",
            "error": "cleanup failed"
        });
        let input = load_value("dirty-post-boundary.json", value).unwrap();
        let view = build_report_view(
            std::slice::from_ref(&input),
            &analyze(std::slice::from_ref(&input)),
            &[],
        )
        .unwrap();
        let coverage = &view.sections[0].cohorts[0].coverage;
        assert_eq!(coverage.workload_succeeded, 1);
        assert_eq!(coverage.workload_blocked, 0);
    }

    #[test]
    fn schema_v4_coverage_includes_requested_repeats_without_rows() {
        let mut value = matrix(3);
        value["schema_version"] = json!(4);
        value["repeat"] = json!(3);
        value["results"][0]["error"] = json!("retained aggregate failure");
        value["results"][0]["repeats"][0]["workload_peak_delta_bytes"] =
            json!(4096);
        let input = load_value("partial-v4.json", value).unwrap();
        let view = build_report_view(
            std::slice::from_ref(&input),
            &analyze(std::slice::from_ref(&input)),
            &[],
        )
        .unwrap();
        let coverage = &view.sections[0].cohorts[0].coverage;
        assert_eq!(coverage.planned_slots, 3);
        assert_eq!(coverage.accounted_workload_slots(), 3);
    }

    #[test]
    fn matrix_capabilities_preserve_each_source_ledger() {
        let first = partial_matrix_report();
        let mut second = first.clone();
        second.capabilities =
            CapabilityEvidence::Available(vec![CapabilityResult {
                capability: Capability::ApiDiskLifecycle,
                status: CapabilityStatus::Fail,
                evidence: None,
                elapsed_millis: Some(10),
                error: Some("one repeat failed".into()),
            }]);
        let inputs = [first, second];
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        let ledgers = &view.sections[0].cohorts[0].matrix_capabilities;
        assert_eq!(ledgers.len(), 2);
        assert_ne!(ledgers[0].results, ledgers[1].results);
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("failure of that matrix-wide proof"));
        assert!(html.contains("do not imply every workload failed"));
    }

    #[test]
    fn schema_v5_report_keeps_launch_sample_when_workload_fails() {
        let input = load_value(
            "v5-workload-failure.json",
            schema_v5_report(super::super::RunStatus::Completed),
        )
        .unwrap();
        assert_eq!(input.identity.source_schema_version, 5);
        let ExperimentPayload::StorageLevers(payload) = &input.payload else {
            unreachable!()
        };
        assert_eq!(
            payload.run_status,
            Some(super::super::RunStatus::Completed)
        );
        assert_eq!(input.repeats[0].metrics.launch_duration_secs, Some(7));
        let RepeatPayload::StorageLevers(repeat) = &input.repeats[0].payload
        else {
            unreachable!()
        };
        assert_eq!(repeat.workload_duration_secs, None);
        assert_eq!(
            repeat.workload_failure.as_deref(),
            Some("workload exploded")
        );
        let candidate = &analyze(&[input]).cohorts[0].candidates[0];
        assert_eq!(candidate.summary.launch_duration.as_ref().unwrap().n, 1);
        assert!(candidate.summary.workload_duration.is_none());
        assert!(!candidate.ineligibility.is_empty());
    }

    #[test]
    fn schema_v5_partial_report_marks_state_stages_and_metric_sample_counts() {
        let running = load_value(
            "running.json",
            schema_v5_report(super::super::RunStatus::Running),
        )
        .unwrap();
        let running_view =
            build_report_view(&[running.clone()], &analyze(&[running]), &[])
                .unwrap();
        let running_html = render_report_html(&running_view).unwrap();
        assert!(running_html.contains("Interrupted/current snapshot"));
        assert!(running_html.contains("not a completed run"));

        let aborted = load_value(
            "aborted.json",
            schema_v5_report(super::super::RunStatus::Aborted),
        )
        .unwrap();
        let aborted_view =
            build_report_view(&[aborted.clone()], &analyze(&[aborted]), &[])
                .unwrap();
        assert!(
            render_report_html(&aborted_view).unwrap().contains("Aborted run")
        );

        let mut partial = schema_v5_report(super::super::RunStatus::Completed);
        let mut successful = partial["combos"][0]["repeats"][0].clone();
        successful["index"] = json!(1);
        successful["workload"] = json!({
            "status":"success",
            "metrics": {
                "workload_bytes": 99,
                "workload_secs": 11,
                "workload_peak_delta_bytes": 2048
            }
        });
        partial["repeat"] = json!(2);
        partial["combos"][0]["repeats"]
            .as_array_mut()
            .unwrap()
            .push(successful);
        let partial = load_value("partial.json", partial).unwrap();
        let view =
            build_report_view(&[partial.clone()], &analyze(&[partial]), &[])
                .unwrap();
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("Partial evidence"));
        assert!(html.contains("(n=2)"));
        assert!(html.contains("(n=1)"));
        assert!(html.contains("Workload failure: workload exploded"));
        assert!(!html.contains("Launch failure: workload exploded"));
        let cohort = &view.sections[0].cohorts[0];
        assert_eq!(
            chart(cohort, ViewChartKind::LaunchDuration).option["series"][0]["name"],
            "none (n=2)"
        );
        assert_eq!(
            chart(cohort, ViewChartKind::WorkloadDuration).option["series"][0]
                ["name"],
            "none (n=1)"
        );
        assert_eq!(
            chart(cohort, ViewChartKind::WorkloadDuration).option["series"][0]
                ["data"],
            json!([["none", 11.0]])
        );
        assert!(
            cohort.storage_summary[0].rows[0].workload_duration_secs.is_none()
        );
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["inputs"][0]["run_status"], "completed");
        assert_eq!(
            json["sections"][0]["cohorts"][0]["storage_summary"][0]["rows"][0]
                ["workload_failure"],
            "workload exploded"
        );
    }

    #[test]
    fn schema_v5_nested_launch_boundary_failure_is_attributed_without_duplicate_text()
     {
        let mut value = schema_v5_report(super::super::RunStatus::Completed);
        value["combos"][0]["repeats"][0]["launch"] = json!({
            "status": "failure",
            "attempt_failures": [{
                "error": "launch exploded",
                "clean_boundary": {"status": "failure", "error": "dirty teardown"}
            }]
        });
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"failure", "error":"dirty teardown"});

        let input = load_value("nested-boundary-v5.json", value).unwrap();
        let RepeatPayload::StorageLevers(repeat) = &input.repeats[0].payload
        else {
            unreachable!()
        };
        assert_eq!(
            repeat.boundary_failure.as_deref(),
            Some("launch attempt 1 clean boundary: dirty teardown")
        );
    }

    #[test]
    fn hostile_v5_stage_errors_are_html_escaped() {
        let mut input = load_value(
            "hostile-v5.json",
            schema_v5_report(super::super::RunStatus::Completed),
        )
        .unwrap();
        let RepeatPayload::StorageLevers(payload) =
            &mut input.repeats[0].payload
        else {
            unreachable!()
        };
        payload.launch_failure = Some("<script>launch()</script>".into());
        payload.workload_failure =
            Some("<img src=x onerror=workload()>".into());
        payload.boundary_failure = Some("</td><svg onload=boundary()>".into());
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let html = render_report_html(&view).unwrap();

        for raw in ["<script>launch", "<img src=x", "<svg onload=boundary"] {
            assert!(!html.contains(raw), "unescaped hostile tag: {raw}");
        }
        for escaped in ["&lt;script&gt;", "&lt;img", "&lt;svg"] {
            assert!(
                html.contains(escaped),
                "missing escaped evidence: {escaped}"
            );
        }
    }

    #[test]
    fn schema_v5_report_allows_pending_only_before_completion_and_excludes_dirty_metrics()
     {
        for status in
            [super::super::RunStatus::Running, super::super::RunStatus::Aborted]
        {
            let mut value = schema_v5_report(status);
            value["combos"][0]["repeats"][0]["pre_boundary"] =
                json!({"status":"pending"});
            value["combos"][0]["repeats"][0]["launch"] =
                json!({"status":"pending"});
            value["combos"][0]["repeats"][0]["preparation"] =
                json!({"status":"pending"});
            value["combos"][0]["repeats"][0]["workload"] =
                json!({"status":"pending"});
            value["combos"][0]["repeats"][0]["post_boundary"] =
                json!({"status":"pending"});
            assert!(load_value("pending-v5.json", value).is_ok());
        }
        let mut completed =
            schema_v5_report(super::super::RunStatus::Completed);
        completed["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"failure", "error":"dirty teardown"});
        assert!(load_value("dirty-v5.json", completed).is_err());

        let mut pending = schema_v5_report(super::super::RunStatus::Completed);
        pending["combos"][0]["repeats"][0]["launch"] =
            json!({"status":"pending"});
        assert!(load_value("completed-pending-v5.json", pending).is_err());
    }

    #[test]
    fn schema_v5_report_loads_transient_preparation_failure_only_while_running()
    {
        let mut running = schema_v5_report(super::super::RunStatus::Running);
        running["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"failure", "error":"zpool inventory missing"});
        running["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"pending"});
        running["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});

        let serialized = serde_json::to_vec(&running).unwrap();
        let captured: Value = serde_json::from_slice(&serialized).unwrap();
        assert!(
            load_value("running-preparation-failure-v5.json", captured.clone())
                .is_ok()
        );

        let mut completed = captured;
        completed["status"] = json!("completed");
        completed["ended"] = json!(101);
        assert!(
            load_value("completed-preparation-failure-v5.json", completed)
                .is_err()
        );
    }

    #[test]
    fn schema_v5_aborted_preflight_reason_is_prominent_and_escaped() {
        let hostile = "static preflight failed: <script>alert('x')</script>";
        let mut value = schema_v5_report(super::super::RunStatus::Aborted);
        value["abort_error"] = json!(hostile);
        value["combos"][0]["repeats"][0]["pre_boundary"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["launch"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});
        let input =
            load_value("aborted-static-preflight-v5.json", value).unwrap();
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("Abort reason:"));
        assert!(html.contains("&lt;script&gt;alert"));
        assert!(!html.contains("<script>alert('x')</script>"));

        for (status, abort_error) in [
            (super::super::RunStatus::Running, Some("wrong")),
            (super::super::RunStatus::Completed, Some("wrong")),
            (super::super::RunStatus::Aborted, None),
            (super::super::RunStatus::Aborted, Some("")),
        ] {
            let mut malformed = schema_v5_report(status);
            malformed["abort_error"] =
                abort_error.map_or(Value::Null, |error| json!(error));
            assert!(
                load_value("malformed-abort-pair-v5.json", malformed).is_err()
            );
        }
    }

    #[test]
    fn schema_v5_report_accepts_initial_running_checkpoint_without_workload() {
        let mut value = schema_v5_report(super::super::RunStatus::Running);
        value["workload"] = serde_json::Value::Null;
        value["combos"][0]["repeats"][0]["pre_boundary"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["launch"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});

        assert!(
            load_value("initial-running-no-workload-v5.json", value).is_ok()
        );
    }

    #[test]
    fn schema_v5_report_accepts_aborted_pre_boundary_failure_without_workload()
    {
        let mut value = schema_v5_report(super::super::RunStatus::Aborted);
        value["workload"] = serde_json::Value::Null;
        value["combos"][0]["repeats"][0]["pre_boundary"] =
            json!({"status":"failure", "error":"pre-boundary failed"});
        value["combos"][0]["repeats"][0]["launch"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});

        assert!(
            load_value("aborted-pre-failure-no-workload-v5.json", value)
                .is_ok()
        );
    }

    #[test]
    fn schema_v5_report_rejects_fabricated_post_boundary_after_pre_boundary_stop()
     {
        for (name, pre_boundary, post_boundary) in [
            (
                "pending-pre-clean-post",
                json!({"status":"pending"}),
                json!({"status":"clean"}),
            ),
            (
                "failed-pre-failed-post",
                json!({"status":"failure", "error":"pre-boundary failed"}),
                json!({"status":"failure", "error":"fabricated post-boundary failure"}),
            ),
        ] {
            let mut value = schema_v5_report(super::super::RunStatus::Aborted);
            value["workload"] = serde_json::Value::Null;
            value["combos"][0]["repeats"][0]["pre_boundary"] = pre_boundary;
            value["combos"][0]["repeats"][0]["launch"] =
                json!({"status":"pending"});
            value["combos"][0]["repeats"][0]["preparation"] =
                json!({"status":"not_requested"});
            value["combos"][0]["repeats"][0]["workload"] =
                json!({"status":"not_requested"});
            value["combos"][0]["repeats"][0]["post_boundary"] = post_boundary;

            assert!(
                load_value(&format!("{name}-v5.json"), value).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn schema_v5_report_rejects_malformed_boundary_and_attempt_states() {
        let malformed = [
            (
                "pending-workload-clean-post",
                json!({"workload":{"status":"pending"}}),
            ),
            (
                "empty-pre-error",
                json!({"pre_boundary":{"status":"failure","error":""}}),
            ),
            (
                "empty-post-error",
                json!({"post_boundary":{"status":"failure","error":""}}),
            ),
        ];
        for (name, changes) in malformed {
            let mut value = schema_v5_report(super::super::RunStatus::Running);
            for (field, replacement) in changes.as_object().unwrap() {
                value["combos"][0]["repeats"][0][field] = replacement.clone();
            }
            assert!(
                load_value(&format!("{name}.json"), value).is_err(),
                "{name}"
            );
        }

        let malformed_launches = [
            (
                "success-dirty-prior",
                json!({"status":"success","metrics":{"bringup_bytes":42,"launch_secs":7,"peak_ram_bytes":1024},"prior_attempt_failures":[{"error":"first","clean_boundary":{"status":"pending"}}]}),
            ),
            (
                "too-many-attempts",
                json!({"status":"failure","attempt_failures":[{"error":"one","clean_boundary":{"status":"clean"}},{"error":"two","clean_boundary":{"status":"clean"}},{"error":"three","clean_boundary":{"status":"clean"}}]}),
            ),
            (
                "dirty-nonfinal",
                json!({"status":"failure","attempt_failures":[{"error":"one","clean_boundary":{"status":"pending"}},{"error":"two","clean_boundary":{"status":"clean"}}]}),
            ),
            (
                "empty-nested-error",
                json!({"status":"failure","attempt_failures":[{"error":"one","clean_boundary":{"status":"failure","error":""}}]}),
            ),
            (
                "terminal-clean-too-soon",
                json!({"status":"failure","attempt_failures":[{"error":"one","clean_boundary":{"status":"clean"}}]}),
            ),
            (
                "cleanup-failure-mismatch",
                json!({"status":"failure","attempt_failures":[{"error":"one","clean_boundary":{"status":"failure","error":"nested"}}]}),
            ),
        ];
        for (name, launch) in malformed_launches {
            let mut value =
                schema_v5_report(super::super::RunStatus::Completed);
            value["combos"][0]["repeats"][0]["launch"] = launch;
            value["combos"][0]["repeats"][0]["workload"] =
                json!({"status":"pending"});
            if name == "cleanup-failure-mismatch"
                || name == "empty-nested-error"
            {
                value["combos"][0]["repeats"][0]["post_boundary"] =
                    json!({"status":"failure","error":"post"});
            }
            assert!(
                load_value(&format!("{name}.json"), value).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn schema_v5_report_allows_in_progress_launch_failure_and_excludes_nested_dirty_metrics()
     {
        let mut pending = schema_v5_report(super::super::RunStatus::Running);
        pending["combos"][0]["repeats"][0]["launch"] = json!({"status":"failure","attempt_failures":[{"error":"launch","clean_boundary":{"status":"pending"}}]});
        pending["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"pending"});
        pending["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});
        assert!(load_value("in-progress-launch-failure.json", pending).is_ok());

        let mut no_workload =
            schema_v5_report(super::super::RunStatus::Running);
        no_workload["workload"] = serde_json::Value::Null;
        no_workload["combos"][0]["repeats"][0]["launch"] = json!({"status":"failure","attempt_failures":[{"error":"launch","clean_boundary":{"status":"pending"}}]});
        no_workload["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"not_requested"});
        no_workload["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"not_requested"});
        no_workload["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});
        assert!(
            load_value(
                "in-progress-launch-failure-no-workload.json",
                no_workload
            )
            .is_ok()
        );

        let mut dirty = schema_v5_report(super::super::RunStatus::Running);
        dirty["combos"][0]["repeats"][0]["launch"]["prior_attempt_failures"] = json!([
            {"error":"launch retry", "clean_boundary":{"status":"failure","error":"dirty nested"}}
        ]);
        let input = normalize_matrix_checkpoint(
            std::path::Path::new("defensive-dirty-nested.json"),
            serde_json::from_value(dirty).unwrap(),
        );
        assert_eq!(input.repeats[0].metrics, CommonMetrics::default());
        let RepeatPayload::StorageLevers(payload) = &input.repeats[0].payload
        else {
            unreachable!()
        };
        assert_eq!(payload.workload_duration_secs, None);
    }

    #[test]
    fn schema_v5_report_accepts_completed_exhausted_launch_failure() {
        let mut value = schema_v5_report(super::super::RunStatus::Completed);
        value["combos"][0]["repeats"][0]["launch"] = json!({"status":"failure","attempt_failures":[
            {"error":"first launch", "clean_boundary":{"status":"clean"}},
            {"error":"second launch", "clean_boundary":{"status":"clean"}}
        ]});
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"clean"});

        assert!(
            load_value("completed-exhausted-launch-failure.json", value)
                .is_ok()
        );
    }

    #[test]
    fn schema_v5_recovered_launch_retry_is_success_with_warning_evidence() {
        let mut value = schema_v5_report(super::super::RunStatus::Completed);
        value["combos"][0]["repeats"][0]["launch"]["prior_attempt_failures"] = json!([{
            "error": "first launch failed",
            "clean_boundary": {"status":"clean"}
        }]);
        value["combos"][0]["repeats"][0]["workload"] = json!({
            "status":"success",
            "metrics":{"workload_bytes":99,"workload_secs":11,"workload_peak_delta_bytes":2048}
        });

        let input = load_value("recovered-retry.json", value).unwrap();
        assert_eq!(input.repeats[0].outcome, RepeatOutcome::Success);
        let RepeatPayload::StorageLevers(payload) = &input.repeats[0].payload
        else {
            unreachable!()
        };
        assert_eq!(payload.launch_failure, None);
        assert_eq!(
            payload.prior_launch_attempt_failures.as_deref(),
            Some("first launch failed")
        );
        let analysis = analyze(&[input]);
        assert!(
            !analysis.cohorts[0].candidates[0]
                .ineligibility
                .contains(&IneligibilityReason::RequiredRepeatFailed)
        );
    }

    #[test]
    fn schema_v5_checkpoint_round_trip_redacts_execution_secret_and_accepts_evidence()
     {
        use super::super::{
            build_report_evidence, checkpoint_capability_ledger,
        };

        let secret = "distinctive-production-recovery-hash";
        let mut base = VoxelConfig::default();
        base.recovery_silo.user_password_hash = secret.into();
        let plan = vec![("none".to_string(), Default::default())];
        let evidence =
            build_report_evidence(&base, &plan, 3, None, None, &[], 1);
        let mut value = schema_v5_report(super::super::RunStatus::Running);
        value["workload"] = Value::Null;
        value["combos"][0]["effective_config"] =
            serde_json::to_value(&evidence.combos[0].effective_config).unwrap();
        value["combos"][0]["repeats"][0]["pre_boundary"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["launch"] =
            json!({"status":"pending"});
        value["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["post_boundary"] =
            json!({"status":"pending"});
        let mut checkpoint: MatrixCheckpoint =
            serde_json::from_value(value).unwrap();
        checkpoint.report_evidence = Some(evidence);
        checkpoint.report_evidence.as_mut().unwrap().capabilities =
            checkpoint_capability_ledger(&checkpoint);

        let raw = serde_json::to_string(&checkpoint).unwrap();
        assert!(!raw.contains(secret));
        assert!(raw.contains(REDACTED_CREDENTIAL));
        assert!(
            load_value(
                "sanitized-production-checkpoint.json",
                serde_json::from_str(&raw).unwrap()
            )
            .is_ok()
        );

        let mut fabricated: Value = serde_json::from_str(&raw).unwrap();
        fabricated["report_evidence"]["capabilities"]["clean_launch_teardown_boundaries"] =
            json!({"status":"pass", "evidence":"fabricated early pass"});
        assert!(
            load_value("fabricated-capability-pass.json", fabricated).is_err()
        );
    }

    #[test]
    fn schema_v5_without_workload_has_no_workload_memory_semantics() {
        let mut value = schema_v5_report(super::super::RunStatus::Completed);
        value["workload"] = serde_json::Value::Null;
        value["combos"][0]["repeats"][0]["preparation"] =
            json!({"status":"not_requested"});
        value["combos"][0]["repeats"][0]["workload"] =
            json!({"status":"not_requested"});
        let input = load_value("v5-no-workload.json", value).unwrap();
        let ExperimentPayload::StorageLevers(payload) = input.payload else {
            unreachable!()
        };
        assert_eq!(payload.workload_memory_semantics, None);
    }

    #[test]
    fn complete_four_proof_schema_v4_storage_run_is_recommendation_eligible() {
        use super::super::{
            ComboAggregate, EvidenceValue, MatrixRun, RepeatSample,
            build_report_evidence,
        };

        let workload = WorkloadSpec::api_disk_lifecycle();
        let session = OxideSessionMetadata {
            profile: "voxel-perftest".into(),
            host: "http://recovery.sys.oxide.test".into(),
            provider: super::super::OxideAuthProviderMetadata::Builtin,
            oxide_cli_version: "oxide 0.1".into(),
        };
        let results = vec![ComboAggregate {
            label: "none".into(),
            levers: Default::default(),
            repeats: vec![RepeatSample {
                bringup_bytes: 42,
                launch_secs: 7,
                peak_ram_bytes: Some(1024),
                workload_bytes: Some(2048),
                workload_secs: Some(9),
                workload_peak_delta_bytes: Some(512),
            }],
            error: None,
        }];
        let plan = vec![("none".to_string(), Default::default())];
        let base = VoxelConfig::default();
        let mut evidence = build_report_evidence(
            &base,
            &plan,
            0,
            Some(workload.clone()),
            Some(session.clone()),
            &results,
            1,
        );
        for value in [
            &mut evidence.provenance.voxel_build,
            &mut evidence.provenance.voxel_binary,
            &mut evidence.provenance.configured_image,
            &mut evidence.provenance.omicron_commit,
            &mut evidence.provenance.host,
        ] {
            *value = EvidenceValue::Available {
                value: "stable-test-identity".into(),
            };
        }
        let run = MatrixRun {
            schema_version: 4,
            name: "eligible-v4".into(),
            started: 1,
            ended: 2,
            rated_tbw: None,
            workload: Some(workload),
            oxide_session: Some(session),
            report_evidence: Some(evidence),
            rss_sleds: 0,
            repeat: 1,
            combos: vec!["none".into()],
            results,
        };
        super::super::validate_matrix_run(&run).unwrap();
        let input = normalize_matrix(Path::new("eligible-v4.json"), 4, run);
        let analysis = analyze(&[input]);
        let cohort = &analysis.cohorts[0];
        assert!(cohort.candidates[0].ineligibility.is_empty());
        assert!(cohort.recommendation.is_some());
    }

    #[test]
    fn matrix_semantics_are_validated_after_deserialization() {
        let mut value = matrix(3);
        value["results"][0]["repeats"][0]
            .as_object_mut()
            .unwrap()
            .remove("peak_ram_bytes");
        let error = load_value("incomplete.json", value).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("validate historical storage matrix semantics")
        );
        assert!(message.contains("missing absolute peak_ram_bytes"));
    }

    #[test]
    fn metrics_and_storage_details_stay_attached_to_each_candidate() {
        let mut value = matrix(3);
        value["repeat"] = json!(1);
        value["combos"] = json!(["none", "1"]);
        value["results"] = json!([
            {"label": "none", "levers": [], "repeats": [{
                "bringup_bytes": 10, "launch_secs": 11, "peak_ram_bytes": 12,
                "workload_bytes": 13, "workload_secs": 14
            }]},
            {"label": "1", "levers": [1], "repeats": [{
                "bringup_bytes": 20, "launch_secs": 21, "peak_ram_bytes": 22,
                "workload_bytes": 23, "workload_secs": 24
            }]}
        ]);
        let input = load_value("two-candidates.json", value).unwrap();
        assert_eq!(input.repeats[0].candidate, "none");
        assert_eq!(input.repeats[0].metrics.writes_bytes, Some(10));
        let RepeatPayload::StorageLevers(details) = &input.repeats[0].payload
        else {
            panic!("expected storage repeat")
        };
        assert!(details.levers.is_empty());
        assert_eq!(details.workload_bytes, Some(13));
        assert_eq!(details.workload_duration_secs, Some(14));
        assert_eq!(input.repeats[1].candidate, "1");
        assert_eq!(input.repeats[1].metrics.writes_bytes, Some(20));
        let RepeatPayload::StorageLevers(details) = &input.repeats[1].payload
        else {
            panic!("expected storage repeat")
        };
        assert_eq!(details.levers.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(details.workload_bytes, Some(23));
        assert_eq!(details.workload_duration_secs, Some(24));
    }

    #[test]
    fn detects_typed_minimum_hardware_fixture() {
        let mut value = minimum_fixture("fixture-only", 3221225472, 1, 1);
        value["provenance"] = Value::Null;
        value["capabilities"] = Value::Null;
        value.as_object_mut().unwrap().remove("contract_name");
        value.as_object_mut().unwrap().remove("contract_version");
        let input = load_value("minimum.json", value).unwrap();
        assert_eq!(input.identity.kind, ExperimentKind::MinimumHardware);
        assert!(matches!(input.payload, ExperimentPayload::MinimumHardware(_)));
    }

    #[test]
    fn empty_evidence_is_unavailable() {
        let mut value = minimum_fixture("empty", 1, 2, 3);
        value["provenance"] = json!({});
        value["capabilities"] = json!([]);
        value.as_object_mut().unwrap().remove("contract_name");
        value.as_object_mut().unwrap().remove("contract_version");
        let input = load_value("empty-evidence.json", value).unwrap();
        assert_eq!(input.provenance, Provenance::Unavailable);
        assert_eq!(input.capabilities, CapabilityEvidence::Unavailable);
    }

    #[test]
    fn capability_contract_rejects_invalid_status_shapes_and_versions() {
        let mut value = minimum_fixture("candidate", 10, 20, 30);
        value["capabilities"] = complete_capabilities("pass", None);
        assert!(load_value("valid.json", value.clone()).is_ok());

        value["capabilities"][0]["error"] = json!("contradicts pass");
        assert!(
            format!("{:#}", load_value("bad.json", value).unwrap_err())
                .contains("passing capability must not have an error")
        );

        let mut future = minimum_fixture("candidate", 10, 20, 30);
        future["contract_version"] = json!(2);
        future["capabilities"] = complete_capabilities("pass", None);
        assert!(
            format!(
                "{:#}",
                load_value("future-contract.json", future).unwrap_err()
            )
            .contains("unsupported capability contract version 2")
        );
    }

    #[test]
    fn analysis_retains_failures_but_excludes_them_and_historical_inputs() {
        let historical = load_value("historical.json", matrix(3)).unwrap();
        let mut input =
            load_value("failed.json", minimum_fixture("small", 10, 20, 30))
                .unwrap();
        input.repeats.push(NormalizedRepeat {
            candidate: "small".into(),
            outcome: RepeatOutcome::Failure("launch failed".into()),
            metrics: CommonMetrics::default(),
            payload: RepeatPayload::MinimumHardware,
        });
        let report = analyze(&[historical, input]);
        assert_eq!(report.cohorts.len(), 2);
        let historical = report
            .cohorts
            .iter()
            .find(|c| matches!(c.key, CohortKey::Storage(_)))
            .unwrap();
        assert!(historical.recommendation.is_none());
        assert!(
            historical.candidates[0]
                .ineligibility
                .contains(&IneligibilityReason::CapabilityEvidenceUnavailable)
        );
        let candidate = report
            .cohorts
            .iter()
            .find(|c| matches!(c.key, CohortKey::MinimumHardware(_)))
            .unwrap()
            .candidates
            .first()
            .unwrap();
        assert_eq!(candidate.repeats.len(), 2);
        assert_eq!(candidate.summary.launch_duration.unwrap().n, 1);
        assert!(
            candidate
                .ineligibility
                .contains(&IneligibilityReason::RequiredRepeatFailed)
        );
    }

    #[test]
    fn cohorts_join_only_on_typed_comparable_identity() {
        let a = load_value("a.json", minimum_fixture("a", 10, 20, 30)).unwrap();
        let mut same = minimum_fixture("b", 11, 21, 31);
        same["identity"]["run_id"] = json!("other-run");
        same["dimensions"]["vdev_count"] = json!(2);
        let b = load_value("b.json", same).unwrap();
        let mut different = minimum_fixture("c", 12, 22, 32);
        different["provenance"]["host_id"] = json!("other-host");
        let c = load_value("c.json", different).unwrap();
        let report = analyze(&[a, b, c]);
        assert_eq!(report.cohorts.len(), 2);
        assert!(
            report.cohorts.iter().any(|cohort| cohort.candidates.len() == 2)
        );
        assert!(report.global_recommendation.is_none());
    }

    #[test]
    fn pareto_ranking_is_cohort_local_noise_aware_and_missing_is_not_best() {
        let a =
            load_value("a.json", minimum_fixture("a", 100, 100, 100)).unwrap();
        let mut b = minimum_fixture("b", 200, 200, 200);
        b["dimensions"]["vdev_count"] = json!(2);
        let b = load_value("b.json", b).unwrap();
        let report = analyze(&[a, b]);
        let cohort = &report.cohorts[0];
        assert_eq!(
            cohort.recommendation.as_ref().map(|r| r.display.as_str()),
            Some("a")
        );
        assert!(!cohort.candidates[0].dominated);
        assert!(cohort.candidates[1].dominated);

        let mut noisy = minimum_fixture("noisy-a", 100, 100, 100);
        noisy["repeats"].as_array_mut().unwrap().push(json!({
            "candidate": "noisy-a", "outcome": {"status": "success"},
            "launch_duration_secs": 300, "peak_ram_bytes": 300
        }));
        noisy["payload"]["expected_repeats"] = json!(2);
        let mut middle = minimum_fixture("middle", 100, 190, 190);
        middle["dimensions"]["vdev_count"] = json!(2);
        let tied = analyze(&[
            load_value("noisy.json", noisy).unwrap(),
            load_value("middle.json", middle).unwrap(),
        ]);
        assert!(tied.cohorts[0].recommendation.is_none());
        assert_eq!(tied.cohorts[0].tie.len(), 2);
    }

    fn minimum_fixture(
        candidate: &str,
        required: u64,
        launch: u64,
        ram: u64,
    ) -> Value {
        json!({
            "kind": "minimum-hardware", "schema_version": 1,
            "contract_name": "oxide-internal-faux-rack", "contract_version": 1,
            "identity": {"run_id": candidate},
            "provenance": {"voxel_revision": "v", "omicron_revision": "o", "image_id": "i", "host_id": "h"},
            "effective_configuration": serde_json::to_value(VoxelConfig::default()).unwrap(),
            "dimensions": {"vdev_size_bytes": 1, "vdev_count": 1,
                "control_plane_storage_buffer_bytes": 1, "cockroachdb_redundancy": 1,
                "svcadm_autoclear": false},
            "repeats": [{"candidate": candidate, "outcome": {"status": "success"},
                "launch_duration_secs": launch, "peak_ram_bytes": ram, "idle_writes_bytes": 1}],
            "capabilities": complete_capabilities("pass", None),
            "payload": {"expected_repeats": 1, "host_storage_capacity_bytes": required + 100,
                "fits_host_storage_envelope": true, "required_allocation_bytes": required,
                "peak_allocation_bytes": required + 1}
        })
    }

    #[test]
    fn minimum_hardware_requires_effective_configuration_and_splits_cohorts_on_it()
     {
        let mut missing = minimum_fixture("missing", 10, 20, 30);
        missing.as_object_mut().unwrap().remove("effective_configuration");
        assert!(
            format!("{:#}", load_value("missing.json", missing).unwrap_err())
                .contains("effective_configuration")
        );

        let a = load_value("a.json", minimum_fixture("a", 10, 20, 30)).unwrap();
        let mut changed = minimum_fixture("b", 11, 21, 31);
        changed["effective_configuration"]["topology"]["sleds"] = json!(5);
        let b = load_value("b.json", changed).unwrap();
        assert_eq!(analyze(&[a, b]).cohorts.len(), 2);
    }

    #[test]
    fn minimum_hardware_loader_retains_failed_and_incomplete_repeats() {
        let mut failed = minimum_fixture("failed", 10, 20, 30);
        failed["payload"]["expected_repeats"] = json!(2);
        failed["repeats"].as_array_mut().unwrap().push(json!({
            "candidate": "failed", "outcome": {"status": "failure", "error": "launch failed"},
            "launch_duration_secs": 9999, "peak_ram_bytes": 8888
        }));
        let failed = load_value("failed.json", failed).unwrap();
        let analysis = analyze(std::slice::from_ref(&failed));
        let candidate = &analysis.cohorts[0].candidates[0];
        assert_eq!(candidate.summary.successful_repeats, 1);
        assert_eq!(candidate.summary.launch_duration.unwrap().mean, 20.0);
        assert!(candidate.repeats.iter().any(|repeat| matches!(
            &repeat.outcome,
            RepeatOutcome::Failure(error) if error == "launch failed"
        )));
        let view = build_report_view(&[failed], &analysis, &[]).unwrap();
        let candidate = &view.sections[0].cohorts[0].candidates[0];
        assert_eq!(candidate.launch_samples_seconds, vec![20]);
        assert_eq!(candidate.peak_ram_samples_bytes, vec![30]);
        assert!(candidate.rows.iter().any(|row| {
            row.outcome != RepeatOutcome::Success
                && row.metrics.launch_duration_secs == Some(9999)
                && row.metrics.peak_ram_bytes == Some(8888)
        }));

        let mut incomplete = minimum_fixture("incomplete", 10, 20, 30);
        incomplete["payload"]["expected_repeats"] = json!(2);
        let incomplete = load_value("incomplete.json", incomplete).unwrap();
        let candidate = &analyze(&[incomplete]).cohorts[0].candidates[0];
        assert_eq!(candidate.summary.successful_repeats, 1);
        assert!(
            candidate
                .ineligibility
                .contains(&IneligibilityReason::RequiredRepeatMissing)
        );
    }

    fn complete_capabilities(status: &str, error: Option<&str>) -> Value {
        Value::Array(
            [
                "rack-readiness",
                "metrics",
                "fleet-api",
                "silo-api",
                "project-disk-lifecycle",
                "topology-fidelity",
                "clean-teardown",
            ]
            .into_iter()
            .map(|capability| {
                json!({
                    "capability": capability, "status": status, "evidence": "probe",
                    "elapsed_millis": 1, "error": error
                })
            })
            .collect(),
        )
    }

    fn assert_rejected(mut value: Value, name: &str, expected: &str) {
        let error = load_value(name, value.take()).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }

    fn minimum_with(
        candidate: &str,
        dimensions: MinimumHardwareDimensions,
        required: u64,
        peak: u64,
        samples: &[(u64, u64, u64)],
    ) -> NormalizedInput {
        let mut value =
            minimum_fixture(candidate, required, samples[0].0, samples[0].1);
        value["dimensions"] = serde_json::to_value(dimensions).unwrap();
        value["payload"]["expected_repeats"] = json!(samples.len());
        value["payload"]["peak_allocation_bytes"] = json!(peak);
        value["payload"]["host_storage_capacity_bytes"] =
            json!(required.max(peak) + 100);
        value["repeats"] = Value::Array(
            samples
                .iter()
                .map(|(launch, ram, writes)| {
                    json!({
                        "candidate": candidate, "outcome": {"status": "success"},
                        "launch_duration_secs": launch, "peak_ram_bytes": ram,
                        "launch_writes_bytes": writes, "idle_writes_bytes": writes
                    })
                })
                .collect(),
        );
        load_value(&format!("{candidate}.json"), value).unwrap()
    }

    fn dimensions(
        vdev_size_bytes: u64,
        vdev_count: usize,
    ) -> MinimumHardwareDimensions {
        MinimumHardwareDimensions {
            vdev_size_bytes,
            vdev_count,
            control_plane_storage_buffer_bytes: 1,
            cockroachdb_redundancy: 1,
            svcadm_autoclear: false,
        }
    }

    #[test]
    fn capability_contract_enforces_completeness_uniqueness_and_bounded_shapes()
    {
        for capability in 0..7 {
            let mut value = minimum_fixture("missing", 1, 2, 3);
            value["capabilities"].as_array_mut().unwrap().remove(capability);
            assert_rejected(
                value,
                "missing.json",
                "requires exactly one result",
            );
        }
        let mut duplicate = minimum_fixture("duplicate", 1, 2, 3);
        let first = duplicate["capabilities"][0].clone();
        duplicate["capabilities"].as_array_mut().unwrap().push(first);
        assert_rejected(
            duplicate,
            "duplicate.json",
            "requires exactly one result",
        );

        for (status, evidence, error, expected) in [
            (
                "pass",
                Some(json!("ok")),
                Some(json!("bad")),
                "passing capability must not have an error",
            ),
            ("pass", None, None, "passing capability requires evidence"),
            (
                "fail",
                Some(json!("bad")),
                Some(json!("failed")),
                "must not include success evidence",
            ),
            ("unavailable", None, None, "requires an actionable error"),
        ] {
            let mut value = minimum_fixture("shape", 1, 2, 3);
            value["capabilities"][0]["status"] = json!(status);
            value["capabilities"][0]["evidence"] =
                evidence.unwrap_or(Value::Null);
            value["capabilities"][0]["error"] = error.unwrap_or(Value::Null);
            assert_rejected(value, "shape.json", expected);
        }
        let mut huge = minimum_fixture("huge", 1, 2, 3);
        huge["capabilities"][0]["evidence"] =
            json!({"payload": "x".repeat(4096)});
        assert_rejected(huge, "huge.json", "exceeds 4096 bytes");
    }

    #[test]
    fn capability_contract_rejects_name_version_and_presence_mismatches() {
        let mut wrong_name = minimum_fixture("name", 1, 2, 3);
        wrong_name["contract_name"] = json!("other");
        assert_rejected(wrong_name, "name.json", "name must be exactly");

        for capabilities in [Value::Null, Value::Array(vec![])] {
            let mut future = minimum_fixture("future", 1, 2, 3);
            future["contract_version"] = json!(2);
            future["capabilities"] = capabilities;
            assert_rejected(future, "future.json", "capability contract");
        }
        let mut absent_version = minimum_fixture("absent", 1, 2, 3);
        absent_version.as_object_mut().unwrap().remove("contract_version");
        assert_rejected(absent_version, "absent.json", "contract_version");
    }

    #[test]
    fn repeat_eligibility_is_per_successful_repeat_and_retains_failure() {
        let mut input =
            load_value("repeat.json", minimum_fixture("repeat", 1, 2, 3))
                .unwrap();
        let ExperimentPayload::MinimumHardware(payload) = &mut input.payload
        else {
            unreachable!()
        };
        payload.expected_repeats = 3;
        input.repeats.push(NormalizedRepeat {
            candidate: "repeat".into(),
            outcome: RepeatOutcome::Success,
            metrics: CommonMetrics {
                launch_duration_secs: Some(2),
                ..Default::default()
            },
            payload: RepeatPayload::MinimumHardware,
        });
        input.repeats.push(NormalizedRepeat {
            candidate: "repeat".into(),
            outcome: RepeatOutcome::Failure("boom".into()),
            metrics: CommonMetrics::default(),
            payload: RepeatPayload::MinimumHardware,
        });
        let candidate = &analyze(&[input]).cohorts[0].candidates[0];
        assert_eq!(candidate.repeats.len(), 3);
        assert!(
            candidate
                .ineligibility
                .contains(&IneligibilityReason::RequiredRepeatFailed)
        );
        assert!(
            candidate
                .ineligibility
                .contains(&IneligibilityReason::RequiredMeasurementMissing)
        );

        let mut underflow =
            load_value("underflow.json", minimum_fixture("underflow", 1, 2, 3))
                .unwrap();
        let ExperimentPayload::MinimumHardware(payload) =
            &mut underflow.payload
        else {
            unreachable!()
        };
        payload.expected_repeats = 2;
        let candidate = &analyze(&[underflow]).cohorts[0].candidates[0];
        assert!(
            candidate
                .ineligibility
                .contains(&IneligibilityReason::RequiredRepeatMissing)
        );
    }

    #[test]
    fn unavailable_capability_and_provenance_are_ineligible_with_typed_trace() {
        let historical = load_value("unknown.json", matrix(3)).unwrap();
        let candidate = &analyze(&[historical]).cohorts[0].candidates[0];
        assert!(
            matches!(&candidate.decision, DecisionTrace::Ineligible(reasons)
            if reasons.contains(&IneligibilityReason::CapabilityEvidenceUnavailable)
                && reasons.contains(&IneligibilityReason::ProvenanceUnavailable))
        );

        let mut unavailable = minimum_fixture("unavailable", 1, 2, 3);
        unavailable["capabilities"][0] = json!({
            "capability": "rack-readiness", "status": "unavailable", "error": "not probed"
        });
        let candidate =
            &analyze(&[load_value("unavailable.json", unavailable).unwrap()])
                .cohorts[0]
                .candidates[0];
        assert!(
            matches!(&candidate.decision, DecisionTrace::Ineligible(reasons) if reasons.contains(
            &IneligibilityReason::CapabilityStatus { capability: Capability::RackReadiness,
                status: CapabilityStatus::Unavailable }))
        );
    }

    #[test]
    fn typed_candidate_and_cohort_keys_do_not_collapse_labels_or_unknown_sources()
     {
        let a = minimum_with(
            "same",
            dimensions(1, 1),
            10,
            11,
            &[(10, 10, 10), (10, 10, 10)],
        );
        let b = minimum_with(
            "same",
            dimensions(2, 1),
            10,
            11,
            &[(10, 10, 10), (10, 10, 10)],
        );
        assert_eq!(analyze(&[a, b]).cohorts[0].candidates.len(), 2);

        let mut first = matrix(3);
        first["name"] = json!("one");
        let mut second = matrix(3);
        second["name"] = json!("two");
        assert_eq!(
            analyze(&[
                load_value("one.json", first).unwrap(),
                load_value("two.json", second).unwrap()
            ])
            .cohorts
            .len(),
            2
        );
    }

    #[test]
    fn same_label_distinct_key_later_candidate_wins_with_exact_typed_traces() {
        let losing_key = CandidateKey::MinimumHardware(dimensions(1, 2));
        let winning_key = CandidateKey::MinimumHardware(dimensions(2, 1));
        let losing = minimum_with(
            "same",
            dimensions(1, 2),
            20,
            21,
            &[(20, 20, 20), (20, 20, 20)],
        );
        let winning = minimum_with(
            "same",
            dimensions(2, 1),
            10,
            11,
            &[(10, 10, 10), (10, 10, 10)],
        );
        let cohort = &analyze(&[losing, winning]).cohorts[0];
        assert_eq!(
            cohort.recommendation.as_ref().map(|r| &r.key),
            Some(&winning_key)
        );
        assert!(matches!(
            cohort
                .candidates
                .iter()
                .find(|c| c.key == winning_key)
                .unwrap()
                .decision,
            DecisionTrace::Selected(_)
        ));
        assert!(matches!(
            &cohort.candidates.iter().find(|c| c.key == losing_key).unwrap().decision,
            DecisionTrace::ParetoDominated { by, .. } if *by == winning_key
        ));
    }

    #[test]
    fn pooled_conflict_is_ineligible_and_order_independent() {
        let mut a = minimum_with(
            "first",
            dimensions(1, 1),
            10,
            11,
            &[(10, 10, 10), (10, 10, 10)],
        );
        let mut b = a.clone();
        b.identity.run_id = "second".into();
        for repeat in &mut b.repeats {
            repeat.candidate = "second".into();
        }
        let ExperimentPayload::MinimumHardware(payload) = &mut b.payload else {
            unreachable!()
        };
        payload.required_allocation_bytes = 12;
        payload.fits_host_storage_envelope = false;

        let CapabilityEvidence::Available(results) = &mut a.capabilities else {
            unreachable!()
        };
        results[0].status = CapabilityStatus::Fail;
        results[0].evidence = None;
        results[0].error = Some("rack failed".into());
        let CapabilityEvidence::Available(results) = &mut b.capabilities else {
            unreachable!()
        };
        results[1].status = CapabilityStatus::Unavailable;
        results[1].evidence = None;
        results[1].error = Some("metrics unavailable".into());

        let forward = analyze(&[a.clone(), b.clone()]);
        let reverse = analyze(&[b, a]);
        let forward_candidate = &forward.cohorts[0].candidates[0];
        assert_eq!(
            forward_candidate.ineligibility,
            vec![
                IneligibilityReason::CapabilityFailed,
                IneligibilityReason::HostStorageEnvelopeExceeded,
                IneligibilityReason::ConflictingPooledSources,
                IneligibilityReason::CapabilityStatus {
                    capability: Capability::RackReadiness,
                    status: CapabilityStatus::Fail,
                },
                IneligibilityReason::CapabilityStatus {
                    capability: Capability::Metrics,
                    status: CapabilityStatus::Unavailable,
                },
            ]
        );
        assert!(forward_candidate.summary.required_allocation_bytes.is_none());
        assert_eq!(
            serde_json::to_value(forward_candidate).unwrap(),
            serde_json::to_value(&reverse.cohorts[0].candidates[0]).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&forward.cohorts[0].no_recommendation)
                .unwrap(),
            serde_json::to_value(&reverse.cohorts[0].no_recommendation)
                .unwrap()
        );
    }

    fn ranking_fixture(
        key: CandidateKey,
        objectives: Vec<Objective>,
    ) -> AnalyzedCandidate {
        let CandidateKey::MinimumHardware(dimensions) = &key else {
            unreachable!()
        };
        let input = minimum_with(
            "fixture",
            dimensions.clone(),
            10,
            11,
            &[(10, 10, 10), (10, 10, 10)],
        );
        let mut candidate = analyze_candidate(key, &[&input]);
        candidate.policy.objectives = objectives;
        candidate
    }

    #[test]
    fn writes_only_affect_rank_when_declared_applicable_at_their_ordered_stage()
    {
        let a_key = CandidateKey::MinimumHardware(dimensions(1, 1));
        let b_key = CandidateKey::MinimumHardware(dimensions(2, 1));
        let stat = |mean| {
            Some(Stats { n: 2, mean, median: mean, stddev: 0.0, cv: Some(0.0) })
        };

        let mut a =
            ranking_fixture(a_key.clone(), vec![Objective::RequiredAllocation]);
        let mut b =
            ranking_fixture(b_key.clone(), vec![Objective::RequiredAllocation]);
        a.summary.required_allocation_bytes = Some(1);
        b.summary.required_allocation_bytes = Some(2);
        a.summary.idle_writes = stat(1000.0);
        b.summary.idle_writes = stat(1.0);
        a.summary.launch_writes = stat(1000.0);
        b.summary.launch_writes = stat(1.0);
        let recommendation = rank(vec![&a, &b]).0.unwrap();
        assert_eq!(recommendation.key, a_key);
        assert_eq!(
            recommendation.rationale,
            SelectionRationale::SelectedAt(Objective::RequiredAllocation)
        );

        a.policy.objectives =
            vec![Objective::IdleWrites, Objective::RequiredAllocation];
        b.policy.objectives = a.policy.objectives.clone();
        assert_eq!(rank(vec![&a, &b]).0.unwrap().key, b_key);

        a.policy.objectives =
            vec![Objective::LaunchWrites, Objective::RequiredAllocation];
        b.policy.objectives = a.policy.objectives.clone();
        assert_eq!(rank(vec![&a, &b]).0.unwrap().key, b_key);
    }

    #[test]
    fn storage_cohorts_split_on_rss_workload_and_provenance_and_join_when_equal()
     {
        let base = load_value("base.json", matrix(3)).unwrap();
        let mut same = base.clone();
        same.identity.source = base.identity.source.clone();
        same.identity.run_id = base.identity.run_id.clone();
        assert_eq!(analyze(&[base.clone(), same]).cohorts.len(), 1);
        let mut rss = base.clone();
        let Dimensions::StorageLevers(d) = &mut rss.dimensions else {
            unreachable!()
        };
        d.rss_sleds += 1;
        let mut workload = base.clone();
        let ExperimentPayload::StorageLevers(p) = &mut workload.payload else {
            unreachable!()
        };
        p.workload = None;
        let mut provenance = base.clone();
        provenance.provenance = Provenance::Available(ProvenanceFields {
            voxel_revision: Some("v".into()),
            omicron_revision: Some("o".into()),
            image_id: Some("i".into()),
            host_id: Some("h".into()),
            voxel_build: None,
            voxel_binary: None,
            configured_image: None,
            omicron_commit: None,
            host: None,
        });
        assert_eq!(
            analyze(&[base, rss, workload, provenance]).cohorts.len(),
            4
        );
    }

    #[test]
    fn noise_boundary_single_sample_and_beyond_threshold_are_typed() {
        let stat = |n, mean, stddev| Stats {
            n,
            mean,
            median: mean,
            stddev,
            cv: Some(0.0),
        };
        let boundary = 2.0_f64 * (1.0_f64 + 1.0_f64).sqrt();
        assert_eq!(
            compare_stat(Some(stat(2, 0.0, 1.0)), Some(stat(2, boundary, 1.0))),
            MetricComparison::WithinNoise
        );
        assert_eq!(
            compare_stat(Some(stat(1, 1.0, 0.0)), Some(stat(2, 2.0, 1.0))),
            MetricComparison::NoiseUnknown
        );
        assert_eq!(
            compare_stat(
                Some(stat(2, 0.0, 1.0)),
                Some(stat(2, boundary + 0.01, 1.0))
            ),
            MetricComparison::Better
        );
    }

    #[test]
    fn pareto_tradeoff_and_lexicographic_objective_order_are_preserved() {
        let a = minimum_with(
            "disk",
            dimensions(1, 1),
            10,
            20,
            &[(100, 10, 100), (100, 10, 100)],
        );
        let b = minimum_with(
            "ram",
            dimensions(2, 1),
            20,
            30,
            &[(10, 1, 10), (10, 1, 10)],
        );
        let cohort = &analyze(&[a, b]).cohorts[0];
        assert_eq!(
            cohort.candidates.iter().filter(|c| !c.dominated).count(),
            2
        );
        assert_eq!(
            cohort.recommendation.as_ref().map(|r| r.display.as_str()),
            Some("disk")
        );
        assert!(matches!(
            cohort
                .candidates
                .iter()
                .find(|c| c.candidate == "disk")
                .unwrap()
                .decision,
            DecisionTrace::Selected(_)
        ));
        assert!(matches!(
            cohort
                .candidates
                .iter()
                .find(|c| c.candidate == "ram")
                .unwrap()
                .decision,
            DecisionTrace::LexicographicLoss {
                criterion: Objective::RequiredAllocation
            }
        ));
    }

    #[test]
    fn noise_unknown_and_asymmetric_objectives_produce_explicit_no_recommendation()
     {
        let a = load_value("one-a.json", minimum_fixture("one-a", 10, 10, 10))
            .unwrap();
        let mut b_value = minimum_fixture("one-b", 10, 10, 10);
        b_value["dimensions"]["vdev_count"] = json!(2);
        let b = load_value("one-b.json", b_value).unwrap();
        let cohort = &analyze(&[a, b]).cohorts[0];
        assert!(cohort.recommendation.is_none());
        assert_eq!(
            cohort.no_recommendation,
            Some(NoRecommendationReason::TradeoffOrNoiseTie)
        );
        assert!(cohort.candidates.iter().all(|candidate| matches!(
            candidate.decision,
            DecisionTrace::TieOrNoiseUnknown
        )));

        let mut missing = minimum_fixture("missing", 10, 10, 10);
        missing["repeats"][0].as_object_mut().unwrap().remove("peak_ram_bytes");
        let cohort =
            &analyze(&[load_value("missing-objective.json", missing).unwrap()])
                .cohorts[0];
        assert!(cohort.recommendation.is_none());
        assert_eq!(
            cohort.no_recommendation,
            Some(NoRecommendationReason::NoEligibleCandidates)
        );
    }

    #[test]
    fn envelope_ineligibility_outranks_better_disk_and_dominator_trace_is_typed()
     {
        let good = minimum_with(
            "good",
            dimensions(1, 1),
            20,
            21,
            &[(20, 20, 20), (20, 20, 20)],
        );
        let mut bad_value = minimum_fixture("bad", 1, 1, 1);
        bad_value["dimensions"]["vdev_count"] = json!(2);
        bad_value["payload"]["fits_host_storage_envelope"] = json!(false);
        let cohort =
            &analyze(&[good, load_value("bad.json", bad_value).unwrap()])
                .cohorts[0];
        assert_eq!(
            cohort.recommendation.as_ref().map(|r| r.display.as_str()),
            Some("good")
        );

        let better = minimum_with(
            "better",
            dimensions(3, 1),
            1,
            2,
            &[(1, 1, 1), (1, 1, 1)],
        );
        let worse = minimum_with(
            "worse",
            dimensions(4, 1),
            2,
            3,
            &[(10, 10, 10), (10, 10, 10)],
        );
        let cohort = &analyze(&[better, worse]).cohorts[0];
        assert!(matches!(
            cohort
                .candidates
                .iter()
                .find(|candidate| candidate.candidate == "worse")
                .unwrap()
                .decision,
            DecisionTrace::ParetoDominated { .. }
        ));
    }

    #[test]
    fn report_preserves_failed_storage_aggregate_but_rejects_malformed_success()
    {
        let mut failed = matrix(3);
        failed["repeat"] = json!(2);
        failed["results"][0]["error"] = json!("second repeat failed");
        let input = load_value("failed-matrix.json", failed).unwrap();
        assert_eq!(input.repeats.len(), 2);
        assert!(matches!(input.repeats[1].outcome, RepeatOutcome::Failure(_)));

        let mut malformed = matrix(3);
        malformed["repeat"] = json!(2);
        assert_rejected(
            malformed,
            "malformed.json",
            "validate storage matrix semantics",
        );

        let mut malformed_retained = matrix(3);
        malformed_retained["repeat"] = json!(2);
        malformed_retained["results"][0]["error"] = json!("stopped");
        malformed_retained["results"][0]["repeats"][0]
            .as_object_mut()
            .unwrap()
            .remove("peak_ram_bytes");
        assert_rejected(
            malformed_retained,
            "bad-retained.json",
            "missing Helios",
        );

        let mut full_with_error = matrix(3);
        full_with_error["results"][0]["error"] = json!("contradiction");
        assert_rejected(
            full_with_error,
            "full-error.json",
            "fewer than expected repeats",
        );
    }

    #[test]
    fn minimum_repeat_counts_and_whitespace_provenance_are_validated() {
        let mut zero = minimum_fixture("zero", 1, 2, 3);
        zero["payload"]["expected_repeats"] = json!(0);
        zero["repeats"] = json!([]);
        assert_rejected(zero, "zero.json", "greater than zero");

        let mut excess = minimum_fixture("excess", 1, 2, 3);
        let duplicate = excess["repeats"][0].clone();
        excess["repeats"].as_array_mut().unwrap().push(duplicate);
        assert_rejected(
            excess,
            "excess.json",
            "completed repeats must not exceed",
        );

        let mut blank = minimum_fixture("blank", 1, 2, 3);
        blank["provenance"]["host_id"] = json!("  ");
        let input = load_value("blank.json", blank).unwrap();
        assert_eq!(input.provenance, Provenance::Unavailable);
    }

    #[test]
    fn errors_name_unknown_and_unsupported_inputs() {
        let unknown =
            load_value("mystery.json", json!({"answer": 42})).unwrap_err();
        let message = format!("{unknown:#}");
        assert!(message.contains("mystery.json"));
        assert!(
            message
                .contains("matrix schema_version must be an unsigned integer")
        );

        let unsupported = load_value(
            "future.json",
            json!({
                "kind": "minimum-hardware", "schema_version": 99
            }),
        )
        .unwrap_err();
        let message = format!("{unsupported:#}");
        assert!(message.contains("future.json"));
        assert!(
            message.contains("unsupported minimum-hardware schema version 99")
        );

        for (kind, expected) in [
            (
                json!("future-experiment"),
                "unsupported perftest input kind 'future-experiment'",
            ),
            (json!(7), "perftest input kind must be a string"),
        ] {
            let error =
                load_value("bad-kind.json", json!({"kind": kind})).unwrap_err();
            assert!(format!("{error:#}").contains(expected));
        }
    }

    #[test]
    fn publishes_manifest_with_raw_and_artifact_digests_without_recursion() {
        let root = tempdir().unwrap();
        let out = root.path().join("result");
        let sources = [PublicationInput::new("raw.json", b"abc")];
        publish_report(
            &out,
            false,
            &sources,
            b"<html>report</html>",
            &json!({"normalized": true}),
        )
        .unwrap();

        assert_eq!(
            fs::read(out.join("report.html")).unwrap(),
            b"<html>report</html>"
        );
        let report_bytes = fs::read(out.join("report.json")).unwrap();
        let manifest_bytes = fs::read(out.join("manifest.json")).unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest.report_generator, REPORT_GENERATOR);
        assert_eq!(manifest.schema, MANIFEST_SCHEMA);
        assert!(manifest.generated_at_unix_seconds > 0);
        assert_eq!(manifest.inputs[0].source_name, "raw.json");
        assert_eq!(
            manifest.inputs[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(manifest.report_html.filename, "report.html");
        assert_eq!(
            manifest.report_html.sha256,
            sha256_hex(b"<html>report</html>")
        );
        assert_eq!(manifest.report_json.filename, "report.json");
        assert_eq!(manifest.report_json.sha256, sha256_hex(&report_bytes));
        assert_eq!(manifest.manifest_filename, "manifest.json");
        let shape: Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert!(shape.get("manifest_sha256").is_none());
        assert!(shape.get("archive").is_none());
    }

    #[test]
    fn archive_has_one_top_level_directory_and_exactly_three_files() {
        let root = tempdir().unwrap();
        let out = root.path().join("portable-report");
        publish_report(&out, true, &[], b"html", &json!({"a": 1})).unwrap();
        let archive =
            fs::File::open(root.path().join("portable-report.tar.gz")).unwrap();
        let mut tar = tar::Archive::new(GzDecoder::new(archive));
        let mut archived = tar
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                assert!(entry.header().entry_type().is_file());
                let path = entry.path().unwrap().into_owned();
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
                (path, bytes)
            })
            .collect::<Vec<_>>();
        archived.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            archived.iter().map(|item| item.0.clone()).collect::<Vec<_>>(),
            vec![
                PathBuf::from("portable-report/manifest.json"),
                PathBuf::from("portable-report/report.html"),
                PathBuf::from("portable-report/report.json"),
            ]
        );
        for (path, bytes) in archived {
            assert_eq!(bytes, fs::read(root.path().join(path)).unwrap());
        }
    }

    #[test]
    fn refuses_directory_and_archive_collisions_before_publication() {
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        fs::create_dir(&out).unwrap();
        let error =
            publish_report(&out, false, &[], b"", &json!({})).unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));

        fs::remove_dir(&out).unwrap();
        fs::write(root.path().join("report.tar.gz"), b"existing").unwrap();
        let error =
            publish_report(&out, true, &[], b"", &json!({})).unwrap_err();
        assert!(format!("{error:#}").contains("report.tar.gz"));
        assert!(!out.exists());
    }

    #[test]
    fn supports_output_without_an_explicit_parent() {
        assert_eq!(usable_parent(Path::new("report")), Path::new("."));
        assert_eq!(
            archive_path(Path::new("report")).unwrap(),
            Path::new("./report.tar.gz")
        );
    }

    #[test]
    fn dangling_symlinks_are_collisions() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        symlink("missing", &out).unwrap();
        let error =
            publish_report(&out, false, &[], b"html", &json!({})).unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));
    }

    #[test]
    fn destination_created_immediately_before_rename_is_preserved() {
        use std::os::unix::fs::MetadataExt;
        use std::sync::atomic::Ordering;
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        let error = publish_report_impl(
            &out,
            false,
            &[],
            b"html",
            &json!({}),
            &[],
            Some(FailurePoint::DestinationBeforeDirectoryRename),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));
        let metadata = fs::metadata(&out).unwrap();
        assert_eq!(
            metadata.ino(),
            COMPETING_DESTINATION_INODE.load(Ordering::SeqCst)
        );
        assert!(fs::read_dir(&out).unwrap().next().is_none());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert!(!publication_lock_path(&out).unwrap().exists());
    }

    #[test]
    fn archive_cleanup_and_sync_errors_are_combined() {
        let error = finish_archive_publication(
            Path::new("report.tar.gz"),
            Path::new(".report-archive.tmp-leftover"),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cleanup denied",
            )),
            Err(anyhow::anyhow!("parent sync denied")),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("parent sync denied"));
        assert!(message.contains("cleanup denied"));
        assert!(message.contains("leftover remains"));

        let error = finish_archive_publication(
            Path::new("report.tar.gz"),
            Path::new(".report-archive.tmp-leftover"),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cleanup denied",
            )),
            Ok(()),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("published and durability was confirmed"));
        assert!(message.contains("leftover remains"));
    }

    #[test]
    fn archive_destination_created_before_persist_is_preserved() {
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        let archive = root.path().join("report.tar.gz");
        let error = publish_report_impl(
            &out,
            true,
            &[],
            b"html",
            &json!({}),
            &[],
            Some(FailurePoint::DestinationBeforeArchivePersist),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("refusing overwrite"));
        assert_eq!(fs::read(archive).unwrap(), b"existing archive");
        assert!(!out.exists());
    }

    #[test]
    fn cleanup_failure_retains_the_initiating_error() {
        let error = combine_cleanup(
            anyhow::anyhow!("publication failed"),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cleanup denied",
            )),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("publication failed"));
        assert!(message.contains("cleanup also failed: cleanup denied"));
    }

    #[test]
    fn failed_generation_removes_temporary_artifacts() {
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        let error = publish_report_impl(
            &out,
            false,
            &[],
            b"html",
            &json!({}),
            &[],
            Some(FailurePoint::AfterHtml),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected publication failure"));
        assert!(!out.exists());
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn archive_failure_removes_partial_requested_publication() {
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        let error = publish_report_impl(
            &out,
            true,
            &[],
            b"html",
            &json!({}),
            &[],
            Some(FailurePoint::DuringArchive),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected archive failure"));
        assert!(!out.exists());
        assert!(!root.path().join("report.tar.gz").exists());
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn derived_write_failure_removes_directory_and_archive() {
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        let derived = [SvgArtifact {
            filename: "section-000-aggregate-chart-000.svg".into(),
            bytes: b"<svg/>".to_vec(),
        }];
        let error = publish_report_impl(
            &out,
            true,
            &[],
            b"html",
            &json!({}),
            &derived,
            Some(FailurePoint::DuringDerivedWrite),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("derived artifact write failure")
        );
        assert!(!out.exists());
        assert!(!root.path().join("report.tar.gz").exists());
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn ordinary_generation_does_not_publish_images() {
        let root = tempdir().unwrap();
        let input = root.path().join("ordinary.json");
        write_json(&input, &matrix(2));
        let out = root.path().join("ordinary-report");
        run(&[input], &out, false).unwrap();
        assert!(!out.join("images").exists());
    }

    #[test]
    fn legacy_schema_v2_compatibility_views_preserve_samples_and_recompute_summaries()
     {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let inputs = [
            root.join(
                "docs/perftest-20260718-011546-crucial/storage-levers.json",
            ),
            root.join(
                "docs/perftest-20260718-162302-crucial/storage-levers.json",
            ),
        ]
        .map(|path| load(&path).unwrap());
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        let storage = view
            .sections
            .iter()
            .find(|section| section.kind == ExperimentKind::StorageLevers)
            .unwrap();
        let descriptive = storage.descriptive_aggregate.as_ref().unwrap();
        let pooled = &descriptive.storage_summary;
        assert_eq!(pooled.len(), 5);
        assert!(pooled.iter().all(|combo| combo.writes_decimal_gb.len() == 6));
        let means = pooled
            .iter()
            .map(|combo| {
                combo.writes_decimal_gb.iter().sum::<f64>()
                    / combo.writes_decimal_gb.len() as f64
            })
            .collect::<Vec<_>>();
        for (combo, mean) in pooled.iter().zip(&means) {
            assert!((combo.writes.unwrap().mean - mean).abs() < 0.000001);
        }
        assert!(
            serde_json::to_value(descriptive)
                .unwrap()
                .get("write_reduction_percent")
                .is_none()
        );
        let longest_launch = inputs
            .iter()
            .flat_map(|input| &input.repeats)
            .filter_map(|repeat| repeat.metrics.launch_duration_secs)
            .max()
            .unwrap();
        assert!(
            pooled
                .iter()
                .flat_map(|combo| &combo.launch_seconds)
                .any(|x| *x == longest_launch)
        );
        assert!(pooled.iter().all(|combo| combo.workload_bytes.is_empty()));
        assert!(
            descriptive.disclaimer.contains("not a default recommendation")
        );
        assert!(
            descriptive
                .charts
                .iter()
                .any(|chart| chart.kind == ViewChartKind::Waterfall)
        );
        assert!(
            descriptive
                .charts
                .iter()
                .all(|chart| !chart.fallback_rows.is_empty())
        );
        let html = render_report_html(&view).unwrap();
        assert_eq!(
            html.matches("data-chart-fallback").count(),
            descriptive.charts.len()
                + storage
                    .cohorts
                    .iter()
                    .map(|cohort| cohort.charts.len())
                    .sum::<usize>()
        );
    }

    #[test]
    fn report_views_accept_variable_runs_and_condition_waterfall_on_a_ladder() {
        let one = load_value("one.json", matrix(3)).unwrap();
        let mut three = vec![one.clone(), one.clone(), one.clone()];
        for (index, input) in three.iter_mut().enumerate() {
            input.identity.source = PathBuf::from(format!("run-{index}"));
        }
        let one_view =
            build_report_view(&[one.clone()], &analyze(&[one]), &[]).unwrap();
        assert_eq!(
            one_view.sections[0].cohorts[0].storage_summary[0]
                .writes_decimal_gb
                .len(),
            1
        );
        let three_view =
            build_report_view(&three, &analyze(&three), &[]).unwrap();
        assert_eq!(
            three_view.sections[0]
                .descriptive_aggregate
                .as_ref()
                .unwrap()
                .storage_summary[0]
                .writes_decimal_gb
                .len(),
            3
        );
        assert!(
            three_view.sections[0]
                .cohorts
                .iter()
                .flat_map(|cohort| &cohort.charts)
                .all(|c| c.kind != ViewChartKind::Waterfall)
        );
    }

    #[test]
    fn descriptive_storage_aggregate_requires_matching_rss_and_workload() {
        let base = load_value("base.json", matrix(3)).unwrap();
        let mut rss = base.clone();
        let Dimensions::StorageLevers(dimensions) = &mut rss.dimensions else {
            unreachable!()
        };
        dimensions.rss_sleds += 1;
        let inputs = [base.clone(), rss];
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        assert!(view.sections[0].descriptive_aggregate.is_none());

        let mut workload = base.clone();
        let ExperimentPayload::StorageLevers(payload) = &mut workload.payload
        else {
            unreachable!()
        };
        payload.workload = None;
        let inputs = [base, workload];
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        assert!(view.sections[0].descriptive_aggregate.is_none());
    }

    #[test]
    fn html_is_offline_inert_accessible_and_contains_serialized_charming_options()
     {
        let attack = "<img src=x onerror=alert(1)></script>\u{2028}\u{2029}";
        let mut input = load_value("attack.json", matrix(3)).unwrap();
        input.identity.run_id = attack.into();
        input.identity.source = PathBuf::from(attack);
        input.repeats[0].candidate = attack.into();
        let analysis = analyze(&[input.clone()]);
        let view = build_report_view(
            &[input],
            &analysis,
            &[InputDigestView {
                source: attack.into(),
                sha256: Some(attack.into()),
                run_status: None,
                evidence_state: None,
                abort_error: None,
            }],
        )
        .unwrap();
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("Apache ECharts v5.5.1"));
        assert!(html.contains("<table"));
        assert!(html.contains("<h1"));
        assert!(html.contains("&lt;img"));
        assert!(!html.contains("</script>\u{2028}"));
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("href=\"http"));
        assert!(!html.contains("fetch("));
        assert!(!html.contains("import("));
        let lower = html
            .split("<!-- Embedded Apache ECharts")
            .next()
            .unwrap()
            .to_ascii_lowercase();
        for forbidden in [
            "<script src",
            "<link",
            "<img",
            "url(",
            "xmlhttprequest",
            "websocket",
            "type=\"module\"",
        ] {
            assert!(
                !lower.contains(forbidden),
                "unexpected browser surface: {forbidden}"
            );
        }
        assert!(html.contains("echarts.init"));
        assert!(html.contains("chart-0"));
        for chart in view
            .sections
            .iter()
            .flat_map(|section| &section.cohorts)
            .flat_map(|cohort| &cohort.charts)
        {
            assert!(chart.option.get("series").is_some());
        }
    }

    #[test]
    fn capability_evidence_is_escaped_after_stable_serialization() {
        let attack = "</td></tr></table><script>alert('evidence')</script>";
        let evidence = Some(BoundedEvidence(json!({"closing": attack})));
        let rendered = stable_html_json(&evidence).unwrap();
        assert!(rendered.contains("&lt;/script&gt;"));
        assert!(!rendered.contains("<script>"));

        let mut fixture = minimum_fixture("evidence", 10, 20, 30);
        fixture["capabilities"][0]["evidence"] = json!({"closing": attack});
        let input = load_value("evidence.json", fixture).unwrap();
        let html = render_report_html(
            &build_report_view(&[input.clone()], &analyze(&[input]), &[])
                .unwrap(),
        )
        .unwrap();
        assert!(html.contains("&lt;/script&gt;"));
        assert!(!html.contains("<script>alert('evidence')</script>"));
    }

    #[test]
    fn minimum_hardware_fixture_has_grouped_capability_resource_and_candidate_views()
     {
        let eligible = load_value(
            "eligible.json",
            minimum_fixture("eligible", 10, 20, 30),
        )
        .unwrap();
        let mut failed = minimum_fixture("failed", 20, 30, 40);
        failed["dimensions"]["vdev_count"] = json!(2);
        failed["capabilities"][0] = json!({
            "capability": "rack-readiness", "status": "fail", "error": "probe failed"
        });
        let failed = load_value("failed.json", failed).unwrap();
        let inputs = vec![eligible, failed];
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        let section = view
            .sections
            .iter()
            .find(|s| s.kind == ExperimentKind::MinimumHardware)
            .unwrap();
        assert!(
            section
                .cohorts
                .iter()
                .flat_map(|cohort| &cohort.charts)
                .any(|c| c.kind == ViewChartKind::Allocation)
        );
        assert!(
            section
                .cohorts
                .iter()
                .flat_map(|c| &c.candidates)
                .any(|c| c.recommended)
        );
        assert!(
            section
                .cohorts
                .iter()
                .flat_map(|c| &c.candidates)
                .any(|c| !c.eligible)
        );
    }

    fn storage_fixture(combos: &[(&str, &[u8], &[u64])]) -> NormalizedInput {
        let mut value = matrix(2);
        value["repeat"] = json!(
            combos.iter().map(|(_, _, samples)| samples.len()).max().unwrap()
        );
        value["combos"] =
            json!(combos.iter().map(|(label, _, _)| label).collect::<Vec<_>>());
        value["results"] = Value::Array(
            combos
                .iter()
                .map(|(label, levers, samples)| {
                    json!({
                        "label": label,
                        "levers": levers,
                        "repeats": samples.iter().map(|writes| json!({
                            "bringup_bytes": writes, "launch_secs": writes / 1_000_000_000,
                            "peak_ram_bytes": writes / 2
                        })).collect::<Vec<_>>()
                    })
                })
                .collect(),
        );
        load_value("storage-fixture.json", value).unwrap()
    }

    fn chart<'a>(cohort: &'a CohortView, kind: ViewChartKind) -> &'a ChartView {
        cohort.charts.iter().find(|chart| chart.kind == kind).unwrap()
    }

    #[test]
    fn condition_factoring_preserves_absent_and_distinct_rows() {
        let first = vec![
            condition("Shared", "same", false),
            condition("Code", "same", true),
            condition("Only first", "not supplied", false),
        ];
        let second = vec![
            condition("Shared", "same", false),
            condition("Code", "same", false),
            condition("Only second", "value", true),
        ];

        let factored = factor_condition_rows(&[first, second]);

        assert_eq!(factored.shared, vec![condition("Shared", "same", false)]);
        assert_eq!(
            factored
                .varying
                .iter()
                .map(|row| (row.label.as_str(), row.values.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "Code",
                    vec![
                        Some(condition("Code", "same", true)),
                        Some(condition("Code", "same", false)),
                    ],
                ),
                (
                    "Only first",
                    vec![
                        Some(condition("Only first", "not supplied", false)),
                        None
                    ],
                ),
                (
                    "Only second",
                    vec![None, Some(condition("Only second", "value", true))],
                ),
            ]
        );
    }

    #[test]
    fn flattening_preserves_empty_objects_and_arrays_as_distinct_values() {
        let mut rows = Vec::new();
        flatten_typed_value("Object", &json!({}), &mut rows);
        flatten_typed_value("Array", &json!([]), &mut rows);
        flatten_typed_value("Null", &Value::Null, &mut rows);
        assert_eq!(
            rows,
            vec![
                condition("Object", "{}", true),
                condition("Array", "[]", true),
                condition("Null", "not supplied", false),
            ]
        );

        let mut settings = Vec::new();
        flatten_candidate_setting("Object", &json!({}), &mut settings);
        flatten_candidate_setting("Array", &json!([]), &mut settings);
        assert_eq!(
            settings,
            vec![
                condition("Object", "{}", true),
                condition("Array", "[]", true),
            ]
        );
    }

    #[test]
    fn storage_projection_survives_missing_effective_configuration() {
        let input =
            storage_fixture(&[("none", &[], &[10]), ("sync", &[1], &[9])]);
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let projection =
            candidate_setting_projection(&view.sections[0].cohorts[0]).unwrap();
        assert_eq!(projection.candidates, ["none", "sync"]);
        assert!(
            projection.varying.iter().any(|row| row.label == "Storage levers")
        );
    }

    #[test]
    fn storage_projection_retains_declared_v4_candidate_without_repeats() {
        let mut value = matrix(3);
        value["schema_version"] = json!(4);
        value["combos"] = json!(["none", "1"]);
        value["results"].as_array_mut().unwrap().push(json!({
            "label": "1", "levers": [1], "error": "candidate setup failed", "repeats": []
        }));
        let input = load_value("partial-v4.json", value).unwrap();
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let cohort = &view.sections[0].cohorts[0];
        let projection = candidate_setting_projection(cohort).unwrap();

        assert_eq!(projection.candidates, ["none", "1"]);
        assert!(!verified_storage_ladder(cohort));
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("candidate setup failed"));
    }

    #[test]
    fn minimum_hardware_allocation_chart_does_not_require_repeat_metrics() {
        let mut value = minimum_fixture("allocation-only", 10, 20, 30);
        value["repeats"] = json!([]);
        let input = load_value("allocation-only.json", value).unwrap();
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let html = render_report_html(&view).unwrap();

        assert!(html.contains("Required and peak allocation"));
        assert!(html.contains("data-chart-fallback=\"Allocation\""));
        assert!(!html.contains("data-chart-fallback=\"LaunchDuration\""));
    }

    #[test]
    fn capability_disclosure_uses_storage_matrix_and_minimum_candidate_evidence()
     {
        let storage = partial_matrix_report();
        let storage_view =
            build_report_view(&[storage.clone()], &analyze(&[storage]), &[])
                .unwrap();
        let storage_html = render_report_html(&storage_view).unwrap();
        assert!(storage_html.contains("Matrix-wide capability evidence"));
        assert!(!storage_html.contains("Capability evidence: unavailable"));

        let minimum =
            load_value("minimum.json", minimum_fixture("minimum", 10, 20, 30))
                .unwrap();
        let minimum_view =
            build_report_view(&[minimum.clone()], &analyze(&[minimum]), &[])
                .unwrap();
        let minimum_html = render_report_html(&minimum_view).unwrap();
        assert!(minimum_html.contains("Capability evidence: available"));
        assert!(minimum_html.contains("rack-readiness"));
    }

    #[test]
    fn closed_complete_evidence_preserves_raw_and_statistical_semantics() {
        let input = partial_matrix_report();
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let html = render_report_html(&view).unwrap();

        assert!(html.contains("<summary>Complete evidence</summary>"));
        for text in [
            "Host capacity (bytes)",
            "Required allocation (bytes)",
            "Peak allocation (bytes)",
            "Completed repeats",
            "Idle writes (bytes)",
            "n / mean / median / stddev / CV",
            "Source / run / repeat",
            "Workload bytes",
            "Workload duration (seconds)",
            "Outcome",
        ] {
            assert!(
                html.contains(text),
                "missing complete evidence field: {text}"
            );
        }
        assert!(!html.contains("Complete concise aggregate summary"));
    }

    #[test]
    fn ergonomic_html_uses_stable_anchors_and_concise_storage_labels() {
        let input =
            storage_fixture(&[("none", &[], &[10]), ("sync", &[1], &[9])]);
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("id=\"section-storage-levers\""));
        assert!(html.contains("id=\"cohort-"));
        assert!(!html.contains("section-0"));
        assert!(!html.contains(">none — none<"));
    }

    #[test]
    fn candidate_settings_factor_exact_storage_configuration() {
        let mut input = partial_matrix_report();
        let ExperimentPayload::StorageLevers(experiment) = &mut input.payload
        else {
            unreachable!()
        };
        let mut configurations = BTreeMap::new();
        for (index, label) in
            ["none", "1", "1+2", "1+2+3", "1+2+3+4"].into_iter().enumerate()
        {
            let mut configuration = VoxelConfig::default();
            configuration.disk_wear.host_sync_disabled = index > 0;
            configurations.insert(label.to_string(), configuration);
        }
        experiment.effective_candidate_configurations_identity =
            Some(serde_json::to_string(&configurations).unwrap());
        experiment.effective_candidate_configurations = Some(configurations);

        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let settings =
            candidate_setting_projection(&view.sections[0].cohorts[0]).unwrap();

        assert_eq!(
            settings.candidates,
            vec!["none", "1", "1+2", "1+2+3", "1+2+3+4"]
        );
        assert_eq!(
            settings
                .varying
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Disk wear / Host sync disabled"]
        );
        assert!(settings.shared.len() > 40);
    }

    #[test]
    fn projection_detects_verified_ladders_empty_cohorts_and_exact_failures() {
        let input = storage_fixture(&[
            ("none", &[], &[]),
            ("sync", &[1], &[]),
            ("compressed", &[1, 2], &[]),
        ]);
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let cohort = &view.sections[0].cohorts[0];

        assert!(!verified_storage_ladder(cohort));
        assert!(cohort_has_no_measurements(cohort));
        assert_eq!(
            group_exact_failures(&[
                "repeat is pending".into(),
                "different".into(),
                "repeat is pending".into(),
            ]),
            vec![
                FailureGroup { message: "repeat is pending".into(), count: 2 },
                FailureGroup { message: "different".into(), count: 1 },
            ]
        );
    }

    #[test]
    fn every_chart_fallback_preserves_visible_categories() {
        let storage = partial_matrix_report();
        let minimum =
            load_value("minimum.json", minimum_fixture("minimum", 10, 20, 30))
                .unwrap();
        for input in [storage, minimum] {
            let view = build_report_view(
                std::slice::from_ref(&input),
                &analyze(std::slice::from_ref(&input)),
                &[],
            )
            .unwrap();
            for chart in &view.sections[0].cohorts[0].charts {
                let categories =
                    chart.option["xAxis"]["data"].as_array().unwrap();
                for category in
                    categories.iter().map(|value| value.as_str().unwrap())
                {
                    assert!(
                        chart
                            .fallback_rows
                            .iter()
                            .any(|row| row.category == category),
                        "fallback for '{}' omitted category '{}'",
                        chart.title,
                        category
                    );
                }
            }
        }
    }

    #[test]
    fn real_five_combo_ordered_ladder_has_exact_incremental_charming_option() {
        let input = storage_fixture(&[
            ("none", &[], &[50_000_000_000]),
            ("sync", &[1], &[40_000_000_000]),
            ("sync+compression", &[1, 2], &[34_000_000_000]),
            ("sync+compression+guest", &[1, 2, 3], &[31_000_000_000]),
            ("all", &[1, 2, 3, 4], &[30_000_000_000]),
        ]);
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let option =
            &chart(&view.sections[0].cohorts[0], ViewChartKind::Waterfall)
                .option;
        assert_eq!(option["xAxis"]["name"], "configuration");
        assert_eq!(
            option["xAxis"]["data"],
            json!([
                "sync — 1",
                "sync+compression — 1+2",
                "sync+compression+guest — 1+2+3",
                "all — 1+2+3+4"
            ])
        );
        assert_eq!(
            option["yAxis"]["name"],
            "change in decimal GB from previous rung"
        );
        assert_eq!(
            option["series"][0]["name"],
            "change in decimal GB from previous rung"
        );
        assert_eq!(
            option["series"][0]["data"],
            json!([-10.0, -6.0, -3.0, -1.0])
        );
    }

    #[test]
    fn non_ladders_and_missing_success_suppress_incremental_chart() {
        let fixtures = [
            storage_fixture(&[
                ("none", &[], &[10]),
                ("branch", &[2], &[9]),
                ("other", &[1, 3], &[8]),
            ]),
            storage_fixture(&[
                ("one", &[1], &[9]),
                ("none", &[], &[10]),
                ("two", &[1, 2], &[8]),
            ]),
            {
                let mut input = storage_fixture(&[
                    ("none", &[], &[10]),
                    ("one", &[1], &[9]),
                    ("two", &[1, 2], &[8]),
                ]);
                input.repeats[1].outcome =
                    RepeatOutcome::Failure("rung failed".into());
                input.repeats[1].metrics = CommonMetrics::default();
                input
            },
        ];
        for input in fixtures {
            let view =
                build_report_view(&[input.clone()], &analyze(&[input]), &[])
                    .unwrap();
            assert!(
                view.sections[0].cohorts[0]
                    .charts
                    .iter()
                    .all(|c| c.kind != ViewChartKind::Waterfall)
            );
        }
    }

    #[test]
    fn sample_chart_serializes_exact_category_points_means_and_names() {
        let labels = vec!["alpha".into(), "beta".into()];
        let option = sample_chart(
            "samples",
            "widgets",
            &labels,
            &[vec![1.0, 3.0], vec![2.0, 6.0]],
        )
        .unwrap();
        assert_eq!(option["xAxis"]["data"], json!(["alpha", "beta"]));
        assert_eq!(option["yAxis"]["name"], "widgets");
        assert_eq!(option["series"][0]["name"], "alpha");
        assert_eq!(
            option["series"][0]["data"],
            json!([["alpha", 1.0], ["alpha", 3.0]])
        );
        assert_eq!(option["series"][1]["name"], "beta");
        assert_eq!(
            option["series"][1]["data"],
            json!([["beta", 2.0], ["beta", 6.0]])
        );
        assert_eq!(option["series"][2]["name"], "Mean");
        assert_eq!(option["series"][2]["data"], json!([2.0, 4.0]));
        assert!(option.get("legend").is_some());
        assert!(option.get("tooltip").is_some());
    }

    #[test]
    fn comparable_cohorts_stay_separate_and_each_html_article_owns_its_charts()
    {
        let a = load_value("a.json", minimum_fixture("a", 10, 20, 30)).unwrap();
        let mut b = minimum_fixture("b", 11, 21, 31);
        b["provenance"]["host_id"] = json!("other-host");
        let b = load_value("b.json", b).unwrap();
        let view = build_report_view(
            &[a, b],
            &analyze(&[
                load_value("a2.json", minimum_fixture("a", 10, 20, 30))
                    .unwrap(),
                {
                    let mut v = minimum_fixture("b", 11, 21, 31);
                    v["provenance"]["host_id"] = json!("other-host");
                    load_value("b2.json", v).unwrap()
                },
            ]),
            &[],
        )
        .unwrap();
        let section = &view.sections[0];
        assert_eq!(section.cohorts.len(), 2);
        let html = render_report_html(&view).unwrap();
        let articles = html
            .split("<article id=\"")
            .skip(1)
            .map(|s| s.split("</article>").next().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(articles.len(), 2);
        for (article, cohort) in articles.iter().zip(&section.cohorts) {
            assert!(article.contains(&html_escape(&cohort.label)));
            assert_eq!(
                article.matches("class=\"chart\"").count(),
                cohort
                    .charts
                    .iter()
                    .filter(|chart| chart
                        .fallback_rows
                        .iter()
                        .any(|row| row.value.is_some()))
                    .count()
            );
        }
    }

    #[test]
    fn same_raw_label_distinct_candidate_keys_are_key_aware_everywhere() {
        let inputs = [
            minimum_with(
                "same",
                dimensions(1, 1),
                20,
                21,
                &[(20, 20, 20), (20, 20, 20)],
            ),
            minimum_with(
                "same",
                dimensions(2, 1),
                10,
                11,
                &[(10, 10, 10), (10, 10, 10)],
            ),
        ];
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        let cohort = &view.sections[0].cohorts[0];
        let labels = cohort
            .candidates
            .iter()
            .map(|c| c.label.clone())
            .collect::<Vec<_>>();
        assert_ne!(labels[0], labels[1]);
        assert!(labels.iter().all(|label| label.starts_with("same — {")
            && cohort.conclusion.contains(label)));
        assert_eq!(
            chart(cohort, ViewChartKind::LaunchDuration).option["xAxis"]["data"],
            json!(labels)
        );
    }

    #[test]
    fn failed_only_storage_combo_renders_attribution_without_fabricated_stats()
    {
        let mut input = storage_fixture(&[("failed", &[], &[10])]);
        input.identity.source = PathBuf::from("failed-source.json");
        input.repeats[0].outcome =
            RepeatOutcome::Failure("disk exploded".into());
        input.repeats[0].metrics = CommonMetrics::default();
        let view = build_report_view(&[input.clone()], &analyze(&[input]), &[])
            .unwrap();
        let row = &view.sections[0].cohorts[0].storage_summary[0];
        assert_eq!(row.writes, None);
        assert!(row.writes_decimal_gb.is_empty());
        assert_eq!(row.rows[0].source, "failed-source.json");
        assert_eq!(row.rows[0].repeat_ordinal, 1);
        assert_eq!(row.failed_repeats, ["disk exploded"]);
        let html = render_report_html(&view).unwrap();
        assert!(html.contains("failed-source.json"));
        assert!(html.contains("disk exploded"));
        assert!(html.contains("unavailable"));
    }

    #[test]
    fn allocation_missing_values_are_null_and_peak_over_capacity_is_infeasible()
    {
        let mut missing = minimum_fixture("missing", 10, 20, 30);
        missing["payload"]
            .as_object_mut()
            .unwrap()
            .remove("required_allocation_bytes");
        missing["payload"]
            .as_object_mut()
            .unwrap()
            .remove("peak_allocation_bytes");
        let mut over = minimum_fixture("over", 10, 20, 30);
        over["dimensions"]["vdev_count"] = json!(2);
        over["payload"]["host_storage_capacity_bytes"] = json!(50);
        over["payload"]["peak_allocation_bytes"] = json!(51);
        over["payload"]["fits_host_storage_envelope"] = json!(false);
        let inputs = [
            load_value("missing.json", missing).unwrap(),
            load_value("over.json", over).unwrap(),
        ];
        let view = build_report_view(&inputs, &analyze(&inputs), &[]).unwrap();
        let cohort = &view.sections[0].cohorts[0];
        let missing = cohort
            .candidates
            .iter()
            .find(|c| c.label.starts_with("missing —"))
            .unwrap();
        let over = cohort
            .candidates
            .iter()
            .find(|c| c.label.starts_with("over —"))
            .unwrap();
        assert_eq!(
            (
                missing.required_allocation_bytes,
                missing.peak_allocation_bytes,
                missing.feasible
            ),
            (None, None, None)
        );
        assert_eq!(over.feasible, Some(false));
        let option = &chart(cohort, ViewChartKind::Allocation).option;
        assert_eq!(option["series"][0]["data"][0], Value::Null);
        assert_eq!(option["series"][1]["data"][0], Value::Null);
    }

    #[test]
    fn capability_chart_has_exact_statuses_labels_and_unavailable_fallback() {
        let mut explicit = minimum_fixture("explicit", 10, 20, 30);
        explicit["capabilities"][1] =
            json!({"capability":"metrics", "status":"fail", "error":"failed"});
        explicit["capabilities"][2] = json!({"capability":"fleet-api", "status":"unavailable", "error":"not probed"});
        let explicit = load_value("explicit.json", explicit).unwrap();
        let explicit_view =
            build_report_view(&[explicit.clone()], &analyze(&[explicit]), &[])
                .unwrap();
        let option = &chart(
            &explicit_view.sections[0].cohorts[0],
            ViewChartKind::Capabilities,
        )
        .option;
        assert_eq!(option["series"][0]["data"], json!([1, 0, -1, 1, 1, 1, 1]));
        let label =
            explicit_view.sections[0].cohorts[0].candidates[0].label.clone();
        assert_eq!(
            option["xAxis"]["data"],
            json!([
                format!("{label} / rack-readiness"),
                format!("{label} / metrics"),
                format!("{label} / fleet-api"),
                format!("{label} / silo-api"),
                format!("{label} / project-disk-lifecycle"),
                format!("{label} / topology-fidelity"),
                format!("{label} / clean-teardown")
            ])
        );
        let mut fallback = minimum_fixture("fallback", 10, 20, 30);
        fallback["capabilities"] = Value::Null;
        fallback.as_object_mut().unwrap().remove("contract_name");
        fallback.as_object_mut().unwrap().remove("contract_version");
        let fallback = load_value("fallback.json", fallback).unwrap();
        let fallback_view =
            build_report_view(&[fallback.clone()], &analyze(&[fallback]), &[])
                .unwrap();
        let fallback_cohort = &fallback_view.sections[0].cohorts[0];
        assert_eq!(
            chart(fallback_cohort, ViewChartKind::Capabilities).option["series"]
                [0]["data"],
            json!([-1, -1, -1, -1, -1, -1, -1])
        );
        assert!(render_report_html(&fallback_view)
            .unwrap()
            .contains("Capability evidence: <span class=\"unavailable\">unavailable</span>"));
    }

    #[test]
    fn workload_charts_and_stats_are_exactly_conditional_on_evidence() {
        let with = load_value("with.json", matrix(3)).unwrap();
        let view =
            build_report_view(&[with.clone()], &analyze(&[with]), &[]).unwrap();
        let cohort = &view.sections[0].cohorts[0];
        assert_eq!(cohort.storage_summary[0].workload_bytes, [2048]);
        assert_eq!(cohort.storage_summary[0].workload_seconds, [9]);
        assert_eq!(
            chart(cohort, ViewChartKind::WorkloadWear).option["series"][0]["data"],
            json!([["none", 0.000002048]])
        );
        assert_eq!(
            chart(cohort, ViewChartKind::WorkloadDuration).option["series"][0]
                ["data"],
            json!([["none", 9.0]])
        );
        let without = storage_fixture(&[("none", &[], &[10])]);
        let view =
            build_report_view(&[without.clone()], &analyze(&[without]), &[])
                .unwrap();
        assert!(view.sections[0].cohorts[0].charts.iter().all(|c| !matches!(
            c.kind,
            ViewChartKind::WorkloadWear | ViewChartKind::WorkloadDuration
        )));
    }

    #[test]
    fn supplied_digests_require_one_to_one_exact_source_identity() {
        let mut first =
            load_value("first.json", minimum_fixture("first", 10, 20, 30))
                .unwrap();
        first.identity.source = PathBuf::from("inputs/first.json");
        let mut second =
            load_value("second.json", minimum_fixture("second", 11, 21, 31))
                .unwrap();
        second.identity.source = PathBuf::from("inputs/second.json");
        let inputs = [first, second];
        let error = build_report_view(
            &inputs,
            &analyze(&inputs),
            &[InputDigestView {
                source: "inputs/first.json".into(),
                sha256: Some("abc123".into()),
                run_status: None,
                evidence_state: None,
                abort_error: None,
            }],
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("digest count 1"));

        let error = build_report_view(
            &inputs,
            &analyze(&inputs),
            &[
                InputDigestView {
                    source: "first.json".into(),
                    sha256: Some("wrong".into()),
                    run_status: None,
                    evidence_state: None,
                    abort_error: None,
                },
                InputDigestView {
                    source: "inputs/second.json".into(),
                    sha256: Some("right".into()),
                    run_status: None,
                    evidence_state: None,
                    abort_error: None,
                },
            ],
        )
        .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("does not match normalized input source")
        );
    }

    #[test]
    fn run_keeps_basename_collision_digests_distinct_everywhere() {
        let root = tempdir().unwrap();
        let nested = root.path().join("dir");
        fs::create_dir(&nested).unwrap();
        let first = root.path().join("foo.json");
        let second = nested.join("foo.json");
        fs::write(&first, serde_json::to_vec(&matrix(2)).unwrap()).unwrap();
        let mut other = matrix(2);
        other["name"] = json!("distinct");
        fs::write(&second, serde_json::to_vec(&other).unwrap()).unwrap();
        let first_digest = sha256_hex(&fs::read(&first).unwrap());
        let second_digest = sha256_hex(&fs::read(&second).unwrap());
        assert_ne!(first_digest, second_digest);
        let out = root.path().join("result");

        run(&[first.clone(), second.clone()], &out, false).unwrap();

        let report: Value =
            serde_json::from_slice(&fs::read(out.join("report.json")).unwrap())
                .unwrap();
        for location in [&report["inputs"], &report["view"]["inputs"]] {
            assert_eq!(location[0]["source"], first.display().to_string());
            assert_eq!(location[0]["sha256"], first_digest);
            assert_eq!(location[1]["source"], second.display().to_string());
            assert_eq!(location[1]["sha256"], second_digest);
        }
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(out.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.inputs[0].source_name, first.display().to_string());
        assert_eq!(manifest.inputs[0].sha256, first_digest);
        assert_eq!(
            manifest.inputs[1].source_name,
            second.display().to_string()
        );
        assert_eq!(manifest.inputs[1].sha256, second_digest);
        let html = fs::read_to_string(out.join("report.html")).unwrap();
        assert!(html.contains(&html_escape(&first.display().to_string())));
        assert!(html.contains(&first_digest));
        assert!(html.contains(&html_escape(&second.display().to_string())));
        assert!(html.contains(&second_digest));
    }

    #[test]
    fn run_orchestrates_normalization_analysis_rendering_and_publication() {
        let root = tempdir().unwrap();
        let first = root.path().join("first.json");
        let second = root.path().join("second.json");
        fs::write(&first, serde_json::to_vec(&matrix(2)).unwrap()).unwrap();
        let mut other = matrix(2);
        other["name"] = json!("other");
        fs::write(&second, serde_json::to_vec(&other).unwrap()).unwrap();
        let out = root.path().join("result");

        run(&[first.clone(), second.clone()], &out, true).unwrap();

        let document: Value =
            serde_json::from_slice(&fs::read(out.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(document["schema"], "voxel-perftest-report-v1");
        assert_eq!(document["generator"]["name"], REPORT_GENERATOR);
        assert_eq!(document["contract"]["name"], CAPABILITY_CONTRACT_NAME);
        assert_eq!(document["inputs"].as_array().unwrap().len(), 2);
        assert_eq!(document["normalized_inputs"].as_array().unwrap().len(), 2);
        assert!(document["analysis"]["cohorts"].is_array());
        assert!(document["view"]["sections"].is_array());
        assert!(document["aggregate_status"].is_string());
        assert!(
            !fs::read_to_string(out.join("report.json"))
                .unwrap()
                .contains("Apache ECharts")
        );
        let html = fs::read_to_string(out.join("report.html")).unwrap();
        assert!(html.contains(&sha256_hex(&fs::read(&first).unwrap())));
        assert!(html.contains("Content-Security-Policy"));
        assert!(root.path().join("result.tar.gz").is_file());
    }

    #[test]
    fn normalized_report_evidence_round_trips_through_replay_boundary() {
        let root = tempdir().unwrap();
        let input = root.path().join("input.json");
        fs::write(&input, serde_json::to_vec(&matrix(2)).unwrap()).unwrap();
        let out = root.path().join("result");
        run(&[input], &out, false).unwrap();

        let replay = parse_normalized_report_evidence(
            &fs::read(out.join("report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].metadata.identity, replay[0].normalized.identity);
    }

    #[test]
    fn normalized_report_replay_accepts_legacy_workload_dispositions() {
        let root = tempdir().unwrap();
        for (version, remove_metrics, expected) in [
            (2, false, WorkloadDisposition::NotRequested),
            (3, false, WorkloadDisposition::Succeeded),
            (3, true, WorkloadDisposition::Pending),
        ] {
            let input = root
                .path()
                .join(format!("input-{version}-{remove_metrics}.json"));
            fs::write(&input, serde_json::to_vec(&matrix(version)).unwrap())
                .unwrap();
            let out =
                root.path().join(format!("result-{version}-{remove_metrics}"));
            run(&[input], &out, false).unwrap();
            let mut report: Value = serde_json::from_slice(
                &fs::read(out.join("report.json")).unwrap(),
            )
            .unwrap();
            let payload = &mut report["normalized_inputs"][0]["repeats"][0]["payload"]
                ["data"];
            payload.as_object_mut().unwrap().remove("workload_disposition");
            if remove_metrics {
                payload["workload_bytes"] = Value::Null;
                payload["workload_duration_secs"] = Value::Null;
            }

            let replay = parse_normalized_report_evidence(
                &serde_json::to_vec(&report).unwrap(),
            )
            .unwrap();
            let RepeatPayload::StorageLevers(payload) =
                &replay[0].normalized.repeats[0].payload
            else {
                panic!("expected storage repeat")
            };
            assert_eq!(payload.workload_disposition, expected);
        }

        let input = root.path().join("explicit.json");
        fs::write(&input, serde_json::to_vec(&matrix(3)).unwrap()).unwrap();
        let out = root.path().join("explicit-report");
        run(&[input], &out, false).unwrap();
        let mut report: Value =
            serde_json::from_slice(&fs::read(out.join("report.json")).unwrap())
                .unwrap();
        report["normalized_inputs"][0]["repeats"][0]["payload"]["data"]["workload_disposition"] =
            json!("pending");
        let replay = parse_normalized_report_evidence(
            &serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let RepeatPayload::StorageLevers(payload) =
            &replay[0].normalized.repeats[0].payload
        else {
            panic!("expected storage repeat")
        };
        assert_eq!(payload.workload_disposition, WorkloadDisposition::Pending);
    }

    #[test]
    fn normalized_report_replay_accepts_full_configuration_identity() {
        let root = tempdir().unwrap();
        let input = root.path().join("input.json");
        fs::write(&input, serde_json::to_vec(&matrix(2)).unwrap()).unwrap();
        let out = root.path().join("result");
        run(&[input], &out, false).unwrap();
        let mut report: Value =
            serde_json::from_slice(&fs::read(out.join("report.json")).unwrap())
                .unwrap();
        let identity = json!({
            "none": {"generated-padding": "x".repeat(MAX_REPLAY_STRING_BYTES)}
        })
        .to_string();
        assert!(identity.len() > MAX_REPLAY_STRING_BYTES);
        report["normalized_inputs"][0]["payload"]["data"]["effective_candidate_configurations_identity"] =
            json!(identity);

        parse_normalized_report_evidence(&serde_json::to_vec(&report).unwrap())
            .unwrap();

        report["normalized_inputs"][0]["payload"]["data"]["effective_candidate_configurations_identity"] =
            json!("x".repeat(MAX_NORMALIZED_INPUT_BYTES));
        let error = parse_normalized_report_evidence(
            &serde_json::to_vec(&report).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("normalized input exceeds"));
    }

    #[test]
    fn normalized_report_replay_rejects_semantically_malformed_evidence() {
        let root = tempdir().unwrap();
        let input = root.path().join("input.json");
        fs::write(&input, serde_json::to_vec(&matrix(2)).unwrap()).unwrap();
        let out = root.path().join("result");
        run(&[input], &out, false).unwrap();
        let report = fs::read(out.join("report.json")).unwrap();

        for (name, mutate, expected) in [
            ("digest", ("inputs", "sha256", json!("ABC")), "SHA-256"),
            (
                "candidate",
                ("normalized_inputs", "candidate", json!("not-canonical")),
                "candidate",
            ),
            (
                "memory",
                ("normalized_inputs", "peak_ram_semantics", Value::Null),
                "memory semantics",
            ),
        ] {
            let mut value: Value = serde_json::from_slice(&report).unwrap();
            match mutate {
                ("inputs", field, replacement) => {
                    value["inputs"][0][field] = replacement
                }
                ("normalized_inputs", "candidate", replacement) => {
                    value["normalized_inputs"][0]["repeats"][0]["candidate"] =
                        replacement
                }
                ("normalized_inputs", field, replacement) => {
                    value["normalized_inputs"][0]["repeats"][0]["metrics"]
                        [field] = replacement
                }
                _ => unreachable!(),
            }
            let error = parse_normalized_report_evidence(
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "{name}: {error:#}"
            );
        }
    }

    #[test]
    fn normalized_report_replay_rejects_unbounded_counts_and_diagnostics() {
        let root = tempdir().unwrap();
        let storage_input = root.path().join("storage.json");
        fs::write(&storage_input, serde_json::to_vec(&matrix(2)).unwrap())
            .unwrap();
        let storage_out = root.path().join("storage-report");
        run(&[storage_input], &storage_out, false).unwrap();
        let storage: Value = serde_json::from_slice(
            &fs::read(storage_out.join("report.json")).unwrap(),
        )
        .unwrap();

        let mut huge_repeats = storage.clone();
        huge_repeats["normalized_inputs"][0]["payload"]["data"]["requested_repeats"] =
            json!(usize::MAX);
        let error = parse_normalized_report_evidence(
            &serde_json::to_vec(&huge_repeats).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("requested_repeats"));

        let mut huge_failure = storage;
        huge_failure["normalized_inputs"][0]["repeats"][0]["outcome"] = json!({
            "status": "failure",
            "error": "x".repeat(MAX_REPLAY_STRING_BYTES + 1),
        });
        let error = parse_normalized_report_evidence(
            &serde_json::to_vec(&huge_failure).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("repeat failure"));

        let minimum_input = root.path().join("minimum.json");
        fs::write(
            &minimum_input,
            serde_json::to_vec(&minimum_fixture("minimum", 1, 2, 3)).unwrap(),
        )
        .unwrap();
        let minimum_out = root.path().join("minimum-report");
        run(&[minimum_input], &minimum_out, false).unwrap();
        let mut minimum: Value = serde_json::from_slice(
            &fs::read(minimum_out.join("report.json")).unwrap(),
        )
        .unwrap();
        minimum["normalized_inputs"][0]["payload"]["data"]["expected_repeats"] =
            json!(usize::MAX);
        let error = parse_normalized_report_evidence(
            &serde_json::to_vec(&minimum).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("expected_repeats"));

        let mut missing_configuration: Value = serde_json::from_slice(
            &fs::read(minimum_out.join("report.json")).unwrap(),
        )
        .unwrap();
        missing_configuration["normalized_inputs"][0]["effective_configuration"] =
            json!({"availability": "unavailable"});
        let error = parse_normalized_report_evidence(
            &serde_json::to_vec(&missing_configuration).unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("effective configuration"));
    }

    #[test]
    fn run_rejects_collisions_before_reading_inputs_and_never_overwrites() {
        let root = tempdir().unwrap();
        let out = root.path().join("result");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("sentinel"), b"keep").unwrap();
        let missing = root.path().join("missing.json");
        let error = run(&[missing], &out, false).unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));
        assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn run_rejects_malformed_input_without_creating_destination() {
        let root = tempdir().unwrap();
        let input = root.path().join("bad.json");
        fs::write(&input, b"not json").unwrap();
        let out = root.path().join("result");
        let error = run(&[input.clone()], &out, false).unwrap_err();
        assert!(format!("{error:#}").contains(&input.display().to_string()));
        assert!(!out.exists());
    }

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn read_run_report(out: &Path) -> Value {
        serde_json::from_slice(&fs::read(out.join("report.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn run_summary_has_exact_kind_and_stable_typed_cohort_identifier() {
        let storage = load_value("storage.json", matrix(2)).unwrap();
        let hardware =
            load_value("hardware.json", minimum_fixture("small", 10, 20, 30))
                .unwrap();
        let inputs = [storage, hardware];
        let analysis = analyze(&inputs);
        let lines = analysis
            .cohorts
            .iter()
            .map(|cohort| {
                let kind = match &cohort.key {
                    CohortKey::Storage(_) => "storage-levers",
                    CohortKey::MinimumHardware(_) => "minimum-hardware",
                };
                let digest =
                    sha256_hex(&serde_json::to_vec(&cohort.key).unwrap());
                let recommendation = cohort
                    .recommendation
                    .as_ref()
                    .map(|r| r.display.as_str())
                    .unwrap_or("none");
                format!(
                    "cohort {kind}/{} recommendation: {recommendation}\n",
                    &digest[..12]
                )
            })
            .collect::<String>();
        assert_eq!(
            format_run_summary(&inputs, &analysis, 1),
            format!(
                "inputs: 2 accepted, 0 rejected\nexperiment kinds: storage-levers=1, minimum-hardware=1\ncohorts: 2; eligible candidates: 1\n{lines}"
            )
        );
        assert_eq!(
            format_run_summary(&inputs, &analysis, 1),
            format_run_summary(&inputs, &analysis, 1)
        );
    }

    #[test]
    fn run_mixed_storage_and_minimum_hardware_keeps_sections_and_recommendation_consistent()
     {
        let root = tempdir().unwrap();
        let storage = root.path().join("storage.json");
        let hardware = root.path().join("hardware.json");
        write_json(&storage, &matrix(2));
        write_json(&hardware, &minimum_fixture("complete-small", 10, 20, 30));
        let out = root.path().join("report");
        run(&[storage, hardware], &out, false).unwrap();

        let report = read_run_report(&out);
        assert_eq!(report["analysis"]["cohorts"].as_array().unwrap().len(), 2);
        let sections = report["view"]["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0]["kind"], "storage-levers");
        assert_eq!(sections[1]["kind"], "minimum-hardware");
        let analysis_recommendation = report["analysis"]["cohorts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["key"]["kind"] == "minimum-hardware")
            .unwrap()["recommendation"]["display"]
            .clone();
        assert_eq!(analysis_recommendation, "complete-small");
        let hardware_cohort = &sections[1]["cohorts"][0];
        assert!(
            hardware_cohort["conclusion"]
                .as_str()
                .unwrap()
                .contains("complete-small")
        );
        assert_eq!(hardware_cohort["candidates"][0]["recommended"], true);
        let html = fs::read_to_string(out.join("report.html")).unwrap();
        assert!(html.contains("Storage levers"));
        assert!(html.contains("Minimum hardware fixture evidence"));
        assert!(html.contains("Advisory recommendation: complete-small"));
    }

    #[test]
    fn run_preserves_failed_and_early_aborted_minimum_hardware_evidence() {
        for (name, add_failure) in [("failed", true), ("aborted", false)] {
            let root = tempdir().unwrap();
            let input = root.path().join("input.json");
            let mut fixture = minimum_fixture(name, 10, 20, 30);
            fixture["payload"]["expected_repeats"] = json!(2);
            if add_failure {
                fixture["repeats"].as_array_mut().unwrap().push(json!({
                    "candidate": name,
                    "outcome": {"status": "failure", "error": "launch failed"}
                }));
            }
            write_json(&input, &fixture);
            let out = root.path().join("report");
            run(&[input], &out, false).unwrap();
            let report = read_run_report(&out);
            let candidate = &report["analysis"]["cohorts"][0]["candidates"][0];
            assert_eq!(candidate["summary"]["successful_repeats"], 1);
            assert!(candidate["ineligibility"].as_array().unwrap().iter().any(
                |reason| {
                    reason
                        == if add_failure {
                            "RequiredRepeatFailed"
                        } else {
                            "RequiredRepeatMissing"
                        }
                }
            ));
            if add_failure {
                let failure = candidate["repeats"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|repeat| repeat["outcome"]["status"] == "failure")
                    .unwrap();
                assert_eq!(failure["outcome"]["error"], "launch failed");
            }
        }
    }

    #[test]
    fn run_accepts_one_and_three_storage_inputs() {
        for count in [1, 3] {
            let root = tempdir().unwrap();
            let paths = (0..count)
                .map(|i| {
                    let path = root.path().join(format!("storage-{i}.json"));
                    let mut value = matrix(2);
                    value["name"] = json!(format!("run-{i}"));
                    write_json(&path, &value);
                    path
                })
                .collect::<Vec<_>>();
            let out = root.path().join("report");
            run(&paths, &out, false).unwrap();
            assert_eq!(
                read_run_report(&out)["inputs"].as_array().unwrap().len(),
                count
            );
        }
    }

    #[test]
    fn run_legacy_schema_v2_compatibility_fixtures_preserves_invariants_and_attribution()
     {
        let repository =
            Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let first = repository
            .join("docs/perftest-20260718-011546-crucial/storage-levers.json");
        let second = repository
            .join("docs/perftest-20260718-162302-crucial/storage-levers.json");
        let root = tempdir().unwrap();
        let out = root.path().join("report");
        run(&[first, second.clone()], &out, false).unwrap();
        let report = read_run_report(&out);
        let aggregate = &report["view"]["sections"][0]["descriptive_aggregate"];
        let rows = aggregate["storage_summary"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        assert!(
            rows.iter().all(|row| row["writes_decimal_gb"]
                .as_array()
                .unwrap()
                .len()
                == 6)
        );
        let means = rows
            .iter()
            .map(|row| {
                let samples = row["writes_decimal_gb"].as_array().unwrap();
                samples
                    .iter()
                    .map(Value::as_f64)
                    .map(Option::unwrap)
                    .sum::<f64>()
                    / samples.len() as f64
            })
            .collect::<Vec<_>>();
        for (row, mean) in rows.iter().zip(&means) {
            assert!(
                (row["writes"]["mean"].as_f64().unwrap() - mean).abs()
                    < 0.000001
            );
        }
        assert!(aggregate.get("write_reduction_percent").is_none());
        assert!(
            report["analysis"]["cohorts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["recommendation"].is_null())
        );
        assert!(
            rows.iter().all(|row| row["workload_bytes"]
                .as_array()
                .unwrap()
                .is_empty())
        );
        assert!(
            rows.iter().all(|row| row["workload_seconds"]
                .as_array()
                .unwrap()
                .is_empty())
        );
        assert!(
            report["view"]["sections"][0]["cohorts"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|cohort| cohort["charts"].as_array().unwrap())
                .all(|chart| chart["kind"] != "workload-wear"
                    && chart["kind"] != "workload-duration")
        );
        let fixture: Value =
            serde_json::from_slice(&fs::read(&second).unwrap()).unwrap();
        let mut expected = None;
        for result in fixture["results"].as_array().unwrap() {
            for (index, repeat) in
                result["repeats"].as_array().unwrap().iter().enumerate()
            {
                let launch = repeat["launch_secs"].as_u64().unwrap();
                if expected
                    .as_ref()
                    .is_none_or(|(_, _, longest)| launch > *longest)
                {
                    expected = Some((
                        result["label"].as_str().unwrap().to_string(),
                        index + 1,
                        launch,
                    ));
                }
            }
        }
        let (candidate, repeat_ordinal, longest_launch) = expected.unwrap();
        let attributed =
            rows.iter().find(|row| row["label"] == candidate).unwrap()["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| {
                    row["metrics"]["launch_duration_secs"] == longest_launch
                })
                .unwrap();
        assert_eq!(attributed["source"], second.display().to_string());
        assert_eq!(attributed["run_id"], "voxel");
        assert_eq!(attributed["repeat_ordinal"], repeat_ordinal);
    }

    #[test]
    fn run_archive_extracts_byte_identical_published_artifacts() {
        let root = tempdir().unwrap();
        let input = root.path().join("input.json");
        write_json(&input, &matrix(2));
        let out = root.path().join("published");
        run(&[input], &out, true).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(
            fs::File::open(root.path().join("published.tar.gz")).unwrap(),
        ));
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let name = path.file_name().unwrap().to_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            assert_eq!(bytes, fs::read(out.join(&name)).unwrap());
            seen.push(name);
        }
        seen.sort();
        assert_eq!(
            seen,
            ["manifest.json", "report.html", "report.json"]
                .map(std::ffi::OsString::from)
        );
    }

    #[test]
    fn run_archive_preflight_and_invalid_inputs_never_create_either_output() {
        let root = tempdir().unwrap();
        let missing = root.path().join("missing.json");
        let out = root.path().join("report");
        fs::write(root.path().join("report.tar.gz"), b"collision").unwrap();
        let error = run(&[missing.clone()], &out, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("archive")
                && !format!("{error:#}").contains("read report input")
        );
        assert!(!out.exists());

        for (name, bytes) in [
            (
                "future.json",
                br#"{"kind":"minimum-hardware","schema_version":99}"#
                    .as_slice(),
            ),
            ("malformed.json", b"not json".as_slice()),
        ] {
            let case = tempdir().unwrap();
            let input = case.path().join(name);
            fs::write(&input, bytes).unwrap();
            let destination = case.path().join("result");
            assert!(run(&[input], &destination, true).is_err());
            assert!(!destination.exists());
            assert!(!case.path().join("result.tar.gz").exists());
        }
    }

    #[test]
    fn colliding_formatted_conditions_keep_distinct_typed_cohorts_and_chart_order()
     {
        let mut a = minimum_fixture("a", 10, 20, 30);
        a["provenance"]["voxel_revision"] = json!("x; omicron_revision=y");
        a["provenance"]["omicron_revision"] = json!("z");
        let mut b = minimum_fixture("b", 11, 21, 31);
        b["provenance"]["voxel_revision"] = json!("x");
        b["provenance"]["omicron_revision"] = json!("y; omicron_revision=z");
        let inputs = [
            load_value("a.json", a).unwrap(),
            load_value("b.json", b).unwrap(),
        ];
        let analysis = analyze(&inputs);
        assert_ne!(analysis.cohorts[0].key, analysis.cohorts[1].key);
        assert_ne!(
            cohort_conditions(&analysis.cohorts[0].key),
            cohort_conditions(&analysis.cohorts[1].key)
        );
        let view = build_report_view(&inputs, &analysis, &[]).unwrap();
        let cohorts = &view.sections[0].cohorts;
        assert_eq!(cohorts.len(), 2);
        let a = cohorts
            .iter()
            .find(|cohort| cohort.candidates[0].label.starts_with("a —"))
            .unwrap();
        let b = cohorts
            .iter()
            .find(|cohort| cohort.candidates[0].label.starts_with("b —"))
            .unwrap();
        for cohort in [a, b] {
            assert_eq!(
                chart(cohort, ViewChartKind::LaunchDuration).option["xAxis"]["data"],
                json!([cohort.candidates[0].label])
            );
        }
        assert_eq!(
            chart(a, ViewChartKind::LaunchDuration).option["series"][0]["data"]
                [0][1],
            30
        );
        assert_eq!(
            chart(b, ViewChartKind::LaunchDuration).option["series"][0]["data"]
                [0][1],
            31
        );
    }

    #[test]
    fn run_aggregates_section_and_cohort_warnings_at_top_level() {
        let root = tempdir().unwrap();
        let input = root.path().join("legacy.json");
        write_json(&input, &matrix(2));
        let out = root.path().join("report");
        run(&[input], &out, false).unwrap();
        let report = read_run_report(&out);
        let warnings = report["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| {
            w.as_str().unwrap().contains("Historical storage inputs")
        }));
        assert!(warnings.iter().any(|w| {
            w.as_str().unwrap().contains("Legacy or incomplete provenance")
        }));
    }

    fn svg_chart(
        kind: ViewChartKind,
        rows: Vec<ChartFallbackRow>,
    ) -> ChartView {
        ChartView {
            kind,
            title: "A <chart> & its results".into(),
            unit: "MiB/s & more".into(),
            option: Value::Null,
            fallback_rows: rows,
        }
    }

    fn svg_row(
        category: &str,
        series: &str,
        value: Option<f64>,
    ) -> ChartFallbackRow {
        ChartFallbackRow {
            category: category.into(),
            series: series.into(),
            value,
        }
    }

    #[test]
    fn standalone_svg_covers_every_layout_and_fallback_value() {
        let cases = [
            svg_chart(
                ViewChartKind::GrossWrites,
                vec![
                    svg_row("alpha", "Sample 1", Some(1.25)),
                    svg_row("alpha", "Mean", Some(1.25)),
                    svg_row("beta", "Mean", None),
                ],
            ),
            svg_chart(
                ViewChartKind::Waterfall,
                vec![
                    svg_row("up", "Delta", Some(2.5)),
                    svg_row("down", "Delta", Some(-1.5)),
                ],
            ),
            svg_chart(
                ViewChartKind::Allocation,
                vec![
                    svg_row("candidate", "Required allocation", Some(1024.0)),
                    svg_row(
                        "candidate",
                        "Peak observed allocation",
                        Some(2048.0),
                    ),
                ],
            ),
            svg_chart(
                ViewChartKind::Capabilities,
                vec![
                    svg_row("rack / metrics", "Capability status", Some(1.0)),
                    svg_row("rack / api", "Capability status", Some(0.0)),
                    svg_row("rack / old", "Capability status", None),
                ],
            ),
        ];
        for chart in cases {
            let svg = render_chart_svg(&chart).unwrap();
            assert!(svg.starts_with("<svg "));
            assert!(
                svg.contains(
                    "<title>A &lt;chart&gt; &amp; its results</title>"
                )
            );
            assert!(svg.contains("<desc>"));
            assert!(svg.contains("MiB/s &amp; more"));
            assert!(svg.contains("fill=\"white\""));
            assert!(!svg.contains("<script"));
            assert!(!svg.contains("href="));
            for row in chart.fallback_rows {
                assert!(svg.contains(&html_escape(&row.category)));
                assert!(svg.contains(&html_escape(&row.series)));
                assert!(svg.contains(
                    &row.value.map_or_else(|| "unavailable".into(), svg_value)
                ));
            }
        }
    }

    #[test]
    fn svg_preserves_fractional_geometry_and_text() {
        let samples = svg_chart(
            ViewChartKind::PeakRam,
            vec![
                svg_row("small", "Mean", Some(1e-9)),
                svg_row("larger", "Mean", Some(2e-9)),
            ],
        );
        let svg = render_chart_svg(&samples).unwrap();
        assert!(svg.contains("cy=\"425.0\""));
        assert!(svg.contains("cy=\"120.0\""));
        assert!(svg.contains("1.000000e-9"));
        assert!(svg.contains("2.000000e-9"));
        assert!(!svg.contains(": 0.000000 MiB/s"));

        let allocation = svg_chart(
            ViewChartKind::Allocation,
            vec![
                svg_row("small", "Required allocation", Some(1e-9)),
                svg_row("larger", "Peak allocation", Some(2e-9)),
            ],
        );
        let svg = render_chart_svg(&allocation).unwrap();
        assert!(
            svg.contains("width=\"210.0\" height=\"14\" class=\"required\"")
        );
        assert!(svg.contains("width=\"420.0\" height=\"14\" class=\"peak\""));
    }

    #[test]
    fn svg_clips_exact_visible_strings_to_the_fixed_canvas() {
        let title = "t".repeat(MAX_SVG_LABEL_BYTES);
        let unit = "u".repeat(MAX_SVG_LABEL_BYTES);
        let category = "c".repeat(59);
        let series = "s".repeat(61);
        let chart = ChartView {
            kind: ViewChartKind::PeakRam,
            title: title.clone(),
            unit: unit.clone(),
            option: Value::Null,
            fallback_rows: vec![svg_row(&category, &series, Some(f64::MAX))],
        };
        let svg = render_chart_svg(&chart).unwrap();
        assert!(svg.contains(&format!("<title id=\"title\">{title}</title>")));
        assert!(svg.contains(&format!("Unit: {unit}.")));
        assert!(svg.contains("1.797693e308"));
        assert!(
            svg.contains(
                "width=\"1120.0\" height=\"28.0\" overflow=\"hidden\""
            )
        );
        assert!(
            svg.contains(
                "width=\"1120.0\" height=\"18.0\" overflow=\"hidden\""
            )
        );
        assert!(
            svg.contains("width=\"880.0\" height=\"18.0\" overflow=\"hidden\"")
        );
        assert!(
            svg.contains(&format!(
                "{category} — {series}: 1.797693e308 {unit}"
            ))
        );
    }

    #[test]
    fn missing_mean_breaks_line_continuity() {
        let chart = svg_chart(
            ViewChartKind::PeakRam,
            vec![
                svg_row("A", "Mean", Some(1.0)),
                svg_row("B", "Mean", None),
                svg_row("C", "Mean", Some(3.0)),
            ],
        );
        let svg = render_chart_svg(&chart).unwrap();
        assert_eq!(svg.matches("<polyline class=\"mean\"").count(), 2);
        assert!(!svg.contains("points=\"235.0,526.7 1135.0,120.0\""));
    }

    #[test]
    fn standalone_svg_escapes_hostile_text_and_marks_status_without_color() {
        let chart = svg_chart(
            ViewChartKind::Capabilities,
            vec![
                svg_row(
                    "</text><script>alert(1)</script>",
                    "x & y",
                    Some(-1.0),
                ),
                svg_row("missing", "status", None),
            ],
        );
        let svg = render_chart_svg(&chart).unwrap();
        assert!(
            svg.contains("&lt;/text&gt;&lt;script&gt;alert(1)&lt;/script&gt;")
        );
        assert!(!svg.contains("<script"));
        assert!(svg.contains("Unavailable"));
        assert!(svg.contains("? unavailable"));
        assert!(!svg.contains("file://"));
        assert!(!svg.contains("href="));
        assert!(!svg.contains("url("));
        assert!(!svg.contains("<image"));
        assert!(!svg.contains("<foreignObject"));
    }

    #[test]
    fn svg_collection_has_deterministic_structural_names_and_skips_empty_charts()
     {
        let chart = svg_chart(
            ViewChartKind::PeakRam,
            vec![svg_row("arbitrary label", "Mean", Some(7.0))],
        );
        let empty = svg_chart(
            ViewChartKind::PeakRam,
            vec![svg_row("never plotted", "Mean", None)],
        );
        let input = [
            (SvgChartIdentity::cohort(2, 3, 4), &chart),
            (SvgChartIdentity::aggregate(2, 5), &empty),
        ];
        let first = render_chart_svgs(&input).unwrap();
        let second = render_chart_svgs(&input).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].filename, "section-002-cohort-003-chart-004.svg");
        assert!(first[0].filename.bytes().all(
            |byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'-'
                || byte == b'.'
        ));
    }

    #[test]
    fn svg_collection_enforces_non_empty_count_and_total_bytes() {
        let chart = svg_chart(
            ViewChartKind::PeakRam,
            vec![svg_row("category", "Mean", Some(7.0))],
        );
        let limits = SvgLimits { artifacts: 2, ..SVG_LIMITS };
        let too_many = (0..=limits.artifacts)
            .map(|chart_index| {
                (SvgChartIdentity::aggregate(0, chart_index), &chart)
            })
            .collect::<Vec<_>>();
        assert!(
            render_chart_svgs_with_limits(&too_many, limits)
                .unwrap_err()
                .to_string()
                .contains("artifact count")
        );

        let one = render_chart_svg(&chart).unwrap().len();
        let total_limits =
            SvgLimits { artifacts: 2, total_bytes: one * 2 - 1, ..SVG_LIMITS };
        let enough = (0..2)
            .map(|chart_index| {
                (SvgChartIdentity::aggregate(0, chart_index), &chart)
            })
            .collect::<Vec<_>>();
        assert!(
            render_chart_svgs_with_limits(&enough, total_limits)
                .unwrap_err()
                .to_string()
                .contains("total byte")
        );
    }

    #[test]
    fn svg_projects_production_row_order_by_category() {
        let labels = vec!["first".to_string(), "second".to_string()];
        let sample_rows =
            sample_fallback_rows(&labels, &[vec![1.0, 3.0], vec![8.0]]);
        let sample_svg =
            render_chart_svg(&svg_chart(ViewChartKind::PeakRam, sample_rows))
                .unwrap();
        assert_eq!(sample_svg.matches("data-category-x=\"235.0\"").count(), 3);
        assert_eq!(sample_svg.matches("data-category-x=\"1135.0\"").count(), 2);

        let mut allocation_rows = series_fallback_rows(
            &labels,
            "Required allocation",
            [Some(1.0), None],
        );
        allocation_rows.extend(series_fallback_rows(
            &labels,
            "Peak allocation",
            [Some(2.0), Some(4.0)],
        ));
        let allocation_svg = render_chart_svg(&svg_chart(
            ViewChartKind::Allocation,
            allocation_rows,
        ))
        .unwrap();
        let first_required =
            allocation_svg.find("first — Required allocation").unwrap();
        let first_peak =
            allocation_svg.find("first — Peak allocation").unwrap();
        let second_required =
            allocation_svg.find("second — Required allocation").unwrap();
        assert!(first_required < first_peak && first_peak < second_required);
        assert_eq!(allocation_svg.matches("class=\"missing\"").count(), 1);
        assert!(!allocation_svg.contains("width=\"0.0\""));
    }

    #[test]
    fn svg_waterfall_uses_independent_zero_based_deltas() {
        let chart = svg_chart(
            ViewChartKind::Waterfall,
            vec![
                svg_row("positive one", "Delta", Some(5.0)),
                svg_row("positive two", "Delta", Some(5.0)),
                svg_row("negative one", "Delta", Some(-5.0)),
                svg_row("negative two", "Delta", Some(-5.0)),
            ],
        );
        let svg = render_chart_svg(&chart).unwrap();
        assert!(svg.contains("class=\"zero-line\""));
        assert_eq!(svg.matches("x=\"900.0\"").count(), 2);
        assert_eq!(svg.matches("x=\"690.0\"").count(), 2);
        assert!(!svg.contains("NaN"));
        assert!(!svg.contains("inf"));
    }

    #[test]
    fn svg_rejects_unrepresentable_ranges_layout_overflow_and_invalid_xml() {
        let huge = svg_chart(
            ViewChartKind::Waterfall,
            vec![
                svg_row("low", "Delta", Some(-f64::MAX)),
                svg_row("high", "Delta", Some(f64::MAX)),
            ],
        );
        assert!(render_chart_svg(&huge).is_err());
        let too_many = svg_chart(
            ViewChartKind::Capabilities,
            (0..=2)
                .map(|index| svg_row(&index.to_string(), "status", Some(1.0)))
                .collect(),
        );
        assert!(
            render_chart_svg_with_limits(
                &too_many,
                SvgLimits { rows: 2, ..SVG_LIMITS }
            )
            .is_err()
        );
        let combined_label = svg_chart(
            ViewChartKind::PeakRam,
            vec![svg_row(
                &"c".repeat(MAX_SVG_ROW_LABEL_BYTES / 2 + 1),
                &"s".repeat(MAX_SVG_ROW_LABEL_BYTES / 2 + 1),
                Some(1.0),
            )],
        );
        assert!(render_chart_svg(&combined_label).is_err());
        for invalid in ["nul\0text", "control\u{1}text", "noncharacter\u{fffe}"]
        {
            let chart = svg_chart(
                ViewChartKind::PeakRam,
                vec![svg_row(invalid, "Mean", Some(1.0))],
            );
            assert!(render_chart_svg(&chart).is_err());
        }
        assert!(check_svg_size(MAX_SVG_BYTES + 1, MAX_SVG_BYTES).is_err());
    }

    #[test]
    fn svg_limits_labels_counts_values_and_output_size() {
        let mut long = svg_chart(
            ViewChartKind::LaunchDuration,
            vec![svg_row(
                &"x".repeat(MAX_SVG_LABEL_BYTES + 1),
                "Mean",
                Some(1.0),
            )],
        );
        assert!(render_chart_svg(&long).is_err());
        long.fallback_rows = vec![
            svg_row("one", "Mean", Some(1.0)),
            svg_row("two", "Mean", Some(2.0)),
        ];
        assert!(
            render_chart_svg_with_limits(
                &long,
                SvgLimits { categories: 1, ..SVG_LIMITS }
            )
            .is_err()
        );
        long.fallback_rows =
            vec![svg_row("finite", "Mean", Some(f64::INFINITY))];
        assert!(render_chart_svg(&long).is_err());
        let normal = svg_chart(
            ViewChartKind::LaunchDuration,
            vec![svg_row("finite", "Mean", Some(1.0))],
        );
        let normal_size = render_chart_svg(&normal).unwrap().len();
        assert!(
            render_chart_svg_with_limits(
                &normal,
                SvgLimits { individual_bytes: normal_size - 1, ..SVG_LIMITS }
            )
            .is_err()
        );
    }

    #[test]
    fn svg_renders_45_fallback_samples_deterministically_in_bounds() {
        let rows = (0..45)
            .map(|index| {
                svg_row(
                    "retained",
                    &format!("Sample {index}"),
                    Some(index as f64 + 0.25),
                )
            })
            .collect::<Vec<_>>();
        let chart = svg_chart(ViewChartKind::PeakRam, rows.clone());

        let first = render_chart_svg(&chart).unwrap();
        let second = render_chart_svg(&chart).unwrap();

        assert_eq!(first, second);
        assert!(first.len() <= MAX_SVG_BYTES);
        assert_eq!(first.matches("class=\"sample\"").count(), rows.len());
        for row in rows {
            assert!(first.contains(&html_escape(&row.series)));
            assert!(first.contains(&svg_value(row.value.unwrap())));
        }
    }
}
