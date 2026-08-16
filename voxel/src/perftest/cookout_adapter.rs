//! Translation from Voxel's durable matrix formats to Cookout evidence.
//!
//! This module deliberately contains no collection or execution logic. It carries
//! durable report facts into Cookout's typed context while removing credential
//! fields from effective-configuration presentation.

use super::*;
use cookout::adapter::{
    AdaptedExperiment, Adapter, AdapterIssue, build_evidence,
};
use cookout::model::{
    Aggregation, ApplicationIdentity,
    CapabilityResult as CookoutCapabilityResult,
    CapabilityStatus as CookoutCapabilityStatus, Constraint, DimensionValue,
    EvidenceEnvelope, ExperimentDocument, ExperimentIdentity, ExperimentKind,
    FailureRecord, HardwareDimensions, IssueImpact, IssueScope, Observation,
    ObservationValue, ObservationWindow, OptimizationDirection, PhaseKind,
    PhaseResult, PhaseStatus, Provenance, Run, RunOutcome, Scenario, Target,
    Unit, Variant,
};
use serde::de::DeserializeOwned;
use std::path::Path;

const STORAGE_COHORT_DIMENSION: &str = "oxide.voxel.storage_cohort";
const STORAGE_COHORT_VERSION: &str = "v1";
const STORAGE_COHORT_DOMAIN: &[u8] = b"oxide.voxel.storage_cohort\0v1\0";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source_schema", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RetainedMatrixSource {
    MatrixRun { source: Value, source_display: String },
    MatrixCheckpoint { source: Value, source_display: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum VoxelIssueClassification {
    StageFailure { stage: String, repeat: u64 },
    MissingMeasurement { stage: String, metric: String, repeat: u64 },
    Reproducibility,
    Pending { repeat: u64 },
    PlannedRepeatShortfall { planned: u64, observed: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VoxelCohortParameters {
    storage_cohort: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VoxelCandidateParameters {
    label: String,
    levers: BTreeSet<u8>,
    effective_config: Value,
}

pub(super) struct VoxelCookoutAdapter;

const NORMALIZATION_VERSION: u32 = 2;

#[derive(Serialize)]
struct VoxelCondition {
    label: String,
    value: String,
    code: bool,
}

#[derive(Serialize)]
struct VoxelRunContext {
    display_id: String,
    workload_disposition: &'static str,
    launch_failure: Option<String>,
    preparation_failure: Option<String>,
    workload_failure: Option<String>,
    boundary_failure: Option<String>,
    prior_launch_attempt_failures: Vec<String>,
    launch_memory_semantics: Option<&'static str>,
    workload_memory_semantics: Option<&'static str>,
}

impl Adapter for VoxelCookoutAdapter {
    const ID: &'static str = "oxide.voxel.perftest";
    const SOURCE_SCHEMA_VERSION: u32 = 1;
    const NORMALIZATION_VERSION: u32 = NORMALIZATION_VERSION;
    const ISSUE_CLASSIFICATION_SCHEMA: &'static str =
        "oxide.voxel.perftest.issue.v1";

    type Source = RetainedMatrixSource;
    type Cohort = VoxelCohortParameters;
    type Candidate = VoxelCandidateParameters;
    type IssueClassification = VoxelIssueClassification;

    fn normalize(
        &self,
        _: u32,
        source: &Self::Source,
    ) -> cookout::Result<
        AdaptedExperiment<
            VoxelCohortParameters,
            VoxelCandidateParameters,
            VoxelIssueClassification,
        >,
    > {
        normalize_retained_source(source).map_err(|error| {
            cookout::Error::InvalidEvidence(format!("Voxel adapter: {error:#}"))
        })
    }
}

pub(super) fn matrix_run_to_evidence(
    run: &MatrixRun,
    source_path: &Path,
) -> Result<EvidenceEnvelope> {
    validate_publishable_matrix_run(run)?;
    build_typed_evidence(RetainedMatrixSource::MatrixRun {
        source: retained_value(run)?,
        source_display: stable_source_display(source_path),
    })
}

pub(super) fn matrix_checkpoint_to_evidence(
    checkpoint: &MatrixCheckpoint,
    source_path: &Path,
) -> Result<EvidenceEnvelope> {
    validate_matrix_checkpoint(checkpoint)?;
    build_typed_evidence(RetainedMatrixSource::MatrixCheckpoint {
        source: retained_value(checkpoint)?,
        source_display: stable_source_display(source_path),
    })
}

fn build_typed_evidence(
    source: RetainedMatrixSource,
) -> Result<EvidenceEnvelope> {
    build_evidence(&VoxelCookoutAdapter, &source, &cookout::Limits::default())
        .context("build typed Cookout evidence")
}

fn normalize_retained_source(
    source: &RetainedMatrixSource,
) -> Result<
    AdaptedExperiment<
        VoxelCohortParameters,
        VoxelCandidateParameters,
        VoxelIssueClassification,
    >,
> {
    let (snapshot, candidate_values) = match source {
        RetainedMatrixSource::MatrixRun { source, source_display } => {
            let run: MatrixRun = decode_retained(source)?;
            let snapshot =
                matrix_run_to_experiment(&run, Path::new(source_display))?;
            let values = run
                .results
                .iter()
                .map(|combo| {
                    let config = run
                        .report_evidence
                        .as_ref()
                        .and_then(|e| {
                            e.combos
                                .iter()
                                .find(|item| item.label == combo.label)
                        })
                        .map(|item| retained_value(&item.effective_config))
                        .transpose()?
                        .unwrap_or(Value::Null);
                    Ok((
                        variant_id(&combo.label),
                        VoxelCandidateParameters {
                            label: combo.label.clone(),
                            levers: combo.levers.clone(),
                            effective_config: config,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            (snapshot, values)
        }
        RetainedMatrixSource::MatrixCheckpoint { source, source_display } => {
            let checkpoint: MatrixCheckpoint = decode_retained(source)?;
            let snapshot = matrix_checkpoint_to_experiment(
                &checkpoint,
                Path::new(source_display),
            )?;
            let values = checkpoint
                .combos
                .iter()
                .map(|combo| {
                    Ok((
                        variant_id(&combo.label),
                        VoxelCandidateParameters {
                            label: combo.label.clone(),
                            levers: combo.levers.clone(),
                            effective_config: retained_value(
                                &combo.effective_config,
                            )?,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            (snapshot, values)
        }
    };
    let cohort = VoxelCohortParameters {
        storage_cohort: snapshot
            .target
            .dimensions
            .namespaced
            .get(STORAGE_COHORT_DIMENSION)
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null),
    };
    let issues = adapter_issues(&snapshot);
    Ok(AdaptedExperiment {
        snapshot,
        cohort,
        candidates: candidate_values,
        issues,
    })
}

fn retained_value(value: &impl Serialize) -> Result<Value> {
    let mut value = serde_json::to_value(value)?;
    sanitize_retained_json(&mut value);
    Ok(value)
}

fn sanitize_retained_json(value: &mut Value) {
    match value {
        Value::Object(values) => {
            values.retain(|key, _| !sensitive_key(key));
            values.values_mut().for_each(sanitize_retained_json);
        }
        Value::Array(values) => {
            values.iter_mut().for_each(sanitize_retained_json)
        }
        Value::String(text) if diagnostic_is_sensitive(text) => {
            *text = "detail was redacted".into()
        }
        _ => {}
    }
}

fn decode_retained<T: DeserializeOwned>(value: &Value) -> Result<T> {
    let mut value = value.clone();
    restore_redacted_config_fields(&mut value);
    serde_json::from_value(value).context("decode retained Voxel source")
}

fn restore_redacted_config_fields(value: &mut Value) {
    match value {
        Value::Object(values) => {
            if let Some(Value::Object(recovery)) =
                values.get_mut("recovery_silo")
            {
                recovery.insert(
                    "user_password_hash".into(),
                    Value::String(REDACTED_CREDENTIAL.into()),
                );
            }
            values.values_mut().for_each(restore_redacted_config_fields);
        }
        Value::Array(values) => {
            values.iter_mut().for_each(restore_redacted_config_fields)
        }
        _ => {}
    }
}

fn adapter_issues(
    snapshot: &ExperimentDocument,
) -> Vec<AdapterIssue<VoxelIssueClassification>> {
    let experiment_id = snapshot.identity.experiment_id.clone();
    let mut issues = Vec::new();
    let tolerated_candidates = snapshot
        .variants
        .iter()
        .map(|variant| {
            let runs = snapshot
                .runs
                .iter()
                .filter(|run| run.variant_id == variant.id)
                .collect::<Vec<_>>();
            let planned = variant.planned_runs.unwrap_or(0);
            let successful = runs
                .iter()
                .filter(|run| run.outcome == RunOutcome::Completed)
                .count() as u64;
            let clean_boundaries = runs.iter().all(|run| {
                run.extensions["oxide.voxel"]["boundary_failure"].is_null()
                    && ["pre-boundary", "cleanup"].into_iter().all(|name| {
                        run.phases.iter().any(|phase| {
                            phase.name == name
                                && phase.status == PhaseStatus::Completed
                        })
                    })
            });
            (
                variant.id.as_str(),
                planned != 0
                    && successful.saturating_mul(100)
                        >= planned.saturating_mul(80)
                    && clean_boundaries,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for run in &snapshot.runs {
        let failed_stages = run
            .phases
            .iter()
            .filter(|phase| phase.status == PhaseStatus::Failed)
            .collect::<Vec<_>>();
        for phase in &failed_stages {
            issues.push(AdapterIssue {
                code: "oxide.voxel.perftest.stage_failure".into(),
                title: format!("{} failed", human_key(&phase.name)),
                description: format!(
                    "Candidate {} repeat {} failed during stage {}.",
                    run.variant_id, run.attempt, phase.name
                ),
                scope: IssueScope::Run {
                    experiment_id: experiment_id.clone(),
                    variant_id: run.variant_id.clone(),
                    run_id: run.id.clone(),
                },
                impact: if tolerated_candidates
                    .get(run.variant_id.as_str())
                    .copied()
                    .unwrap_or(false)
                    && matches!(
                        phase.name.as_str(),
                        "launch" | "preparation" | "workload"
                    ) {
                    IssueImpact::Diagnostic
                } else {
                    IssueImpact::Blocking
                },
                classification: Some(VoxelIssueClassification::StageFailure {
                    stage: phase.name.clone(),
                    repeat: run.attempt,
                }),
            });
        }
        if run
            .phases
            .iter()
            .any(|phase| phase.status == PhaseStatus::Incomplete)
        {
            issues.push(AdapterIssue {
                code: "oxide.voxel.perftest.repeat_pending".into(),
                title: "Repeat is pending".into(),
                description: format!(
                    "Candidate {} repeat {} has not reached a terminal state.",
                    run.variant_id, run.attempt
                ),
                scope: IssueScope::Run {
                    experiment_id: experiment_id.clone(),
                    variant_id: run.variant_id.clone(),
                    run_id: run.id.clone(),
                },
                impact: IssueImpact::Blocking,
                classification: Some(VoxelIssueClassification::Pending {
                    repeat: run.attempt,
                }),
            });
        }
        if failed_stages.is_empty() {
            for phase in run
                .phases
                .iter()
                .filter(|phase| phase.status == PhaseStatus::Completed)
            {
                let required: &[&str] = match phase.name.as_str() {
                    "launch" => &[
                        "launch.bytes_written",
                        "launch.duration",
                        "launch.peak_ram_delta",
                    ],
                    "workload" => &[
                        "workload.bytes_written",
                        "workload.duration",
                        "workload.peak_ram_delta",
                    ],
                    _ => &[],
                };
                for metric in required.iter().filter(|metric| {
                    !phase
                        .observations
                        .iter()
                        .any(|observation| observation.metric == **metric)
                }) {
                    issues.push(AdapterIssue {
                        code: "oxide.voxel.perftest.missing_measurement".into(),
                        title: format!("Missing measurement: {metric}"),
                        description: format!(
                            "Candidate {} repeat {} completed stage {} without required measurement {metric}.",
                            run.variant_id, run.attempt, phase.name
                        ),
                        scope: IssueScope::Measurement {
                            experiment_id: experiment_id.clone(),
                            variant_id: run.variant_id.clone(),
                            run_id: run.id.clone(),
                            phase: phase.name.clone(),
                            metric: (*metric).into(),
                        },
                        impact: IssueImpact::Blocking,
                        classification: Some(VoxelIssueClassification::MissingMeasurement {
                            stage: phase.name.clone(),
                            metric: (*metric).into(),
                            repeat: run.attempt,
                        }),
                    });
                }
            }
        }
    }
    if snapshot.extensions.contains_key("oxide.voxel") {
        for variant in &snapshot.variants {
            let observed = snapshot
                .runs
                .iter()
                .filter(|run| run.variant_id == variant.id)
                .count() as u64;
            let planned = variant.planned_runs.unwrap_or(0);
            if observed < planned {
                issues.push(AdapterIssue {
                    code: "oxide.voxel.perftest.planned_repeat_shortfall".into(),
                    title: "Planned repeats are incomplete".into(),
                    description: format!(
                        "Candidate {} has {observed} of {planned} planned repeats.",
                        variant.id
                    ),
                    scope: IssueScope::Candidate {
                        experiment_id: experiment_id.clone(),
                        variant_id: variant.id.clone(),
                    },
                    impact: IssueImpact::Blocking,
                    classification: Some(VoxelIssueClassification::PlannedRepeatShortfall {
                        planned,
                        observed,
                    }),
                });
            }
        }
        if snapshot.extensions["oxide.voxel"]["provenance_state"] != "complete"
        {
            issues.push(AdapterIssue {
                code: "oxide.voxel.perftest.reproducibility_incomplete".into(),
                title: "Reproducibility evidence is incomplete".into(),
                description: "One or more reproducibility facts were unavailable in the retained matrix evidence.".into(),
                scope: IssueScope::Experiment { experiment_id },
                impact: IssueImpact::Diagnostic,
                classification: Some(VoxelIssueClassification::Reproducibility),
            });
        }
    }
    issues
}

#[derive(Serialize)]
struct StorageCohortIdentity<'a> {
    rss_sleds: usize,
    combinations: Vec<&'a str>,
    workload: Option<&'a WorkloadSpec>,
    oxide_session: Option<&'a OxideSessionMetadata>,
    effective_candidate_configurations:
        Option<BTreeMap<&'a str, &'a VoxelConfig>>,
    capability_contract_version: Option<u32>,
    launch_memory_semantics: &'static str,
    workload_memory_semantics: Option<&'static str>,
    provenance: StorageCohortProvenance<'a>,
}

#[derive(Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
enum StorageCohortProvenance<'a> {
    Available {
        voxel_build: &'a str,
        voxel_binary: &'a str,
        configured_image: &'a str,
        omicron_commit: &'a str,
        host: &'a str,
    },
    Unavailable {
        source: String,
        run_id: &'a str,
    },
}

pub(super) fn matrix_run_to_experiment(
    run: &MatrixRun,
    source_path: &Path,
) -> Result<ExperimentDocument> {
    let variants = run
        .results
        .iter()
        .map(|combo| {
            let effective_config =
                run.report_evidence.as_ref().and_then(|evidence| {
                    evidence
                        .combos
                        .iter()
                        .find(|evidence| evidence.label == combo.label)
                        .map(|evidence| &evidence.effective_config)
                });
            variant(
                &combo.label,
                &combo.levers,
                run.kind,
                run.repeat,
                effective_config,
            )
        })
        .collect();
    let mut runs = Vec::new();
    for combo in &run.results {
        for (index, sample) in combo.repeats.iter().enumerate() {
            runs.push(aggregate_run(
                combo,
                index,
                sample,
                run.workload.is_some(),
                &run.name,
                "launch-baseline-delta",
                run.workload.as_ref().map(|_| "workload-baseline-delta"),
            ));
        }
        if combo.error.is_some() {
            runs.push(failed_aggregate_run(
                combo,
                combo.repeats.len(),
                run.workload.is_some(),
                &run.name,
            ));
        }
    }
    let failed = run.results.iter().any(|combo| combo.error.is_some());
    build_document(BuildDocumentArgs {
        kind: run.kind,
        source_name: &run.name,
        started: run.started,
        rss_sleds: run.rss_sleds,
        workload: run.workload.as_ref(),
        evidence: run.report_evidence.as_ref(),
        variants,
        runs,
        source: serde_json::to_vec(run)?,
        session: run.oxide_session.as_ref(),
        execution_state: if failed { "failed" } else { "completed" },
        abort_error: failed.then(|| {
            "matrix execution stopped before all planned repeats completed"
                .into()
        }),
        planned_repeats: run.repeat.saturating_mul(run.results.len()),
        source_path,
        launch_memory_semantics: "launch-baseline-delta",
        workload_memory_semantics: run
            .workload
            .as_ref()
            .map(|_| "workload-baseline-delta"),
    })
}

pub(super) fn matrix_checkpoint_to_experiment(
    checkpoint: &MatrixCheckpoint,
    source_path: &Path,
) -> Result<ExperimentDocument> {
    let variants = checkpoint
        .combos
        .iter()
        .map(|combo| {
            variant(
                &combo.label,
                &combo.levers,
                checkpoint.kind,
                checkpoint.repeat,
                Some(&combo.effective_config),
            )
        })
        .collect();
    let runs = checkpoint
        .combos
        .iter()
        .flat_map(|combo| {
            combo.repeats.iter().map(|repeat| {
                checkpoint_run(&combo.label, repeat, &checkpoint.name)
            })
        })
        .collect::<Vec<_>>();
    build_document(BuildDocumentArgs {
        kind: checkpoint.kind,
        source_name: &checkpoint.name,
        started: checkpoint.started,
        rss_sleds: checkpoint.rss_sleds,
        workload: checkpoint.workload.as_ref(),
        evidence: checkpoint.report_evidence.as_ref(),
        variants,
        runs,
        source: serde_json::to_vec(checkpoint)?,
        session: checkpoint.oxide_session.as_ref(),
        execution_state: execution_state(checkpoint.status),
        abort_error: checkpoint.abort_error.clone(),
        planned_repeats: checkpoint
            .repeat
            .saturating_mul(checkpoint.combos.len()),
        source_path,
        launch_memory_semantics: "launch-baseline-delta",
        workload_memory_semantics: checkpoint
            .workload
            .as_ref()
            .map(|_| "workload-baseline-delta"),
    })
}

struct BuildDocumentArgs<'a> {
    kind: MatrixKind,
    source_name: &'a str,
    started: u64,
    rss_sleds: usize,
    workload: Option<&'a WorkloadSpec>,
    evidence: Option<&'a MatrixReportEvidence>,
    variants: Vec<Variant>,
    runs: Vec<Run>,
    source: Vec<u8>,
    session: Option<&'a OxideSessionMetadata>,
    execution_state: &'static str,
    abort_error: Option<String>,
    planned_repeats: usize,
    source_path: &'a Path,
    launch_memory_semantics: &'static str,
    workload_memory_semantics: Option<&'static str>,
}

fn build_document(args: BuildDocumentArgs<'_>) -> Result<ExperimentDocument> {
    let BuildDocumentArgs {
        kind,
        source_name,
        started,
        rss_sleds,
        workload,
        evidence,
        variants,
        runs,
        source,
        session,
        execution_state,
        abort_error,
        planned_repeats,
        source_path,
        launch_memory_semantics,
        workload_memory_semantics,
    } = args;
    let digest = hex_digest(&source);
    let digest_prefix = digest
        .strip_prefix("sha256:")
        .expect("hex_digest uses the sha256 scheme")
        .get(..12)
        .expect("SHA-256 digest has a 12-character prefix");
    let constraints = target_constraint_capability_names()
        .into_iter()
        .map(|capability| Constraint::Capability {
            capability: capability.into(),
            required: true,
        })
        .collect();
    let mut target_extensions = BTreeMap::new();
    target_extensions.insert(
        "oxide.voxel".into(),
        json!({
            "capabilities": evidence.map(|evidence| capability_states(&evidence.capabilities)),
            "session": session.map(|session| json!({
                "profile": session.profile,
                "cli_version": session.oxide_cli_version,
                "provider": match session.provider { OxideAuthProviderMetadata::Builtin => "builtin", OxideAuthProviderMetadata::Helper { .. } => "helper" },
            })),
        }),
    );
    let mut namespaced = BTreeMap::new();
    namespaced.insert(
        "oxide.rss_sleds".into(),
        DimensionValue::Count(rss_sleds as u64),
    );
    let cohort_args = StorageCohortArgs {
        rss_sleds,
        variants: &variants,
        workload,
        session,
        evidence,
        source_path,
        run_id: source_name,
        launch_memory_semantics,
        workload_memory_semantics,
    };
    if kind == MatrixKind::Storage {
        namespaced.insert(
            STORAGE_COHORT_DIMENSION.into(),
            DimensionValue::Text(storage_cohort_dimension(&cohort_args)?),
        );
    }
    let conditions = experiment_conditions(&cohort_args)?;
    let variant_kind = match kind {
        MatrixKind::Storage => "storage-tuning",
        MatrixKind::Topology => "topology",
    };
    let workload_clause = workload
        .map(|_| " and API disk lifecycle workloads")
        .unwrap_or_default();
    let document = ExperimentDocument {
        identity: ExperimentIdentity {
            experiment_id: format!("voxel-matrix-{started}-{digest_prefix}"),
            name: "Voxel performance report".into(),
            description: Some(format!(
                "Compares host NVMe writes, launch duration, and peak memory across Voxel \
                 {variant_kind} variants during simulated Oxide rack launch{workload_clause}."
            )),
            created_at: started.to_string(),
            application: ApplicationIdentity {
                id: "oxide.voxel".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            kind: ExperimentKind::Benchmark,
            campaign: None,
        },
        scenario: Scenario {
            name: workload
                .map(|_| "rack launch and API disk lifecycle")
                .unwrap_or("rack launch")
                .into(),
            requested_concurrency: workload.map(|value| value.parallel as u64),
            offered_request_rate: None,
            operation_mix: BTreeMap::new(),
            data_volume_bytes: workload
                .map(|value| value.size_bytes * value.count as u64),
            duration_seconds: None,
            ramp: None,
            phase_boundaries: Vec::new(),
            extensions: BTreeMap::new(),
        },
        target: Target {
            name: "Oxide rack".into(),
            application_version: "unknown".into(),
            topology: format!("{rss_sleds} RSS sleds"),
            environment: "Helios".into(),
            configuration: BTreeMap::new(),
            dimensions: HardwareDimensions { namespaced, ..Default::default() },
            constraints,
            extensions: target_extensions,
        },
        variants,
        runs,
        provenance: Provenance {
            producer: "oxide.voxel".into(),
            producer_version: env!("CARGO_PKG_VERSION").into(),
            invocation: match kind {
                MatrixKind::Storage => "voxel perftest matrix",
                MatrixKind::Topology => "voxel perftest topology-matrix",
            }
            .into(),
            source_digest: digest,
            source_revision: None,
            generated_at: None,
            attributes: BTreeMap::from([(
                "source_format".into(),
                "voxel matrix".into(),
            )]),
        },
        capabilities: evidence
            .map(|evidence| capability_ledger(&evidence.capabilities))
            .unwrap_or_default(),
        extensions: BTreeMap::from([(
            "oxide.voxel".into(),
            json!({
                "adapter_version": VoxelCookoutAdapter::NORMALIZATION_VERSION,
                "matrix_kind": match kind { MatrixKind::Storage => "storage", MatrixKind::Topology => "topology" },
                "execution_state": execution_state,
                "abort_error": abort_error.as_deref().map(|value| safe_diagnostic(value, "matrix abort detail was redacted")),
                "planned_repeats": planned_repeats,
                "workload": workload.map(workload_summary),
                "provenance_state": provenance_state(evidence),
                "source_display": stable_source_display(source_path),
                "conditions": conditions,
            }),
        )]),
    };
    Ok(document)
}

struct StorageCohortArgs<'a> {
    rss_sleds: usize,
    variants: &'a [Variant],
    workload: Option<&'a WorkloadSpec>,
    session: Option<&'a OxideSessionMetadata>,
    evidence: Option<&'a MatrixReportEvidence>,
    source_path: &'a Path,
    run_id: &'a str,
    launch_memory_semantics: &'static str,
    workload_memory_semantics: Option<&'static str>,
}

fn storage_cohort_dimension(args: &StorageCohortArgs<'_>) -> Result<String> {
    let StorageCohortArgs {
        rss_sleds,
        variants,
        workload,
        session,
        evidence,
        source_path,
        run_id,
        launch_memory_semantics,
        workload_memory_semantics,
    } = args;
    let effective_candidate_configurations = evidence.map(|evidence| {
        evidence
            .combos
            .iter()
            .map(|combo| (combo.label.as_str(), &combo.effective_config))
            .collect()
    });
    let provenance = evidence
        .and_then(|evidence| {
            let provenance = &evidence.provenance;
            Some(StorageCohortProvenance::Available {
                voxel_build: comparable_provenance_evidence(
                    &provenance.voxel_build,
                )?,
                voxel_binary: comparable_provenance_evidence(
                    &provenance.voxel_binary,
                )?,
                configured_image: comparable_provenance_evidence(
                    &provenance.configured_image,
                )?,
                omicron_commit: comparable_provenance_evidence(
                    &provenance.omicron_commit,
                )?,
                host: comparable_provenance_evidence(&provenance.host)?,
            })
        })
        .unwrap_or_else(|| StorageCohortProvenance::Unavailable {
            source: source_path.to_string_lossy().into_owned(),
            run_id,
        });
    let identity = StorageCohortIdentity {
        rss_sleds: *rss_sleds,
        combinations: variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect(),
        workload: *workload,
        oxide_session: *session,
        effective_candidate_configurations,
        capability_contract_version: evidence
            .map(|evidence| evidence.evidence_version),
        launch_memory_semantics,
        workload_memory_semantics: *workload_memory_semantics,
        provenance,
    };
    let canonical = serde_json::to_vec(&identity)
        .context("serialize Voxel storage cohort identity")?;
    let mut hasher = Sha256::new();
    hasher.update(STORAGE_COHORT_DOMAIN);
    hasher.update(canonical);
    Ok(format!("{STORAGE_COHORT_VERSION}:sha256:{:x}", hasher.finalize()))
}

fn variant(
    label: &str,
    levers: &BTreeSet<u8>,
    kind: MatrixKind,
    expected_repeats: usize,
    effective_config: Option<&VoxelConfig>,
) -> Variant {
    let kind_name = match kind {
        MatrixKind::Storage => "storage",
        MatrixKind::Topology => "topology",
    };
    let lever_label = if levers.is_empty() {
        "none".to_owned()
    } else {
        levers.iter().map(u8::to_string).collect::<Vec<_>>().join("+")
    };
    Variant {
        id: variant_id(label),
        name: label.into(),
        dimensions: BTreeMap::from([
            (
                "oxide.voxel.matrix_kind".into(),
                DimensionValue::Text(kind_name.into()),
            ),
            ("oxide.voxel.levers".into(), DimensionValue::Text(lever_label)),
        ]),
        planned_runs: Some(expected_repeats as u64),
        extensions: BTreeMap::from([(
            "oxide.voxel".into(),
            json!({"effective_configuration": effective_config.map(sanitized_config_value)}),
        )]),
    }
}

fn sanitized_config_value(config: &VoxelConfig) -> Value {
    let mut value =
        serde_json::to_value(config).expect("Voxel configuration serializes");
    sanitize_retained_json(&mut value);
    value
}

fn variant_id(label: &str) -> String {
    format!("levers-{}", label.replace('+', "-"))
}

fn base_run(
    label: &str,
    index: usize,
    phases: Vec<PhaseResult>,
    failure_message: Option<String>,
    report_context: VoxelRunContext,
) -> Run {
    let outcome = if phases
        .iter()
        .any(|phase| phase.status == PhaseStatus::Failed)
    {
        RunOutcome::Failed
    } else if phases.iter().any(|phase| phase.status == PhaseStatus::Incomplete)
    {
        RunOutcome::Incomplete
    } else {
        RunOutcome::Completed
    };
    Run {
        id: format!("{}-repeat-{}", variant_id(label), index + 1),
        variant_id: variant_id(label),
        attempt: (index + 1) as u64,
        outcome,
        failure: failure_message.map(|message| FailureRecord {
            code: "voxel_phase_failed".into(),
            message,
        }),
        guardrail: None,
        phases,
        extensions: BTreeMap::from([(
            "oxide.voxel".into(),
            serde_json::to_value(report_context)
                .expect("run context serializes"),
        )]),
    }
}

fn aggregate_run(
    combo: &ComboAggregate,
    index: usize,
    sample: &RepeatSample,
    workload_requested: bool,
    display_id: &str,
    launch_memory_semantics: &'static str,
    workload_memory_semantics: Option<&'static str>,
) -> Run {
    let launch_duration = sample.launch_secs as f64;
    let mut launch =
        completed_phase("launch", PhaseKind::Setup, Some(launch_duration));
    launch.observations = vec![
        observation(
            "launch.bytes_written",
            Unit::Bytes,
            sample.bringup_bytes,
            launch_duration,
        ),
        observation(
            "launch.duration",
            Unit::Seconds,
            sample.launch_secs,
            launch_duration,
        ),
    ];
    if let Some(value) = sample.peak_ram_bytes {
        launch.observations.push(observation(
            "launch.peak_ram_delta",
            Unit::Bytes,
            value,
            launch_duration,
        ));
    }
    let workload = match (
        sample.workload_bytes,
        sample.workload_secs,
        sample.workload_peak_delta_bytes,
    ) {
        (Some(bytes), Some(seconds), Some(ram)) => {
            let duration = seconds as f64;
            let mut phase = completed_phase(
                "workload",
                PhaseKind::Workload,
                Some(duration),
            );
            phase.observations = vec![
                observation(
                    "workload.bytes_written",
                    Unit::Bytes,
                    bytes,
                    duration,
                ),
                observation(
                    "workload.duration",
                    Unit::Seconds,
                    seconds,
                    duration,
                ),
                observation(
                    "workload.peak_ram_delta",
                    Unit::Bytes,
                    ram,
                    duration,
                ),
            ];
            phase
        }
        _ if workload_requested => {
            incomplete_phase("workload", PhaseKind::Workload)
        }
        _ => not_executed_phase("workload", PhaseKind::Workload),
    };
    base_run(
        &combo.label,
        index,
        vec![
            completed_phase("pre-boundary", PhaseKind::Setup, Some(0.0)),
            launch,
            if workload_requested {
                completed_phase("preparation", PhaseKind::WarmUp, Some(0.0))
            } else {
                not_executed_phase("preparation", PhaseKind::WarmUp)
            },
            workload,
            completed_phase("cleanup", PhaseKind::Cleanup, Some(0.0)),
        ],
        None,
        VoxelRunContext {
            display_id: display_id.into(),
            workload_disposition: if workload_requested {
                if sample.workload_bytes.is_some()
                    && sample.workload_secs.is_some()
                    && sample.workload_peak_delta_bytes.is_some()
                {
                    "succeeded"
                } else {
                    "pending"
                }
            } else {
                "not_requested"
            },
            launch_failure: None,
            preparation_failure: None,
            workload_failure: None,
            boundary_failure: None,
            prior_launch_attempt_failures: Vec::new(),
            launch_memory_semantics: sample
                .peak_ram_bytes
                .map(|_| launch_memory_semantics),
            workload_memory_semantics: sample
                .workload_peak_delta_bytes
                .and(workload_memory_semantics),
        },
    )
}

fn failed_aggregate_run(
    combo: &ComboAggregate,
    index: usize,
    workload_requested: bool,
    display_id: &str,
) -> Run {
    let failure = safe_diagnostic(
        combo
            .error
            .as_deref()
            .unwrap_or("matrix candidate failed without a completed repeat"),
        "matrix candidate failure detail was redacted",
    );
    let mut run = base_run(
        &combo.label,
        index,
        vec![
            incomplete_phase("pre-boundary", PhaseKind::Setup),
            incomplete_phase("launch", PhaseKind::Setup),
            not_executed_phase("preparation", PhaseKind::WarmUp),
            not_executed_phase("workload", PhaseKind::Workload),
            not_executed_phase("cleanup", PhaseKind::Cleanup),
        ],
        Some(failure.clone()),
        VoxelRunContext {
            display_id: display_id.into(),
            workload_disposition: if workload_requested {
                "blocked"
            } else {
                "not_requested"
            },
            launch_failure: None,
            preparation_failure: None,
            workload_failure: None,
            boundary_failure: None,
            prior_launch_attempt_failures: Vec::new(),
            launch_memory_semantics: None,
            workload_memory_semantics: None,
        },
    );
    run.outcome = RunOutcome::Failed;
    run
}

fn checkpoint_run(
    label: &str,
    repeat: &MatrixCheckpointRepeat,
    display_id: &str,
) -> Run {
    let pre_boundary =
        boundary_phase("pre-boundary", PhaseKind::Setup, &repeat.pre_boundary);
    let launch = match &repeat.launch {
        LaunchOutcome::Pending {} => {
            incomplete_phase("launch", PhaseKind::Setup)
        }
        LaunchOutcome::Failure { .. } => {
            failed_phase("launch", PhaseKind::Setup)
        }
        LaunchOutcome::Success { metrics, .. } => {
            let duration = metrics.launch_secs as f64;
            let mut phase =
                completed_phase("launch", PhaseKind::Setup, Some(duration));
            phase.observations = vec![
                observation(
                    "launch.bytes_written",
                    Unit::Bytes,
                    metrics.bringup_bytes,
                    duration,
                ),
                observation(
                    "launch.duration",
                    Unit::Seconds,
                    metrics.launch_secs,
                    duration,
                ),
                observation(
                    "launch.peak_ram_delta",
                    Unit::Bytes,
                    metrics.peak_ram_bytes,
                    duration,
                ),
            ];
            phase
        }
    };
    let preparation = simple_outcome_phase(
        "preparation",
        PhaseKind::WarmUp,
        &repeat.preparation,
    );
    let workload = match &repeat.workload {
        WorkloadOutcome::Pending {} => {
            incomplete_phase("workload", PhaseKind::Workload)
        }
        WorkloadOutcome::NotRequested {} => {
            not_executed_phase("workload", PhaseKind::Workload)
        }
        WorkloadOutcome::Failure { .. } => {
            failed_phase("workload", PhaseKind::Workload)
        }
        WorkloadOutcome::Success { metrics } => {
            let duration = metrics.workload_secs as f64;
            let mut phase = completed_phase(
                "workload",
                PhaseKind::Workload,
                Some(duration),
            );
            phase.observations = vec![
                observation(
                    "workload.bytes_written",
                    Unit::Bytes,
                    metrics.workload_bytes,
                    duration,
                ),
                observation(
                    "workload.duration",
                    Unit::Seconds,
                    metrics.workload_secs,
                    duration,
                ),
                observation(
                    "workload.peak_ram_delta",
                    Unit::Bytes,
                    metrics.workload_peak_delta_bytes,
                    duration,
                ),
            ];
            phase
        }
    };
    let context = checkpoint_run_context(repeat, display_id);
    let failure = context
        .boundary_failure
        .clone()
        .or_else(|| context.preparation_failure.clone())
        .or_else(|| context.workload_failure.clone())
        .or_else(|| context.launch_failure.clone());
    base_run(
        label,
        repeat.index,
        vec![
            pre_boundary,
            launch,
            preparation,
            workload,
            boundary_phase(
                "cleanup",
                PhaseKind::Cleanup,
                &repeat.post_boundary,
            ),
        ],
        failure,
        context,
    )
}

fn boundary_phase(
    name: &str,
    kind: PhaseKind,
    outcome: &BoundaryOutcome,
) -> PhaseResult {
    match outcome {
        BoundaryOutcome::Pending {} => incomplete_phase(name, kind),
        BoundaryOutcome::Clean {} => completed_phase(name, kind, Some(0.0)),
        BoundaryOutcome::Failure { .. } => failed_phase(name, kind),
    }
}

fn simple_outcome_phase(
    name: &str,
    kind: PhaseKind,
    outcome: &PreparationOutcome,
) -> PhaseResult {
    match outcome {
        PreparationOutcome::Pending {} => incomplete_phase(name, kind),
        PreparationOutcome::NotRequested {} => not_executed_phase(name, kind),
        PreparationOutcome::Success {} => {
            completed_phase(name, kind, Some(0.0))
        }
        PreparationOutcome::Failure { .. } => failed_phase(name, kind),
    }
}

fn completed_phase(
    name: &str,
    kind: PhaseKind,
    duration_seconds: Option<f64>,
) -> PhaseResult {
    phase(name, kind, PhaseStatus::Completed, duration_seconds, None)
}

fn incomplete_phase(name: &str, kind: PhaseKind) -> PhaseResult {
    phase(name, kind, PhaseStatus::Incomplete, Some(0.0), None)
}

fn failed_phase(name: &str, kind: PhaseKind) -> PhaseResult {
    phase(
        name,
        kind,
        PhaseStatus::Failed,
        Some(0.0),
        Some(FailureRecord {
            code: "voxel_phase_failed".into(),
            message: "Voxel phase failure detail was redacted".into(),
        }),
    )
}

fn not_executed_phase(name: &str, kind: PhaseKind) -> PhaseResult {
    phase(name, kind, PhaseStatus::NotExecuted, None, None)
}

fn phase(
    name: &str,
    kind: PhaseKind,
    status: PhaseStatus,
    duration_seconds: Option<f64>,
    failure: Option<FailureRecord>,
) -> PhaseResult {
    PhaseResult {
        name: name.into(),
        kind,
        status,
        started_at: (status != PhaseStatus::NotExecuted)
            .then(|| "relative".into()),
        duration_seconds,
        observations: Vec::new(),
        failure,
        guardrail: None,
        extensions: BTreeMap::new(),
    }
}

fn observation(
    metric: &str,
    unit: Unit,
    value: u64,
    duration_seconds: f64,
) -> Observation {
    let aggregation = match metric {
        "launch.bytes_written" | "workload.bytes_written" => Aggregation::Sum,
        "launch.duration" | "workload.duration" => Aggregation::Last,
        "launch.peak_ram_delta" | "workload.peak_ram_delta" => {
            Aggregation::Maximum
        }
        _ => unreachable!("adapter observation metric is fixed"),
    };
    Observation {
        metric: metric.into(),
        unit,
        direction: OptimizationDirection::LowerIsBetter,
        aggregation,
        window: ObservationWindow {
            start_offset_seconds: 0.0,
            duration_seconds,
        },
        value: ObservationValue::Scalar { value: value as f64 },
        extensions: BTreeMap::new(),
    }
}

fn execution_state(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "completed",
        RunStatus::Running => "running",
        RunStatus::Aborted => "failed",
    }
}

fn workload_summary(workload: &WorkloadSpec) -> Value {
    json!({
        "count": workload.count,
        "parallel": workload.parallel,
        "size_bytes": workload.size_bytes,
        "snapshot": workload.snapshot,
    })
}

fn workload_label(workload: &WorkloadSpec) -> String {
    let size = if workload.size_bytes.is_multiple_of(1 << 30) {
        format!("{} GiB", workload.size_bytes / (1 << 30))
    } else if workload.size_bytes.is_multiple_of(1 << 20) {
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

fn stable_source_display(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn condition(
    label: impl Into<String>,
    value: impl Into<String>,
    code: bool,
) -> VoxelCondition {
    VoxelCondition { label: label.into(), value: value.into(), code }
}

fn human_key(key: &str) -> String {
    let mut chars = key.replace('_', " ").chars().collect::<Vec<_>>();
    if let Some(first) = chars.first_mut() {
        first.make_ascii_uppercase();
    }
    chars.into_iter().collect()
}

fn sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_', ' '], "");
    normalized.contains("password")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("authorization")
        || normalized.contains("apikey")
        || normalized.contains("privatekey")
}

fn diagnostic_is_sensitive(value: &str) -> bool {
    let mut segmented = String::with_capacity(value.len());
    for (index, character) in value.char_indices() {
        if index > 0
            && character.is_ascii_uppercase()
            && value[..index]
                .chars()
                .next_back()
                .is_some_and(|previous| previous.is_ascii_lowercase())
        {
            segmented.push(' ');
        }
        segmented.push(character.to_ascii_lowercase());
    }
    let tokens = segmented
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let sensitive_name = tokens.iter().any(|token| {
        matches!(
            *token,
            "password"
                | "credential"
                | "secret"
                | "token"
                | "authorization"
                | "apikey"
        )
    }) || tokens
        .windows(2)
        .any(|tokens| tokens == ["api", "key"] || tokens == ["private", "key"]);
    let normalized = value.to_ascii_lowercase();
    sensitive_name
        || normalized.contains("-----begin")
        || normalized.contains("bearer ")
        || [
            "$argon2", "$2a$", "$2b$", "$2y$", "$scrypt$", "$7$", "$1$", "$5$",
            "$6$", "$y$",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
        || normalized.contains("{scrypt}")
}

fn safe_diagnostic(value: &str, redacted: &str) -> String {
    if diagnostic_is_sensitive(value) { redacted.into() } else { value.into() }
}

fn flatten_value(label: &str, value: &Value, rows: &mut Vec<VoxelCondition>) {
    match value {
        Value::Object(object) => {
            if object.is_empty() {
                rows.push(condition(label, "{}", true));
            }
            for (key, value) in object {
                if !sensitive_key(key) {
                    flatten_value(
                        &format!("{label} / {}", human_key(key)),
                        value,
                        rows,
                    );
                }
            }
        }
        Value::Array(values) => {
            if values.is_empty() {
                rows.push(condition(label, "[]", true));
            }
            for (index, value) in values.iter().enumerate() {
                flatten_value(&format!("{label} / {}", index + 1), value, rows);
            }
        }
        Value::String(value) => rows.push(condition(label, value, true)),
        Value::Null => rows.push(condition(label, "not supplied", false)),
        value => rows.push(condition(label, value.to_string(), false)),
    }
}

fn experiment_conditions(
    args: &StorageCohortArgs<'_>,
) -> Result<Vec<VoxelCondition>> {
    let StorageCohortArgs {
        rss_sleds,
        variants,
        workload,
        session,
        evidence,
        source_path,
        run_id,
        launch_memory_semantics,
        workload_memory_semantics,
    } = args;
    let combinations = variants
        .iter()
        .map(|variant| variant.name.as_str())
        .collect::<Vec<_>>()
        .join(" → ");
    let mut rows = vec![
        condition("RSS sleds", rss_sleds.to_string(), false),
        condition(
            "Combinations",
            if combinations.is_empty() { "none".into() } else { combinations },
            false,
        ),
        condition(
            "Workload",
            workload
                .map(workload_label)
                .unwrap_or_else(|| "Not requested".into()),
            false,
        ),
    ];
    if let Some(session) = session {
        flatten_value("Oxide session", &safe_session_value(session), &mut rows);
    } else {
        rows.push(condition("Oxide session", "not supplied", false));
    }
    rows.push(condition(
        "Capability contract version",
        evidence
            .map(|evidence| evidence.evidence_version.to_string())
            .unwrap_or_else(|| "not supplied".into()),
        false,
    ));
    rows.push(condition(
        "Launch memory semantics",
        memory_semantics_label(launch_memory_semantics),
        false,
    ));
    rows.push(condition(
        "Workload memory semantics",
        workload_memory_semantics
            .map(memory_semantics_label)
            .unwrap_or("Not applicable"),
        false,
    ));
    rows.extend(provenance_conditions(*evidence, source_path, run_id));
    Ok(rows)
}

fn memory_semantics_label(semantics: &str) -> &'static str {
    match semantics {
        "legacy-absolute-host-peak" => "Legacy absolute host peak",
        "launch-baseline-delta" => "Launch baseline-adjusted delta",
        "workload-baseline-delta" => "Workload baseline-adjusted delta",
        _ => "Unknown memory semantics",
    }
}

fn safe_session_value(session: &OxideSessionMetadata) -> Value {
    json!({
        "profile": session.profile,
        "oxide_cli_version": session.oxide_cli_version,
        "provider": match session.provider {
            OxideAuthProviderMetadata::Builtin => "builtin",
            OxideAuthProviderMetadata::Helper { .. } => "helper",
        },
    })
}

fn provenance_conditions(
    evidence: Option<&MatrixReportEvidence>,
    source_path: &Path,
    run_id: &str,
) -> Vec<VoxelCondition> {
    let Some(evidence) = evidence else {
        return vec![
            condition("Provenance", "unavailable", false),
            condition("Source", stable_source_display(source_path), true),
            condition("Run ID", run_id, true),
        ];
    };
    let provenance = &evidence.provenance;
    let fields = [
        ("Voxel build", &provenance.voxel_build),
        ("Voxel binary", &provenance.voxel_binary),
        ("Configured image", &provenance.configured_image),
        ("Omicron commit", &provenance.omicron_commit),
        ("Host", &provenance.host),
    ];
    let partial = complete_provenance(Some(evidence)).is_none();
    let mut rows = Vec::new();
    if partial {
        rows.push(condition("Provenance", "unavailable", false));
    }
    rows.extend(fields.into_iter().map(|(label, value)| match value {
        EvidenceValue::Available { value } => condition(
            label,
            safe_diagnostic(value, "available; detail was redacted"),
            true,
        ),
        EvidenceValue::Unavailable { reason } => condition(
            label,
            format!(
                "unavailable: {}",
                safe_diagnostic(reason, "detail was redacted")
            ),
            false,
        ),
    }));
    if partial {
        rows.extend([
            condition("Source", stable_source_display(source_path), true),
            condition("Run ID", run_id, true),
        ]);
    }
    rows
}

fn available_evidence(value: &EvidenceValue<String>) -> Option<&str> {
    match value {
        EvidenceValue::Available { value } => Some(value),
        EvidenceValue::Unavailable { .. } => None,
    }
}

fn comparable_provenance_evidence(
    value: &EvidenceValue<String>,
) -> Option<&str> {
    available_evidence(value)
        .filter(|value| !value.trim().is_empty() && value.len() <= 1024)
}

fn complete_provenance(
    evidence: Option<&MatrixReportEvidence>,
) -> Option<[(&'static str, String); 5]> {
    let provenance = &evidence?.provenance;
    Some([
        ("Voxel build", available_evidence(&provenance.voxel_build)?.into()),
        ("Voxel binary", available_evidence(&provenance.voxel_binary)?.into()),
        (
            "Configured image",
            available_evidence(&provenance.configured_image)?.into(),
        ),
        (
            "Omicron commit",
            available_evidence(&provenance.omicron_commit)?.into(),
        ),
        ("Host", available_evidence(&provenance.host)?.into()),
    ])
}

fn provenance_state(evidence: Option<&MatrixReportEvidence>) -> &'static str {
    if complete_provenance(evidence).is_some() {
        "complete"
    } else {
        "unavailable"
    }
}

fn capability_ledger(
    ledger: &MatrixCapabilityLedger,
) -> Vec<CookoutCapabilityResult> {
    let result = |name: &str, status: &CapabilityStatus| {
        let (status, evidence, error) = match status {
            CapabilityStatus::Pass { evidence } => (
                CookoutCapabilityStatus::Passed,
                Some(safe_diagnostic(
                    evidence,
                    "capability evidence detail was redacted",
                )),
                None,
            ),
            CapabilityStatus::Fail { evidence } => (
                CookoutCapabilityStatus::Failed,
                None,
                Some(safe_diagnostic(
                    evidence,
                    "capability failure detail was redacted",
                )),
            ),
            CapabilityStatus::Unavailable { reason } => (
                CookoutCapabilityStatus::Unavailable,
                None,
                Some(safe_diagnostic(
                    reason,
                    "capability unavailable detail was redacted",
                )),
            ),
        };
        CookoutCapabilityResult {
            name: name.into(),
            status,
            evidence,
            error,
            elapsed_millis: None,
        }
    };
    vec![
        result("matrix_host_storage_scope", &ledger.matrix_host_storage_scope),
        result(
            "clean_launch_teardown_boundaries",
            &ledger.clean_launch_teardown_boundaries,
        ),
        result("api_disk_lifecycle", &ledger.api_disk_lifecycle),
        result(
            "simulated_zpool_preparation",
            &ledger.simulated_zpool_preparation,
        ),
    ]
}

fn checkpoint_run_context(
    repeat: &MatrixCheckpointRepeat,
    display_id: &str,
) -> VoxelRunContext {
    let launch_attempts = match &repeat.launch {
        LaunchOutcome::Success { prior_attempt_failures, .. } => {
            prior_attempt_failures.as_slice()
        }
        LaunchOutcome::Failure { attempt_failures } => {
            attempt_failures.as_slice()
        }
        LaunchOutcome::Pending {} => &[],
    };
    let nested_boundary_errors = launch_attempts
        .iter()
        .filter_map(|attempt| match &attempt.clean_boundary {
            BoundaryOutcome::Failure { error } => Some(error.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut boundary_failures = launch_attempts
        .iter()
        .enumerate()
        .filter_map(|(index, attempt)| match &attempt.clean_boundary {
            BoundaryOutcome::Failure { error } => Some(format!(
                "launch attempt {} clean boundary: {}",
                index + 1,
                safe_diagnostic(error, "detail was redacted")
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    boundary_failures.extend(
        [&repeat.pre_boundary, &repeat.post_boundary].into_iter().filter_map(
            |outcome| match outcome {
                BoundaryOutcome::Failure { error }
                    if !nested_boundary_errors.contains(&error.as_str()) =>
                {
                    Some(safe_diagnostic(
                        error,
                        "boundary failure detail was redacted",
                    ))
                }
                _ => None,
            },
        ),
    );
    let boundary_failure =
        (!boundary_failures.is_empty()).then(|| boundary_failures.join("; "));
    let launch_failure = match &repeat.launch {
        LaunchOutcome::Failure { attempt_failures } => Some(
            attempt_failures
                .iter()
                .map(|failure| {
                    safe_diagnostic(
                        &failure.error,
                        "launch failure detail was redacted",
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        ),
        _ => None,
    };
    let prior_launch_attempt_failures = match &repeat.launch {
        LaunchOutcome::Success { prior_attempt_failures, .. } => {
            prior_attempt_failures
                .iter()
                .map(|failure| {
                    safe_diagnostic(
                        &failure.error,
                        "prior launch failure detail was redacted",
                    )
                })
                .collect()
        }
        _ => Vec::new(),
    };
    let preparation_failure = match &repeat.preparation {
        PreparationOutcome::Failure { error } => Some(safe_diagnostic(
            error,
            "preparation failure detail was redacted",
        )),
        _ => None,
    };
    let workload_failure = match (&repeat.workload, &preparation_failure) {
        (WorkloadOutcome::Failure { error }, None) => {
            Some(safe_diagnostic(error, "workload failure detail was redacted"))
        }
        _ => None,
    };
    let workload_disposition = match &repeat.workload {
        WorkloadOutcome::Success { .. } => "succeeded",
        WorkloadOutcome::Failure { .. } if preparation_failure.is_some() => {
            "blocked"
        }
        WorkloadOutcome::Failure { .. } => "failed",
        WorkloadOutcome::Pending {}
            if launch_failure.is_some()
                || boundary_failure.is_some()
                || preparation_failure.is_some() =>
        {
            "blocked"
        }
        WorkloadOutcome::Pending {} => "pending",
        WorkloadOutcome::NotRequested {} => "not_requested",
    };
    VoxelRunContext {
        display_id: display_id.into(),
        workload_disposition,
        launch_failure,
        preparation_failure,
        workload_failure,
        boundary_failure,
        prior_launch_attempt_failures,
        launch_memory_semantics: matches!(
            repeat.launch,
            LaunchOutcome::Success { .. }
        )
        .then_some("launch-baseline-delta"),
        workload_memory_semantics: matches!(
            repeat.workload,
            WorkloadOutcome::Success { .. }
        )
        .then_some("workload-baseline-delta"),
    }
}

fn target_constraint_capability_names() -> [&'static str; 2] {
    ["matrix_host_storage_scope", "clean_launch_teardown_boundaries"]
}

fn capability_states(ledger: &MatrixCapabilityLedger) -> Value {
    json!({
        "ledger_version": ledger.ledger_version,
        "matrix_host_storage_scope": capability_state(&ledger.matrix_host_storage_scope),
        "clean_launch_teardown_boundaries": capability_state(&ledger.clean_launch_teardown_boundaries),
        "api_disk_lifecycle": capability_state(&ledger.api_disk_lifecycle),
        "simulated_zpool_preparation": capability_state(&ledger.simulated_zpool_preparation),
    })
}

fn capability_state(status: &CapabilityStatus) -> &'static str {
    match status {
        CapabilityStatus::Pass { .. } => "eligible",
        CapabilityStatus::Fail { .. } => "ineligible",
        CapabilityStatus::Unavailable { .. } => "unknown",
    }
}

pub(super) fn hex_digest(source: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source);
    format!("sha256:{:x}", hasher.finalize())
}
