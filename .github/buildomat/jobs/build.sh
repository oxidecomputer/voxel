#!/bin/bash
#:
#: name = "build"
#: variety = "basic"
#: target = "helios-3.0"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/out/voxel",
#:   "/out/voxel.sha256.txt",
#:   "/out/voxel-init",
#:   "/out/voxel-init.sha256.txt",
#:   "/out/voxel-init-linux",
#:   "/out/voxel-init-linux.sha256.txt",
#:   "/out/sp-emu",
#:   "/out/sp-emu.sha256.txt",
#:   "/out/voxel.tar.gz",
#:   "/out/voxel.tar.gz.sha256.txt",
#:   "/out/voxel-up.sh",
#: ]
#:
#: [[publish]]
#: series = "release"
#: name = "voxel"
#: from_output = "/out/voxel"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel.sha256.txt"
#: from_output = "/out/voxel.sha256.txt"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel-init"
#: from_output = "/out/voxel-init"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel-init.sha256.txt"
#: from_output = "/out/voxel-init.sha256.txt"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel-init-linux"
#: from_output = "/out/voxel-init-linux"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel-init-linux.sha256.txt"
#: from_output = "/out/voxel-init-linux.sha256.txt"
#:
#: [[publish]]
#: series = "release"
#: name = "sp-emu"
#: from_output = "/out/sp-emu"
#:
#: [[publish]]
#: series = "release"
#: name = "sp-emu.sha256.txt"
#: from_output = "/out/sp-emu.sha256.txt"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel.tar.gz"
#: from_output = "/out/voxel.tar.gz"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel.tar.gz.sha256.txt"
#: from_output = "/out/voxel.tar.gz.sha256.txt"
#:
#: [[publish]]
#: series = "release"
#: name = "voxel-up.sh"
#: from_output = "/out/voxel-up.sh"

set -o errexit
set -o pipefail
set -o xtrace

banner "check"
cargo fmt -- --check
cargo clippy --all-targets -- --deny warnings

banner "test"
cargo test

# Everything published comes from `cargo xtask dist`, so a developer's
# local archive and CI's are built the same way.
banner "dist"
pfexec mkdir -p /out
pfexec chown "$USER" /out
ptime -m cargo xtask dist --out-dir /out
