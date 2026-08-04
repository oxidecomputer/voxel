#!/bin/bash
#
# install-cp.sh - bootstrap shim for the voxel-cp image build.
#
# The install logic now lives in the agent: `voxel-init install --role cp`.
# This shim exists only because the builder invokes `bash ./<INSTALL_SCRIPT>`
# from the cargo-bay and the 9p mount drops the exec bit, so the agent has to be
# copied to local disk before it can run. It disappears once the builder itself
# is Rust and can run the agent directly.
set -euo pipefail
cp /opt/cargo-bay/voxel-init /tmp/voxel-init
chmod +x /tmp/voxel-init
exec /tmp/voxel-init install --role cp
