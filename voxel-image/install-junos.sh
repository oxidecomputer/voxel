#!/bin/bash
#
# install-junos.sh - Juniper cRPD BAKED install (image-build time).
#
# Runs INSIDE the Debian builder node. Installs Docker, loads cRPD, writes only
# neutral bootstrap config, and installs a systemd service that starts cRPD and
# attaches data interfaces at boot. License and topology config are deliberately
# NOT baked; falcon-lab applies them at launch.
#
set -euo pipefail

VERSION="${VERSION:-${JUNOS_VERSION:-23.2R1.13}}"
READY_MARKER=/var/voxel-image-ready
FALCON_S3="https://oxide-falcon-assets.s3.us-west-2.amazonaws.com"
JUNOS_IMAGE="junos-routing-crpd-docker-amd64-23.2R1.13.tgz"
CRPD_IMAGE="crpd:23.2R1.13"
CRPD_CONTAINER="crpd1"
CRPD_CONFIG_DIR="/var/run/juniper"
CRPD_CONFIG="${CRPD_CONFIG_DIR}/juniper.conf"
JET_PORT="51051"
HELPER="/usr/local/bin/voxel-container-router"
SERVICE="/etc/systemd/system/voxel-crpd.service"

log() { echo "[install-junos] $*"; }

# --- reach apt ----------------------------------------------------------------
# Falcon's default ext link gives the node a DHCP NIC; make sure DNS works.
echo 'nameserver 1.1.1.1' > /etc/resolv.conf
log "waiting for DNS..."
for _ in $(seq 1 15); do
    getent hosts deb.debian.org >/dev/null 2>&1 && break
    sleep 2
done

# apt-daily timers race the apt lock; disable them like the FRR image does.
systemctl disable --now apt-daily-upgrade.timer apt-daily.timer 2>/dev/null || true

# Falcon drives guests over the serial console and expects a quiet login shell.
# Match the cEOS image hardening: avoid network-online stalls on data-only NICs
# and keep kernel/journald chatter off the root console.
log "hardening serial console behavior"
systemctl mask systemd-networkd-wait-online.service >/dev/null 2>&1 || true
cat > /etc/sysctl.d/99-console-loglevel.conf <<'EOF_SYSCTL'
kernel.printk = 2 4 1 4
EOF_SYSCTL
sysctl -p /etc/sysctl.d/99-console-loglevel.conf >/dev/null 2>&1 || true
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/99-falcon-console.conf <<'EOF_JOURNALD'
[Journal]
MaxLevelConsole=crit
EOF_JOURNALD
cat > /etc/systemd/system/fix-console-loglevel.service <<'EOF_SERVICE'
[Unit]
Description=Force Falcon-friendly console log level
After=multi-user.target
After=systemd-journald.service

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'sysctl --system'
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF_SERVICE
systemctl daemon-reload
systemctl enable fix-console-loglevel.service >/dev/null 2>&1 || true
systemctl start fix-console-loglevel.service >/dev/null 2>&1 || true

# --- install Docker + operator convenience tools ------------------------------
export DEBIAN_FRONTEND=noninteractive
install_pkgs() {
    apt-get update -y && \
        apt-get install -y docker.io curl jq ssh tmux vim-nox
}
n=0
until install_pkgs; do
    n=$((n + 1))
    if [[ $n -ge 25 ]]; then log "FATAL: apt install failed after ${n} attempts"; exit 1; fi
    log "apt attempt ${n} failed; retrying"
    sleep 2
done

systemctl enable docker 2>/dev/null || true
systemctl start docker >/dev/null 2>&1 || service docker start >/dev/null 2>&1 || true

log "waiting for docker..."
docker_deadline=$((SECONDS + 120))
while ! docker info >/dev/null 2>&1; do
    if [[ "${SECONDS}" -ge "${docker_deadline}" ]]; then
        log "FATAL: docker did not become ready"
        exit 1
    fi
    sleep 2
done

# --- load cRPD image ----------------------------------------------------------
log "downloading cRPD image archive"
if [[ ! -s "/opt/${JUNOS_IMAGE}" ]]; then
    tmp_image="/opt/${JUNOS_IMAGE}.tmp"
    rm -f "${tmp_image}"
    curl -fL --progress-bar --retry 10 --retry-all-errors \
        -o "${tmp_image}" \
        "${FALCON_S3}/${JUNOS_IMAGE}"
    mv "${tmp_image}" "/opt/${JUNOS_IMAGE}"
fi

log "loading cRPD docker image"
if ! docker image inspect "${CRPD_IMAGE}" >/dev/null 2>&1; then
    docker load -i "/opt/${JUNOS_IMAGE}"
fi
CRPD_IMAGE_ID="$(docker image inspect "${CRPD_IMAGE}" --format '{{.Id}}' 2>/dev/null || echo '?')"

# --- neutral cRPD bootstrap config -------------------------------------------
# No license and no topology config are baked here.
log "writing neutral cRPD bootstrap config"
umask 077
mkdir -p "${CRPD_CONFIG_DIR}/license"
cat > "${CRPD_CONFIG}" <<'EOF_CONFIG'
system {
    services {
        extension-service {
            request-response {
                grpc {
                    clear-text {
                        port 51051;
                    }
                }
            }
        }
    }
}
EOF_CONFIG
chmod 0600 "${CRPD_CONFIG}"

# --- in-guest container router helper -----------------------------------------
if [[ -f /opt/cargo-bay/voxel-container-router ]]; then
    log "baking voxel-container-router helper"
    cp /opt/cargo-bay/voxel-container-router "${HELPER}"
    chmod +x "${HELPER}"
else
    log "FATAL: voxel-container-router not staged at /opt/cargo-bay/voxel-container-router"
    exit 1
fi

log "installing cRPD boot service"
docker volume inspect "${CRPD_CONTAINER}-varlog" >/dev/null 2>&1 || \
    docker volume create "${CRPD_CONTAINER}-varlog" >/dev/null

cat > "${SERVICE}" <<EOF_SERVICE
[Unit]
Description=Juniper cRPD container with auto-configured networks
After=docker.service network-online.target
Requires=docker.service
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=${HELPER} \\
  --container ${CRPD_CONTAINER} \\
  --image ${CRPD_IMAGE} \\
  --hostname ${CRPD_CONTAINER} \\
  --privileged \\
  --volume ${CRPD_CONFIG_DIR}:/config:rw \\
  --docker-volume ${CRPD_CONTAINER}-varlog \\
  --volume ${CRPD_CONTAINER}-varlog:/var/log \\
  --publish ${JET_PORT}:${JET_PORT} \\
  --readiness-exec cli \\
  --readiness-exec=-c \\
  --readiness-exec "show version | no-more" \\
  --readiness-contains Junos
ExecStop=/usr/bin/docker stop ${CRPD_CONTAINER}
Restart=on-failure
RestartSec=10
TimeoutStartSec=420

[Install]
WantedBy=multi-user.target
EOF_SERVICE

# --- Falcon runtime setup services -------------------------------------------
# These services intentionally own all runtime cargo-bay/license/config work from
# inside the guest. falcon-lab should only stage files into cargo-bay; repeated
# serial exec is too fragile for cRPD setup.
log "installing Falcon cargo-bay mount service"
cat > /usr/local/bin/falcon-cargo-bay-mount <<'EOF_SCRIPT'
#!/bin/bash
set -euo pipefail

CARGO_BAY=/opt/cargo-bay
mkdir -p "${CARGO_BAY}"

if mountpoint -q "${CARGO_BAY}"; then
    echo "cargo-bay already mounted"
    exit 0
fi

deadline=$((SECONDS + 300))
while ! mountpoint -q "${CARGO_BAY}"; do
    if timeout 10s mount -t 9p -o ro,msize=65536 "${CARGO_BAY}" "${CARGO_BAY}" >/dev/null 2>&1; then
        break
    fi
    if [[ "${SECONDS}" -ge "${deadline}" ]]; then
        echo "cargo-bay mount did not become ready"
        exit 1
    fi
    sleep 2
done

echo "cargo-bay mounted"
EOF_SCRIPT
chmod +x /usr/local/bin/falcon-cargo-bay-mount

cat > /etc/systemd/system/falcon-cargo-bay.service <<'EOF_SERVICE'
[Unit]
Description=Mount Falcon cargo-bay 9p share
After=local-fs.target
Before=falcon-junos-apply.service

[Service]
Type=oneshot
ExecStart=/usr/local/bin/falcon-cargo-bay-mount
RemainAfterExit=yes
TimeoutStartSec=330

[Install]
WantedBy=multi-user.target
EOF_SERVICE

log "installing Falcon Junos config apply service"
cat > /usr/local/bin/falcon-junos-apply <<'EOF_SCRIPT'
#!/bin/bash
set -euo pipefail

CRPD_CONTAINER=crpd1
CARGO_BAY=/opt/cargo-bay
CONFIG_DIR=/var/run/juniper
LICENSE_FILE=${CONFIG_DIR}/license/falcon.key
STAGED_DIR=${CONFIG_DIR}/falcon-lab
STAGED_CONFIG=${STAGED_DIR}/topology.set
APPLIED_MARKER=${STAGED_DIR}/applied
STATUS=/run/falcon-junos-apply.status

status() {
    mkdir -p "$(dirname "${STATUS}")"
    printf '%s
' "$1" > "${STATUS}"
    echo "$1"
}

# This script handles a license file. Do not enable xtrace, print file contents,
# or place the license value in argv/stdout/stderr.
umask 077
mkdir -p "${CONFIG_DIR}/license" "${STAGED_DIR}"

status "waiting for staged Juniper license and config"
config_file=""
while true; do
    for candidate in "${CARGO_BAY}"/*-junos.set; do
        if [[ -s "${candidate}" ]]; then
            config_file="${candidate}"
            break
        fi
    done
    if [[ -s "${CARGO_BAY}/falcon-juniper-license.key" && -n "${config_file}" ]]; then
        break
    fi
    sleep 2
done

status "staging Juniper license and config"
tmp_license="$(mktemp "${CONFIG_DIR}/license/falcon.key.XXXXXX")"
cp "${CARGO_BAY}/falcon-juniper-license.key" "${tmp_license}"
chmod 0600 "${tmp_license}"
mv "${tmp_license}" "${LICENSE_FILE}"
cp "${config_file}" "${STAGED_CONFIG}"
chmod 0600 "${STAGED_CONFIG}"

status "waiting for cRPD CLI"
while ! docker exec "${CRPD_CONTAINER}" cli -c 'show version | no-more' >/dev/null 2>&1; do
    sleep 2
done

if [[ -f "${APPLIED_MARKER}" ]]; then
    status "already applied"
    exit 0
fi

status "installing Juniper license"
docker exec "${CRPD_CONTAINER}"     cli -c 'request system license add config/license/falcon.key'     >/dev/null 2>&1 || { status "failed to install Juniper license"; exit 1; }

status "applying Juniper routing config"
if ! docker exec "${CRPD_CONTAINER}" \
    cli -f /config/falcon-lab/topology.set \
    > "${STAGED_DIR}/apply.out" 2>&1; then
    status "failed to apply Juniper routing config"
    exit 1
fi

# Do not report success unless at least one staged interface line is visible in
# committed config. This catches no-op loads while avoiding full-config logging
# (which may contain license material on some Junos variants).
first_interface_line="$(awk '/^set interfaces / { print; exit }' "${STAGED_CONFIG}")"
if [[ -n "${first_interface_line}" ]]; then
    if ! docker exec "${CRPD_CONTAINER}" \
        cli -c 'show configuration interfaces | display set | no-more' \
        2>/dev/null | grep -Fqx -- "${first_interface_line}"; then
        status "failed to verify Juniper routing config"
        exit 1
    fi
fi

date '+%Y-%m-%dT%H:%M:%S' > "${APPLIED_MARKER}"
status "applied"
EOF_SCRIPT
chmod +x /usr/local/bin/falcon-junos-apply

cat > /etc/systemd/system/falcon-junos-apply.service <<'EOF_SERVICE'
[Unit]
Description=Apply Falcon-staged Juniper license and routing config
After=falcon-cargo-bay.service voxel-crpd.service
Requires=falcon-cargo-bay.service voxel-crpd.service

[Service]
Type=simple
ExecStart=/usr/local/bin/falcon-junos-apply
Restart=on-failure
RestartSec=5
TimeoutStartSec=0

[Install]
WantedBy=multi-user.target
EOF_SERVICE

systemctl daemon-reload
systemctl enable voxel-crpd.service
systemctl enable falcon-cargo-bay.service
systemctl enable falcon-junos-apply.service

# Smoke-test the helper once in the builder VM. It should skip the management
# NIC, start cRPD with the neutral config, and wait until Junos CLI responds.
log "smoke-testing cRPD helper"
"${HELPER}" \
  --container "${CRPD_CONTAINER}" \
  --image "${CRPD_IMAGE}" \
  --hostname "${CRPD_CONTAINER}" \
  --privileged \
  --volume "${CRPD_CONFIG_DIR}:/config:rw" \
  --docker-volume "${CRPD_CONTAINER}-varlog" \
  --volume "${CRPD_CONTAINER}-varlog:/var/log" \
  --publish "${JET_PORT}:${JET_PORT}" \
  --network-prefix "bake_rtr_" \
  --readiness-exec cli \
  --readiness-exec=-c \
  --readiness-exec "show version | no-more" \
  --readiness-contains Junos

# The boot service owns final topology container creation. Remove the bake-time
# smoke-test container and networks so first boot discovers final interfaces.
log "cleaning image before capture"
docker container rm -f "${CRPD_CONTAINER}" >/dev/null 2>&1 || true
for net in $(docker network ls --format '{{.Name}}' | grep '^bake_rtr_' || true); do
    docker network rm "${net}" >/dev/null 2>&1 || true
done
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/* /var/cache/*
journalctl --vacuum-size=0 >/dev/null 2>&1 || true
sync
systemctl stop docker containerd >/dev/null 2>&1 || service docker stop >/dev/null 2>&1 || true

# --- mark ready ---------------------------------------------------------------
printf 'junos version=%s crpd=%s helper=voxel-container-router built=%s license=baked:no\n' \
    "${VERSION}" \
    "${CRPD_IMAGE_ID}" \
    "$(date '+%Y-%m-%dT%H:%M:%S')" > "${READY_MARKER}"
log "image ready: $(cat "${READY_MARKER}")"
