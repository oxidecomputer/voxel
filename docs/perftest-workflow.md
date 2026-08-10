# Perftest workflow (INTERNAL)

> This runbook and the `voxel perftest` subcommand exist only to measure the
> disk-wear levers evaluated by this workflow.
> Remove both before the repository is open-sourced.

The immediate goal is to determine which storage changes reduce flash wear and
what each one costs. Success means that an operator can reproducibly evaluate
the quality-of-life objectives (less wear without unacceptable launch-time,
memory, or workload regressions), not that a run reproduces any historical
percentage. The Omicron revision is a fixed input to this experiment, not an
experimental variable. Use one known-working CP image, one FRR image, one Voxel
binary, and one topology for the benchmark.

The matrix measures three axes for every lever combination. Smoothness is an
optional follow-up for the finalists:

| Axis | Metric | Where |
| --- | --- | --- |
| wear | NVMe bytes written, scoped to the Falcon pool's drives | `matrix` `BRING-UP` and optional `WORKLOAD` columns |
| fast | rack bring-up time | `matrix` `LAUNCH` column |
| fits | peak host RAM | `matrix` `PEAK-RAM` column |
| smooth | control-plane operation latency and jitter | optional `perftest smooth` follow-up |

`matrix` runs the normal fail-closed launch path. A successful repeat therefore
requires the router and gimlet completion milestones, RSS completion, external
route reachability, the requested host and guest lever states, strict Falcon
drive scope, and clean pre/post-repeat teardown boundaries. Matrix evidence is
durable by stage: a successful launch remains descriptive evidence when its
workload fails, but the partial repeat is not recommendation-eligible. A launch
failure is retried once only after teardown and host-property reset prove
another clean boundary. A workload failure after a successful launch is not
retried by launching another rack. A failed clean-boundary proof aborts
immediately rather than continuing from uncertain state.

---

## One-time host preparation

### 1. Build and retain the exact Voxel binary under test

```sh
cargo build -p voxel
cargo test -p voxel perftest::       # run on Helios
```

Do not rebuild or switch Voxel revisions during the benchmark. Record the
binary version or source revision with the results.

### 2. Use a dedicated measurement pool

NVMe "Data Units Written" is a whole-drive counter. Put the Falcon dataset on
its own pool, on its own drive, with nothing else writing to it. Otherwise OS
background writes pollute the wear numbers, and repeated create/teardown churn
hammers the root pool.

```sh
# Replace this example with the dedicated measurement drive.
MEASUREMENT_DISK=c5t0d0
pfexec zpool create voxel "$MEASUREMENT_DISK"
voxel config set falcon.dataset voxel/falcon
```

### 3. Select one fixed, known-working image set

Prefer images that are already built and have completed a normal launch:

```sh
pfexec voxel image ls
CP_IMAGE=voxel-cp-REPLACE_WITH_KNOWN_WORKING_COMMIT
voxel config set image.cp "$CP_IMAGE"
voxel config set image.frr voxel-frr-proto
```

If an image must be built, build it once before the benchmark. Keep the same
resolved build root for image creation and launch so Voxel can find the
commit-pinned `voxel-rss-gen`:

```sh
COMMIT="SOME_OMICRON_COMMIT"
BUILD_ROOT="$HOME/voxel-builds"

voxel config set falcon.build_root "$BUILD_ROOT"
BUILD_ROOT="$BUILD_ROOT" voxel image create "$COMMIT"
voxel config set image.cp "voxel-cp-${COMMIT}"
```

The FRR image is independent of the Omicron commit currently checked out:

```sh
FALCON_DATASET=voxel/falcon bash voxel-image/build-frr.sh proto
voxel config set image.frr voxel-frr-proto
```

On a single-NIC host, an image-builder VNIC can flap the management link during
teardown. If needed, create one persistent VNIC and scope it only to each image
build command:

```sh
pfexec dladm create-vnic -l ixgbe0 voxel_ext0
EXT_INTERFACE=voxel_ext0 EXT_EXCLUSIVE=1 \
  BUILD_ROOT="$BUILD_ROOT" voxel image create "$COMMIT"
```

Do not export `EXT_INTERFACE` or `EXT_EXCLUSIVE` into the rack-launch shell.
Rack launch treats `EXT_INTERFACE` non-exclusively and cannot create a VNIC over
the `voxel_ext0` VNIC. Before launch, ensure that both variables are absent:

```sh
unset EXT_INTERFACE EXT_EXCLUSIVE
```

### 4. Dry-run the initial topology

Before starting the matrix, launch and destroy the configured topology once to
catch setup problems that would otherwise invalidate multiple runs:

```sh
pfexec voxel launch
pfexec voxel destroy
```

## Run the storage-lever matrix

Keep the Voxel binary, images, topology, Falcon pool, and host workload fixed
while the matrix runs. Run the repository's wrapper as your unprivileged user;
it elevates only the matrix process:

```sh
docs/run-perftest.sh run api-lifecycle
```

The wrapper defaults to `RESULTS_ROOT=$HOME/voxel-perftest-results`,
`VOXEL_BIN=voxel`, and `PFEXEC=pfexec`. Override them in the environment when
using another results location, retained binary, or privilege command:

```sh
RESULTS_ROOT=/var/tmp/voxel-results \
VOXEL_BIN=$HOME/bin/voxel-under-test \
PFEXEC=/usr/bin/pfexec \
  docs/run-perftest.sh run api-lifecycle
```

Labels may contain only ASCII letters, digits, dots, underscores, and hyphens.
By default `run` supplies `--workload api-disk-lifecycle`, `--repeat 3`, and
`--keep-going`. The operator wrapper always enables Voxel's presence-only
`--keep-going` flag: it removes every exact caller-supplied `--keep-going` and
appends one canonical flag, so Voxel always receives exactly one. Boolean
assignment forms are not wrapper controls.
It also uses the matrix's built-in cumulative ladder because it does not supply
`--combos`. Any matrix option may follow the label. The workload and repeat
defaults are overrideable: supplying `--workload` or `--repeat` explicitly
suppresses that option's wrapper default, and both `--option value` and
`--option=value` forms are recognized for these value-taking options. For
example:

```sh
docs/run-perftest.sh run six-repeats \
  --workload api-disk-lifecycle \
  --repeat=6 \
  --combos 'none;1;2;3;4;all'
```

The script prints the results directory before running Voxel and creates it
exclusively as:

```text
$RESULTS_ROOT/perftest-YYYYMMDD-HHMMSS-LABEL-PID/
├── batch.pid
├── invocation.txt
├── storage-levers.csv
├── storage-levers.json
├── storage-levers.log
└── batch.status
```

`invocation.txt` is the shell-escaped effective command. Standard output and
standard error, including `[batch]` start and finish records, go to
`storage-levers.log`; follow it with `tail -f <run-dir>/storage-levers.log`.
`batch.status` is atomically published only after the matrix child returns and
contains its integer exit status. Zero means normal successful completion;
nonzero means the matrix returned a handled failure. A missing status means the
wrapper was interrupted or failed outside normal child completion, so inspect
the log and process recorded in `batch.pid`. The exclusive timestamp-, label-,
and PID-qualified directory prevents stale results. The matrix creates its JSON
checkpoint exclusively, then atomically replaces that file as its owner while
the run advances. CSV remains a final derived summary.

### Durable incremental evidence contract

Schema-v5 `storage-levers.json` is the authoritative matrix artifact. The
matrix creates it after validating arguments and configuration but before
environmental preflight or rack mutation. The initial checkpoint records the
run identity, provenance, complete matrix plan, requested repeat count, and
exact effective configuration of every candidate. Its run status is `running`
and its planned repeat stages are initially pending.

Each requested repeat records separate pre-boundary, launch, workload, and
post-boundary outcomes. Launch and workload outcomes carry their own metrics or
error rather than sharing one all-or-nothing repeat outcome. A workload omitted
by configuration is `not_requested`, not a success with zero measurements.
Launch retry errors remain bounded prior-attempt evidence on the requested
repeat. Boundary outcomes are `pending`, `clean`, or `failure`; launch outcomes
are `pending`, `success`, or `failure`; workload outcomes are `pending`,
`not_requested`, `success`, or `failure`.

The matrix checkpoints after every meaningful transition, including:

1. pre-launch clean-boundary proof;
2. launch success or exhausted launch failure;
3. collection of launch writes, duration, and peak memory;
4. workload success or failure; and
5. post-repeat clean-boundary proof.

For each checkpoint, Voxel serializes the whole next document to a temporary
sibling, flushes it, and atomically renames it over `storage-levers.json`.
Voxel must stop before doing more expensive or state-mutating work if it cannot
publish a checkpoint. An interruption therefore leaves the previous or next
valid JSON document, never a partially written document. The monotonically
increasing checkpoint sequence and update time identify the snapshot.

The run-level status is one of:

- `running` while planned work remains; an artifact left in this state after a
  process or host failure is an interrupted snapshot;
- `completed` after every planned repeat was attempted, including matrices with
  individual launch or workload failures; or
- `aborted` when a boundary or contract failure prevents safe continuation.

Completed matrices may contain failed or partial repeats. Reaching the end of
the plan is a successful matrix execution so the operator workflow can produce
its report. Fatal aborts return nonzero but retain the last checkpoint. The
headless wrapper generates a report archive whenever the JSON checkpoint
exists, while recording the matrix exit status separately; an aborted run must
not suppress the report that explains the failure.

Checkpoint artifacts are reportable but not resumable. A restarted command
creates a new run and artifact. Reports may group that run with the interrupted
run only when their provenance and cohort identities are comparable. This
avoids claiming continuity across an unproven cross-process host-state
boundary.

Implementation verification must cover initial publication before mutation,
atomic replacement and checkpoint sequencing, launch evidence surviving a
workload failure, launch-only retries, fatal boundary handling, interrupted
checkpoint loading, per-metric sample counts, recommendation exclusion, and
schema-v4 compatibility. Injected write and rename failures must prove that the
previous checkpoint remains intact. The wrapper must generate a partial report
after a nonzero matrix exit whenever a valid checkpoint exists. Automatic
resume, workload-only retry, an append-only journal, and legacy artifact
conversion are outside this contract.

The default ladder tests `none`, `1`, `1+2`, `1+2+3`, and `all` (`1+2+3+4`):

1. host `sync=disabled`;
2. host compression and metadata tuning;
3. guest `rpool` plus `oxi_*`/`oxp_*` tuning; and
4. reduced RSS participation.

Each combination is exact: named levers are enabled and omitted levers are
disabled. `none` is therefore a true all-off baseline, including lever 3, and
`all` enables all four. To test different combinations, add an explicit list
such as `--combos "none;1;2;3;4;all"` to the same command.

The matrix validates the Falcon drive scope and requested lever states itself;
no separate sampling preflight is needed. Invoke `run` again with a suitable
label rather than reusing a run directory. See `voxel perftest matrix --help`
for controls such as RSS count and the current workload choices.

`voxel-init` is baked into the CP image. Rebuilding only the host `voxel` binary
does not update guest-side lever behavior; after changing `voxel-init`, rebuild
or reimport the configured CP image before running the matrix.

Select a workload with `--workload <name>`. The wrapper's default,
`api-disk-lifecycle`, requires an authenticated `oxide` CLI and adds a separate
`WORKLOAD` result for each combination. Use the matrix help to select a
different supported workload explicitly.

## Choose the result

The table reports mean `BRING-UP`, `RATE/s`, `LAUNCH`, and `PEAK-RAM` values,
plus `CV%` for bring-up wear when a combination has repeated samples. Choose the
combination with the lowest `BRING-UP` wear whose launch time and peak RAM are
acceptable. If the difference between combinations is comparable to their
run-to-run variation, increase `--repeat` before calling a winner.

When a workload is selected, use `WORKLOAD` to evaluate runtime writes
separately from bring-up. A workload failure does not erase valid launch
measurements. A failed launch produces no launch metrics, and a failed boundary
retains prior measurements only as descriptive evidence from an invalid
boundary.

### Generate a portable report

Pass one or more completed, aborted, or interrupted run directories to the
wrapper in the comparison order you want. It resolves each directory's
`storage-levers.json`, validates that each exists, and preserves the supplied
order. Inputs may contain different supported experiment kinds; the report
keeps kinds and comparability cohorts separate. Recommendations are
cohort-local: do not use a report to recommend a winner across incomparable
cohorts.

```sh
docs/run-perftest.sh report before-after \
  "$RESULTS_ROOT/perftest-20260728-010000-before-1234" \
  "$RESULTS_ROOT/perftest-20260728-030000-after-5678"
```

Report generation is unprivileged (`PFEXEC` is not used). It creates an
exclusive comparison directory and refuses to reuse or overwrite one:

```text
$RESULTS_ROOT/comparison-YYYYMMDD-HHMMSS-LABEL-PID/
├── batch.pid
├── batch.status
├── inputs.txt
├── invocation.txt
├── report.log
└── report/
    ├── report.html
    ├── report.json
    └── manifest.json
```

`inputs.txt` records the resolved JSON paths in exactly the supplied directory
order, `invocation.txt` records the shell-escaped native report command, and
`report.log` captures its standard output and error. As with a run,
`batch.status` is atomically published only after normal child completion and
contains the report command's integer exit status; a missing status indicates
interruption or wrapper failure.

The report directory contains:

- `report.html`, a self-contained interactive report with vendored ECharts;
- `report.json`, the normalized inputs, analysis, recommendations, and chart
  options; and
- `manifest.json`, including input and artifact SHA-256 digests.

The HTML starts with cohort-local verdicts, coverage, candidate differences,
and non-empty charts. Detailed statistics, tabulated chart results, capability
evidence, samples, complete conditions, and provenance remain available in
disclosures and appendices. Empty cohorts are reduced to their coverage and
failure evidence rather than displaying empty charts and placeholder tables.

Add `--archive` anywhere after the report label to request the native sibling
archive, `report.tar.gz`, alongside `report/` in the comparison directory:

```sh
docs/run-perftest.sh report before-after --archive \
  "$RESULTS_ROOT/perftest-20260728-010000-before-1234" \
  "$RESULTS_ROOT/perftest-20260728-030000-after-5678"
```

The archive contains the same three files beneath one top-level directory.
This native Rust/Charming workflow is offline and headless; it does not require
a browser, display server, network access, Python, or Pillow during generation.

Helios machines are commonly headless. Generate an archive there, copy it to a
workstation (for example, `scp helios:/path/to/report.tar.gz .`), extract it,
and open `report.html` locally in a graphical browser. Keep all three files
together so operators can inspect normalized evidence and verify the manifest.

### Aggregate report archives

Use `superreport` when several retained `report.tar.gz` files contain comparable
runs that should contribute to one larger sample. The command validates each
archive, recovers its normalized run-level evidence, and recomputes cohorts,
statistics, eligibility, and best-supported recommendations. It never combines
the reports' precomputed means or recommendations.

```sh
voxel perftest superreport \
  copied-results/batch-a/report.tar.gz \
  copied-results/batch-b/report.tar.gz \
  copied-results/batch-c/report.tar.gz \
  --out aggregate-report \
  --archive
```

This publishes `aggregate-report/` with the same `report.html`, `report.json`,
and `manifest.json` contract as an ordinary report, plus an `images/` directory
containing one standalone SVG for every non-empty chart in the recomputed
superreport:

```text
aggregate-report/
├── images/
│   └── section-NNN-…-chart-NNN.svg
├── manifest.json
├── report.html
└── report.json
```

The SVG files are inert, dependency-free derived exports intended for direct
viewing or attachment. They are not referenced by `report.html`, are not
canonical evidence or manifest artifacts, and are not included in
`aggregate-report.tar.gz`. Ordinary reports do not create `images/`. The
archive retains exactly the canonical three files and can be supplied to a
later `superreport`, including beside ordinary report archives:

```sh
voxel perftest superreport \
  aggregate-report.tar.gz \
  copied-results/batch-d/report.tar.gz \
  --out expanded-report \
  --archive
```

Each underlying raw-result SHA-256 digest contributes at most once. First
occurrence follows command-line order and then archive order; every accepted
archive origin is retained so overlap remains visible. Recursive aggregation
therefore does not inflate sample counts when inputs overlap.

An invalid, unsupported, unsafe, or internally inconsistent archive is excluded
without suppressing valid siblings. Rejections and reasons appear in the
command summary, HTML, normalized JSON, and manifest. Generation succeeds when
at least one valid unique normalized input remains and fails without publishing
output when none remains. Output directories and sibling archives are never
overwritten.

Superreports replay the normalized evidence embedded in current
`voxel-perftest-report-v1` archives. Those archives do not contain the original
raw matrix JSON, so a superreport cannot recover fields discarded during the
original normalization or renormalize old evidence under a future contract.
Generate new ordinary reports from raw results when normalization policy
changes.

Fresh evidence for changing defaults must come from schema-v5,
baseline-adjusted experiments with the API workload enabled. Each experiment
must have complete provenance, exact effective configurations, stable session
and cohort identity, complete repeats and required metrics, and passing results
for every capability in the current storage contract: strict Falcon/NVMe scope,
clean launch and teardown boundaries, the API disk lifecycle, and simulated
zpool preparation. A recommendation is advisory and cohort-local; review its
eligibility, capability evidence, Pareto status, and decision trace before
changing defaults.

Broader Fleet API, Silo API, and multirack/topology probes are future perftest
workflow additions. Add them only with concrete proof boundaries and a new
capability-contract version; the current storage-default experiment neither
claims nor requires those broader fidelity results.

Pre-evidence and legacy inputs are descriptive compatibility data only. In
particular, the two raw schema-v2 files under
`docs/perftest-20260718-011546-crucial/` and
`docs/perftest-20260718-162302-crucial/` are retained solely as legacy parser and
report compatibility fixtures; they are not active performance evidence or
evidence for defaults.

Reports normalize schema-v5 stages independently. Launch charts use every
launch with valid metrics and clean boundaries; workload charts use only
successful workload measurements. Every chart and table displays its own sample
count so unequal populations are explicit. Running, aborted, partial, and
failed evidence is descriptive only. A recommendation additionally requires a
completed run, the requested number of repeats, clean boundaries, successful
required workloads, complete provenance and effective configuration, passing
capabilities, and a comparable cohort. Missing workload evidence is never
converted to a zero or inferred from launch evidence.

Hyperfine is not part of this harness. Command timing alone cannot preserve or
validate the rack lifecycle, clean state boundaries, stable session/cohort
identity, capability results, and complete evidence needed by these
experiments.

## Optional follow-ups

- Compare two matrix runs containing the same combination labels with `voxel
  perftest compare run-a.json run-b.json`.
- Inspect one interval with `perftest sample` before and after the operation,
  followed by `perftest sample-report`. Unlike matrix, this diagnostic path can
  fall back to host-wide totals, so verify `falcon_controllers` in its samples.
- With a finalist running and the `oxide` CLI authenticated, measure API latency
  and jitter with `pfexec voxel perftest smooth --count 50 --json-out
  smooth.json`.

---

## Caveats

- Every matrix combination is a full, state-mutating, multi-minute rack launch
  and runs serially. These are Helios operator commands, not nextest tests.
- Avoid unrelated host activity while a set of runs is running, in order to avoid skewing results.
