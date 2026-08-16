use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// The raw fixtures and renderer options/SVG oracle were generated against this
// Cookout revision; those artifacts are intentionally independent of the
// currently locked dependency. Normalized-source digests below instead cover
// the current Voxel typed/defaulted schema passed to the Cookout renderer.
const RENDERER_ORACLE_GENERATION_COOKOUT_COMMIT: &str =
    "60ecf91bc13793d83a708dba3c6d96f648024efb";
const RAW_FIXTURE_REVISION: &str = "3ed372e6168ae99c1e19a1d3fcc8f70cb41eccf2";
const RENDERER_ORACLE_REVISION: &str =
    "60ecf91bc13793d83a708dba3c6d96f648024efb";

struct Expected {
    name: &'static str,
    raw_sha256: &'static str,
    normalized_source_sha256: &'static str,
    options_sha256: &'static str,
    candidates: usize,
    successful_repeats: &'static [usize],
    svgs: &'static [(&'static str, usize, &'static str)],
    execution_state: &'static str,
    run_outcomes: [usize; 3],
    phase_statuses: [usize; 4],
    capabilities: usize,
    issues: usize,
    metrics: usize,
}

const COMPLETE_SVGS: &[(&str, usize, &str)] = &[
    (
        "section-000-cohort-000-chart-000.svg",
        2780,
        "041fff9b8543bfee3fa0b66981e2b2dad662ccebe6ee29610ba2b8062c88d62e",
    ),
    (
        "section-000-cohort-000-chart-001.svg",
        2615,
        "dc90fe3dc33c4c9906cbbcb2b4d2ec8b35f1fffacb468446682d2a16129690be",
    ),
    (
        "section-000-cohort-000-chart-002.svg",
        2414,
        "db9dbde24ad64956b4a45a002a310e223490df34cc981bfb23cf9ea3503faeda",
    ),
    (
        "section-000-cohort-000-chart-003.svg",
        2746,
        "3845551009ea39d5be7b5ec7a4c35a06293885345d6c212356cc6095ec0b78cc",
    ),
    (
        "section-000-cohort-000-chart-004.svg",
        2353,
        "d0ccb7ad3b50490bdfb55cae36de3edc436286d99978363519a25804128b890d",
    ),
    (
        "section-000-cohort-000-chart-005.svg",
        2581,
        "02552066f2530259b92ec1083b4448317d1bc16428c96bdb2740ef15d044b666",
    ),
];
const PARTIAL_SVGS: &[(&str, usize, &str)] = &[
    (
        "section-000-cohort-000-chart-000.svg",
        2780,
        "041fff9b8543bfee3fa0b66981e2b2dad662ccebe6ee29610ba2b8062c88d62e",
    ),
    (
        "section-000-cohort-000-chart-001.svg",
        2615,
        "dc90fe3dc33c4c9906cbbcb2b4d2ec8b35f1fffacb468446682d2a16129690be",
    ),
    (
        "section-000-cohort-000-chart-002.svg",
        2414,
        "db9dbde24ad64956b4a45a002a310e223490df34cc981bfb23cf9ea3503faeda",
    ),
];
const CASES: &[Expected] = &[
    Expected {
        name: "complete",
        raw_sha256: "94040b0ee669945f9ad0431c3e232c679fd5ebfd04f605f8448f102c5db34fc2",
        normalized_source_sha256: "5e51f1e2b4c4c0b79fc474b4067760e2684b704a4be9e75417c95ead95a1d1cb",
        options_sha256: "0d4afae61a2d3eb649ff95b86718a4798d6bc31017aad0191299d68707fed7a2",
        candidates: 1,
        successful_repeats: &[1],
        svgs: COMPLETE_SVGS,
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
        normalized_source_sha256: "b16dbc7cc641c4dfa4f9fc14f1f0bc9c772872c3c7d7a41c6b9dc7e968ced3e4",
        options_sha256: "083aebf3d5c745172ae3521353b29e05adf2a37dbe7d3eabdb43412ffebca456",
        candidates: 1,
        successful_repeats: &[0],
        svgs: PARTIAL_SVGS,
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
        normalized_source_sha256: "c24559f9fe2ecdcf076d9d6fb9875b0cfe10b0d3a4668b6eb9d4663f0ba17c53",
        options_sha256: "3aa8ef90c4684239ddb99650915a2d5d2ade7711b970911f8ba285b81cc97378",
        candidates: 2,
        successful_repeats: &[0, 0],
        svgs: &[],
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
        normalized_source_sha256: "a7f035c7ae34a7e627939d69225442a5e5e7d6bab2f1d19b65b5c6e525c42ac4",
        options_sha256: "d6e84e49655b18e58de8f07d3f84bba4891543b747547d4ecfce8a23c3159959",
        candidates: 1,
        successful_repeats: &[0],
        svgs: &[],
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

fn canonical_digest(value: &Value) -> String {
    digest(canonical_json(value))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => {
            if value.is_i64() || value.is_u64() {
                return value.to_string();
            }
            let value = value.as_f64().unwrap();
            if value != 0.0 && value.abs() < 0.0001 {
                let scientific = format!("{value:e}");
                let (mantissa, exponent) = scientific.split_once('e').unwrap();
                let exponent: i32 = exponent.parse().unwrap();
                format!("{mantissa}e{exponent:+03}")
            } else {
                serde_json::Number::from_f64(value).unwrap().to_string()
            }
        }
        Value::String(value) => serde_json::to_string(value).unwrap(),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(object) => format!(
            "{{{}}}",
            object
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}",
                    serde_json::to_string(key).unwrap(),
                    canonical_json(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
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
    for artifact in manifest["artifacts"].as_array().unwrap() {
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

fn echarts_options(html: &str) -> Vec<Value> {
    let mut options = Vec::new();
    let mut offset = 0;
    while let Some(relative) = html[offset..].find("setOption(") {
        let call = offset + relative + "setOption(".len();
        let start = call
            + html[call..]
                .find(|character: char| !character.is_whitespace())
                .unwrap();
        if !matches!(html.as_bytes()[start], b'{' | b'[')
            || (html.as_bytes()[start] == b'{'
                && html.as_bytes().get(start + 1) != Some(&b'"'))
        {
            offset = call;
            continue;
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
                    assert_eq!(stack.pop(), Some(*byte));
                    if stack.is_empty() {
                        end = Some(index + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.expect("balanced literal setOption JSON");
        options.push(serde_json::from_str(&html[start..end]).unwrap());
        offset = end;
    }
    options
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

#[test]
fn neutral_cookout_report_preserves_experiment_evidence_and_publication() {
    let root = tempfile::tempdir().unwrap();

    for expected in CASES {
        let input_path = fixture(expected.name);
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
        assert_eq!(report["schema"], "cookout.report");
        assert!(report.get("view").is_none());
        assert_eq!(
            report["inputs"][0]["identity"]["name"],
            "Voxel performance report"
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
        }
        assert_eq!(
            input["capabilities"].as_array().unwrap().len(),
            expected.capabilities
        );
        let constraints = input["target"]["constraints"].as_array().unwrap();
        assert_eq!(constraints.len(), 2);
        assert_eq!(constraints[0]["capability"], "matrix_host_storage_scope");
        assert_eq!(
            constraints[1]["capability"],
            "clean_launch_teardown_boundaries"
        );

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
        let first_options = echarts_options(
            &fs::read_to_string(first.join("report.html")).unwrap(),
        );
        let second_options = echarts_options(
            &fs::read_to_string(second.join("report.html")).unwrap(),
        );
        assert_eq!(first_options.len(), expected.metrics);
        assert_eq!(first_options, second_options);

        let serialized =
            String::from_utf8(fs::read(first.join("report.json")).unwrap())
                .unwrap()
                .to_lowercase();
        for sensitive in [
            "token",
            "password",
            "authorization",
            "bearer ",
            "api_key",
            "private_key",
            "ssh ",
            "omdb ",
            "helper command",
        ] {
            assert!(
                !serialized.contains(sensitive),
                "{} exposed {sensitive}",
                expected.name
            );
        }
        let svg_names = artifact_names(&first)
            .into_iter()
            .filter(|name| name.ends_with(".svg"))
            .collect::<Vec<_>>();
        assert_eq!(svg_names.len(), expected.metrics);
        for (metric, name) in svg_names.iter().enumerate() {
            assert_eq!(name, &format!("cohort-000-metric-{metric:03}.svg"));
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
    }
}

#[test]
fn raw_voxel_fixtures_match_the_reviewed_cookout_report_oracle() {
    assert_eq!(
        RENDERER_ORACLE_GENERATION_COOKOUT_COMMIT,
        "60ecf91bc13793d83a708dba3c6d96f648024efb"
    );
    assert_eq!(
        RAW_FIXTURE_REVISION,
        "3ed372e6168ae99c1e19a1d3fcc8f70cb41eccf2"
    );
    assert_eq!(
        RENDERER_ORACLE_REVISION,
        "60ecf91bc13793d83a708dba3c6d96f648024efb"
    );
    let root = tempfile::tempdir().unwrap();
    let mut archives = Vec::new();

    for expected in CASES {
        let input = fixture(expected.name);
        assert_eq!(digest(fs::read(&input).unwrap()), expected.raw_sha256);
        let output = root.path().join(expected.name);
        voxel(&[
            Path::new("report"),
            &input,
            Path::new("--out"),
            &output,
            Path::new("--archive"),
        ]);

        let report: Value = serde_json::from_slice(
            &fs::read(output.join("report.json")).unwrap(),
        )
        .unwrap();
        let view = &report["view"];
        assert_eq!(
            view["title"], "Voxel performance report",
            "{} title",
            expected.name
        );
        assert_eq!(
            view["sections"][0]["title"], "Storage levers",
            "{} section",
            expected.name
        );
        let candidates =
            view["sections"][0]["cohorts"][0]["candidates"].as_array().unwrap();
        assert_eq!(
            candidates.len(),
            expected.candidates,
            "{} candidates",
            expected.name
        );
        for (index, candidate) in candidates.iter().enumerate() {
            let canonical = candidate["key"]["configuration"]
                .as_array()
                .unwrap()
                .iter()
                .map(|lever| lever.as_u64().unwrap().to_string())
                .collect::<Vec<_>>()
                .join("+");
            let canonical = if canonical.is_empty() {
                "none".to_owned()
            } else {
                canonical
            };
            assert_eq!(
                candidate["label"],
                format!("{canonical} — {canonical}"),
                "{} candidate {index} label",
                expected.name
            );
            assert_eq!(
                candidate["successful_repeats"],
                expected.successful_repeats[index],
                "{} candidate {index} successful repeats",
                expected.name
            );
        }
        assert_eq!(
            view["inputs"][0]["sha256"], expected.normalized_source_sha256,
            "{} normalized source provenance digest",
            expected.name
        );
        let options = report["view"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|section| section["cohorts"].as_array().unwrap())
            .flat_map(|cohort| cohort["charts"].as_array().unwrap())
            .map(|chart| chart["option"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_digest(&Value::Array(options)),
            expected.options_sha256,
            "{} ECharts options",
            expected.name
        );
        let html = fs::read_to_string(output.join("report.html")).unwrap();
        assert!(html.contains("Voxel performance report"));
        assert!(html.contains("Embedded Apache ECharts"));
        assert!(!html.contains("https://cdn"));

        let mut actual_svgs = fs::read_dir(&output)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "svg")
            })
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual_svgs.sort();
        let expected_svgs = expected
            .svgs
            .iter()
            .map(|entry| entry.0.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            actual_svgs, expected_svgs,
            "{} SVG inventory",
            expected.name
        );
        for (name, bytes, sha256) in expected.svgs {
            let actual = fs::read(output.join(name)).unwrap();
            assert_eq!(actual.len(), *bytes, "{name} bytes");
            assert_eq!(digest(actual), *sha256, "{name} corrected bytes");
        }
        verify_manifest(&output);
        archives.push(root.path().join(format!("{}.tar.gz", expected.name)));
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
    let aggregate = root.path().join("aggregate");
    let mut args = vec![Path::new("superreport")];
    args.extend(archives.iter().map(PathBuf::as_path));
    args.extend([
        Path::new("--out"),
        aggregate.as_path(),
        Path::new("--archive"),
    ]);
    voxel(&args);
    let report: Value = serde_json::from_slice(
        &fs::read(aggregate.join("report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["aggregation"]["unique_input_count"], 4);
    assert_eq!(report["aggregation"]["duplicate_count"], 1);
    verify_manifest(&aggregate);

    let recursive = root.path().join("recursive");
    voxel(&[
        Path::new("superreport"),
        &root.path().join("aggregate.tar.gz"),
        archives[0].as_path(),
        Path::new("--out"),
        &recursive,
    ]);
    let report: Value = serde_json::from_slice(
        &fs::read(recursive.join("report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["aggregation"]["unique_input_count"], 4);
    assert_eq!(report["aggregation"]["duplicate_count"], 1);
    verify_manifest(&recursive);
}
