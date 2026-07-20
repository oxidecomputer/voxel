# Faithful mupdate in voxel — verified chain + build plan

Verified against the box's authoritative source (`voxel-cp-5e2a6a6` →
`/root/voxel-builds/omicron-5e2a6a6`, `management-gateway-service`, `hubris/lib/host-sp-messages`
+ `hubris/task/control-plane-agent`). Lane: **MGS/sp-emu side only** — the host-boots-the-real-oxide-OS
frontier (propolis Level A, blocked at Milan DF `topo_init`) is out of scope and stays with the propolis agent.

## The real chain (what "faithful" means)

Recovery/mupdate = install a new host OS by booting the **trampoline** (installinator) via the
MGS → SP → control-UART **phase-2** path, then installinator writes phase-1/phase-2/control-plane to M.2.
The whole thing is orchestrated by **wicketd** (`wicketd/src/update_tracker.rs`), per compute-sled SP:

1. **Stage phase-2 in MGS** — `recovery_host_phase2_upload` → `POST /recovery/host-phase2`
   (returns `HostPhase2RecoveryImageId` = hash). MGS caches the trampoline image.
2. **Set installinator image id** — `sp_installinator_image_id_set` →
   `POST /sp/{type}/{slot}/ipcc/installinator-image-id` with
   `InstallinatorImageId{ host_phase_2, control_plane, update_id }`
   (newer TUF: `host_phase_2` = installinator *doc* hash, `control_plane` = zeroes).
3. **Point host boot flash at the trampoline** — `set_component_active_slot(HOST_CPU_BOOT_FLASH, TRAMPOLINE_PHASE_1_BOOT_SLOT)`.
4. **Arm recovery** — `sp_startup_options_set{ phase2_recovery_mode: true, .. }` → SP flag `PHASE2_RECOVERY_MODE`.
5. **Reset** → host boots trampoline phase-1, needs phase-2.
6. **Phase-2 pull (the crux):** host issues `HostToSp::GetPhase2Data{ hash, offset }` over the SP
   control UART (RFD 316 / host-sp-messages). SP Hubris `control-plane-agent`
   (`mgs_compute_sled.rs` `HostPhase2Requester`) relays it to MGS as `SpRequest::HostPhase2Data{hash,offset}`;
   MGS replies `MgsResponse::HostPhase2Data{..}` (or `HostPhase2Unavailable`/`HostPhase2ImageBadOffset`);
   SP forwards `SpToHost::Phase2Data`. Repeat by offset until the image is transferred.
7. wicketd polls `sp_host_phase2_progress_get` → `GET /sp/{}/host-phase2-progress` to watch the pull.
8. **Installinator runs in the trampoline.** Reads `InstallinatorImageId` via IPCC **KeyLookup**
   (`ipcc` crate, `IpccKey::InstallinatorImageId`). Discovers artifact peers
   (`DiscoveryMechanism::Bootstrap` → switch-zone addrs on `BOOTSTRAP_ARTIFACT_PORT`, or `--mechanism list:`).
   Fetches artifacts by hash: `GET /artifacts/by-hash/{kind}/{hash}` from **wicketd** (installinator-api).
   Writes M.2, reports via `POST /report-progress/{update_id}`.
9. wicketd clears image id + recovery mode, resets → new host OS boots.

Note both the **phase-2 pull** (`GetPhase2Data`) and **IPCC KeyLookup** (installinator-image-id, identity)
ride the *same* SP control-UART protocol (`host-sp-messages::HostToSp`) — the one the existing
`voxel sp ipcc` exerciser already speaks via the sp-emu `ipcc` host-role probe.

## Component reality in voxel today

| Link | State | Notes |
|---|---|---|
| MGS gateway | ✅ real | `svc:/oxide/mgs`; relay target |
| SP phase-2 relay + RoT | ✅ real Hubris | `control-plane-agent` `mgs_compute_sled.rs`; under `--emu-sp`/`--emu-rot` |
| wicketd orchestrator | ✅ real, drivable | already driven by `--wicket-setup` (drive-real-wicketd pattern) |
| host `GetPhase2Data` / IPCC KeyLookup | ⚠ stand-in only | sp-emu `ipcc` probe (`voxel sp ipcc`, identity/bsu); extendable to `get-phase2` |
| the real host asking (phbl→phase-1→GetPhase2Data) | ❌ propolis-gated | Level A, Milan DF crash — NOT our lane |
| i86 GZ TUF repo | ❌ we must build | roadmap: our own `tufaceous` assembly, build-cp already emits i86 GZ artifacts |
| installinator run | ⚠ stand-in | real binary, run directly against wicketd (no booted host) |

## The first faithful milestone (reachable NOW, no propolis)

`voxel sp ipcc <gimlet-SP> --cmd get-phase2` → sp-emu `ipcc` probe emits `HostToSp::GetPhase2Data`
over UART7 → **real Hubris** relay → **real MGS** serves the image staged via `POST /recovery/host-phase2`
→ `SpToHost::Phase2Data` returns. Exercises real Hubris + real MGS + the real protocol framing; the
**only** faked piece is the host requestor — exactly the seam propolis Level A will later fill. This is
the milestone you named ("host in recovery issues a real GetPhase2Data over IPCC → SP → MGS").

Requires an **emu-backed gimlet/compute-sled SP** (not the sidecar — sidecars have no host; the
compute-sled `control-plane-agent` is the one with `HostPhase2Requester`). sp-emu can run gimlet
Hubris already (`voxel sp reflash g1 <gimlet-c-emu.zip>` validated).

## Build plan

### Task 2 — MGS phase-2 staging + recovery trigger

- **2a (exercisable now — THE milestone):** direct MGS staging + exerciser pull, host-free.
  - `voxel mupdate stage <gimlet> <phase2-image>` → `POST /recovery/host-phase2` to MGS via `oxz_switch`
    (+ optionally set installinator-image-id / arm recovery), same ssh-into-switch-zone plumbing as `wicket_setup.rs`.
  - Extend the sp-emu `ipcc` host-role subcommand with `GetPhase2Data`; add `voxel sp ipcc <gimlet> --cmd get-phase2 [--hash H --offset N]`.
  - Verify: `SpToHost::Phase2Data` returned; `GET /sp/{}/host-phase2-progress` shows the request; MGS logs the relay.
  - Lane-clean: NO Hubris change, NO SP-firmware change — only voxel + the sp-emu *host-role probe*.
- **2b (faithful production path):** drive real wicketd end-to-end.
  - `voxel update start <sp> <tuf-repo>` → upload TUF (`PUT /repository`), `POST /clear-update-state`, `POST /update/{sp}`;
    wicketd's real `update_tracker` runs steps 1–9. Same drive-real-wicketd pattern as `--wicket-setup`.
  - Full run blocks on a valid i86 TUF (task 3) + the host-boot seam (propolis); the SP/MGS staging
    steps (1–5, 7) run against real MGS + real SP now.

### Task 3 — installinator artifact supply

- Build our own **i86 GZ TUF repo** (`tufaceous`) pinned to the image commit (build-cp already emits the artifacts).
- **Faithful stand-in exercise (no booted host):** run the real `installinator` binary in a zone with
  `--mechanism list:[<wicketd bootstrap addr>]:<BOOTSTRAP_ARTIFACT_PORT>` + a real `InstallinatorImageId`/update-id,
  writing to a scratch M.2-like zvol. Confirms fetch-by-hash + write + report-progress against real wicketd.
- Wires into 2b: once TUF is uploaded and installinator can fetch, the whole chain is exercised except
  the host-boots-trampoline seam.

## Explicitly propolis-gated (not our lane)

The real host booting phbl → phase-1 → `GetPhase2Data` during recovery, and the real oxide-kernel
`/dev/ipcc` driver. Our stand-ins (sp-emu `ipcc` probe for `GetPhase2Data`; direct `installinator` run)
fill exactly that seam, so everything MGS-ward is faithful and validated the moment Level A lands.
