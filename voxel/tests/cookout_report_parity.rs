use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VOXEL_LAUNCH_REPORT_DESCRIPTION: &str = "Compares host NVMe writes, launch duration, and peak memory across Voxel storage-tuning variants during simulated Oxide rack launch.";
const VOXEL_REPORT_DESCRIPTION: &str = "Compares host NVMe writes, launch duration, and peak memory across Voxel storage-tuning variants during simulated Oxide rack launch and API disk lifecycle workloads.";

struct Expected {
    name: &'static str,
    raw_sha256: &'static str,
    candidates: usize,
    execution_state: &'static str,
    run_outcomes: [usize; 3],
    phase_statuses: [usize; 4],
    capabilities: usize,
    issues: usize,
    metrics: usize,
}

const CASES: &[Expected] = &[
    Expected {
        name: "complete",
        raw_sha256: "94040b0ee669945f9ad0431c3e232c679fd5ebfd04f605f8448f102c5db34fc2",
        candidates: 1,
        execution_state: "completed",
        run_outcomes: [1, 0, 0],
        phase_statuses: [5, 0, 0, 0],
        capabilities: 0,
        issues: 1,
        metrics: 6,
    },
    Expected {
        name: "partial",
        raw_sha256: "fd7770243f98f93658988d30491f3981b0951ec7099a843479a0604b3999e898",
        candidates: 1,
        execution_state: "running",
        run_outcomes: [0, 1, 1],
        phase_statuses: [4, 5, 1, 0],
        capabilities: 0,
        issues: 3,
        metrics: 3,
    },
    Expected {
        name: "failed",
        raw_sha256: "27afc4ba08129ef345777f588f54173881d392c9403f10edfb06b10a57adfcf1",
        candidates: 2,
        execution_state: "failed",
        run_outcomes: [0, 0, 2],
        phase_statuses: [6, 2, 2, 0],
        capabilities: 0,
        issues: 4,
        metrics: 6,
    },
    Expected {
        name: "checkpoint",
        raw_sha256: "5a7e040ff0724ce33258e5774cea2fd49d32e537191baaba7b4dd9e492b3f200",
        candidates: 1,
        execution_state: "running",
        run_outcomes: [0, 0, 1],
        phase_statuses: [1, 1, 1, 2],
        capabilities: 0,
        issues: 3,
        metrics: 0,
    },
];

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn fixture(name: &str) -> PathBuf {
    let file = if name == "checkpoint" {
        "checkpoint-in-progress-v5.json"
    } else {
        match name {
            "complete" => "matrix-complete-v5.json",
            "partial" => "matrix-partial-v5.json",
            "failed" => "matrix-failed-v5.json",
            _ => unreachable!(),
        }
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/perftest")
        .join(file)
}

fn voxel(args: &[&Path]) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_voxel"));
    command.arg("perftest");
    for arg in args {
        command.arg(arg);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "voxel failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn verify_manifest(directory: &Path) {
    let manifest: Value = serde_json::from_slice(
        &fs::read(directory.join("manifest.json")).unwrap(),
    )
    .unwrap();
    let artifacts = manifest["artifacts"].as_array().unwrap();
    let published = artifact_names(directory)
        .into_iter()
        .filter(|name| name != "manifest.json")
        .collect::<Vec<_>>();
    let mut inventoried = artifacts
        .iter()
        .map(|artifact| artifact["filename"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    inventoried.sort();
    assert_eq!(inventoried, published, "manifest must list each artifact once");
    for artifact in artifacts {
        let bytes =
            fs::read(directory.join(artifact["filename"].as_str().unwrap()))
                .unwrap();
        assert_eq!(artifact["bytes"], bytes.len());
        assert_eq!(artifact["sha256"], digest(bytes));
        assert!(
            artifact["media_type"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }
}

fn verify_cohorts_csv(directory: &Path, report: &Value) {
    let csv = fs::read_to_string(directory.join("cohorts.csv")).unwrap();
    let mut rows = parse_csv_rows(&csv).into_iter();
    assert_eq!(
        rows.next().unwrap(),
        [
            "row_type",
            "experiment_id",
            "cohort",
            "variant_id",
            "phase",
            "metric",
            "unit",
            "count",
            "mean",
            "median",
            "standard_deviation",
            "dimension",
            "reason",
        ]
    );
    let rows = rows.collect::<Vec<_>>();
    assert!(rows.iter().all(|row| row.len() == 13));
    let mut csv_metrics = rows
        .iter()
        .filter(|row| {
            row[0] == "cohort"
                && !row[4].is_empty()
                && !row[5].is_empty()
                && row[7].parse::<u64>().is_ok()
        })
        .map(|row| {
            (
                row[2].parse::<usize>().unwrap(),
                row[4].clone(),
                row[5].clone(),
                row[6].to_ascii_lowercase(),
                row[7].parse::<u64>().unwrap(),
                row[8].parse::<f64>().unwrap().to_bits(),
                row[9].parse::<f64>().unwrap().to_bits(),
                row[10].parse::<f64>().unwrap().to_bits(),
            )
        })
        .collect::<Vec<_>>();
    let mut report_metrics = Vec::new();
    for (cohort_index, cohort) in
        report["cohorts"].as_array().unwrap().iter().enumerate()
    {
        let metrics = cohort["metrics"].as_array().unwrap();
        if metrics.is_empty() {
            assert!(
                rows.iter().any(|row| row[0] == "cohort"
                    && row[2] == cohort_index.to_string())
            );
        }
        for metric in metrics {
            report_metrics.push((
                cohort_index,
                metric["key"]["phase"].as_str().unwrap().to_owned(),
                metric["key"]["metric"].as_str().unwrap().to_owned(),
                metric["key"]["unit"].as_str().unwrap().to_owned(),
                metric["count"].as_u64().unwrap(),
                metric["mean"].as_f64().unwrap().to_bits(),
                metric["median"].as_f64().unwrap().to_bits(),
                metric["standard_deviation"].as_f64().unwrap().to_bits(),
            ));
        }
    }
    csv_metrics.sort();
    report_metrics.sort();
    assert_eq!(csv_metrics, report_metrics, "CSV and JSON metric inventories");
}

fn parse_csv_rows(csv: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = csv.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\n' if !quoted => {
                if field.ends_with('\r') {
                    field.pop();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            character => field.push(character),
        }
    }
    assert!(!quoted, "unterminated quoted CSV field");
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[test]
fn csv_parser_handles_commas_and_escaped_quotes() {
    assert_eq!(
        parse_csv_rows("plain,\"with, comma\",\"with \"\"quotes\"\"\"\r\n"),
        vec![vec!["plain", "with, comma", "with \"quotes\""]]
    );
}

fn echarts_options(html: &str) -> Result<Vec<Value>, String> {
    let mut options = Vec::new();
    let mut offset = 0;
    while let Some(relative) = html[offset..].find("setOption(") {
        let call = offset + relative + "setOption(".len();
        let start = call
            + html[call..]
                .find(|character: char| !character.is_whitespace())
                .ok_or_else(|| "setOption call has no argument".to_owned())?;
        if !matches!(html.as_bytes()[start], b'{' | b'[') {
            offset = call;
            continue;
        }
        if html.as_bytes()[start] == b'{' {
            let first_member = start
                + 1
                + html[start + 1..]
                    .find(|character: char| !character.is_whitespace())
                    .ok_or_else(|| {
                        format!("unterminated literal setOption call at byte {start}")
                    })?;
            if !matches!(html.as_bytes()[first_member], b'}' | b'"') {
                offset = call;
                continue;
            }
        }
        let bytes = html.as_bytes();
        let mut stack = Vec::new();
        let mut quoted = false;
        let mut escaped = false;
        let mut end = None;
        for (index, byte) in bytes.iter().enumerate().skip(start) {
            if quoted {
                if escaped {
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    quoted = false;
                }
                continue;
            }
            match *byte {
                b'"' => quoted = true,
                b'{' => stack.push(b'}'),
                b'[' => stack.push(b']'),
                b'}' | b']' => {
                    if stack.pop() != Some(*byte) {
                        return Err(format!(
                            "mismatched delimiter in literal setOption call at byte {start}"
                        ));
                    }
                    if stack.is_empty() {
                        end = Some(index + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.ok_or_else(|| {
            format!("unterminated literal setOption call at byte {start}")
        })?;
        options.push(serde_json::from_str(&html[start..end]).map_err(
            |error| {
                format!(
                    "malformed literal setOption JSON at byte {start}: {error}"
                )
            },
        )?);
        offset = end;
    }
    Ok(options)
}

#[test]
fn echarts_extractor_accepts_literal_json_layouts() {
    let html = "setOption( {} ); setOption(\n [\n {\"label\": \"} escaped \\\" quote\"}\n ]\n);";
    assert_eq!(
        echarts_options(html).unwrap(),
        vec![
            serde_json::json!({}),
            serde_json::json!([{"label": "} escaped \" quote"}]),
        ]
    );
}

#[test]
fn echarts_extractor_reports_malformed_literal_calls() {
    let malformed =
        echarts_options("setOption({\"broken\": true,})").unwrap_err();
    assert!(malformed.contains("malformed literal setOption JSON"));
    let unterminated =
        echarts_options("setOption( {\n\"value\": 1").unwrap_err();
    assert!(unterminated.contains("unterminated literal setOption call"));
}

fn artifact_names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn assert_publications_equal(first: &Path, second: &Path) {
    let names = artifact_names(first);
    assert_eq!(names, artifact_names(second));
    for name in names {
        assert_eq!(
            fs::read(first.join(&name)).unwrap(),
            fs::read(second.join(&name)).unwrap(),
            "deterministic artifact {name}"
        );
    }
}

fn assert_no_sensitive_content(bytes: &[u8], context: &str) {
    let serialized = String::from_utf8_lossy(bytes).to_lowercase();
    let sensitive = [
        "token",
        "password",
        "authorization",
        "bearer ",
        "api_key",
        "private_key",
        "helper command",
        "$argon2",
        "-----begin private key",
        "ssh ",
        "omdb ",
    ];
    for sensitive in sensitive {
        assert!(
            !serialized.contains(sensitive),
            "{context} exposed {sensitive}"
        );
    }
}

fn application_owned_html(html: &str) -> String {
    let marker =
        "<!-- Embedded Apache ECharts v5.5.1; no network resources. -->";
    let library = html.find(marker).and_then(|marker_start| {
        let after_marker = marker_start + marker.len();
        let whitespace = html[after_marker..]
            .find(|character: char| !character.is_whitespace())?;
        let start = after_marker + whitespace;
        if !html[start..].starts_with("<script") {
            return None;
        }
        let end = start + html[start..].find("</script>")? + "</script>".len();
        let script = &html[start..end];
        (script.contains("Licensed to the Apache Software Foundation")
            && script.contains("echarts"))
        .then_some((start, end))
    });
    match library {
        Some((start, end)) => format!("{}{}", &html[..start], &html[end..]),
        None => html.to_owned(),
    }
}

#[test]
fn html_sensitivity_filter_removes_only_marked_echarts_library() {
    let html = "<script>application_owned_token_data</script><!-- Embedded Apache ECharts v5.5.1; no network resources. --> \n\t<script>Licensed to the Apache Software Foundation; echarts library</script><script>Licensed to the Apache Software Foundation; echarts later_application_data</script>";
    let filtered = application_owned_html(html);
    assert!(filtered.contains("application_owned_token_data"));
    assert!(filtered.contains("later_application_data"));
    assert!(!filtered.contains("echarts library"));

    let separated = "<!-- Embedded Apache ECharts v5.5.1; no network resources. --><script>application boundary</script><script>Licensed to the Apache Software Foundation; echarts later_application_data</script>";
    assert_eq!(application_owned_html(separated), separated);
}

#[test]
fn neutral_cookout_report_preserves_experiment_evidence_and_publication() {
    let root = tempfile::tempdir().unwrap();
    let mut archives = Vec::new();

    for expected in CASES {
        let input_path = fixture(expected.name);
        assert_eq!(
            digest(fs::read(&input_path).unwrap()),
            expected.raw_sha256,
            "{} raw fixture digest",
            expected.name
        );
        let first = root.path().join(format!("{}-first", expected.name));
        let second = root.path().join(format!("{}-second", expected.name));
        for output in [&first, &second] {
            voxel(&[
                Path::new("report"),
                &input_path,
                Path::new("--out"),
                output,
                Path::new("--archive"),
            ]);
        }

        let report: Value = serde_json::from_slice(
            &fs::read(first.join("report.json")).unwrap(),
        )
        .unwrap();
        let expected_description = if report["inputs"][0]["scenario"]["name"]
            == "rack launch and API disk lifecycle"
        {
            VOXEL_REPORT_DESCRIPTION
        } else {
            VOXEL_LAUNCH_REPORT_DESCRIPTION
        };
        assert_eq!(report["schema"], "cookout.report");
        assert!(report.get("view").is_none());
        assert_eq!(
            report["inputs"][0]["identity"]["name"],
            "Voxel performance report"
        );
        assert_eq!(
            report["inputs"][0]["identity"]["description"],
            expected_description
        );
        assert_eq!(
            report["experiments"][0]["variants"].as_array().unwrap().len(),
            expected.candidates
        );
        let input = &report["inputs"][0];
        assert_eq!(
            input["variants"].as_array().unwrap().len(),
            expected.candidates
        );
        assert_eq!(input["provenance"]["producer"], "oxide.voxel");
        assert!(input["provenance"]["producer_version"].is_string());
        assert_eq!(input["provenance"]["invocation"], "voxel perftest matrix");
        assert!(input["provenance"]["source_digest"].as_str().is_some_and(
            |digest| digest.starts_with("sha256:") && digest.len() == 71
        ));
        assert_eq!(input["provenance"]["source_revision"], Value::Null);
        assert_eq!(input["provenance"]["generated_at"], Value::Null);
        assert_eq!(
            input["provenance"]["attributes"]["source_format"],
            "voxel matrix"
        );
        assert!(
            input["identity"]["experiment_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("voxel-matrix-1700000000-"))
        );
        assert_eq!(input["identity"]["created_at"], "1700000000");
        assert_eq!(input["identity"]["application"]["id"], "oxide.voxel");
        assert!(input["identity"]["application"]["version"].is_string());
        assert_eq!(input["identity"]["kind"], "benchmark");
        assert_eq!(
            input["extensions"]["oxide.voxel"]["execution_state"],
            expected.execution_state
        );
        for variant in input["variants"].as_array().unwrap() {
            assert!(variant["planned_runs"].as_u64().is_some());
            assert!(
                variant["dimensions"]["oxide.voxel.matrix_kind"]["value"]
                    .is_string()
            );
            assert!(
                variant["dimensions"]["oxide.voxel.levers"]["value"]
                    .is_string()
            );
        }
        for run in input["runs"].as_array().unwrap() {
            assert!(
                run["extensions"]["oxide.voxel"]["workload_disposition"]
                    .is_string()
            );
            assert_eq!(run["phases"].as_array().unwrap().len(), 5);
            for observation in run["phases"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|phase| phase["observations"].as_array().unwrap())
            {
                assert!(observation["metric"].is_string());
                assert!(observation["unit"].is_string());
                assert_eq!(observation["direction"], "lower_is_better");
                assert!(observation["aggregation"].is_string());
                assert_eq!(
                    observation["window"]["start_offset_seconds"].as_f64(),
                    Some(0.0)
                );
                assert!(observation["window"]["duration_seconds"].is_number());
                assert_eq!(observation["value"]["value_kind"], "scalar");
                assert!(observation["value"]["value"].is_number());
            }
        }
        assert_eq!(
            input["capabilities"].as_array().unwrap().len(),
            expected.capabilities
        );
        let constraints = input["target"]["constraints"].as_array().unwrap();
        assert_eq!(constraints.len(), 2);
        assert_eq!(constraints[0]["capability"], "matrix_host_storage_scope");
        assert_eq!(constraints[0]["required"], true);
        assert_eq!(
            constraints[1]["capability"],
            "clean_launch_teardown_boundaries"
        );
        assert_eq!(constraints[1]["required"], true);

        let outcomes = ["completed", "incomplete", "failed"].map(|status| {
            input["runs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|run| run["outcome"] == status)
                .count()
        });
        assert_eq!(
            outcomes, expected.run_outcomes,
            "{} outcomes",
            expected.name
        );
        let phase_statuses =
            ["completed", "incomplete", "failed", "not_executed"].map(
                |status| {
                    input["runs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|run| run["phases"].as_array().unwrap())
                        .filter(|phase| phase["status"] == status)
                        .count()
                },
            );
        assert_eq!(
            phase_statuses, expected.phase_statuses,
            "{} phases",
            expected.name
        );
        assert_eq!(report["issues"].as_array().unwrap().len(), expected.issues);
        if matches!(expected.name, "failed" | "partial" | "checkpoint") {
            assert!(
                report["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["impact"] == "blocking")
            );
        }
        if matches!(expected.name, "partial" | "checkpoint") {
            assert!(phase_statuses[1] > 0);
            assert!(
                report["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["code"]
                        == "oxide.voxel.perftest.repeat_pending")
            );
        }

        let metric_count: usize = report["cohorts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cohort| cohort["metrics"].as_array().unwrap().len())
            .sum();
        assert_eq!(metric_count, expected.metrics);
        verify_cohorts_csv(&first, &report);
        let first_options = echarts_options(
            &fs::read_to_string(first.join("report.html")).unwrap(),
        )
        .unwrap();
        let second_options = echarts_options(
            &fs::read_to_string(second.join("report.html")).unwrap(),
        )
        .unwrap();
        assert_eq!(first_options.len(), expected.metrics);
        assert_eq!(first_options, second_options);

        assert_no_sensitive_content(
            &fs::read(first.join("report.json")).unwrap(),
            &format!("{} report JSON", expected.name),
        );
        let first_html = fs::read_to_string(first.join("report.html")).unwrap();
        assert!(first_html.contains(expected_description));
        assert_no_sensitive_content(
            application_owned_html(&first_html).as_bytes(),
            &format!("{} report HTML", expected.name),
        );
        let svg_names = artifact_names(&first)
            .into_iter()
            .filter(|name| name.ends_with(".svg"))
            .collect::<Vec<_>>();
        assert_eq!(svg_names.len(), expected.metrics);
        let expected_svg_names = report["cohorts"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .flat_map(|(cohort, value)| {
                (0..value["metrics"].as_array().unwrap().len()).map(
                    move |metric| {
                        format!("cohort-{cohort:03}-metric-{metric:03}.svg")
                    },
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(svg_names, expected_svg_names);
        for name in &svg_names {
            let svg = fs::read_to_string(first.join(name)).unwrap();
            assert!(svg.starts_with("<svg "));
            assert!(svg.contains("role=\"img\""));
            assert!(svg.contains("x-axis") && svg.contains("y-axis"));
            assert!(!svg.to_lowercase().contains("<script"));
            assert!(!svg.contains("NaN") && !svg.contains("Infinity"));
        }
        assert_publications_equal(&first, &second);
        verify_manifest(&first);
        verify_manifest(&second);
        let evidence: Value = serde_json::from_slice(
            &fs::read(first.join("evidence-0000.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(evidence["schema"], "cookout.evidence");
        assert_eq!(evidence["adapter"]["id"], "oxide.voxel.perftest");
        assert!(evidence["adapter"]["normalization_version"].is_number());
        assert_eq!(
            evidence["source"]["value"]["source_schema"],
            "matrix_checkpoint"
        );
        assert!(evidence["source"]["value"].is_object());
        assert!(evidence["source"]["value"]["source"]["started"].is_number());
        assert!(evidence["source"]["value"]["source"]["combos"].is_array());
        assert_no_sensitive_content(
            &fs::read(first.join("evidence-0000.json")).unwrap(),
            &format!("{} retained evidence", expected.name),
        );
        archives
            .push(root.path().join(format!("{}-first.tar.gz", expected.name)));
    }

    let duplicate = root.path().join("complete-duplicate");
    voxel(&[
        Path::new("report"),
        &fixture("complete"),
        Path::new("--out"),
        &duplicate,
        Path::new("--archive"),
    ]);
    archives.push(root.path().join("complete-duplicate.tar.gz"));
    let mut aggregates = Vec::new();
    for name in ["aggregate-first", "aggregate-second"] {
        let output = root.path().join(name);
        let mut args = vec![Path::new("superreport")];
        args.extend(archives.iter().map(PathBuf::as_path));
        args.extend([
            Path::new("--out"),
            output.as_path(),
            Path::new("--archive"),
        ]);
        voxel(&args);
        let report: Value = serde_json::from_slice(
            &fs::read(output.join("report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["schema"], "cookout.report");
        assert_eq!(report["aggregation"]["unique_input_count"], 4);
        assert_eq!(report["aggregation"]["duplicate_count"], 1);
        verify_manifest(&output);
        for evidence in artifact_names(&output)
            .into_iter()
            .filter(|name| name.starts_with("evidence-"))
        {
            let bytes = fs::read(output.join(&evidence)).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap()["schema"],
                "cookout.evidence"
            );
            assert_no_sensitive_content(
                &bytes,
                &format!("aggregate {evidence}"),
            );
        }
        assert_no_sensitive_content(
            &fs::read(output.join("report.json")).unwrap(),
            "aggregate report JSON",
        );
        let aggregate_html =
            fs::read_to_string(output.join("report.html")).unwrap();
        assert_no_sensitive_content(
            application_owned_html(&aggregate_html).as_bytes(),
            "aggregate report HTML",
        );
        aggregates.push(output);
    }
    assert_publications_equal(&aggregates[0], &aggregates[1]);

    let recursive = root.path().join("recursive");
    voxel(&[
        Path::new("superreport"),
        &root.path().join("aggregate-first.tar.gz"),
        archives[0].as_path(),
        Path::new("--out"),
        &recursive,
    ]);
    let recursive_report: Value = serde_json::from_slice(
        &fs::read(recursive.join("report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(recursive_report["schema"], "cookout.report");
    assert_eq!(recursive_report["aggregation"]["unique_input_count"], 4);
    assert_eq!(recursive_report["aggregation"]["duplicate_count"], 1);
    verify_manifest(&recursive);
}
