//! Thin compatibility wrappers for Cookout's offline report APIs.

use super::{
    MatrixCheckpoint, MatrixRun, cookout_adapter, validate_matrix_checkpoint,
    validate_publishable_matrix_run,
};
use anyhow::{Context, Result, bail};
use cookout::analysis::{Analysis, Cohort, NoiseStatus};
use cookout::model::{Aggregation, OptimizationDirection, Unit};
use cookout::policy::{
    Objective, RecommendationMode, RecommendationPolicy, SelectionMode,
};
use cookout::{
    AggregateRequest, AnalysisPolicy, Comparison, EvidenceEnvelope,
    ExperimentDocument, Limits, MetricKey, PartialAcceptance, PublishRequest,
    aggregate_archives_with_adapter, analyze, compare, publish_report,
};
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const STORAGE_COHORT_SELECTOR: &str =
    "target.dimension.oxide.voxel.storage_cohort";

pub(super) fn voxel_analysis_policy() -> AnalysisPolicy {
    let objective = |phase: &str, metric: &str, unit, aggregation| Objective {
        metric: MetricKey {
            phase: phase.into(),
            metric: metric.into(),
            unit,
            direction: OptimizationDirection::LowerIsBetter,
            aggregation,
        },
    };
    AnalysisPolicy {
        compatibility_dimensions: vec![STORAGE_COHORT_SELECTOR.into()],
        recommendation: Some(RecommendationPolicy {
            mode: RecommendationMode::UsabilityDefault,
            selection: SelectionMode::Pareto,
            objectives: vec![
                objective(
                    "launch",
                    "launch.bytes_written",
                    Unit::Bytes,
                    Aggregation::Sum,
                ),
                objective(
                    "workload",
                    "workload.bytes_written",
                    Unit::Bytes,
                    Aggregation::Sum,
                ),
                objective(
                    "launch",
                    "launch.duration",
                    Unit::Seconds,
                    Aggregation::Last,
                ),
                objective(
                    "launch",
                    "launch.peak_ram_delta",
                    Unit::Bytes,
                    Aggregation::Maximum,
                ),
            ],
            constraints: Vec::new(),
            resource_order: None,
        }),
        ..AnalysisPolicy::default()
    }
}

pub(super) fn run_report(
    inputs: &[PathBuf],
    out: &Path,
    archive: bool,
) -> Result<()> {
    let limits = Limits::default();
    if inputs.is_empty() {
        bail!("at least one report input is required");
    }
    if inputs.len() > limits.documents {
        bail!("report input count exceeds Cookout document limit");
    }

    let evidence = inputs
        .iter()
        .map(|path| load_evidence(path, &limits))
        .collect::<Result<Vec<_>>>()?;
    publish_report(&PublishRequest {
        evidence: &evidence,
        policy: voxel_analysis_policy(),
        destination: out,
        archive,
        include_csv: true,
        limits,
    })
    .context("publish Cookout report")?;
    Ok(())
}

pub(super) fn run_superreport(
    reports: &[PathBuf],
    out: &Path,
    archive: bool,
) -> Result<()> {
    aggregate_archives_with_adapter(
        &AggregateRequest {
            archives: reports,
            destination: out,
            policy: voxel_analysis_policy(),
            archive,
            limits: Limits::default(),
            partial: PartialAcceptance::RejectAny,
        },
        &cookout_adapter::VoxelCookoutAdapter,
    )
    .context("aggregate Cookout report archives")?;
    Ok(())
}

pub(super) fn run_compare(baseline: &Path, candidate: &Path) -> Result<()> {
    for line in compare_report(baseline, candidate)? {
        println!("{line}");
    }
    Ok(())
}

fn compare_report(baseline: &Path, candidate: &Path) -> Result<Vec<String>> {
    let base = read_matrix_run(baseline)?;
    let cand = read_matrix_run(candidate)?;
    super::validate_comparison_compatibility(&base, &cand)?;
    let base_doc = cookout_adapter::matrix_run_to_experiment(&base, baseline)
        .with_context(|| {
        format!("adapt matrix run {}", baseline.display())
    })?;
    let cand_doc = cookout_adapter::matrix_run_to_experiment(&cand, candidate)
        .with_context(|| format!("adapt matrix run {}", candidate.display()))?;
    let policy = AnalysisPolicy {
        compatibility_dimensions: vec![],
        percentiles: vec![],
        noise_multiplier: 2.0,
        recommendation: None,
    };
    let mut lines = vec![
        format!("perftest compare: baseline '{}' -> candidate '{}'", base.name, cand.name),
        format!("  baseline: {} combo(s), repeat {}    candidate: {} combo(s), repeat {}", base.results.len(), base.repeat, cand.results.len(), cand.repeat),
        "  noise flag: [*] delta > 2*sqrt(sd_b^2+sd_c^2)   [ ] within noise   [?] variance unknown (repeat<2)".into(),
    ];
    let mut labels: Vec<&str> =
        base.results.iter().map(|r| r.label.as_str()).collect();
    for result in &cand.results {
        if !labels.contains(&result.label.as_str()) {
            labels.push(&result.label);
        }
    }
    for label in labels {
        let b = base.results.iter().find(|r| r.label == label);
        let c = cand.results.iter().find(|r| r.label == label);
        if b.is_none() {
            lines.push(format!(
                "\ncombo '{label}': only in candidate (skipped)"
            ));
            continue;
        }
        if c.is_none() {
            lines
                .push(format!("\ncombo '{label}': only in baseline (skipped)"));
            continue;
        }
        lines.push(format!("\ncombo '{label}':"));
        let ba = analyze(&[select_variant(&base_doc, label)], &policy)
            .context("analyze baseline combo")?;
        let ca = analyze(&[select_variant(&cand_doc, label)], &policy)
            .context("analyze candidate combo")?;
        let baseline_cohort = only_cohort(&ba, "baseline", label)?;
        let candidate_cohort = only_cohort(&ca, "candidate", label)?;
        for (name, metric, bytes) in metrics() {
            let bp = baseline_cohort.metrics.iter().any(|m| m.key == metric);
            let cp = candidate_cohort.metrics.iter().any(|m| m.key == metric);
            if !bp && !cp {
                continue;
            }
            if bp != cp {
                bail!(
                    "combo '{label}' metric '{name}' is present on only one side"
                );
            }
            lines.push(render_comparison(
                name,
                bytes,
                &compare(&ba, &ca, &metric).with_context(|| {
                    format!("compare combo '{label}' metric '{name}'")
                })?,
            ));
        }
    }
    Ok(lines)
}

fn only_cohort<'a>(
    analysis: &'a Analysis,
    side: &str,
    label: &str,
) -> Result<&'a Cohort> {
    match analysis.cohorts.as_slice() {
        [cohort] => Ok(cohort),
        cohorts => bail!(
            "combo '{label}' {side} analysis produced {} cohorts; expected exactly one",
            cohorts.len()
        ),
    }
}

fn read_matrix_run(path: &Path) -> Result<MatrixRun> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let run = serde_json::from_str(&text).with_context(|| {
        format!(
            "parse matrix run {} (from `matrix --json-out`)",
            path.display()
        )
    })?;
    super::validate_matrix_run(&run).with_context(|| {
        format!("validate complete matrix run {}", path.display())
    })?;
    Ok(run)
}

fn select_variant(
    document: &ExperimentDocument,
    label: &str,
) -> ExperimentDocument {
    let mut selected = document.clone();
    let id = format!("levers-{}", label.replace('+', "-"));
    selected.variants.retain(|variant| variant.id == id);
    selected.runs.retain(|run| run.variant_id == id);
    selected
}

fn metrics() -> [(&'static str, MetricKey, bool); 5] {
    let key = |phase: &str, metric: &str, unit, aggregation| MetricKey {
        phase: phase.into(),
        metric: metric.into(),
        unit,
        direction: OptimizationDirection::LowerIsBetter,
        aggregation,
    };
    [
        (
            "bring-up",
            key(
                "launch",
                "launch.bytes_written",
                Unit::Bytes,
                Aggregation::Sum,
            ),
            true,
        ),
        (
            "launch",
            key("launch", "launch.duration", Unit::Seconds, Aggregation::Last),
            false,
        ),
        (
            "launch-delta-ram",
            key(
                "launch",
                "launch.peak_ram_delta",
                Unit::Bytes,
                Aggregation::Maximum,
            ),
            true,
        ),
        (
            "workload",
            key(
                "workload",
                "workload.bytes_written",
                Unit::Bytes,
                Aggregation::Sum,
            ),
            true,
        ),
        (
            "workload-delta-ram",
            key(
                "workload",
                "workload.peak_ram_delta",
                Unit::Bytes,
                Aggregation::Maximum,
            ),
            true,
        ),
    ]
}

fn render_comparison(
    name: &str,
    bytes: bool,
    comparison: &Comparison,
) -> String {
    let format_value = |value: f64| {
        if bytes {
            super::human_bytes(value as u64)
        } else {
            format!("{value:.0}s")
        }
    };
    let relative = comparison
        .relative_delta
        .map(|delta| format!("{:+.1}%", delta * 100.0))
        .unwrap_or_else(|| {
            if comparison.candidate.mean != 0.0 {
                "new".into()
            } else {
                "0.0%".into()
            }
        });
    let marker = match comparison.noise {
        NoiseStatus::Significant => "[*]",
        NoiseStatus::WithinNoise => "[ ]",
        NoiseStatus::Unknown => "[?]",
    };
    format!(
        "  {name:<10} {:>12} -> {:>12}   {relative:>8}  {marker}",
        format_value(comparison.baseline.mean),
        format_value(comparison.candidate.mean)
    )
}

fn load_evidence(path: &Path, limits: &Limits) -> Result<EvidenceEnvelope> {
    let file = File::open(path)
        .with_context(|| format!("open report input {}", path.display()))?;
    let mut bytes = Vec::new();
    let limit =
        u64::try_from(limits.input_bytes).unwrap_or(u64::MAX).saturating_add(1);
    file.take(limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read report input {}", path.display()))?;
    if bytes.len() > limits.input_bytes {
        bail!(
            "report input {} exceeds Cookout input byte limit",
            path.display()
        );
    }
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse report input {}", path.display()))?;
    let schema =
        value.get("schema_version").and_then(Value::as_u64).with_context(
            || format!("report input {} has no schema_version", path.display()),
        )?;
    let envelope = match schema {
        4 => {
            let run: MatrixRun =
                serde_json::from_value(value).with_context(|| {
                    format!("decode matrix run {}", path.display())
                })?;
            validate_publishable_matrix_run(&run).with_context(|| {
                format!(
                    "validate storage matrix semantics for {}",
                    path.display()
                )
            })?;
            cookout_adapter::matrix_run_to_evidence(&run, path)
                .with_context(|| format!("adapt matrix run {}", path.display()))
        }
        5 => {
            let checkpoint: MatrixCheckpoint = serde_json::from_value(value)
                .with_context(|| {
                    format!("decode matrix checkpoint {}", path.display())
                })?;
            validate_matrix_checkpoint(&checkpoint).with_context(|| {
                format!(
                    "validate schema-v5 storage matrix checkpoint semantics for {}",
                    path.display()
                )
            })?;
            cookout_adapter::matrix_checkpoint_to_evidence(&checkpoint, path)
                .with_context(|| {
                    format!("adapt matrix checkpoint {}", path.display())
                })
        }
        version => bail!(
            "report input {} uses unsupported schema version {version}",
            path.display()
        ),
    }?;
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perftest::{
        ComboAggregate, MATRIX_SCHEMA_VERSION, OxideAuthProviderMetadata,
        OxideSessionMetadata, RepeatSample, WorkloadSpec,
    };
    use std::collections::BTreeSet;

    fn matrix_run(
        name: &str,
        repeat: usize,
        workload: bool,
        combos: &[(&str, &[u8], &[u64])],
    ) -> MatrixRun {
        MatrixRun {
            schema_version: MATRIX_SCHEMA_VERSION,
            name: name.into(),
            started: 1,
            ended: 2,
            rated_tbw: None,
            workload: workload.then(WorkloadSpec::api_disk_lifecycle),
            oxide_session: workload.then(|| OxideSessionMetadata {
                profile: "test".into(),
                host: "https://oxide.test".into(),
                provider: OxideAuthProviderMetadata::Builtin,
                oxide_cli_version: "test".into(),
            }),
            report_evidence: None,
            rss_sleds: 3,
            repeat,
            combos: combos
                .iter()
                .map(|(label, _, _)| (*label).into())
                .collect(),
            results: combos
                .iter()
                .map(|(label, levers, values)| ComboAggregate {
                    label: (*label).into(),
                    levers: levers.iter().copied().collect::<BTreeSet<_>>(),
                    repeats: values
                        .iter()
                        .map(|value| RepeatSample {
                            bringup_bytes: *value,
                            launch_secs: *value,
                            peak_ram_bytes: Some(*value),
                            workload_bytes: workload.then_some(*value),
                            workload_secs: workload.then_some(*value),
                            workload_peak_delta_bytes: workload
                                .then_some(*value),
                        })
                        .collect(),
                    error: None,
                })
                .collect(),
        }
    }

    fn compare_runs(
        baseline: &MatrixRun,
        candidate: &MatrixRun,
    ) -> Result<Vec<String>> {
        let directory = tempfile::tempdir()?;
        let baseline_path = directory.path().join("baseline.json");
        let candidate_path = directory.path().join("candidate.json");
        std::fs::write(&baseline_path, serde_json::to_vec(baseline)?)?;
        std::fs::write(&candidate_path, serde_json::to_vec(candidate)?)?;
        compare_report(&baseline_path, &candidate_path)
    }

    #[test]
    fn comparison_preserves_headers_order_and_skipped_labels() {
        let baseline = matrix_run(
            "old",
            2,
            false,
            &[("none", &[], &[100, 100]), ("1", &[1], &[100, 100])],
        );
        let candidate = matrix_run(
            "new",
            2,
            false,
            &[("none", &[], &[200, 200]), ("2", &[2], &[200, 200])],
        );
        let lines = compare_runs(&baseline, &candidate).unwrap();

        assert_eq!(
            lines[0],
            "perftest compare: baseline 'old' -> candidate 'new'"
        );
        assert_eq!(
            lines[1],
            "  baseline: 2 combo(s), repeat 2    candidate: 2 combo(s), repeat 2"
        );
        assert_eq!(
            lines[2],
            "  noise flag: [*] delta > 2*sqrt(sd_b^2+sd_c^2)   [ ] within noise   [?] variance unknown (repeat<2)"
        );
        let labels: Vec<&str> = lines
            .iter()
            .filter(|line| line.starts_with("\ncombo"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            labels,
            [
                "\ncombo 'none':",
                "\ncombo '1': only in baseline (skipped)",
                "\ncombo '2': only in candidate (skipped)",
            ]
        );
        assert!(
            lines.iter().any(|line| line.starts_with("  bring-up ")
                && line.ends_with("+100.0%  [*]")),
            "got:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn comparison_marks_noise_and_repeat_one() {
        let within_baseline =
            matrix_run("old", 2, false, &[("none", &[], &[100, 200])]);
        let within_candidate =
            matrix_run("new", 2, false, &[("none", &[], &[110, 190])]);
        let within = compare_runs(&within_baseline, &within_candidate).unwrap();
        assert!(within.iter().any(|line| {
            line.starts_with("  bring-up ") && line.ends_with("+0.0%  [ ]")
        }));

        let once_baseline =
            matrix_run("old", 1, false, &[("none", &[], &[100])]);
        let once_candidate =
            matrix_run("new", 1, false, &[("none", &[], &[110])]);
        let once = compare_runs(&once_baseline, &once_candidate).unwrap();
        assert!(once.iter().any(|line| {
            line.starts_with("  bring-up ") && line.ends_with("+10.0%  [?]")
        }));
    }

    #[test]
    fn comparison_formats_zero_baselines() {
        let baseline = matrix_run("old", 2, false, &[("none", &[], &[0, 0])]);
        let new_candidate =
            matrix_run("new", 2, false, &[("none", &[], &[10, 10])]);
        let new_lines = compare_runs(&baseline, &new_candidate).unwrap();
        assert!(new_lines.iter().any(|line| {
            line.starts_with("  bring-up ")
                && line.contains("0.00 B ->      10.00 B")
                && line.ends_with("new  [*]")
        }));

        let zero_lines = compare_runs(&baseline, &baseline).unwrap();
        assert!(zero_lines.iter().any(|line| {
            line.starts_with("  bring-up ") && line.ends_with("0.0%  [ ]")
        }));
    }

    #[test]
    fn comparison_omits_workload_metrics_when_not_measured() {
        let baseline = matrix_run("old", 2, false, &[("none", &[], &[10, 10])]);
        let candidate =
            matrix_run("new", 2, false, &[("none", &[], &[20, 20])]);
        let lines = compare_runs(&baseline, &candidate).unwrap();
        assert!(!lines.iter().any(|line| line.starts_with("  workload ")));
        assert!(
            !lines.iter().any(|line| line.starts_with("  workload-delta-ram "))
        );
    }

    #[test]
    fn comparison_reports_workload_mismatch() {
        let baseline = matrix_run("old", 2, false, &[("none", &[], &[10, 10])]);
        let candidate = matrix_run("new", 2, true, &[("none", &[], &[20, 20])]);
        let error = compare_runs(&baseline, &candidate).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "workload mismatch: baseline and candidate matrix runs are not comparable"
        );
    }
}
