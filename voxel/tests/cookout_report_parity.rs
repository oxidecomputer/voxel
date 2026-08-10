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
    "d8f6d922dc8424c047b2858dcb18884d894da0c5";
const RAW_FIXTURE_REVISION: &str = "3ed372e6168ae99c1e19a1d3fcc8f70cb41eccf2";
const RENDERER_ORACLE_REVISION: &str =
    "fcc9e8c2a1ac1e80278ef9e785da061cd5e0b144";

struct Expected {
    name: &'static str,
    raw_sha256: &'static str,
    normalized_source_sha256: &'static str,
    options_sha256: &'static str,
    candidates: usize,
    successful_repeats: &'static [usize],
    svgs: &'static [(&'static str, usize, &'static str)],
}

const COMPLETE_SVGS: &[(&str, usize, &str)] = &[
    (
        "section-000-cohort-000-chart-000.svg",
        1747,
        "7a64a4d5dad0e8a2b4c67147a8dd8edee569d59ff4d7d4ed9685bc2cb5c69b0c",
    ),
    (
        "section-000-cohort-000-chart-001.svg",
        1679,
        "7dab7d62a17f5bbd3e680a48009ef05558a1b88eaee0266eb515cf0a927ce855",
    ),
    (
        "section-000-cohort-000-chart-002.svg",
        1725,
        "b6132b11067914f4f22bd8f39e0f03090af7392303be0331c2fbe16beb9a6267",
    ),
    (
        "section-000-cohort-000-chart-003.svg",
        1737,
        "cf070dbf8e50c2fcc5b67abf8d696b0d5ab305d7ba59a9da347b998b3ad48028",
    ),
    (
        "section-000-cohort-000-chart-004.svg",
        1677,
        "2a21296e8aae7b2c29fca9b13444edb034c28222ab4399241b4f0008735e2f81",
    ),
    (
        "section-000-cohort-000-chart-005.svg",
        1665,
        "3e1c80190ff0513c56b0e4ca1ef3b1dbab3c4e85fbc5a272b3a701d67117cfe0",
    ),
];
const PARTIAL_SVGS: &[(&str, usize, &str)] = &[
    (
        "section-000-cohort-000-chart-000.svg",
        1747,
        "7a64a4d5dad0e8a2b4c67147a8dd8edee569d59ff4d7d4ed9685bc2cb5c69b0c",
    ),
    (
        "section-000-cohort-000-chart-001.svg",
        1679,
        "7dab7d62a17f5bbd3e680a48009ef05558a1b88eaee0266eb515cf0a927ce855",
    ),
    (
        "section-000-cohort-000-chart-002.svg",
        1725,
        "b6132b11067914f4f22bd8f39e0f03090af7392303be0331c2fbe16beb9a6267",
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
    },
    Expected {
        name: "partial",
        raw_sha256: "fd7770243f98f93658988d30491f3981b0951ec7099a843479a0604b3999e898",
        normalized_source_sha256: "b16dbc7cc641c4dfa4f9fc14f1f0bc9c772872c3c7d7a41c6b9dc7e968ced3e4",
        options_sha256: "083aebf3d5c745172ae3521353b29e05adf2a37dbe7d3eabdb43412ffebca456",
        candidates: 1,
        successful_repeats: &[0],
        svgs: PARTIAL_SVGS,
    },
    Expected {
        name: "failed",
        raw_sha256: "27afc4ba08129ef345777f588f54173881d392c9403f10edfb06b10a57adfcf1",
        normalized_source_sha256: "c24559f9fe2ecdcf076d9d6fb9875b0cfe10b0d3a4668b6eb9d4663f0ba17c53",
        options_sha256: "3aa8ef90c4684239ddb99650915a2d5d2ade7711b970911f8ba285b81cc97378",
        candidates: 2,
        successful_repeats: &[0, 0],
        svgs: &[],
    },
    Expected {
        name: "checkpoint",
        raw_sha256: "5a7e040ff0724ce33258e5774cea2fd49d32e537191baaba7b4dd9e492b3f200",
        normalized_source_sha256: "a7f035c7ae34a7e627939d69225442a5e5e7d6bab2f1d19b65b5c6e525c42ac4",
        options_sha256: "d6e84e49655b18e58de8f07d3f84bba4891543b747547d4ecfce8a23c3159959",
        candidates: 1,
        successful_repeats: &[0],
        svgs: &[],
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

#[test]
fn raw_voxel_fixtures_match_the_reviewed_cookout_report_oracle() {
    assert_eq!(
        RENDERER_ORACLE_GENERATION_COOKOUT_COMMIT,
        "d8f6d922dc8424c047b2858dcb18884d894da0c5"
    );
    assert_eq!(
        RAW_FIXTURE_REVISION,
        "3ed372e6168ae99c1e19a1d3fcc8f70cb41eccf2"
    );
    assert_eq!(
        RENDERER_ORACLE_REVISION,
        "fcc9e8c2a1ac1e80278ef9e785da061cd5e0b144"
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
