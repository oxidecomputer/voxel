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
# falcon's default ext link normally gives the node a DHCP NIC. Isolated mode
# runs no DHCP server on the segment, so if VOXEL_BUILDER_NET was staged as
# builder-net, apply "<cidr> <gw>" as a static address on the first ethernet
# NIC instead.
if [[ -f /opt/cargo-bay/builder-net ]]; then
    read -r BUILDER_CIDR BUILDER_GW < /opt/cargo-bay/builder-net
    BUILDER_IF="$(ip -o link 2>/dev/null | awk -F': ' '$2 != "lo" {print $2; exit}')"
    log "static builder net: ${BUILDER_CIDR} via ${BUILDER_GW} on ${BUILDER_IF}"
    ip link set "${BUILDER_IF}" up || true
    ip addr add "${BUILDER_CIDR}" dev "${BUILDER_IF}" || true
    ip route add default via "${BUILDER_GW}" || true
fi
# /etc/resolv.conf is a symlink to systemd-resolved's placeholder ("No DNS
# servers known") in isolated mode—no DHCP populated systemd-networkd, so
# the target file stays empty. Replace the symlink with a static file so our
# nameserver line actually sticks for apt.
rm -f /etc/resolv.conf
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

# enable bgpd; frr.conf itself is generated per-topology at launch.
sed -i 's/^bgpd=no/bgpd=yes/' /etc/frr/daemons

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

# --- scrub the builder VM's identity ------------------------------------------
# The builder may have leased its address over DHCP during this image build
# (LAN mode). A dhclient.leases carried into the image makes every router
# clone re-request the builder's old address at boot, conflicting with other
# routers on the LAN. We wipe the lease DB and reset machine-id (an empty
# file regenerates on first boot) so each clone builds a fresh DHCP identity.
# Isolated mode doesn't DHCP, but the scrubbing is harmless there.
rm -f /var/lib/dhcp/dhclient*.leases
: > /etc/machine-id
rm -f /var/lib/dbus/machine-id

# --- mark ready ---------------------------------------------------------------
sync
printf 'voxel-frr version=%s frr=%s built=%s\n' \
    "${VERSION}" "$(dpkg-query -W -f='${Version}' frr 2>/dev/null || echo '?')" \
    "$(date '+%Y-%m-%dT%H:%M:%S')" > "${READY_MARKER}"
log "image ready: $(cat "${READY_MARKER}")"
