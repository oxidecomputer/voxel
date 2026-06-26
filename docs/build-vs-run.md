# Voxel: build time vs run time

The guiding split: **the image carries version-pinned binaries + neutral baked
config (built once per omicron commit); everything topology- and identity-specific
is generated in Rust and applied at launch.** One `voxel-cp-<commit>` image serves
every topology - 3 sleds, 4 sleds, different networks - because nothing about a
particular rack is baked in.

```
  BUILD TIME (once per omicron commit)        RUN TIME (every launch)
  ────────────────────────────────────        ───────────────────────────
  voxel image create <commit>                  voxel launch
    -> build-cp.sh   (host)                       -> voxel (Rust)        (host)
    -> install-cp.sh (builder VM)                 -> voxel-init <role>   (in guest)
  produces: voxel-cp-<commit>@base             produces: a live, RSS-initialized rack
```

---

## Build time - `voxel image create <commit>`

Produces the `voxel-cp-<commit>` falcon image (and the commit-pinned
`voxel-rss-gen`). Runs only on the Helios box. Build-from-source, **not** TUF -
TUF's `control-plane.tar.gz` carries only the service zones, not the i86pc
global-zone software (sled-agent/switch/opte/mgs), so the GZ half must be an
i86pc omicron build (see `docs/voxel-roadmap.md`).

**Driver: `voxel-image/build-cp.sh`** (orchestrates, on the host):

| Step | What | Mechanism |
|------|------|-----------|
| 1 | clone + checkout omicron `<commit>` | `git`, idempotent (skips if present) |
| 2 | builder prereqs + softnpu machinery | `install_builder_prerequisites.sh`, `ci_download_softnpu_machinery` (-> `out/npuzone`) |
| 3 | build the package tools | `cargo build -p omicron-package -p xtask -p xtask-downloader` |
| 4 | **generate build-time smf configs** | `voxel image render-smf` -> `voxel-config` writes `smf/mgs-sim` (switch0), `smf/sp-sim`, `smf/sled-agent/non-gimlet` (replaces the static configs a4x2 used to supply) |
| 5 | package the control plane | `omicron-package package` -> 54 zone tarballs |
| 6 | fetch the SoftNPU sidecar | `fetch-sidecar.sh` (buildomat, `forward_v6`-capable rev, cached) |
| 7 | stage curated omicron dir | `rsync` -> `voxel-image/cargo-bay/vbuild/omicron` + `.../sidecar` |
| 8 | **bake + capture the image** | `build-image.sh` (see below) |
| 9 | build commit-pinned renderer | `build-rss-gen.sh` (typed `RackInitializeRequest`; npm-style "out of date" warning on schema drift) |

**The bake: `install-cp.sh`** runs *inside* a single-node Helios builder VM
(launched by `build-image.sh`):
- `omicron-package unpack` -> control-plane zone artifacts into `/opt/oxide`
- strip the default `config-rss.toml` (so RSS waits for voxel's per-launch one)
- bake the omicron CLI (`omicron-package`/`xtask`) into `/opt/oxide/omicron`
- bake the SoftNPU sidecar (`scadm` + `libsidecar_lite.so`) into `/opt/oxide/sidecar`
- write the ready marker; no topology state is written

**Capture: `build-image.sh`** boots the builder (runs `install-cp.sh`), verifies
the ready marker, clears the device-instance map (`/etc/path_to_inst`) so each
deployed node rebuilds it for its own virtual hardware, cleanly halts to flush,
then captures the zvol -> `<dataset>/img/voxel-cp-<commit>@base` (fast `zfs
send/recv`, or a portable `.raw.xz`).

**Baked into the image:** omicron control-plane zones + GZ software
(sled-agent/switch/opte/mgs), the omicron CLI, the SoftNPU sidecar, and the three
**neutral** smf configs (switch0 MGS, sp-sim, a g0 sled-agent config). Nothing
rack-specific.

**Deliberately NOT baked** (applied at launch instead): `config-rss.toml`, per-sled
`sled-config.toml`, `frr.conf`, sprockets keys, SMBIOS identity, the switch1
identity, the emulated U.2/M.2 vdevs, the sidecar program load, and the
`rpool/dump` zvol.

---

## Run time - `voxel launch`

Turns the static image into a live, topology-specific rack. All config is
generated from `VoxelConfig` (`voxel.toml`); none of it is in the image.

**`voxel launch` - Rust, on the host (`rack::cmd_launch`):**
| Phase | What | Source |
|-------|------|--------|
| preflight | guard the topology (≥3 sleds + exactly 2 scrimlets per rack), confirm the cp/frr images exist, check host RAM, reset each node's `cargo-bay/` | `voxel` |
| `stage_config` | per-sled `sled-config.toml`, `config-rss.toml` (RSS node; suppressed under `--wicket-setup`), per-router `frr.conf`, 2nd scrimlet's switch1 MGS config; under `--emu-sp`/`--emu-rot` also stages the `sp-emu` binary + a `<port>.flash` per emulated SP | `voxel-config::{sled,rss->voxel-rss-gen,frr,mgs}` |
| `stage_sprockets` | per-sled trust-quorum test keys | `sprockets-tls-test-utils` |
| `build_topo` | falcon nodes (boot `voxel-cp`/`voxel-frr`), SoftNPU fabric, SMBIOS, mount `cargo-bay/<node>` -> `/opt/cargo-bay` | `libfalcon` |
| boot | boot every VM (auto-retries the whole topology up to 3x - the all-at-once RAM spike can flake a cargo-bay mount) | `libfalcon` |
| bring-up | run `/opt/oxide/voxel-init router` then `voxel-init gimlet` in each guest; multi-rack staggers rack-by-rack (concurrent zone-init thrashes the box) | `falcon exec` |
| watch | stream the RSS bring-up (or, under `--wicket-setup`, drive RSS through wicketd first); then set + verify the external host route per rack | bootstrap-agent status API / wicketd |

**In-guest - `voxel-init gimlet` (per sled, baked at `/opt/oxide/voxel-init`, run via `falcon exec`):**
- detect the jumbo underlay NIC(s), patch this sled's `sled-config` data links (via `toml_edit`)
- `xtask virtual-hardware create` - ephemeral emulated U.2/M.2 (kept out of the image; wipes any stale vdevs from a prior launch first)
- inject `config-rss.toml` (RSS node) + the patched `sled-config.toml` as the runtime sled-agent configs
- scrimlet: `scadm propolis load-program` the SoftNPU sidecar (gimlets have no softnpu device, so this is gated on sled mode)
- 2nd scrimlet: swap the switch1 MGS config into the switch zone and bounce MGS (so RSS inventories both switches)
- `omicron-package activate` -> sled-agent starts -> RSS bootstraps the rack

(`voxel-init router` is the Debian/FRR counterpart; SSH setup, formerly the sourced
`setup_ssh`, is now a function inside `voxel-init`.)

**Mechanism in one line:** falcon mounts each node's `cargo-bay/` at
`/opt/cargo-bay`; the baked `voxel-init` agent runs in-guest via `falcon exec`,
injects the generated config into the baked control plane, and lets sled-agent +
RSS take over.

---

## Access - `voxel host` / `voxel tp` (run time, no image involvement)

Discovers each node's host-LAN IP via libfalcon (`ip` for Debian routers, `ipadm`
for Helios sleds; first non-loopback IPv4) and hands the terminal to `ssh`:
`host login` -> the sled GZ; `tp login` -> `ssh -t ... zlogin oxz_switch`.

---

## Why split it this way

- **Build is expensive, version-pinned, and topology-agnostic** -> do it once and
  cache it as an image.
- **Launch is cheap, topology/identity-specific, and Rust-generated** -> fast
  iteration, and one image drives any number of differently-shaped racks.
- The only thing that crosses the line is the typed RSS renderer: it's compiled
  against the image's omicron, so a schema change is a build-time compile error
  rather than a silent runtime misconfig.
