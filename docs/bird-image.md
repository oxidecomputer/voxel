# Debian BIRD images for falcon-lab

`voxel image create-bird` builds a reusable **Debian 13.2 / BIRD 2** image
using the same boot/install/snapshot pipeline as `voxel image create-frr`.
There is no install-from-apt step at launch. The image can be used by any
`libfalcon` topology, including maghemite's `falcon-lab`.

This does **not** replace Voxel's built-in FRR rack routers: `image.frr`,
`voxel-init router`, and Voxel's generated `frr.conf` remain FRR-specific.
Your falcon-lab topology supplies BIRD configuration and interface setup.

## Build on Helios / illumos

The host needs the same ZFS, bhyve/propolis, Falcon prerequisites, source
checkout, and Rust toolchain as an FRR bake. macOS can compile and test the
guest agent, but cannot run the host's image build.

From the Voxel checkout:

```sh
cargo build --release -p voxel
pfexec ./target/release/voxel --dataset testbed/falcon image create-bird proto
```

Replace `testbed/falcon` with your Falcon dataset. The result is
`testbed/falcon/img/voxel-bird-proto@base`, usable as node image
`voxel-bird-proto`. `proto` is an image label, **not** a package-version pin.
The bake installs the `bird2` version available from Debian at build time and
records it in `/var/voxel-image-ready`. Use a new label to retain an old image;
rebuilding an existing label replaces it through the existing capture path.

The builder boots `debian-13.2`, cross-compiles a static
`x86_64-unknown-linux-musl` `voxel-init`, and installs:

- `bird2`: `/usr/sbin/bird`, `birdc`, and `birdcl`, plus `bird.service`;
- `iproute2`, `iputils-ping`, `tcpdump`, `jq`, and `openssh-server`;
- `/opt/oxide/voxel-init` and persistent IPv4/IPv6 forwarding.

It disables apt-daily timers and leaves `bird.service` disabled/stopped until
you supply configuration. Builder DHCP leases and machine identity are
scrubbed, as for FRR. It does not bake your topology config or init script.

The builder needs Internet access for apt. Normal LAN mode uses Falcon's
default external interface (or `EXT_INTERFACE`). With Voxel's configured
`[external] mode = "isolated"`, the command prepares the isolated segment and
stages its static builder address just like `create-frr`. Set
`VOXEL_REPO_ROOT` to the absolute checkout path if running an installed binary
away from the checkout. The source tree is required to cross-compile the agent.

## Launch-time contract

Stage a host directory per node, for example:

```text
cargo-bay/bird1/
  bird.conf
  init.sh
```

In the falcon-lab topology, select the baked image and use **`mount_linux`**:

```rust
let bird = d.node("bird1", "voxel-bird-proto", 2, gb(2));
d.reserve(bird, 20);
// Add the topology's links here, in the intended NIC order.
d.mount_linux("cargo-bay/bird1", "/opt/cargo-bay", bird)?;
```

Keep Falcon's normal guest setup enabled. `d.launch().await?` mounts the
read-only 9p share and sets the hostname. After launch, explicitly initialize:

```rust
let log = d.exec(
    bird,
    "/opt/oxide/voxel-init bird --init-script /opt/cargo-bay/init.sh",
).await?;
println!("{log}");
let ready = d.exec(
    bird,
    "test -f /run/voxel-bird-ready && echo ready",
).await?;
anyhow::ensure!(ready.lines().any(|line| line.trim() == "ready"),
    "BIRD initialization failed");
```

`voxel-init bird`:

1. Removes any previous `/run/voxel-bird-ready` marker.
2. Runs the optional `--init-script` as root via `bash -e`. No executable bit
   is needed, and nothing writes to the read-only mount. Only pass trusted
   scripts; make repeated application safe (e.g. `ip address replace`).
3. Validates `--config` (default `/opt/cargo-bay/bird.conf`) with `bird -p`.
4. Copies it to `/etc/bird/bird.conf` with owner/group `bird` and mode `0640`.
5. Restarts BIRD, waits up to roughly five seconds for `birdc show status`,
   enables it on subsequent boots, and creates the readiness marker.

The marker is important: Falcon's `exec` returns transport status, **not the
guest command's exit status**. Check it after every initialization, and use
`birdc show protocols all` to check routing-session convergence separately.

The script is optional; omit the flag when interfaces are already configured.
Alternate mount locations/config filenames work via `--config` and
`--init-script`. Only the main config is copied automatically. If it includes
other files, have the init script install those into writable guest storage
with permissions readable by `bird`, and use absolute include paths.

No script runs automatically at boot. After a guest reboot, BIRD uses the last
copied config; rerun your networking setup and `voxel-init bird` as needed.
After replacing a host config, rerun the command to copy/restart it. For live
configuration changes already on the guest, the standard `birdc configure`
and `systemctl reload bird` are available. Logs: `journalctl -u bird`.

## Offline smoke test on illumos

The checked-in `bird-smoke` example boots one node **without any network
links**, mounts your files, applies them twice, verifies the control CLI and
readiness, and destroys the test deployment afterward. It cannot depend on
apt at launch. It does not test BGP peering; add that in falcon-lab afterward.

Build it from the checkout, then run in a **fresh separate working directory**
so Falcon cannot disturb a live rack's `.falcon/` state:

```sh
cargo build --release -p voxel --example bird-smoke
export VOXEL_REPO_ROOT="$PWD"
mkdir -p /var/tmp/voxel-bird-smoke/cargo-bay
cd /var/tmp/voxel-bird-smoke

cat > cargo-bay/bird.conf <<'EOF'
router id 192.0.2.1;
log stderr all;
protocol device {}
protocol static static4 {
    ipv4;
    route 198.51.100.0/24 blackhole;
}
EOF

cat > cargo-bay/init.sh <<'EOF'
set -eu
ip link set lo up
sysctl -w net.ipv4.conf.all.rp_filter=0
sysctl -w net.ipv4.conf.default.rp_filter=0
EOF

pfexec env FALCON_DATASET=testbed/falcon \
  "$VOXEL_REPO_ROOT/target/release/examples/bird-smoke" \
  --image voxel-bird-proto --cargo-bay "$PWD/cargo-bay"
```

Expected final line: `BIRD offline smoke test passed`. If initialization
fails, its output is printed before cleanup. For interactive debugging, use
your falcon-lab topology and inspect `/var/voxel-image-ready`,
`/etc/bird/bird.conf`, `journalctl -u bird`, and `birdc show status`.

Next test two fresh clones in falcon-lab with different router IDs/configs,
bring up a BGP session, and confirm learned routes. The image alone does not
infer interface addresses, BGP peers, NAT, or external-network policy.

## Checks available on macOS

```sh
cargo fmt --all -- --check
cargo test -p voxel-init --locked
cargo clippy -p voxel-init --all-targets --locked -- -D warnings
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS='-C linker=rust-lld -C link-self-contained=yes' \
  cargo build -p voxel-init --release --target x86_64-unknown-linux-musl --locked
```

The tests run the guest CLI with fake OS commands and real Bash, covering
read-only config/script inputs, repeated application, command failures, and
stale-readiness removal. They do not substitute for the illumos bake and
Debian daemon smoke test above.
