//! The rack's **SP/RoT fleet** - the single contract shared by MGS and whatever
//! provides the service processors - plus `sp-sim` config generation.
//!
//! Every gimlet and the sidecar (switch) carries an **SP** (STM32H753, the
//! MGS-facing management processor) and a **RoT** (LPC55, identity/attestation
//! via DICE). Today omicron's `sp-sim` fakes all of them, on loopback, inside
//! the switch zone; MGS reaches them at `[::1]:333xx`. Tomorrow the same SPs can
//! be backed by *real Hubris firmware* - Renode (interim) or the native Rust
//! emulator - reachable at a real address. The thing that must stay constant
//! across all three is the **fleet**: the per-sled + sidecar identities (serials,
//! DICE cert seeds) and the address/ports MGS uses to reach each one.
//!
//! [`SpFleet`] is that contract. [`crate::mgs`] derives MGS's `[[switch.port]]`
//! table from it, and [`SpFleet::sp_sim_config`] renders `sp-sim`'s side - so the
//! two ends agree *by construction* rather than by parallel hand-maintained port
//! maps. Swapping the [`SpBackend`] (sim -> emulated) is the whole pluggability:
//! it only changes the address MGS dials; identities and ports are invariant.
//!
//! Port scheme: the sidecar binds `33300`/`33301` (switch0/switch1 views) with
//! ereports on `44400`/`44401`; gimlet `i` binds `333{i+1}0/1`, ereports
//! `444{i+1}0/1`, host-cpu serial console `333{i+1}2`.

use std::fmt::Write as _;

/// SP/RoT port scheme (see the module docs). The sidecar binds [`SP_PORT_BASE`] /
/// [`EREPORT_BASE`]; gimlet `i` offsets both by [`PORT_STRIDE`]`*(i+1)`. The
/// switch0/switch1 instances use `base + 0/1`. The host-cpu serial console sits at
/// the SP's `base_port +` [`CONSOLE_PORT_OFFSET`]. `pub` so the `voxel` CLI derives
/// its in-zone port math from this single source.
pub const SP_PORT_BASE: u16 = 33300;
/// Ereport port base; see [`SP_PORT_BASE`].
pub const EREPORT_BASE: u16 = 44400;
/// Per-gimlet port-group stride; see [`SP_PORT_BASE`].
pub const PORT_STRIDE: u16 = 10;
/// Host-cpu serial-console offset from an SP's `base_port`; see [`SP_PORT_BASE`].
pub const CONSOLE_PORT_OFFSET: u16 = 2;

/// The gimlet board part number reported by every sled SP.
const GIMLET_PART_NUMBER: &str = "913-0000019";
/// The (simulated) sidecar SP serial.
const SIDECAR_SERIAL: &str = "SimSidecar0";

/// Manufacturing root cert seed - a constant test value shared by every SP's RoT
/// (matches a4x2's known-good config; attestation is verified against it).
const ROOT_SEED: &str = "01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de";

/// Per-SP RoT device-id cert seed: `01de` followed by a 60-hex-digit index. The
/// sidecar is 0; gimlet `i` is `i + 1`.
fn device_seed(index: usize) -> String {
    format!("01de{index:060x}")
}

/// What an SP is within the rack - the switch's SP, or a sled's SP by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpRole {
    /// The switch's SP (the sidecar).
    Sidecar,
    /// A sled's SP, by sled index.
    Gimlet(usize),
}

/// Which provider backs the fleet, and therefore the address MGS dials to reach
/// each SP. This is the pluggable knob: identities + ports are fixed; only the
/// host changes.
#[derive(Debug, Clone, PartialEq)]
pub enum SpBackend {
    /// omicron `sp-sim` on loopback in the switch zone (today's default).
    Sim,
    /// Real Hubris firmware on the native Rust SP emulator (`sp-emu`), running in
    /// the switch zone on loopback exactly like `sp-sim` - so MGS reaches it at the
    /// same `[::1]:333xx` unicast surface (the emulator's VLAN/trust/location logic
    /// is internal to its bridge and never seen by MGS). Selectable per-SP, so a
    /// fleet can run real firmware for the SPs under test and sim for the rest.
    Emu,
    /// Real Hubris firmware reached at an explicit remote `host` (SPs running on
    /// another box). Future/out-of-zone variant; exercised only by unit tests.
    Central { host: String },
}

impl SpBackend {
    /// The host portion MGS connects to. In-zone backends (sim + emu) bind
    /// loopback; a remote emulator binds an explicit `host`.
    fn mgs_host(&self) -> &str {
        match self {
            SpBackend::Sim | SpBackend::Emu => "[::1]",
            SpBackend::Central { host } => host,
        }
    }
}

/// One SP (with its RoT identity) the control plane expects - everything MGS and
/// the SP provider must agree on: identity, the MGS-facing address/ports, and the
/// switch-port wiring. The unit of the shared contract.
#[derive(Debug, Clone)]
pub struct Sp {
    pub role: SpRole,
    pub serial: String,
    /// Gimlets carry a board part number; the sidecar doesn't.
    pub part_number: Option<String>,
    pub root_cert_seed: String,
    pub device_id_seed: String,
    /// MGS↔SP UDP base port; the two switch instances use `base_port + 0/1`.
    pub base_port: u16,
    /// Ereport base port; instances `ereport_base + 0/1`.
    pub ereport_base: u16,
    /// Address MGS connects to (loopback for sp-sim, a real host for emulators).
    pub mgs_host: String,
    /// MGS `[[switch.port]]` `fake-interface` name.
    pub fake_interface: String,
    /// MGS `[[switch.port]]` `ignition-target`.
    pub ignition_target: u8,
    /// Which provider backs this SP (sp-sim / sp-emu / remote). Per-SP so a fleet
    /// can mix emulated and simulated SPs.
    pub backend: SpBackend,
}

impl Sp {
    /// The `location` table for this SP's MGS port (switch vs sled).
    pub fn mgs_location(&self) -> String {
        match self.role {
            SpRole::Sidecar => {
                "{ switch0 = [\"switch\", 0], switch1 = [\"switch\", 1] }".to_string()
            }
            SpRole::Gimlet(i) => {
                format!("{{ switch0 = [\"sled\", {i}], switch1 = [\"sled\", {i}] }}")
            }
        }
    }

    /// Whether a `[sp].emu` selector names this SP: `"sidecar"`, or `"g{index}"`
    /// (the global gimlet index).
    pub fn matches_selector(&self, sel: &str) -> bool {
        match self.role {
            SpRole::Sidecar => sel == "sidecar",
            SpRole::Gimlet(i) => {
                sel.strip_prefix('g').and_then(|d| d.parse::<usize>().ok()) == Some(i)
            }
        }
    }

    /// This SP's `[sp].emu` selector form: `"sidecar"` or `"g{index}"`.
    pub fn selector(&self) -> String {
        match self.role {
            SpRole::Sidecar => "sidecar".to_string(),
            SpRole::Gimlet(i) => format!("g{i}"),
        }
    }
}

/// Render one `[[simulated_sps.<key>]]` block (identity + the two per-instance
/// `network_config`/`ereport_network_config` tables) for `sp`. The sidecar and
/// gimlet blocks are identical but for the table `key` (and the sidecar's absent
/// `part_number`, which is `None` so emits nothing); the gimlet's extra host-cpu
/// `components` block is emitted by the caller.
fn render_sp_block(o: &mut String, key: &str, sp: &Sp) {
    writeln!(o, "\n[[simulated_sps.{key}]]").unwrap();
    if let Some(pn) = &sp.part_number {
        writeln!(o, "part_number = \"{pn}\"").unwrap();
    }
    writeln!(o, "serial_number = \"{}\"", sp.serial).unwrap();
    writeln!(
        o,
        "manufacturing_root_cert_seed = \"{}\"",
        sp.root_cert_seed
    )
    .unwrap();
    writeln!(o, "device_id_cert_seed = \"{}\"", sp.device_id_seed).unwrap();
    for inst in 0u16..2 {
        writeln!(o, "\n[[simulated_sps.{key}.network_config]]").unwrap();
        writeln!(o, "[simulated_sps.{key}.network_config.simulated]").unwrap();
        writeln!(o, "bind_addr = \"[::]:{}\"", sp.base_port + inst).unwrap();
    }
    for inst in 0u16..2 {
        writeln!(o, "\n[[simulated_sps.{key}.ereport_network_config]]").unwrap();
        writeln!(o, "[simulated_sps.{key}.ereport_network_config.simulated]").unwrap();
        writeln!(o, "bind_addr = \"[::1]:{}\"", sp.ereport_base + inst).unwrap();
    }
}

/// The rack's SP/RoT fleet: the sidecar SP + one gimlet SP per sled, built for a
/// given [`SpBackend`]. The single source of truth `crate::mgs` and the SP
/// provider both read from.
#[derive(Debug, Clone)]
pub struct SpFleet {
    pub backend: SpBackend,
    /// Sidecar first, then gimlet `0..num_gimlets`.
    pub sps: Vec<Sp>,
}

impl SpFleet {
    /// Build the fleet for `num_gimlets` sleds (one SP each) + the sidecar - i.e.
    /// the gimlet *global* indices `0..num_gimlets`.
    pub fn new(num_gimlets: usize, backend: SpBackend) -> Self {
        Self::for_gimlets(&(0..num_gimlets).collect::<Vec<_>>(), backend)
    }

    /// Build the fleet for an explicit set of gimlet **global** indices (one SP
    /// each) + the sidecar. Used for a single rack within a multi-rack deployment:
    /// e.g. rack 1's sleds are `[3, 4, 5]`.
    ///
    /// Identity-bearing fields (serial, device-id seed, MGS ports, fake-interface)
    /// derive from the **global** index so they stay aligned with the sled's
    /// SMBIOS serial + sprockets identity (which `voxel` keys off the global
    /// index). The `ignition-target` instead uses the gimlet's **position within
    /// this fleet**, so each rack gets a clean target permutation that can't
    /// collide with the sidecar's target (1). With `indices = 0..n` this is
    /// byte-identical to the old single-rack fleet.
    pub fn for_gimlets(gimlet_indices: &[usize], backend: SpBackend) -> Self {
        let n = gimlet_indices.len();
        let mut sps = Vec::with_capacity(n + 1);

        // Sidecar SP: base 33300 / ereport 44400, fake-switch0, ignition 1.
        sps.push(Sp {
            role: SpRole::Sidecar,
            serial: SIDECAR_SERIAL.to_string(),
            part_number: None,
            root_cert_seed: ROOT_SEED.to_string(),
            device_id_seed: device_seed(0),
            base_port: SP_PORT_BASE,
            ereport_base: EREPORT_BASE,
            mgs_host: backend.mgs_host().to_string(),
            fake_interface: "fake-switch0".to_string(),
            ignition_target: 1,
            backend: backend.clone(),
        });

        // Gimlet SPs: one per sled. Port group base 33300 + 10*(i+1), keyed by the
        // GLOBAL index `i`; ignition-target keyed by the LOCAL position `pos`.
        for (pos, &i) in gimlet_indices.iter().enumerate() {
            let role = SpRole::Gimlet(i);
            sps.push(Sp {
                role,
                // Matches the SMBIOS serial `voxel` bakes (`2{index:07}`); for
                // i < 10 this is byte-identical to the old `2000000{i}`.
                serial: format!("2{i:07}"),
                part_number: Some(GIMLET_PART_NUMBER.to_string()),
                root_cert_seed: ROOT_SEED.to_string(),
                device_id_seed: device_seed(i + 1),
                base_port: SP_PORT_BASE + PORT_STRIDE * (i as u16 + 1),
                ereport_base: EREPORT_BASE + PORT_STRIDE * (i as u16 + 1),
                mgs_host: backend.mgs_host().to_string(),
                fake_interface: format!("fake-sled{i}"),
                ignition_target: ((pos + 2) % (n + 1)) as u8,
                backend: backend.clone(),
            });
        }

        Self { backend, sps }
    }

    /// The loopback `sp-sim` fleet (today's default).
    pub fn sim(num_gimlets: usize) -> Self {
        Self::new(num_gimlets, SpBackend::Sim)
    }

    /// The loopback `sp-sim` fleet for an explicit set of gimlet global indices -
    /// one rack of a multi-rack deployment (see [`SpFleet::for_gimlets`]).
    pub fn sim_for_gimlets(gimlet_indices: &[usize]) -> Self {
        Self::for_gimlets(gimlet_indices, SpBackend::Sim)
    }

    /// A hybrid fleet for `gimlet_indices`: sp-sim by default, with the SPs named
    /// in `emu` backed by `sp-emu` instead. Selectors are `"sidecar"` / `"g{index}"`
    /// (e.g. `["sidecar", "g0"]`); unknown selectors are ignored. Both providers run
    /// in-zone on loopback, so the MGS port surface is unchanged.
    pub fn sim_with_emu(gimlet_indices: &[usize], emu: &[String]) -> Self {
        let mut fleet = Self::for_gimlets(gimlet_indices, SpBackend::Sim);
        for sp in &mut fleet.sps {
            if emu.iter().any(|sel| sp.matches_selector(sel)) {
                sp.backend = SpBackend::Emu;
                sp.mgs_host = SpBackend::Emu.mgs_host().to_string();
            }
        }
        fleet
    }

    /// Whether any SP is emulator-backed - drives MGS's RPC timeouts (the emulator
    /// is slow; see [`crate::mgs`]) and the in-zone sp-emu process launch.
    pub fn has_emu(&self) -> bool {
        self.sps.iter().any(|sp| sp.backend == SpBackend::Emu)
    }

    /// The emulator-backed SPs in fleet order (sidecar first) - one in-zone sp-emu
    /// process + flash file each.
    pub fn emu_sps(&self) -> Vec<&Sp> {
        self.sps
            .iter()
            .filter(|sp| sp.backend == SpBackend::Emu)
            .collect()
    }

    /// The sidecar SP (always present, first).
    pub fn sidecar(&self) -> &Sp {
        &self.sps[0]
    }

    /// The gimlet SPs, in sled-index order.
    pub fn gimlets(&self) -> &[Sp] {
        &self.sps[1..]
    }

    /// Render `smf/sp-sim/config.toml` for this fleet. Meaningful for the `Sim`
    /// backend (the emulated backends run real firmware and bind their own
    /// addresses); the ports are sourced from the fleet either way.
    pub fn sp_sim_config(&self) -> String {
        let mut o = String::new();
        writeln!(o, "#").unwrap();
        writeln!(o, "# SP simulator config - generated by voxel-config.").unwrap();
        writeln!(o, "#").unwrap();

        // Sidecar SP: the switch. Emitted only when sp-sim backs it (an emulated
        // sidecar is run by sp-emu, not sp-sim).
        let sidecar = self.sidecar();
        if sidecar.backend == SpBackend::Sim {
            render_sp_block(&mut o, "sidecar", sidecar);
        }

        // Gimlet SPs: one per sled (sp-sim-backed only; emulated ones run on sp-emu).
        for sp in self.gimlets() {
            if sp.backend != SpBackend::Sim {
                continue;
            }
            render_sp_block(&mut o, "gimlet", sp);
            // The host-cpu component is gimlet-only (the sidecar has none).
            writeln!(o, "\n[[simulated_sps.gimlet.components]]").unwrap();
            writeln!(o, "id = \"sp3-host-cpu\"").unwrap();
            writeln!(o, "device = \"sp3-host-cpu\"").unwrap();
            writeln!(o, "description = \"FAKE host cpu\"").unwrap();
            writeln!(o, "capabilities = 0").unwrap();
            writeln!(o, "presence = \"Present\"").unwrap();
            writeln!(
                o,
                "serial_console = \"[::1]:{}\"",
                sp.base_port + CONSOLE_PORT_OFFSET
            )
            .unwrap();
        }

        writeln!(o, "\n[log]").unwrap();
        writeln!(o, "level = \"debug\"").unwrap();
        writeln!(o, "mode = \"stderr-terminal\"").unwrap();

        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_toml_with_expected_sps() {
        let s = SpFleet::sim(4).sp_sim_config();
        let v: toml::Value = toml::from_str(&s).expect("sp-sim renders valid TOML");

        let sidecars = v["simulated_sps"]["sidecar"].as_array().unwrap();
        assert_eq!(sidecars.len(), 1);
        assert_eq!(sidecars[0]["serial_number"].as_str(), Some("SimSidecar0"));

        let gimlets = v["simulated_sps"]["gimlet"].as_array().unwrap();
        assert_eq!(gimlets.len(), 4);
        assert_eq!(gimlets[0]["serial_number"].as_str(), Some("20000000"));
        assert_eq!(gimlets[3]["serial_number"].as_str(), Some("20000003"));
    }

    #[test]
    fn device_seeds_are_distinct_64_hex() {
        assert_eq!(device_seed(0), "01de".to_string() + &"0".repeat(60));
        assert!(device_seed(4).ends_with("04"));
        assert_eq!(device_seed(4).len(), 64);
    }

    #[test]
    fn gimlet_count_scales() {
        let s = SpFleet::sim(2).sp_sim_config();
        let v: toml::Value = toml::from_str(&s).unwrap();
        assert_eq!(v["simulated_sps"]["gimlet"].as_array().unwrap().len(), 2);
        // gimlet1's first bind port is 33320.
        assert!(s.contains("bind_addr = \"[::]:33320\""));
        // No third gimlet's ports.
        assert!(!s.contains("33330"));
    }

    #[test]
    fn per_rack_fleet_keeps_global_identity_local_ignition() {
        // Rack 1 of an a3x2x2 deployment: sleds g3,g4,g5.
        let f = SpFleet::for_gimlets(&[3, 4, 5], SpBackend::Sim);
        let g = f.gimlets();
        assert_eq!(g.len(), 3);
        // Identity-bearing fields use the GLOBAL index (aligned with SMBIOS/sprockets).
        assert_eq!(g[0].serial, "20000003");
        assert_eq!(g[2].serial, "20000005");
        assert_eq!(g[0].base_port, 33340); // 33300 + 10*(3+1)
        assert_eq!(g[2].fake_interface, "fake-sled5");
        assert!(g[0].device_id_seed.ends_with("04")); // device_seed(3+1)
                                                      // The SP slot (location) is the global index - rack 1 sits in cubbies 3,4,5.
        assert_eq!(
            g[0].mgs_location(),
            "{ switch0 = [\"sled\", 3], switch1 = [\"sled\", 3] }"
        );
        // ignition-target is a per-rack permutation (pos+2 mod n+1): 2,3,0 - and
        // never collides with the sidecar's target (1).
        assert_eq!(f.sidecar().ignition_target, 1);
        let targets: Vec<u8> = g.iter().map(|s| s.ignition_target).collect();
        assert_eq!(targets, vec![2, 3, 0]);
        assert!(!targets.contains(&1));
    }

    #[test]
    fn hybrid_emu_splits_providers() {
        // sidecar + g0 on the emulator; g1..g3 stay on sp-sim.
        let f = SpFleet::sim_with_emu(&[0, 1, 2, 3], &["sidecar".into(), "g0".into()]);
        assert!(f.has_emu());
        // emu set is sidecar + g0, in fleet order.
        let emu: Vec<&str> = f
            .emu_sps()
            .iter()
            .map(|s| s.fake_interface.as_str())
            .collect();
        assert_eq!(emu, vec!["fake-switch0", "fake-sled0"]);
        // sp-sim config omits the emulated SPs: no sidecar block, gimlets g1..g3 only.
        let v: toml::Value = toml::from_str(&f.sp_sim_config()).expect("valid TOML");
        assert!(
            v["simulated_sps"].get("sidecar").is_none(),
            "emu sidecar not in sp-sim"
        );
        assert_eq!(v["simulated_sps"]["gimlet"].as_array().unwrap().len(), 3);
        // Both providers are in-zone on loopback - MGS surface is unchanged.
        assert_eq!(f.sidecar().mgs_host, "[::1]");
        assert_eq!(f.gimlets()[0].mgs_host, "[::1]");
    }

    #[test]
    fn all_sim_fleet_has_no_emu() {
        let f = SpFleet::sim(4);
        assert!(!f.has_emu());
        assert!(f.emu_sps().is_empty());
        // sp-sim still renders all 4 gimlets + sidecar (provider split is a no-op).
        let v: toml::Value = toml::from_str(&f.sp_sim_config()).unwrap();
        assert_eq!(v["simulated_sps"]["gimlet"].as_array().unwrap().len(), 4);
        assert!(v["simulated_sps"].get("sidecar").is_some());
    }

    #[test]
    fn new_is_for_gimlets_zero_to_n() {
        // The single-rack constructor must stay byte-identical to the explicit form.
        assert_eq!(
            SpFleet::sim(4).sp_sim_config(),
            SpFleet::for_gimlets(&[0, 1, 2, 3], SpBackend::Sim).sp_sim_config()
        );
    }

    #[test]
    fn fleet_identities_are_per_sled_and_backend_independent() {
        // Identities + ports are invariant across backends; only the MGS host
        // (the pluggable bit) changes.
        let sim = SpFleet::sim(4);
        let emu = SpFleet::new(
            4,
            SpBackend::Central {
                host: "[fdb0:a840:2500:1::1]".into(),
            },
        );

        // Same fleet shape + identities.
        assert_eq!(sim.sps.len(), 5); // sidecar + 4 gimlets
        assert_eq!(sim.sidecar().serial, "SimSidecar0");
        assert_eq!(sim.gimlets()[0].base_port, emu.gimlets()[0].base_port);
        assert_eq!(
            sim.gimlets()[2].device_id_seed,
            emu.gimlets()[2].device_id_seed
        );

        // Only the MGS-facing host differs.
        assert_eq!(sim.sidecar().mgs_host, "[::1]");
        assert_eq!(emu.sidecar().mgs_host, "[fdb0:a840:2500:1::1]");
    }
}
