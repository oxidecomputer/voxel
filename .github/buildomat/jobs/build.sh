#!/bin/bash
#:
#: name = "build"
#: variety = "basic"
#: target = "helios-3.0"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/out/voxel",
#: ]
#:
#: [[publish]]
#: series = "release"
#: name = "voxel"
#: from_output = "/out/voxel"

set -o errexit
set -o pipefail
set -o xtrace

banner "check"
cargo fmt -- --check
cargo clippy --all-targets -- --deny warnings

banner "build"
ptime -m cargo build --release

pfexec mkdir -p /out
pfexec chown "$USER" /out
cp target/release/voxel /out/
