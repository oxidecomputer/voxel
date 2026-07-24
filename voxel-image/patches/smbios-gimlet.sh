#!/bin/bash
#
# smbios-gimlet.sh <omicron-src> - patch sled-hardware to report a Gimlet baseboard.
#
# sled-hardware's parse_smbios_output returns a Pc baseboard for the i86pc voxel
# sleds, but wicketd's RACK SETUP correlates each sled by matching the SP's Gimlet
# baseboard (from MGS). A Pc never equals a Gimlet, so every sled shows "bootstrap
# address UNKNOWN". Return a Gimlet (revision 2, matching the emulated SP VPD
# 0XV2:...:002:) so the two correlate. The checkout resets each build, so this
# re-applies every time. (Extracted from build-cp.sh so the VM build can stage +
# apply it in-guest.)
#
set -euo pipefail

SRC="${1:?usage: smbios-gimlet.sh <omicron-src>}"
F="${SRC}/sled-hardware/src/illumos/mod.rs"

perl -pi -e 's/Some\(Baseboard::new_pc\(serial_number, product\)\)/Some(Baseboard::new_gimlet(serial_number, product, 2))/' "${F}"
grep -q 'new_gimlet(serial_number, product, 2)' "${F}" \
    || { echo "FATAL: smbios baseboard patch did not apply" >&2; exit 1; }
