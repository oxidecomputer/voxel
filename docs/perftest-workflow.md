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
| fits | peak launch-window host RAM increase above baseline | `matrix` `LAUNCH ΔRAM` column; optional workload increase is `WORKLOAD ΔRAM` |
| smooth | control-plane operation latency and jitter | optional `perftest smooth` follow-up |

## Voxel and Cookout boundary

Voxel owns the platform-specific side of the workflow: rack lifecycle,
Falcon and ZFS state, NVMe and host-memory collection, `omdb` and Oxide API
workloads, durable matrix checkpoints, cleanup, and validation and conversion
of supported Voxel result artifacts into Cookout evidence. Cookout is a
platform-neutral library and CLI. It validates that evidence and owns reusable
cohorting, statistics, comparison, projection, report publication, archive
replay, and aggregation.

`voxel perftest report`, `superreport`, and `compare` are compatibility entry
points. They adapt Voxel matrix artifacts and delegate the reusable work to
Cookout while retaining Voxel's existing command syntax and compare output.
Cookout does not launch a rack, inspect a host, invoke Falcon, ZFS, NVMe,
`omdb`, or the Oxide CLI, or read credentials.

Voxel follows Cookout's remote `main` branch:

```toml
cookout = { git = "https://github.com/oxidecomputer/cookout.git", branch = "main" }
```

`Cargo.lock` records the exact Cookout commit resolved for a given Voxel
revision. Updating the dependency advances that lock entry to the latest
`main` commit. The development and reporting commands do not push either
repository, create remotes, alter repository history, or otherwise perform
network operations at runtime.

`matrix` keeps the normal fail-closed launch gates for router and gimlet
completion, RSS completion, external route installation, requested host and
guest lever states, strict Falcon drive scope, and clean pre/post-repeat
teardown boundaries. It probes every configured external DNS server; when none
answers after the bounded convergence window, matrix retains the valid launch
measurements, records the workload as blocked, and keeps the repeat as
launch-only evidence. The ordinary `voxel launch` command still treats that
condition as a launch failure.
Launch-only evidence is descriptive, not recommendation-eligible. A launch
failure is retried once only after teardown and host-property reset prove
another clean boundary. A workload failure after a successful launch is not
retried by launching another rack. A failed clean-boundary proof aborts
immediately rather than continuing from uncertain state.

---

## One-time host preparation

### 1. Build and retain the exact Voxel binary under test

```sh
cargo fmt --all -- --check
cargo clippy --locked -p voxel --all-targets -- -D warnings
cargo test --locked -p voxel         # includes cookout_report_parity; run on Helios
cargo build --release --locked -p voxel
VOXEL_BIN="$PWD/target/release/voxel"
```

Do not rebuild or switch Voxel revisions during the benchmark. Keep
`VOXEL_BIN` pointed at that retained release binary, and record its source
revision with the results. The package-wide test command is intentional: a
`perftest::` name filter does not run the `cookout_report_parity` integration
test.

Cookout itself is portable. In its checkout, run the complete ordinary-host
gate before copying Voxel to Helios:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Voxel links Helios platform libraries. `cargo check -p voxel --tests` is useful
for source validation on a development host, but run Voxel perftest tests,
Voxel Clippy, and all rack- or host-interacting Voxel commands in this runbook
on Helios. Cookout's standalone validation, reporting, comparison, and
aggregation commands are portable.

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
catch setup problems that would otherwise invalidate multiple runs. Do not skip
this on a fresh workdir: normal launch creates the per-node `cargo-bay/`
directories that the matrix's initial clean-boundary check expects.

```sh
pfexec "$VOXEL_BIN" launch
pfexec "$VOXEL_BIN" network validate
pfexec "$VOXEL_BIN" destroy
```

When the matrix will use `api-disk-lifecycle`, also run its destructive
end-to-end preflight once. It proves a clean initial boundary, launches the
rack, provisions the isolated recovery-silo profile, executes the fixed 20-disk
API lifecycle, and restores the final boundary:

```sh
pfexec "$VOXEL_BIN" perftest preflight \
  --workload api-disk-lifecycle
```

Pass the same `--oxide-auth-helper PATH` that the matrix will use when the
configured recovery credentials require it. A normal launch/destroy proves the
rack lifecycle; `preflight` additionally proves the API workload. Neither is a
performance sample.

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

Voxel's global options are accepted after `perftest matrix`, so they may be
passed through the wrapper with the matrix options. Use explicit paths for a
disposable checkout or isolated smoke environment rather than relying on
configuration discovery:

```sh
VOXEL_BIN=$HOME/bin/voxel-under-test \
  docs/run-perftest.sh run isolated-smoke \
  --config /var/tmp/voxel-smoke/voxel.toml \
  --workdir /var/tmp/voxel-smoke/voxel-image \
  --dataset testbed/falcon-smoke \
  --repeat 1 \
  --combos none
```

The workdir must be the project root under which Voxel manages `cargo-bay/`
and `.falcon/`; it is not the results directory. The explicit dataset overrides
`[falcon].dataset` for that invocation. Complete the launch/destroy preparation
above in the same workdir before starting its first matrix.

Use a deterministic external segment for repeated matrices. Prefer
`[external] mode = "isolated"` with an explicit `uplink`; LAN mode depends on
ambient DHCP, and a customer-edge lease outside the host's directly connected
subnet cannot be installed as a route gateway. If LAN mode is required, pin
`EXT_INTERFACE` and configure `[topology].ce_external_ip` to an unused address
on that interface's subnet rather than relying on changing DHCP leases.

Labels may contain only ASCII letters, digits, dots, underscores, and hyphens.
By default `run` supplies `--workload api-disk-lifecycle`, `--repeat 3`, and
`--keep-going`. The operator wrapper always enables Voxel's presence-only
`--keep-going` flag: it removes every exact caller-supplied `--keep-going` and
appends one canonical flag. Do not use an assignment form such as
`--keep-going=true`: the wrapper forwards it in addition to the canonical flag,
and Voxel rejects it because `--keep-going` takes no value.
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
  --combos 'none;1;2;3;all'
```

The script prints the results directory before running Voxel and creates it
exclusively as:

```text
$RESULTS_ROOT/perftest-YYYYMMDD-HHMMSS-LABEL-PID/
├── batch.pid
├── invocation.txt
├── report.log
├── report/
├── report.tar.gz
├── storage-levers.csv
├── storage-levers.json
├── storage-levers.log
└── batch.status
```

`report.log`, `report/`, and `report.tar.gz` are conditional. Whenever
`storage-levers.json` exists, the wrapper attempts an unprivileged
Cookout-backed Voxel report and archive even if the matrix exits nonzero.
`batch.status` records the matrix exit status, not the report status; the report
status is recorded in `storage-levers.log`. If the matrix succeeds but report
generation fails, the wrapper exits with the report status. After the
privileged matrix finishes each private checkpoint or the final CSV, it
transfers the open temporary file to the invoking user before atomically
installing it. Reporting therefore needs no privileged pathname operation and
the artifacts are never world-readable. The wrapper sets `umask 077`, so the
run/comparison directories, logs, invocation metadata, and reports are private
even when the invoking shell has a permissive umask.

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

Workloads are not rerun after failure, but the API disk lifecycle has bounded
operation-local recovery. Idempotent quota updates receive three attempts;
owned disk deletes receive ten. Disk creation retries an explicit HTTP 5xx at
most twice and only after five successful inventory reads prove the exact
nonce-owned disk remains absent. Status-less create failures are never
resubmitted and instead use bounded reconciliation. Provisioning has a
120-second deadline, and disk settlement is polled for about five minutes in
both the measured operation and cleanup.

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

The default storage ladder tests `none`, `1`, `1+2`, and `all` (`1+2+3`):

1. host `sync=disabled`;
2. host compression and metadata tuning;
3. guest ZFS tuning: the bounded tuner attempts `rpool` and every non-`rpool`
   pool it discovers, while matrix evidence verifies `rpool` and the
   `oxi_*`/`oxp_*` pools.

Each combination is exact: named levers are enabled and omitted levers are
disabled. `none` is therefore a true all-off baseline, including lever 3, and
`all` enables all three. To test different combinations, add an explicit list
such as `--combos "none;1;2;3;all"` to the same command.

To measure RSS participation, run the independent topology experiment:

```sh
pfexec voxel perftest topology-matrix \
  --rss-sleds 3 \
  --workload api-disk-lifecycle \
  --repeat 5 \
  --json-out topology-levers.json \
  --out topology-levers.csv \
  --keep-going
```

`topology-matrix` compares the base storage configuration twice: once with all
sleds participating in RSS and once with the requested reduced participant
count. The command emits `TopologyLevers` evidence, so Cookout reports it
separately from `StorageLevers`; it is never included in a storage
recommendation.

The matrix validates the Falcon drive scope and requested lever states itself;
no separate sampling preflight is needed. Invoke `run` again with a suitable
label rather than reusing a run directory. See `voxel perftest matrix --help`
and `voxel perftest topology-matrix --help` for the current controls.

`voxel-init` is baked into the CP image. Rebuilding only the host `voxel` binary
does not update guest-side lever behavior; after changing `voxel-init`, rebuild
or reimport the configured CP image before running the matrix.

Select a workload with `--workload <name>`. The wrapper's default,
`api-disk-lifecycle`, provisions an isolated temporary `oxide` profile from the
configured recovery silo and adds a separate `WORKLOAD` result for each
combination. For a custom recovery password hash, pass `--oxide-auth-helper` to
populate that private profile. The workflow never consumes a pre-authenticated
global CLI profile. Use the matrix help to select a different supported
workload explicitly.

## Choose the result

At completion, `perftest matrix` prints an immediate terminal summary table and
writes the raw CSV and JSON artifacts. That table reports mean `BRING-UP`,
`RATE/s`, `LAUNCH`, and `LAUNCH ΔRAM` values, plus `CV%` for bring-up wear when
a combination has repeated samples. The wrapper then feeds the JSON artifact to
Cookout to produce the durable report described below. Use the terminal table
for live operator feedback; use the Cookout report, including its eligibility,
cohort, and decision-trace evidence, for review and final decisions.

Choose the combination with the lowest `BRING-UP` wear whose launch time and
baseline-adjusted launch-window RAM increase are acceptable. If the difference
between combinations is comparable to their run-to-run variation, increase
`--repeat` before calling a winner.

When a workload is selected, use `WORKLOAD` to evaluate runtime writes
and `WORKLOAD ΔRAM` to evaluate runtime writes and baseline-adjusted memory
growth separately from bring-up. A workload failure does not erase valid
launch measurements. A failed launch produces no launch metrics, and a failed
boundary retains prior measurements only as descriptive evidence from an
invalid boundary.

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
    ├── cohort-NNN-metric-NNN.svg
    ├── cohorts.csv
    ├── evidence-NNNN.json
    ├── manifest.json
    ├── report.html
    └── report.json
```

`inputs.txt` records the resolved JSON paths in exactly the supplied directory
order, `invocation.txt` records the shell-escaped native report command, and
`report.log` captures its standard output and error. As with a run,
`batch.status` is atomically published only after normal child completion and
contains the report command's integer exit status; a missing status indicates
interruption or wrapper failure.

The report directory contains:

- `report.html`, a self-contained static report;
- `report.json`, the normalized analysis and recommendations;
- `cohorts.csv`, a tabular export of cohort metrics;
- `cohort-NNN-metric-NNN.svg` for every rendered metric (for example,
  `cohort-000-metric-000.svg`);
- `evidence-NNNN.json`, the normalized Cookout evidence used to build the
  report; and
- `manifest.json`, including artifact media types, sizes, and SHA-256 digests.

The HTML summarizes experiments, cohort metrics, exclusions, recommendations,
and decision traces. The JSON retains the complete normalized analysis but does
not currently persist Cookout's mature presentation view or composite-chart
catalog. The evidence files retain the validated inputs. Empty cohorts remain visible with
their outcome counts and an explicit absence of scalar observations rather
than placeholder metric rows or SVGs.

Add `--archive` anywhere after the report label to request the native sibling
archive, `report.tar.gz`, alongside `report/` in the comparison directory:

```sh
docs/run-perftest.sh report before-after --archive \
  "$RESULTS_ROOT/perftest-20260728-010000-before-1234" \
  "$RESULTS_ROOT/perftest-20260728-030000-after-5678"
```

The archive contains `manifest.json` and the same manifest-inventoried files
beneath the fixed top-level `report/` directory. This Cookout workflow is
offline and headless; it does not require a browser, display server, network
access, Python, Pillow, or ECharts during generation.

Helios machines are commonly headless. Generate an archive there, copy it to a
workstation (for example, `scp helios:/path/to/report.tar.gz .`), extract it,
and open `report/report.html` locally in a graphical browser. Keep the extracted
directory together so operators can inspect normalized evidence and verify the
manifest.

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

This publishes `aggregate-report/` with the same contract as an ordinary
Cookout report:

```text
aggregate-report/
├── cohort-NNN-metric-NNN.svg # zero or more
├── cohorts.csv
├── evidence-NNNN.json    # one or more
├── manifest.json
├── report.html
└── report.json
```

The SVG files are inert, dependency-free derived exports intended for direct
viewing or attachment. Every generated report artifact other than
`manifest.json` is covered by the manifest; the manifest and all inventoried
artifacts are included in `aggregate-report.tar.gz`. That archive can be
supplied to a later `superreport`, including beside ordinary report archives:

```sh
voxel perftest superreport \
  aggregate-report.tar.gz \
  copied-results/batch-d/report.tar.gz \
  --out expanded-report \
  --archive
```

Each underlying source SHA-256 digest contributes at most once. Cookout orders
unique evidence deterministically by digest and retains every accepted archive
origin so overlap remains visible. Recursive aggregation therefore does not
inflate sample counts when inputs overlap.

Voxel's compatibility command rejects the complete aggregation if any archive
is invalid, unsupported, unsafe, or internally inconsistent, and publishes no
output. The Cookout library also exposes an explicit partial-acceptance policy
for consumers that want rejected siblings recorded while valid evidence is
aggregated. Output directories and sibling archives are never overwritten.

Superreports replay the `cookout.evidence` evidence inventoried in Cookout
archives. Those archives retain sanitized, replayable Voxel source rather than
byte-for-byte raw matrix JSON. Future compatible adapters can renormalize the
retained fields, but cannot recover fields sanitized or discarded by the
adapter. Generate new ordinary reports from raw results when discarded fields
become relevant.

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

Reports normalize schema-v5 stages independently. Launch metrics use every
launch with valid measurements and clean boundaries; workload metrics use only
successful workload measurements. Every metric row and SVG carries its own
sample count so unequal populations are explicit. Running, aborted, partial,
and failed evidence is descriptive only. A candidate from a completed matrix is
recommendation-eligible when at least 80% of its planned repeats succeed and
every attempted repeat proves a clean boundary. Bounded launch, preparation, or
workload failures remain visible as warnings and are not converted to zero;
boundary failures remain blocking. Recommendations additionally require
complete provenance and effective configuration, passing matrix-wide scope and
boundary capabilities, and a comparable cohort.

Superreports retain exact cohorts for formal statistics and recommendations,
and also publish a descriptive cross-cohort view for storage experiments. That
view places compatible metric observations from the retained runs on shared
charts while preserving source attribution. It does not pool eligibility or
override cohort-local recommendations.

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
- With a finalist running, measure API latency and jitter with `pfexec voxel
  perftest smooth --count 50 --json-out smooth.json`. Smooth provisions the
  same isolated temporary recovery-silo profile as matrix; use
  `--oxide-auth-helper` when the configured recovery credentials require it.

Cookout's standalone CLI accepts replayable `cookout.evidence` envelopes
only, rather than raw Voxel checkpoints. These portable commands expose
Cookout's native validation, publication, aggregation, and comparison interfaces
on an ordinary host; their output and comparison semantics are not identical to
Voxel's compatibility commands:

```sh
cookout validate evidence.json
cookout report evidence.json --out report --archive
cookout aggregate report.tar.gz --out aggregate --archive
cookout compare baseline-evidence.json candidate-evidence.json
```

Use `voxel perftest report` for raw Voxel matrix JSON; it validates and adapts
that input into Cookout evidence before publication. Use `voxel perftest
superreport` for Cookout report archives produced through the Voxel workflow;
it validates retained evidence and invokes the Voxel adapter while aggregating
and replaying that evidence.

---

## Caveats

- Every matrix combination is a full, state-mutating, multi-minute rack launch
  and runs serially. These are Helios operator commands, not nextest tests.
- Avoid unrelated host activity while a set of runs is running, in order to avoid skewing results.
