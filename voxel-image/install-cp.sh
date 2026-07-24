#!/bin/bash
#
# install-cp.sh - Voxel control-plane BAKED install (image-build time).
#
# Runs INSIDE the Helios builder node (launched by voxel-image-builder). It
# installs the version-pinned control-plane software into the image but applies
# NO topology-specific configuration. The resulting disk is captured by
# build-image.sh into voxel-cp-<version>_0.raw.xz.
#
# Cut line vs a4x2 cargo-bay/g0/init.sh: we stop right after
# `omicron-package unpack`. Everything downstream of that is per-topology /
# per-node and is applied by voxel at LAUNCH, not baked here:
#   - config-rss.toml injection            (generated on the fly per topology)
#   - omicron-package activate / RSS        (starts services for THIS rack)
#   - sprockets keys + SMBIOS identity      (per-sled identity)
#   - setup_ssh                             (per-deployment)
# We also deliberately DO NOT bake:
#   - `xtask virtual-hardware create`       (per-node emulated U.2/M.2; ties
#                                            into the disk-killer + cold-boot
#                                            QoL bugs - keep ephemeral)
#   - `scadm propolis load-program ...`     (propolis-runtime; cannot persist)
#   - the rpool/dump zvol                   (per-VM runtime device)
#
set -euo pipefail

VERSION="${VOXEL_CP_VERSION:-unknown}"
READY_MARKER=/var/voxel-image-ready

log() { echo "[install-cp] $*"; }

# --- networking: reach pkg.oxide.computer -------------------------------------
# The builder has a single external NIC. Find a vioif and bring it up via DHCP.
EXT_IF="${EXT_IF:-}"
if [[ -z "$EXT_IF" ]]; then
    EXT_IF="$(dladm show-phys -o link -p 2>/dev/null | grep '^vioif' | head -1 || true)"
fi
EXT_IF="${EXT_IF:-vioif0}"
log "using external interface ${EXT_IF}"
ipadm create-addr -T dhcp "${EXT_IF}/v4" || true
echo 'nameserver 1.1.1.1' > /etc/resolv.conf

log "waiting for DHCP lease..."
for _ in $(seq 1 30); do
    ipadm show-addr "${EXT_IF}/v4" -p -o addr 2>/dev/null | grep -q '/' && break
    sleep 2
done
log "waiting for DNS..."
for _ in $(seq 1 15); do
    getent hosts pkg.oxide.computer >/dev/null 2>&1 && break
    sleep 2
done

# --- pinned package deps (baked) ----------------------------------------------
# pkg exit 4 = "nothing to do" (already installed, e.g. on a pre-provisioned
# voxel-builder base); treat it as success, not a retry.
install_packages() {
    pkg install tofino looker htop jq
    local rc=$?
    [[ $rc -eq 0 || $rc -eq 4 ]]
}
n=0
until install_packages; do
    n=$((n + 1))
    if [[ $n -ge 25 ]]; then log "FATAL: pkg install failed after ${n} attempts"; exit 1; fi
    log "pkg install attempt ${n} failed; retrying"
    sleep 2
done

cd omicron
chmod +x tools/*.sh tools/ci* omicron-package xtask xtask-downloader
export XTASK_BIN="$PWD/xtask"
export XTASK_DOWNLOADER_BIN="$PWD/xtask-downloader"

# --- control-plane prerequisites + unpack (THE bake) --------------------------
n=0
until ./tools/install_runner_prerequisites.sh -y; do
    n=$((n + 1))
    if [[ $n -ge 5 ]]; then log "FATAL: install_runner_prerequisites failed"; exit 1; fi
    log "prerequisites attempt ${n} failed; retrying in 20s"
    sleep 20
done

log "unpacking control-plane zone artifacts into /opt/oxide ..."
./omicron-package --force unpack

artifact_count="$(find /opt/oxide -name '*.tar.gz' 2>/dev/null | wc -l | tr -d ' ')"
if [[ "${artifact_count}" -eq 0 ]]; then
    log "FATAL: omicron-package unpack produced no artifacts in /opt/oxide"
    exit 1
fi
log "unpacked ${artifact_count} zone artifacts into /opt/oxide"

# Strip the default config-rss.toml that omicron v20+ ships in the sled-agent
# non-gimlet package (985304a6 did not). sled-agent's SMF auto-starts at boot and
# would RSS-init the rack from that default (rack_subnet fd00:1122:3344) BEFORE
# voxel's per-launch config-rss is injected by gimlet-launch - then RSS retries
# with voxel's config and sled-agent refuses ("Sled Agent already running" with a
# different request). Removing it makes RSS wait for voxel's injected config.
rm -f /opt/oxide/sled-agent/pkg/config-rss.toml
log "removed baked default config-rss (RSS will use voxel's injected one)"

# --- bake launch-time bits (de-a4x2) ------------------------------------------
# So `voxel launch` no longer rsyncs the omicron CLI to every sled or stages the
# sidecar from a4x2's cargo-bay. We're in /opt/cargo-bay/omicron (cd'd above);
# bake what gimlet-launch.sh needs at LAUNCH - omicron-package activate + xtask
# virtual-hardware (reads smf/sled-agent/non-gimlet/config.toml). out/ is NOT
# baked: the zones are already unpacked into /opt/oxide.
BAKE=/opt/oxide/omicron
log "baking omicron CLI dir into ${BAKE}"
mkdir -p "${BAKE}"
# Bake the WHOLE staged omicron dir - we can't cherry-pick: `omicron-package
# activate` reads out/target/active and `xtask virtual-hardware` needs
# out/npuzone/. The out/*.tar.gz zones duplicate the unpacked /opt/oxide, but
# that's the price of a self-contained activate (trim the tarballs later).
cp -r . "${BAKE}/"
chmod +x "${BAKE}/omicron-package" "${BAKE}/xtask" "${BAKE}/xtask-downloader" "${BAKE}"/tools/*.sh 2>/dev/null || true

# SoftNPU sidecar_lite (scrimlets load it into propolis at launch). Staged into
# the builder cargo-bay by build-image.sh (the builder VM may not reach
# buildomat.eng - only the host does), then baked into the image.
if [[ -f /opt/cargo-bay/sidecar/scadm && -f /opt/cargo-bay/sidecar/libsidecar_lite.so ]]; then
    log "baking sidecar_lite from cargo-bay"
    mkdir -p /opt/oxide/sidecar
    cp /opt/cargo-bay/sidecar/scadm /opt/cargo-bay/sidecar/libsidecar_lite.so /opt/oxide/sidecar/
    chmod +x /opt/oxide/sidecar/scadm
else
    log "FATAL: sidecar not staged at /opt/cargo-bay/sidecar"; exit 1
fi

# In-guest bring-up agent (replaces gimlet-launch.sh). Copied onto local disk +
# chmod'd here, so /opt/oxide/voxel-init is executable (the cargo-bay 9p mount
# drops the exec bit). `voxel launch` runs `/opt/oxide/voxel-init gimlet`.
if [[ -f /opt/cargo-bay/voxel-init ]]; then
    log "baking voxel-init agent"
    cp /opt/cargo-bay/voxel-init /opt/oxide/voxel-init
    chmod +x /opt/oxide/voxel-init
else
    log "FATAL: voxel-init not staged at /opt/cargo-bay/voxel-init"; exit 1
fi

# Emulated SP/RoT fleet (sp-emu binary + faux-mgs + per-role firmware flashes).
# Staged into the builder cargo-bay by build-cp.sh from the [sp] image paths, then
# baked here so a launched rack runs the emulated SPs/RoTs WITHOUT the operator
# needing the sp-emu sources or [sp] paths on the box. voxel-init's setup_sp_emu
# copies these into oxz_switch at bring-up (a staged cargo-bay [sp].emu_bin still
# wins, for dev iteration). Optional: absent on images built without [sp].emu_bin.
if [[ -d /opt/cargo-bay/sp-emu ]]; then
    log "baking sp-emu fleet from cargo-bay"
    mkdir -p /opt/oxide/sp-emu
    cp /opt/cargo-bay/sp-emu/* /opt/oxide/sp-emu/
    chmod +x /opt/oxide/sp-emu/sp-emu 2>/dev/null || true
    chmod +x /opt/oxide/sp-emu/faux-mgs 2>/dev/null || true
    log "baked sp-emu: $(ls /opt/oxide/sp-emu | tr '\n' ' ')"
else
    log "no sp-emu staged in cargo-bay (image relies on launch-time [sp].emu_bin)"
fi

# Switch-slot enforcer as a baked SMF service. The 2nd scrimlet of each rack must
# present as switch1, but the single image bakes switch0 for everyone; voxel-init
# swaps the live config at bring-up. Doing that swap from a one-shot detached
# process is fragile - if the sled restarts or the process is killed under load
# before/at the swap, the scrimlet silently reverts to switch0 and its rack's
# Nexus handoff wedges ("switch-port qsfp0 not found"). As an SMF service, startd
# re-runs the enforcer at EVERY boot and restarts it if it dies, so the slot can't
# be silently lost. It reads the slot from the (persistent) cargo-bay and is a
# no-op on gimlets / switch0. The switch-zone config dataset is persistent, so once
# applied the swap survives reboots; this guarantees it actually gets applied.
log "baking voxel-switch-enforcer SMF service"
mkdir -p /lib/svc/manifest/site
cat > /lib/svc/manifest/site/voxel-switch-enforcer.xml <<'XML'
<?xml version="1.0"?>
<!DOCTYPE service_bundle SYSTEM "/usr/share/lib/xml/dtd/service_bundle.dtd.1">
<service_bundle type='manifest' name='voxel-switch-enforcer'>
  <service name='oxide/voxel-switch-enforcer' type='service' version='1'>
    <create_default_instance enabled='true'/>
    <single_instance/>
    <dependency name='fs-local' grouping='require_all' restart_on='none' type='service'>
      <service_fmri value='svc:/system/filesystem/local:default'/>
    </dependency>
    <exec_method type='method' name='start'
      exec='/opt/oxide/voxel-init switch-enforcer-svc'
      timeout_seconds='1800'/>
    <exec_method type='method' name='stop' exec=':true' timeout_seconds='60'/>
    <property_group name='startd' type='framework'>
      <propval name='duration' type='astring' value='transient'/>
    </property_group>
    <stability value='Unstable'/>
    <template>
      <common_name><loctext xml:lang='C'>voxel switch-slot enforcer</loctext></common_name>
    </template>
  </service>
</service_bundle>
XML
# Import into the baked SMF repository so it's present (enabled) on every boot.
svccfg import /lib/svc/manifest/site/voxel-switch-enforcer.xml \
    && log "imported voxel-switch-enforcer" \
    || log "WARN: svccfg import voxel-switch-enforcer failed (manifest staged for boot-time import)"

# Commit-pinned voxel-rss-gen: baked so the RSS node renders config-rss in-guest
# at launch (voxel-init), keeping rss-gen off the host entirely. Staged into the
# cargo-bay by the build driver (host build) or build-cp-guest.sh (VM build).
if [[ -f /opt/cargo-bay/voxel-rss-gen ]]; then
    log "baking voxel-rss-gen"
    cp /opt/cargo-bay/voxel-rss-gen /opt/oxide/voxel-rss-gen
    chmod +x /opt/oxide/voxel-rss-gen
else
    log "FATAL: voxel-rss-gen not staged at /opt/cargo-bay/voxel-rss-gen"; exit 1
fi

# Schema manifest (sled-agent config shapes for this commit). Baked here and also
# mirrored to the host stub by the build driver; voxel reads it at launch.
if [[ -f /opt/cargo-bay/voxel-image.toml ]]; then
    log "baking voxel-image.toml manifest"
    cp /opt/cargo-bay/voxel-image.toml /opt/oxide/voxel-image.toml
else
    log "WARN: no voxel-image.toml staged (launch falls back to oldest sled schema)"
fi

# --- quiesce & mark ready -----------------------------------------------------
# NOTE: clearing the device-instance map (/etc/path_to_inst) is done by
# build-image.sh as the LAST exec before capture - doing it here doesn't stick
# (later steps regenerate it for the build VM's hardware).
# No topology-specific state has been written; the disk is a clean baked image.
sync
printf 'voxel-cp version=%s unpacked_artifacts=%s built=%s\n' \
    "${VERSION}" "${artifact_count}" "$(date '+%Y-%m-%dT%H:%M:%S')" > "${READY_MARKER}"
log "image ready: $(cat "${READY_MARKER}")"
