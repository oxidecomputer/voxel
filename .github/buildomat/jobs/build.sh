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

set -o errexit
set -o pipefail
set -o xtrace

banner "check"
cargo fmt -- --check
cargo clippy --all-targets -- --deny warnings

banner "test"
cargo test

banner "build"
ptime -m cargo build --release

pfexec mkdir -p /out
pfexec chown "$USER" /out
cp target/release/voxel target/release/voxel-init /out/

# The router image's agent, a static musl build. The Debian guest needs no
# toolchain and voxel needs no source tree to bake it.
banner "router agent"
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS='-C linker=rust-lld -C link-self-contained=yes' \
    ptime -m cargo build --release -p voxel-init --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/voxel-init /out/voxel-init-linux

# Re-export the sp-emu build this commit pins (pins.toml), verified against
# its published digest, so one release directory carries the whole trio a
# lab host needs.
banner "sp-emu"
rev=$(awk '/^\[sp-emu\]/{f=1;next} /^\[/{f=0} f && $1=="rev"{gsub(/"/,""); print $3}' pins.toml)
url="https://buildomat.eng.oxide.computer/public/file/oxidecomputer/sp-emu/illumos/$rev"
curl -sSfL --retry 5 -o /out/sp-emu "$url/sp-emu"
curl -sSfL --retry 5 -o /out/sp-emu.sha256.txt "$url/sp-emu.sha256.txt"
[[ "$(digest -a sha256 /out/sp-emu)" == "$(awk '{print $1}' /out/sp-emu.sha256.txt)" ]]

banner "digests"
for b in voxel voxel-init voxel-init-linux; do
	digest -a sha256 "/out/$b" > "/out/$b.sha256.txt"
done

# One archive with the four files a lab host needs. A fresh install is a
# download, a digest check, and an extract. The individual files stay
# published because voxel fetches a pinned sp-emu by name from this layout.
banner "bundle"
chmod 755 /out/voxel /out/voxel-init /out/voxel-init-linux /out/sp-emu
(cd /out && tar czf voxel.tar.gz voxel voxel-init voxel-init-linux sp-emu)
digest -a sha256 /out/voxel.tar.gz > /out/voxel.tar.gz.sha256.txt
