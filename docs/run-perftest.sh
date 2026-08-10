#!/usr/bin/env bash

# Run a storage matrix, or combine completed matrices with Voxel's native
# reporting command.  This deliberately requires Bash for printf %q.

usage() {
	cat >&2 <<'EOF'
usage: run-perftest.sh run <label> [matrix options...]
       run-perftest.sh report <label> [--archive] <run-directory>...

run always enables --keep-going.
EOF
}

die() {
	printf 'run-perftest.sh: %s\n' "$*" >&2
	exit 2
}

validate_label() {
	local label=$1
	LC_ALL=C
	case "$label" in
		''|*[!A-Za-z0-9._-]*) die "invalid label: $label" ;;
	esac
}

make_output_dir() {
	local kind=$1 label=$2 timestamp
	timestamp=$(date -u '+%Y%m%d-%H%M%S') || die 'could not read UTC time'
	OUTPUT_DIR=$RESULTS_ROOT/${kind}-${timestamp}-${label}-$$
	mkdir -p -- "$RESULTS_ROOT" || die "could not create results root: $RESULTS_ROOT"
	mkdir -- "$OUTPUT_DIR" || die "refusing to reuse output directory: $OUTPUT_DIR"
}

write_invocation() {
	local arg
	: > "$1" || die "could not write invocation: $1"
	shift
	for arg in "$@"; do
		printf '%q ' "$arg" >> "$INVOCATION_FILE" || die "could not write invocation"
	done
	printf '\n' >> "$INVOCATION_FILE" || die "could not write invocation"
}

publish_status() {
	local directory=$1 status=$2 temporary
	temporary=$directory/.batch.status.$$.tmp
	printf '%s\n' "$status" > "$temporary" || return 1
	mv -- "$temporary" "$directory/batch.status"
}

has_option() {
	local wanted=$1 arg
	shift
	for arg in "$@"; do
		case "$arg" in
			"$wanted"|"$wanted"=*) return 0 ;;
		esac
	done
	return 1
}

run_matrix() {
	[ "$#" -ge 1 ] || { usage; exit 2; }
	local label=$1 matrix_rc report_rc final_rc arg
	shift
	validate_label "$label"
	local -a options=("$@")
	make_output_dir perftest "$label"
	printf '%s\n' "$$" > "$OUTPUT_DIR/batch.pid" || die 'could not write batch PID'

	local -a command defaults=() canonical_options=()
	has_option --workload "${options[@]}" || defaults+=(--workload api-disk-lifecycle)
	has_option --repeat "${options[@]}" || defaults+=(--repeat 3)
	for arg in "${options[@]}"; do
		[ "$arg" = --keep-going ] || canonical_options+=("$arg")
	done
	command=("$PFEXEC" "$VOXEL_BIN" perftest matrix
		"${defaults[@]}" "${canonical_options[@]}" --keep-going
		--out "$OUTPUT_DIR/storage-levers.csv"
		--json-out "$OUTPUT_DIR/storage-levers.json")
	INVOCATION_FILE=$OUTPUT_DIR/invocation.txt
	write_invocation "$INVOCATION_FILE" "${command[@]}"
	printf '[batch] results: %s\n' "$OUTPUT_DIR"

	if {
		printf '[batch] started: %s pid=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$$"
		"${command[@]}"
	} > "$OUTPUT_DIR/storage-levers.log" 2>&1; then
		matrix_rc=0
	else
		matrix_rc=$?
	fi
	printf '[batch] finished: %s status=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$matrix_rc" >> "$OUTPUT_DIR/storage-levers.log"
	publish_status "$OUTPUT_DIR" "$matrix_rc" || die 'could not publish batch status'

	final_rc=$matrix_rc
	if [ -f "$OUTPUT_DIR/storage-levers.json" ]; then
		printf '[report] attempt: input=%s output=%s archive=%s\n' \
			"$OUTPUT_DIR/storage-levers.json" "$OUTPUT_DIR/report" "$OUTPUT_DIR/report.tar.gz" \
			>> "$OUTPUT_DIR/storage-levers.log"
		if "$VOXEL_BIN" perftest report "$OUTPUT_DIR/storage-levers.json" \
			--out "$OUTPUT_DIR/report" --archive \
			> "$OUTPUT_DIR/report.log" 2>&1; then
			report_rc=0
		else
			report_rc=$?
		fi
		printf '[report] finished: status=%s archive=%s\n' \
			"$report_rc" "$OUTPUT_DIR/report.tar.gz" >> "$OUTPUT_DIR/storage-levers.log"
		if [ "$matrix_rc" -eq 0 ] && [ "$report_rc" -ne 0 ]; then
			final_rc=$report_rc
		fi
	else
		printf '[report] skipped: checkpoint missing: %s\n' \
			"$OUTPUT_DIR/storage-levers.json" >> "$OUTPUT_DIR/storage-levers.log"
	fi
	exit "$final_rc"
}

run_report() {
	[ "$#" -ge 2 ] || { usage; exit 2; }
	local label=$1 archive=false arg rc directory json
	shift
	validate_label "$label"
	local -a inputs=()
	for arg in "$@"; do
		if [ "$arg" = --archive ]; then
			[ "$archive" = false ] || die 'duplicate --archive'
			archive=true
		else
			inputs+=("$arg")
		fi
	done
	[ "${#inputs[@]}" -gt 0 ] || die 'report requires at least one run directory'
	local -a json_inputs=()
	for directory in "${inputs[@]}"; do
		[ -d "$directory" ] || die "not a run directory: $directory"
		json=$directory/storage-levers.json
		[ -f "$json" ] || die "missing result JSON: $json"
		json_inputs+=("$json")
	done

	make_output_dir comparison "$label"
	printf '%s\n' "$$" > "$OUTPUT_DIR/batch.pid" || die 'could not write batch PID'
	printf '%s\n' "${json_inputs[@]}" > "$OUTPUT_DIR/inputs.txt" || die 'could not write report inputs'
	local -a command=("$VOXEL_BIN" perftest report "${json_inputs[@]}" --out "$OUTPUT_DIR/report")
	[ "$archive" = true ] && command+=(--archive)
	INVOCATION_FILE=$OUTPUT_DIR/invocation.txt
	write_invocation "$INVOCATION_FILE" "${command[@]}"
	printf '[batch] results: %s\n' "$OUTPUT_DIR"

	"${command[@]}" > "$OUTPUT_DIR/report.log" 2>&1
	rc=$?
	publish_status "$OUTPUT_DIR" "$rc" || die 'could not publish batch status'
	exit "$rc"
}

RESULTS_ROOT=${RESULTS_ROOT:-"$HOME/voxel-perftest-results"}
VOXEL_BIN=${VOXEL_BIN:-voxel}
PFEXEC=${PFEXEC:-pfexec}

[ "$#" -gt 0 ] || { usage; exit 2; }
subcommand=$1
shift
case "$subcommand" in
	run) run_matrix "$@" ;;
	report) run_report "$@" ;;
	-h|--help|help) usage ;;
	*) usage; die "unknown subcommand: $subcommand" ;;
esac
