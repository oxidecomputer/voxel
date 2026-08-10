//! Safe, in-memory ingestion for native report archives.

use super::{
    AggregationMetadata, MANIFEST_SCHEMA, MAX_MANIFEST_JSON, MAX_REPORT_HTML,
    MAX_REPORT_JSON, Manifest, PreparedInput, REPORT_GENERATOR,
    RejectedArchive, check_candidate_report_size, check_manifest_size,
    generate_and_publish_report, parse_normalized_report_document,
};
use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MAX_ENTRIES: usize = 16;
const MAX_ARCHIVES: usize = 64;
const MAX_UNIQUE_INPUTS: usize = 4096;
const MAX_NORMALIZED_EVIDENCE: usize = 48 * 1024 * 1024;
const MAX_TAR_OVERHEAD: u64 = 4 * 1024 * 1024;
const MAX_DECOMPRESSED_ARCHIVE: u64 =
    MAX_REPORT_JSON + MAX_MANIFEST_JSON + MAX_REPORT_HTML + MAX_TAR_OVERHEAD;
const MAX_DISPLAY_PATH: usize = 128;
const MAX_REJECTION_REASON: usize = 256;
const MAX_ORIGIN: usize = 4096;
const MAX_ACCEPTED_PROVENANCE: usize = 640 * 1024;
const MAX_REJECTED_PROVENANCE: usize = 256 * 1024;
// Every byte in both bounded strings may require a six-byte JSON escape. The
// additional 256 bytes per entry covers keys, punctuation, and pretty-printing.
const REJECTION_MANIFEST_RESERVE: u64 = (MAX_ARCHIVES
    * (6 * (MAX_DISPLAY_PATH + MAX_REJECTION_REASON) + 256))
    as u64;
// A rejection is represented both in aggregation metadata and JSON warnings;
// HTML additionally escapes it. Reserve for every archive before accepting
// evidence so all later bounded rejections remain publishable.
const REJECTION_REPORT_RESERVE: u64 = (MAX_ARCHIVES
    * (12 * (MAX_DISPLAY_PATH + MAX_REJECTION_REASON) + 1024))
    as u64;
const REJECTION_HTML_RESERVE: u64 = (MAX_ARCHIVES
    * (6 * (MAX_DISPLAY_PATH + MAX_REJECTION_REASON) + 1024))
    as u64;

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

fn provenance_cost(value: &impl serde::Serialize) -> Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .context("measure aggregation provenance")
}

fn push_rejection(
    rejected: &mut Vec<RejectedArchive>,
    used: &mut usize,
    path: &Path,
    reason: &str,
) {
    let item = RejectedArchive {
        path: truncate_utf8(&path.display().to_string(), MAX_DISPLAY_PATH),
        reason: truncate_utf8(reason, MAX_REJECTION_REASON),
    };
    let cost = provenance_cost(&item).unwrap_or(MAX_REJECTED_PROVENANCE);
    *used = used.saturating_add(cost);
    rejected.push(item);
}

pub(crate) fn run(
    archives: &[PathBuf],
    out: &Path,
    archive: bool,
) -> Result<()> {
    run_with_accepted_manifest_limit(
        archives,
        out,
        archive,
        MAX_MANIFEST_JSON - REJECTION_MANIFEST_RESERVE,
    )
}

fn run_with_accepted_manifest_limit(
    archives: &[PathBuf],
    out: &Path,
    archive: bool,
    accepted_manifest_limit: u64,
) -> Result<()> {
    run_with_limits(
        archives,
        out,
        archive,
        accepted_manifest_limit,
        MAX_REPORT_JSON - REJECTION_REPORT_RESERVE,
        MAX_REPORT_HTML - REJECTION_HTML_RESERVE,
    )
}

fn run_with_limits(
    archives: &[PathBuf],
    out: &Path,
    archive: bool,
    accepted_manifest_limit: u64,
    accepted_json_limit: u64,
    accepted_html_limit: u64,
) -> Result<()> {
    if archives.len() > MAX_ARCHIVES {
        bail!(
            "at most {MAX_ARCHIVES} report archives may be aggregated at once"
        );
    }
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    let mut unique = Vec::<PreparedInput>::new();
    let mut seen = BTreeSet::new();
    let mut origins = BTreeMap::<String, Vec<String>>::new();
    let mut duplicate_count: usize = 0;
    let mut normalized_evidence_bytes: usize = 0;
    let mut accepted_provenance_bytes = 0usize;
    let mut rejected_provenance_bytes = 0usize;
    let mut fingerprints = BTreeMap::<String, String>::new();

    for path in archives {
        match read_archive(path) {
            Ok(inputs) => {
                let mut batch_new = BTreeSet::new();
                let mut batch_fingerprints = BTreeMap::new();
                let batch_evidence = inputs
                    .iter()
                    .map(|input| {
                        let digest = input.digest().to_owned();
                        let fingerprint = input.normalized_fingerprint()?;
                        if fingerprints
                            .get(&digest)
                            .is_some_and(|known| known != &fingerprint)
                        {
                            bail!("digest {digest} carries conflicting normalized evidence");
                        }
                        if batch_fingerprints
                            .insert(digest.clone(), fingerprint.clone())
                            .is_some_and(|known| known != fingerprint)
                        {
                            bail!("digest {digest} carries conflicting normalized evidence");
                        }
                        let is_new = !seen.contains(&digest) && batch_new.insert(digest);
                        Ok((is_new, input.normalized_size()?, fingerprint))
                    })
                    .collect::<Result<Vec<_>>>();
                let batch_evidence = match batch_evidence {
                    Ok(items) => items,
                    Err(error) => {
                        push_rejection(
                            &mut rejected,
                            &mut rejected_provenance_bytes,
                            path,
                            &format!("{error:#}"),
                        );
                        continue;
                    }
                };
                let batch_bytes = batch_evidence
                    .iter()
                    .filter(|item| item.0)
                    .map(|item| item.1)
                    .sum::<usize>();
                let display_path = truncate_utf8(
                    &path.display().to_string(),
                    MAX_DISPLAY_PATH,
                );
                let mut batch_origins = Vec::with_capacity(inputs.len());
                let mut batch_provenance = provenance_cost(&display_path)?;
                let mut invalid_origin = None;
                for input in &inputs {
                    let origin =
                        format!("{}#{}", path.display(), input.source());
                    if origin.len() > MAX_ORIGIN {
                        invalid_origin = Some(
                            "archive source identity is too long for aggregate provenance",
                        );
                        break;
                    }
                    batch_provenance = batch_provenance.saturating_add(
                        provenance_cost(&(input.digest(), &origin))?,
                    );
                    if !seen.contains(input.digest()) {
                        batch_provenance = batch_provenance.saturating_add(
                            provenance_cost(&(input.source(), input.digest()))?,
                        );
                    }
                    batch_origins.push(origin);
                }
                if unique.len() + batch_new.len() > MAX_UNIQUE_INPUTS
                    || normalized_evidence_bytes.saturating_add(batch_bytes)
                        > MAX_NORMALIZED_EVIDENCE
                    || accepted_provenance_bytes
                        .saturating_add(batch_provenance)
                        > MAX_ACCEPTED_PROVENANCE
                    || invalid_origin.is_some()
                {
                    push_rejection(&mut rejected, &mut rejected_provenance_bytes, path,
                        invalid_origin.unwrap_or("archive batch exceeds aggregate evidence, input-count, or provenance limits"));
                    continue;
                }

                let mut candidate_accepted = accepted.clone();
                candidate_accepted.push(display_path.clone());
                let mut candidate_origins = origins.clone();
                let mut candidate_digests = unique
                    .iter()
                    .map(|input| input.digest().to_string())
                    .collect::<Vec<_>>();
                let mut candidate_inputs = unique
                    .iter()
                    .map(|input| (input.source(), input.digest()))
                    .collect::<Vec<_>>();
                let mut candidate_duplicate_count = duplicate_count;
                let mut candidate_seen = seen.clone();
                for (input, origin) in inputs.iter().zip(&batch_origins) {
                    candidate_origins
                        .entry(input.digest().to_string())
                        .or_default()
                        .push(origin.clone());
                    if candidate_seen.insert(input.digest().to_string()) {
                        candidate_digests.push(input.digest().to_string());
                        candidate_inputs.push((input.source(), input.digest()));
                    } else {
                        candidate_duplicate_count = candidate_duplicate_count
                            .checked_add(1)
                            .context("aggregate duplicate count overflow")?;
                    }
                }
                let candidate_metadata = AggregationMetadata {
                    accepted_archives: candidate_accepted,
                    rejected_archives: rejected.clone(),
                    unique_input_count: candidate_inputs.len(),
                    duplicate_count: candidate_duplicate_count,
                    digest_order: candidate_digests,
                    origins: candidate_origins,
                };
                if let Err(error) = check_manifest_size(
                    &candidate_inputs,
                    Some(&candidate_metadata),
                    accepted_manifest_limit,
                ) {
                    push_rejection(
                        &mut rejected,
                        &mut rejected_provenance_bytes,
                        path,
                        &format!("{error:#}"),
                    );
                    continue;
                }
                let mut candidate_prepared = unique.clone();
                let mut candidate_seen_for_inputs = seen.clone();
                candidate_prepared.extend(
                    inputs
                        .iter()
                        .filter(|input| {
                            candidate_seen_for_inputs
                                .insert(input.digest().to_string())
                        })
                        .cloned()
                        .map(super::ReplayInput::into_prepared),
                );
                if let Err(error) = check_candidate_report_size(
                    &candidate_prepared,
                    &candidate_metadata,
                    accepted_json_limit,
                    accepted_html_limit,
                ) {
                    push_rejection(
                        &mut rejected,
                        &mut rejected_provenance_bytes,
                        path,
                        &format!("{error:#}"),
                    );
                    continue;
                }
                accepted.push(display_path);
                normalized_evidence_bytes += batch_bytes;
                accepted_provenance_bytes += batch_provenance;
                for ((input, origin), (_, _, fingerprint)) in
                    inputs.into_iter().zip(batch_origins).zip(batch_evidence)
                {
                    let digest = input.digest().to_string();
                    origins.entry(digest.clone()).or_default().push(origin);
                    fingerprints.entry(digest.clone()).or_insert(fingerprint);
                    if seen.insert(digest) {
                        unique.push(input.into_prepared());
                    } else {
                        duplicate_count = duplicate_count
                            .checked_add(1)
                            .context("aggregate duplicate count overflow")?;
                    }
                }
            }
            Err(error) => push_rejection(
                &mut rejected,
                &mut rejected_provenance_bytes,
                path,
                &format!("{error:#}"),
            ),
        }
    }
    if unique.is_empty() {
        let details = rejected
            .iter()
            .map(|item| format!("{}: {}", item.path, item.reason))
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "no valid unique normalized inputs were found{details}",
            details = if details.is_empty() {
                String::new()
            } else {
                format!(": {details}")
            }
        );
    }
    let metadata = AggregationMetadata {
        accepted_archives: accepted,
        rejected_archives: rejected,
        unique_input_count: unique.len(),
        duplicate_count,
        digest_order: unique
            .iter()
            .map(|input| input.digest().to_string())
            .collect(),
        origins,
    };
    let archive_path = PathBuf::from(format!("{}.tar.gz", out.display()));
    generate_and_publish_report(
        &unique,
        out,
        archive,
        &archive_path,
        Some(&metadata),
    )?;
    println!(
        "archives: {} accepted, {} rejected; unique inputs: {}; duplicates: {}",
        metadata.accepted_archives.len(),
        metadata.rejected_archives.len(),
        metadata.unique_input_count,
        metadata.duplicate_count
    );
    for item in &metadata.rejected_archives {
        println!("rejected {}: {}", item.path, item.reason);
    }
    Ok(())
}

fn read_archive(path: &Path) -> Result<Vec<super::ReplayInput>> {
    let file = File::open(path)
        .with_context(|| format!("open archive {}", path.display()))?;
    let decoder = MultiGzDecoder::new(file).take(MAX_DECOMPRESSED_ARCHIVE + 1);
    let mut archive = tar::Archive::new(decoder);
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut top: Option<String> = None;
    let mut count = 0;
    for entry in archive.entries().context("read tar entries")? {
        count += 1;
        if count > MAX_ENTRIES {
            bail!("archive contains more than {MAX_ENTRIES} entries");
        }
        let mut entry = entry.context("read tar entry")?;
        if !entry.header().entry_type().is_file() {
            bail!("archive entry is not a regular file");
        }
        let entry_path = entry.path().context("decode tar entry path")?;
        let components = entry_path.components().collect::<Vec<_>>();
        if components.len() != 2
            || components
                .iter()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            bail!(
                "unsafe or unexpected archive layout: {}",
                entry_path.display()
            );
        }
        let root = components[0]
            .as_os_str()
            .to_str()
            .context("archive path is not UTF-8")?
            .to_owned();
        if top.get_or_insert_with(|| root.clone()) != &root {
            bail!("archive has more than one top-level directory");
        }
        let name = components[1]
            .as_os_str()
            .to_str()
            .context("archive path is not UTF-8")?
            .to_owned();
        let limit = match name.as_str() {
            "report.json" => MAX_REPORT_JSON,
            "manifest.json" => MAX_MANIFEST_JSON,
            "report.html" => MAX_REPORT_HTML,
            _ => bail!("unexpected archive artifact '{name}'"),
        };
        let declared = entry.size();
        if declared > limit {
            bail!("archive artifact '{name}' exceeds {limit} byte limit");
        }
        if files.contains_key(&name) {
            bail!("duplicate archive artifact '{name}'");
        }
        let mut bytes = Vec::with_capacity(declared as usize);
        let mut bounded = entry.by_ref().take(limit + 1);
        bounded
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {name}"))?;
        if bytes.len() as u64 != declared {
            bail!("archive artifact '{name}' size does not match its header");
        }
        files.insert(name, bytes);
    }
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())
        .context("finish gzip stream")?;
    if decoder.limit() == 0 {
        bail!(
            "archive exceeds {MAX_DECOMPRESSED_ARCHIVE} decompressed byte limit"
        );
    }
    if files.len() != 3 {
        bail!(
            "archive must contain exactly report.json, manifest.json, and report.html"
        );
    }
    let manifest: Manifest = serde_json::from_slice(&files["manifest.json"])
        .context("parse manifest.json")?;
    if manifest.schema != MANIFEST_SCHEMA
        || manifest.report_generator != REPORT_GENERATOR
    {
        bail!("manifest schema or generator is incompatible");
    }
    if manifest.manifest_filename != "manifest.json"
        || manifest.report_json.filename != "report.json"
        || manifest.report_html.filename != "report.html"
    {
        bail!("manifest artifact filenames are invalid");
    }
    verify_digest(
        "report.json",
        &files["report.json"],
        &manifest.report_json.sha256,
    )?;
    verify_digest(
        "report.html",
        &files["report.html"],
        &manifest.report_html.sha256,
    )?;
    let replay = parse_normalized_report_document(&files["report.json"])
        .context("validate replay evidence")?;
    if manifest.aggregation != replay.aggregation {
        bail!("manifest aggregation does not match report.json aggregation");
    }
    if manifest.inputs.len() != replay.inputs.len()
        || manifest.inputs.iter().zip(&replay.inputs).any(
            |(manifest, replay)| {
                manifest.source_name != replay.source()
                    || manifest.sha256 != replay.digest()
            },
        )
    {
        bail!(
            "manifest input metadata does not match report.json replay metadata"
        );
    }
    Ok(replay.inputs)
}

fn verify_digest(name: &str, bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("{name} SHA-256 mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use serde_json::{Value, json};
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    fn matrix(name: &str) -> Value {
        json!({
            "schema_version": 2, "name": name, "started": 1, "ended": 2,
            "load": false, "rss_sleds": 3, "repeat": 1, "combos": ["none"],
            "results": [{"label": "none", "levers": [], "repeats": [
                {"bringup_bytes": 42, "launch_secs": 7, "peak_ram_bytes": 1024}
            ]}]
        })
    }

    fn ordinary_archive(root: &Path, name: &str) -> PathBuf {
        let input = root.join(format!("{name}.json"));
        fs::write(&input, serde_json::to_vec(&matrix(name)).unwrap()).unwrap();
        let out = root.join(format!("{name}-report"));
        super::super::run(&[input], &out, true).unwrap();
        root.join(format!("{name}-report.tar.gz"))
    }

    fn archive_from_value(root: &Path, name: &str, value: Value) -> PathBuf {
        let input = root.join(format!("{name}.json"));
        fs::write(&input, serde_json::to_vec(&value).unwrap()).unwrap();
        let out = root.join(format!("{name}-report"));
        super::super::run(&[input], &out, true).unwrap();
        root.join(format!("{name}-report.tar.gz"))
    }

    fn archive_files(path: &Path) -> BTreeMap<String, Vec<u8>> {
        let decoder = MultiGzDecoder::new(File::open(path).unwrap());
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = entry
                    .path()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (name, bytes)
            })
            .collect()
    }

    fn write_archive(path: &Path, files: &BTreeMap<String, Vec<u8>>) {
        let encoder =
            GzEncoder::new(File::create(path).unwrap(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    Path::new("report").join(name),
                    bytes.as_slice(),
                )
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap().flush().unwrap();
    }

    fn write_entries(
        path: &Path,
        entries: &[(&str, tar::EntryType, &[u8], u64)],
    ) {
        let mut encoder =
            GzEncoder::new(File::create(path).unwrap(), Compression::default());
        for (name, kind, bytes, declared) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_entry_type(*kind);
            header.set_size(*declared);
            header.set_mode(0o644);
            header.set_cksum();
            encoder.write_all(header.as_bytes()).unwrap();
            encoder.write_all(bytes).unwrap();
            let padding = (512 - bytes.len() % 512) % 512;
            encoder.write_all(&vec![0; padding]).unwrap();
        }
        encoder.write_all(&[0; 1024]).unwrap();
        encoder.finish().unwrap();
    }

    fn report_json(out: &Path) -> Value {
        serde_json::from_slice(&fs::read(out.join("report.json")).unwrap())
            .unwrap()
    }

    fn expected_svg_names(document: &Value) -> Vec<String> {
        let mut names = Vec::new();
        for (section_index, section) in
            document["view"]["sections"].as_array().unwrap().iter().enumerate()
        {
            if let Some(charts) =
                section["descriptive_aggregate"]["charts"].as_array()
            {
                for (chart_index, chart) in charts.iter().enumerate() {
                    if chart["fallback_rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| !row["value"].is_null())
                    {
                        names.push(format!(
                            "section-{section_index:03}-aggregate-chart-{chart_index:03}.svg"
                        ));
                    }
                }
            }
            for (cohort_index, cohort) in
                section["cohorts"].as_array().unwrap().iter().enumerate()
            {
                for (chart_index, chart) in
                    cohort["charts"].as_array().unwrap().iter().enumerate()
                {
                    if chart["fallback_rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| !row["value"].is_null())
                    {
                        names.push(format!(
                            "section-{section_index:03}-cohort-{cohort_index:03}-chart-{chart_index:03}.svg"
                        ));
                    }
                }
            }
        }
        names.sort();
        names
    }

    fn image_files(out: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(out.join("images"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                (
                    entry.file_name().into_string().unwrap(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn aggregates_disjoint_archives_and_deduplicates_overlap() {
        let root = tempdir().unwrap();
        let first = ordinary_archive(root.path(), "first");
        let second = ordinary_archive(root.path(), "second");
        let out = root.path().join("combined");
        run(&[first.clone(), second], &out, true).unwrap();
        let document = report_json(&out);
        assert_eq!(document["inputs"].as_array().unwrap().len(), 2);
        assert_eq!(document["normalized_inputs"].as_array().unwrap().len(), 2);
        assert_eq!(document["aggregation"]["unique_input_count"], 2);

        let deduped = root.path().join("deduped");
        run(&[first.clone(), first], &deduped, false).unwrap();
        let document = report_json(&deduped);
        assert_eq!(document["inputs"].as_array().unwrap().len(), 1);
        assert_eq!(document["aggregation"]["duplicate_count"], 1);
        let digest =
            document["aggregation"]["digest_order"][0].as_str().unwrap();
        assert_eq!(
            document["aggregation"]["origins"][digest]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn superreport_publishes_complete_deterministic_images_only_outside_archive()
     {
        let root = tempdir().unwrap();
        let first = ordinary_archive(root.path(), "svg-first");
        let second = ordinary_archive(root.path(), "svg-second");
        let out = root.path().join("svg-combined");
        run(&[first.clone(), second.clone()], &out, true).unwrap();

        let document = report_json(&out);
        let images = image_files(&out);
        assert_eq!(
            images.keys().cloned().collect::<Vec<_>>(),
            expected_svg_names(&document)
        );
        assert!(images.values().all(|bytes| bytes.starts_with(b"<svg ")));
        let html = fs::read(out.join("report.html")).unwrap();
        let manifest = fs::read(out.join("manifest.json")).unwrap();
        assert!(!String::from_utf8_lossy(&html).contains(".svg"));
        assert!(!String::from_utf8_lossy(&manifest).contains(".svg"));

        let archived = archive_files(&root.path().join("svg-combined.tar.gz"));
        assert_eq!(
            archived.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["manifest.json", "report.html", "report.json"]
        );
        for (name, bytes) in archived {
            assert_eq!(bytes, fs::read(out.join(name)).unwrap());
        }

        let second_out = root.path().join("svg-combined-again");
        run(&[first, second], &second_out, false).unwrap();
        assert_eq!(images, image_files(&second_out));
    }

    #[test]
    fn records_rejections_and_fails_cleanly_when_all_are_invalid() {
        let root = tempdir().unwrap();
        let valid = ordinary_archive(root.path(), "valid");
        let invalid = root.path().join("invalid.tar.gz");
        fs::write(&invalid, b"not gzip").unwrap();
        let out = root.path().join("partial");
        run(&[valid, invalid.clone()], &out, true).unwrap();
        let document = report_json(&out);
        assert_eq!(
            document["aggregation"]["rejected_archives"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(document["warnings"].as_array().unwrap().iter().any(
            |warning| {
                warning
                    .as_str()
                    .is_some_and(|warning| warning.contains("Rejected archive"))
            }
        ));
        let manifest: Value = serde_json::from_slice(
            &fs::read(out.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["aggregation"]["rejected_archives"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let html = fs::read_to_string(out.join("report.html")).unwrap();
        assert!(
            html.contains("Rejected:")
                && html.contains("Input digest order and origins")
        );

        let failed = root.path().join("failed");
        assert!(run(&[invalid], &failed, true).is_err());
        assert!(!failed.exists());
        assert!(!root.path().join("failed.tar.gz").exists());
    }

    #[test]
    fn excludes_manifest_oversize_batch_without_losing_prior_archive() {
        let root = tempdir().unwrap();
        let first = ordinary_archive(root.path(), "first-fit");
        let second = ordinary_archive(root.path(), "second-too-large");
        let baseline = root.path().join("baseline");
        run(std::slice::from_ref(&first), &baseline, false).unwrap();
        // The candidate helper uses u64::MAX rather than the shorter current
        // timestamp, so leave a small allowance while retaining a tight bound.
        let one_archive_limit =
            fs::read(baseline.join("manifest.json")).unwrap().len() as u64 + 16;

        let out = root.path().join("bounded");
        run_with_accepted_manifest_limit(
            &[first, second.clone()],
            &out,
            false,
            one_archive_limit,
        )
        .unwrap();
        let document = report_json(&out);
        assert_eq!(
            document["aggregation"]["accepted_archives"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            document["aggregation"]["rejected_archives"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            document["aggregation"]["rejected_archives"][0]["path"]
                .as_str()
                .unwrap()
                .contains("second-too-large")
        );
        assert_eq!(document["aggregation"]["unique_input_count"], 1);
    }

    #[test]
    fn rejects_exact_render_oversize_batch_without_losing_prior_evidence() {
        let root = tempdir().unwrap();
        let first = ordinary_archive(root.path(), "render-first");
        let baseline = root.path().join("render-baseline");
        run(std::slice::from_ref(&first), &baseline, false).unwrap();
        let json_limit = fs::read(baseline.join("report.json")).unwrap().len()
            as u64
            + 2_000;
        let html_limit = fs::read(baseline.join("report.html")).unwrap().len()
            as u64
            + 2_000;

        let mut amplified = matrix("render-amplified");
        amplified["name"] = json!("<&\"'".repeat(700));
        let second =
            archive_from_value(root.path(), "render-amplified", amplified);
        let out = root.path().join("render-bounded");
        run_with_limits(
            &[first, second],
            &out,
            false,
            MAX_MANIFEST_JSON,
            json_limit,
            html_limit,
        )
        .unwrap();

        let document = report_json(&out);
        assert_eq!(document["aggregation"]["unique_input_count"], 1);
        assert_eq!(
            document["aggregation"]["rejected_archives"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            fs::read(out.join("report.json")).unwrap().len() as u64
                <= json_limit
        );
        assert!(
            fs::read(out.join("report.html")).unwrap().len() as u64
                <= html_limit
        );
    }

    #[test]
    fn records_every_allowed_rejection_including_escaped_paths() {
        let root = tempdir().unwrap();
        let valid = ordinary_archive(root.path(), "valid-for-rejections");
        let mut archives = vec![valid];
        for index in 0..MAX_ARCHIVES - 1 {
            let path = root.path().join(format!("invalid-\"\\-{index}.tar.gz"));
            fs::write(&path, b"not gzip").unwrap();
            archives.push(path);
        }

        let out = root.path().join("all-rejections");
        run(&archives, &out, false).unwrap();
        let document = report_json(&out);
        let rejected =
            document["aggregation"]["rejected_archives"].as_array().unwrap();
        assert_eq!(rejected.len(), MAX_ARCHIVES - 1);
        assert!(rejected.iter().all(|item| {
            item["path"]
                .as_str()
                .is_some_and(|path| path.contains("invalid-\"\\-"))
        }));
    }

    #[test]
    fn recursively_accepts_superreports_and_preserves_deduplication() {
        let root = tempdir().unwrap();
        let ordinary = ordinary_archive(root.path(), "ordinary");
        let aggregate = root.path().join("aggregate");
        run(std::slice::from_ref(&ordinary), &aggregate, true).unwrap();
        let recursive = root.path().join("recursive");
        run(
            &[root.path().join("aggregate.tar.gz"), ordinary],
            &recursive,
            false,
        )
        .unwrap();
        let document = report_json(&recursive);
        assert_eq!(document["inputs"].as_array().unwrap().len(), 1);
        assert_eq!(document["aggregation"]["duplicate_count"], 1);
    }

    #[test]
    fn refuses_existing_output_directory_or_archive() {
        let root = tempdir().unwrap();
        let valid = ordinary_archive(root.path(), "valid");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("sentinel"), b"keep").unwrap();
        assert!(run(std::slice::from_ref(&valid), &directory, false).is_err());
        assert_eq!(fs::read(directory.join("sentinel")).unwrap(), b"keep");

        let out = root.path().join("archive-collision");
        let sibling = root.path().join("archive-collision.tar.gz");
        fs::write(&sibling, b"keep").unwrap();
        assert!(run(&[valid], &out, true).is_err());
        assert_eq!(fs::read(sibling).unwrap(), b"keep");
        assert!(!out.exists());
    }

    #[test]
    fn rejects_unsafe_non_file_duplicate_and_oversized_entries() {
        let root = tempdir().unwrap();
        let unsafe_path = root.path().join("unsafe.tar.gz");
        write_entries(
            &unsafe_path,
            &[("root/nested/report.json", tar::EntryType::Regular, b"x", 1)],
        );
        assert!(
            format!("{:#}", read_archive(&unsafe_path).unwrap_err())
                .contains("unsafe")
        );

        let symlink = root.path().join("symlink.tar.gz");
        write_entries(
            &symlink,
            &[("root/report.json", tar::EntryType::Symlink, b"", 0)],
        );
        assert!(
            format!("{:#}", read_archive(&symlink).unwrap_err())
                .contains("regular file")
        );

        let duplicate = root.path().join("duplicate.tar.gz");
        write_entries(
            &duplicate,
            &[
                ("root/report.json", tar::EntryType::Regular, b"x", 1),
                ("root/report.json", tar::EntryType::Regular, b"x", 1),
            ],
        );
        assert!(
            format!("{:#}", read_archive(&duplicate).unwrap_err())
                .contains("duplicate")
        );

        let oversized = root.path().join("oversized.tar.gz");
        write_entries(
            &oversized,
            &[(
                "root/manifest.json",
                tar::EntryType::Regular,
                b"",
                MAX_MANIFEST_JSON + 1,
            )],
        );
        assert!(
            format!("{:#}", read_archive(&oversized).unwrap_err())
                .contains("exceeds")
        );
    }

    #[test]
    fn rejects_manifest_input_metadata_mismatch() {
        let root = tempdir().unwrap();
        let valid = ordinary_archive(root.path(), "one");
        let mut files = archive_files(&valid);
        let mut manifest: Value =
            serde_json::from_slice(&files["manifest.json"]).unwrap();
        manifest["inputs"][0]["source_name"] = json!("different.json");
        files.insert(
            "manifest.json".into(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        );
        let mismatched = root.path().join("mismatched.tar.gz");
        write_archive(&mismatched, &files);

        let error = read_archive(&mismatched).unwrap_err();
        assert!(format!("{error:#}").contains("manifest input metadata"));
    }

    #[test]
    fn rejects_corrupt_gzip_trailer() {
        let root = tempdir().unwrap();
        let valid = ordinary_archive(root.path(), "trailer");
        let corrupt = root.path().join("corrupt.tar.gz");
        let mut bytes = fs::read(valid).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&corrupt, bytes).unwrap();

        assert!(read_archive(&corrupt).is_err());
    }

    #[test]
    fn rejects_corrupt_trailer_in_appended_gzip_member() {
        let root = tempdir().unwrap();
        let valid = ordinary_archive(root.path(), "multi-trailer");
        let corrupt = root.path().join("multi-corrupt.tar.gz");
        let mut bytes = fs::read(valid).unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"ignored second member").unwrap();
        let mut second = encoder.finish().unwrap();
        let last = second.len() - 1;
        second[last] ^= 0xff;
        bytes.extend(second);
        fs::write(&corrupt, bytes).unwrap();
        assert!(read_archive(&corrupt).is_err());
    }

    #[test]
    fn rejects_manifest_aggregation_mismatch() {
        let root = tempdir().unwrap();
        let ordinary = ordinary_archive(root.path(), "aggregation");
        let aggregate = root.path().join("aggregate");
        run(&[ordinary], &aggregate, true).unwrap();
        let valid = root.path().join("aggregate.tar.gz");
        let mut files = archive_files(&valid);
        let mut manifest: Value =
            serde_json::from_slice(&files["manifest.json"]).unwrap();
        manifest["aggregation"]["duplicate_count"] = json!(99);
        files.insert(
            "manifest.json".into(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        );
        let mismatched = root.path().join("aggregation-mismatch.tar.gz");
        write_archive(&mismatched, &files);

        let error = read_archive(&mismatched).unwrap_err();
        assert!(format!("{error:#}").contains("aggregation"));
    }

    #[test]
    fn rejects_repeated_digests_that_masquerade_as_unique() {
        let root = tempdir().unwrap();
        let ordinary = ordinary_archive(root.path(), "repeated-digest");
        let aggregate = root.path().join("aggregate");
        run(&[ordinary], &aggregate, true).unwrap();
        let valid = root.path().join("aggregate.tar.gz");
        let mut files = archive_files(&valid);
        let mut report: Value =
            serde_json::from_slice(&files["report.json"]).unwrap();
        let metadata = report["inputs"][0].clone();
        let normalized = report["normalized_inputs"][0].clone();
        report["inputs"].as_array_mut().unwrap().push(metadata);
        report["normalized_inputs"].as_array_mut().unwrap().push(normalized);
        let digest = report["aggregation"]["digest_order"][0].clone();
        report["aggregation"]["digest_order"] = json!([digest.clone(), digest]);
        report["aggregation"]["unique_input_count"] = json!(2);
        report["aggregation"]["origins"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(json!("duplicate-origin"));
        let report_bytes = serde_json::to_vec_pretty(&report).unwrap();
        files.insert("report.json".into(), report_bytes.clone());
        let mut manifest: Value =
            serde_json::from_slice(&files["manifest.json"]).unwrap();
        let manifest_input = manifest["inputs"][0].clone();
        manifest["inputs"].as_array_mut().unwrap().push(manifest_input);
        manifest["aggregation"] = report["aggregation"].clone();
        manifest["report_json"]["sha256"] =
            json!(format!("{:x}", Sha256::digest(&report_bytes)));
        files.insert(
            "manifest.json".into(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        );
        let malformed = root.path().join("repeated.tar.gz");
        write_archive(&malformed, &files);
        assert!(
            format!("{:#}", read_archive(&malformed).unwrap_err())
                .contains("not unique")
        );
    }
}
