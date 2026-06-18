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
    /// Real Hubris firmware (Renode interim, or the native Rust emulator) reached
    /// at a single shared `host` for every SP - e.g. a router's management
    /// address while the SPs run centrally. (Per-sled distribution, where each SP
    /// has its own address, is a future variant.)
    ///
    /// Not yet wired into a `voxel launch`: production always uses [`SpBackend::Sim`]
    /// today. This variant is the seam the emulator track plugs into (see the
    /// module docs); for now it's exercised only by the unit tests.
    Central { host: String },
}

impl SpBackend {
    /// The host portion MGS connects to. Sim binds loopback; an emulator binds a
    /// real `host` (same for every SP today).
    fn mgs_host(&self) -> &str {
        match self {
            SpBackend::Sim => "[::1]",
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
            serial: "SimSidecar0".to_string(),
            part_number: None,
            root_cert_seed: ROOT_SEED.to_string(),
            device_id_seed: device_seed(0),
            base_port: 33300,
            ereport_base: 44400,
            mgs_host: backend.mgs_host().to_string(),
            fake_interface: "fake-switch0".to_string(),
            ignition_target: 1,
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
                part_number: Some("913-0000019".to_string()),
                root_cert_seed: ROOT_SEED.to_string(),
                device_id_seed: device_seed(i + 1),
                base_port: 33300 + 10 * (i as u16 + 1),
                ereport_base: 44400 + 10 * (i as u16 + 1),
                mgs_host: backend.mgs_host().to_string(),
                fake_interface: format!("fake-sled{i}"),
                ignition_target: ((pos + 2) % (n + 1)) as u8,
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

        // Sidecar SP: the switch.
        let sidecar = self.sidecar();
        writeln!(o, "\n[[simulated_sps.sidecar]]").unwrap();
        writeln!(o, "serial_number = \"{}\"", sidecar.serial).unwrap();
        writeln!(o, "manufacturing_root_cert_seed = \"{}\"", sidecar.root_cert_seed).unwrap();
        writeln!(o, "device_id_cert_seed = \"{}\"", sidecar.device_id_seed).unwrap();
        for inst in 0u16..2 {
            writeln!(o, "\n[[simulated_sps.sidecar.network_config]]").unwrap();
            writeln!(o, "[simulated_sps.sidecar.network_config.simulated]").unwrap();
            writeln!(o, "bind_addr = \"[::]:{}\"", sidecar.base_port + inst).unwrap();
        }
        for inst in 0u16..2 {
            writeln!(o, "\n[[simulated_sps.sidecar.ereport_network_config]]").unwrap();
            writeln!(o, "[simulated_sps.sidecar.ereport_network_config.simulated]").unwrap();
            writeln!(o, "bind_addr = \"[::1]:{}\"", sidecar.ereport_base + inst).unwrap();
        }

        // Gimlet SPs: one per sled.
        for sp in self.gimlets() {
            writeln!(o, "\n[[simulated_sps.gimlet]]").unwrap();
            if let Some(pn) = &sp.part_number {
                writeln!(o, "part_number = \"{pn}\"").unwrap();
            }
            writeln!(o, "serial_number = \"{}\"", sp.serial).unwrap();
            writeln!(o, "manufacturing_root_cert_seed = \"{}\"", sp.root_cert_seed).unwrap();
            writeln!(o, "device_id_cert_seed = \"{}\"", sp.device_id_seed).unwrap();
            for inst in 0u16..2 {
                writeln!(o, "\n[[simulated_sps.gimlet.network_config]]").unwrap();
                writeln!(o, "[simulated_sps.gimlet.network_config.simulated]").unwrap();
                writeln!(o, "bind_addr = \"[::]:{}\"", sp.base_port + inst).unwrap();
            }
            for inst in 0u16..2 {
                writeln!(o, "\n[[simulated_sps.gimlet.ereport_network_config]]").unwrap();
                writeln!(o, "[simulated_sps.gimlet.ereport_network_config.simulated]").unwrap();
                writeln!(o, "bind_addr = \"[::1]:{}\"", sp.ereport_base + inst).unwrap();
            }
            writeln!(o, "\n[[simulated_sps.gimlet.components]]").unwrap();
            writeln!(o, "id = \"sp3-host-cpu\"").unwrap();
            writeln!(o, "device = \"sp3-host-cpu\"").unwrap();
            writeln!(o, "description = \"FAKE host cpu\"").unwrap();
            writeln!(o, "capabilities = 0").unwrap();
            writeln!(o, "presence = \"Present\"").unwrap();
            writeln!(o, "serial_console = \"[::1]:{}\"", sp.base_port + 2).unwrap();
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
        assert_eq!(g[0].mgs_location(), "{ switch0 = [\"sled\", 3], switch1 = [\"sled\", 3] }");
        // ignition-target is a per-rack permutation (pos+2 mod n+1): 2,3,0 - and
        // never collides with the sidecar's target (1).
        assert_eq!(f.sidecar().ignition_target, 1);
        let targets: Vec<u8> = g.iter().map(|s| s.ignition_target).collect();
        assert_eq!(targets, vec![2, 3, 0]);
        assert!(!targets.contains(&1));
    }

    #[test]
    fn new_is_for_gimlets_zero_to_n() {
        // The single-rack constructor must stay byte-identical to the explicit form.
        assert_eq!(SpFleet::sim(4).sp_sim_config(), SpFleet::for_gimlets(&[0, 1, 2, 3], SpBackend::Sim).sp_sim_config());
    }

    #[test]
    fn fleet_identities_are_per_sled_and_backend_independent() {
        // Identities + ports are invariant across backends; only the MGS host
        // (the pluggable bit) changes.
        let sim = SpFleet::sim(4);
        let emu = SpFleet::new(4, SpBackend::Central { host: "[fdb0:a840:2500:1::1]".into() });

        // Same fleet shape + identities.
        assert_eq!(sim.sps.len(), 5); // sidecar + 4 gimlets
        assert_eq!(sim.sidecar().serial, "SimSidecar0");
        assert_eq!(sim.gimlets()[0].base_port, emu.gimlets()[0].base_port);
        assert_eq!(sim.gimlets()[2].device_id_seed, emu.gimlets()[2].device_id_seed);

        // Only the MGS-facing host differs.
        assert_eq!(sim.sidecar().mgs_host, "[::1]");
        assert_eq!(emu.sidecar().mgs_host, "[fdb0:a840:2500:1::1]");
    }
}
