#!/bin/bash
#
# install-frr.sh - Voxel FRR router BAKED install (image-build time).
#
# Runs INSIDE the Debian builder node. Installs + enables FRR but applies NO
# topology configuration. The per-topology frr.conf (unnumbered BGP neighbors,
# interface settings, CE NAT, etc.) is generated on the fly in Rust and pushed
# at LAUNCH, not baked here.
#
set -euo pipefail

VERSION="${VOXEL_FRR_VERSION:-unknown}"
READY_MARKER=/var/voxel-image-ready

log() { echo "[install-frr] $*"; }

# --- reach apt ----------------------------------------------------------------
# falcon's default ext link gives the node a DHCP NIC; just make sure DNS works.
echo 'nameserver 1.1.1.1' > /etc/resolv.conf
log "waiting for DNS..."
for _ in $(seq 1 15); do
    getent hosts deb.debian.org >/dev/null 2>&1 && break
    sleep 2
done

# apt-daily timers race the apt lock and can wipe FRR state - disable them
# (matches a4x2 cr/init.sh).
systemctl disable --now apt-daily-upgrade.timer apt-daily.timer 2>/dev/null || true

# --- install FRR (baked) ------------------------------------------------------
export DEBIAN_FRONTEND=noninteractive
install_pkgs() { apt-get update -y && apt-get install -y frr frr-pythontools jq; }
n=0
until install_pkgs; do
    n=$((n + 1))
    if [[ $n -ge 25 ]]; then log "FATAL: apt install failed after ${n} attempts"; exit 1; fi
    log "apt attempt ${n} failed; retrying"
    sleep 2
done

# enable bgpd + bfdd (static mode uses BFD-tracked routes); frr.conf itself is
# generated per-topology at launch.
sed -i 's/^bgpd=no/bgpd=yes/' /etc/frr/daemons
sed -i 's/^bfdd=no/bfdd=yes/' /etc/frr/daemons

# persistent IP forwarding (generic; per-interface knobs set at launch).
cat > /etc/sysctl.d/99-voxel-frr.conf <<'EOF'
net.ipv4.ip_forward=1
net.ipv6.conf.all.forwarding=1
EOF
sysctl -p /etc/sysctl.d/99-voxel-frr.conf || true

systemctl enable frr 2>/dev/null || true

# --- in-guest bring-up agent (replaces router-launch.sh) ----------------------
# The static linux-musl voxel-init, staged into the builder cargo-bay by
# build-frr.sh. Copied onto local disk + chmod'd here (the cargo-bay mount drops
# the exec bit). `voxel launch` runs `/opt/oxide/voxel-init router`.
if [[ -f /opt/cargo-bay/voxel-init ]]; then
    log "baking voxel-init agent"
    mkdir -p /opt/oxide
    cp /opt/cargo-bay/voxel-init /opt/oxide/voxel-init
    chmod +x /opt/oxide/voxel-init
else
    log "FATAL: voxel-init not staged at /opt/cargo-bay/voxel-init"; exit 1
fi

# --- mark ready ---------------------------------------------------------------
sync
printf 'voxel-frr version=%s frr=%s built=%s\n' \
    "${VERSION}" "$(dpkg-query -W -f='${Version}' frr 2>/dev/null || echo '?')" \
    "$(date '+%Y-%m-%dT%H:%M:%S')" > "${READY_MARKER}"
log "image ready: $(cat "${READY_MARKER}")"
