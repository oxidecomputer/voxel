// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rack's SP and RoT fleet, the contract MGS and the SP provider share,
//! plus sp-sim config generation. Identities and ports are fixed per backend.

use std::fmt::Write as _;

/// SP port scheme: the sidecar binds this base, gimlet i offsets it by
/// PORT_STRIDE * (i + 1), and switch0 and switch1 use base + 0 and base + 1.
pub const SP_PORT_BASE: u16 = 33300;
/// Ereport port base, same scheme as SP_PORT_BASE.
pub const EREPORT_BASE: u16 = 44400;
/// Per-gimlet port group stride.
pub const PORT_STRIDE: u16 = 10;
/// Host CPU serial console offset from an SP's base port.
pub const CONSOLE_PORT_OFFSET: u16 = 2;

/// The gimlet board part number reported by every sled SP.
const GIMLET_PART_NUMBER: &str = "913-0000019";
/// The simulated sidecar SP serial.
const SIDECAR_SERIAL: &str = "SimSidecar0";
/// The sidecar board part number, sp-emu's placeholder. The VPD barcode field
/// caps it at 11 characters.
const SIDECAR_PART_NUMBER: &str = "SIDECAR-C";

/// Manufacturing root cert seed, a constant test value shared by every RoT.
/// Attestation is verified against it.
const ROOT_SEED: &str =
    "01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de";

/// Per-SP RoT device id cert seed: 01de then a 60 hex digit index. The sidecar
/// is 0, gimlet i is i + 1.
fn device_seed(index: usize) -> String {
    format!("01de{index:060x}")
}

/// What an SP is within the rack: the switch's SP, or a sled's SP by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpRole {
    /// The switch's SP, the sidecar.
    Sidecar,
    /// A sled's SP, by sled index.
    Gimlet(usize),
}

/// Which provider backs the fleet, and therefore the address MGS dials.
/// Identities and ports are fixed; only the host changes.
#[derive(Debug, Clone, PartialEq)]
pub enum SpBackend {
    /// omicron sp-sim on loopback in the switch zone, the default.
    Sim,
    /// Real Hubris firmware on sp-emu, running on the falcon host at addr. One
    /// fleet backs the whole rack; each SP binds base_port + 0 and + 1 there.
    Emu { addr: String },
}

impl SpBackend {
    /// The bracketed host MGS connects to: loopback for sp-sim, the rack's
    /// fleet address on the falcon host for sp-emu.
    fn mgs_host(&self) -> String {
        match self {
            SpBackend::Sim => "[::1]".to_string(),
            SpBackend::Emu { addr } => format!("[{addr}]"),
        }
    }
}

/// One SP with its RoT identity: everything MGS and the SP provider must agree
/// on, identity, MGS-facing address and ports, and switch port wiring.
#[derive(Debug, Clone)]
pub struct Sp {
    pub role: SpRole,
    pub serial: String,
    /// Board part number: real on gimlets, a placeholder on the sidecar.
    pub part_number: Option<String>,
    pub root_cert_seed: String,
    pub device_id_seed: String,
    /// MGS to SP UDP base port. The switch instances use base_port + 0 and + 1.
    pub base_port: u16,
    /// Ereport base port. Instances use ereport_base + 0 and + 1.
    pub ereport_base: u16,
    /// Address MGS connects to: loopback for sp-sim, a real host for emulators.
    pub mgs_host: String,
    /// MGS switch.port fake-interface name.
    pub fake_interface: String,
    /// MGS switch.port ignition-target.
    pub ignition_target: u8,
    /// Which provider backs this SP. Per SP, so a fleet can mix emulated and
    /// simulated SPs.
    pub backend: SpBackend,
}

impl Sp {
    /// The location table for this SP's MGS port.
    pub fn mgs_location(&self) -> String {
        match self.role {
            SpRole::Sidecar => {
                "{ switch0 = [\"switch\", 0], switch1 = [\"switch\", 1] }"
                    .to_string()
            }
            SpRole::Gimlet(i) => {
                format!(
                    "{{ switch0 = [\"sled\", {i}], switch1 = [\"sled\", {i}] }}"
                )
            }
        }
    }

    /// Whether an [sp].emu selector names this SP: sidecar, or g<index> by the
    /// global gimlet index.
    pub fn matches_selector(&self, sel: &str) -> bool {
        match self.role {
            SpRole::Sidecar => sel == "sidecar",
            SpRole::Gimlet(i) => {
                sel.strip_prefix('g').and_then(|d| d.parse::<usize>().ok())
                    == Some(i)
            }
        }
    }

    /// This SP's [sp].emu selector: sidecar or g<index>.
    pub fn selector(&self) -> String {
        match self.role {
            SpRole::Sidecar => "sidecar".to_string(),
            SpRole::Gimlet(i) => format!("g{i}"),
        }
    }

    /// The port an emulated SP serves its host power bridge on, after the MGS
    /// pair.
    pub fn power_port(&self) -> u16 {
        self.base_port + 2
    }

    /// This SP as one SP_EMU_IGNITION entry, its ignition port and system type.
    /// The emulated ignition controller then matches the MGS configuration.
    pub fn ignition_entry(&self) -> String {
        let kind = match self.role {
            SpRole::Sidecar => "sidecar",
            SpRole::Gimlet(_) => "gimlet",
        };
        format!("{}:{kind}", self.ignition_target)
    }
}

/// Render one simulated_sps block for sp: identity plus the two per-instance
/// network tables. The caller emits the gimlet's host CPU component block.
fn render_sp_block(o: &mut String, key: &str, sp: &Sp) {
    writeln!(o, "\n[[simulated_sps.{key}]]").unwrap();
    if let Some(pn) = &sp.part_number {
        writeln!(o, "part_number = \"{pn}\"").unwrap();
    }
    writeln!(o, "serial_number = \"{}\"", sp.serial).unwrap();
    writeln!(o, "manufacturing_root_cert_seed = \"{}\"", sp.root_cert_seed)
        .unwrap();
    writeln!(o, "device_id_cert_seed = \"{}\"", sp.device_id_seed).unwrap();
    for inst in 0u16..2 {
        writeln!(o, "\n[[simulated_sps.{key}.network_config]]").unwrap();
        writeln!(o, "[simulated_sps.{key}.network_config.simulated]").unwrap();
        writeln!(o, "bind_addr = \"[::]:{}\"", sp.base_port + inst).unwrap();
    }
    for inst in 0u16..2 {
        writeln!(o, "\n[[simulated_sps.{key}.ereport_network_config]]")
            .unwrap();
        writeln!(o, "[simulated_sps.{key}.ereport_network_config.simulated]")
            .unwrap();
        writeln!(o, "bind_addr = \"[::1]:{}\"", sp.ereport_base + inst)
            .unwrap();
    }
}

/// The rack's SP and RoT fleet: the sidecar SP and one gimlet SP per sled, for
/// one backend. The single source both mgs and the SP provider read from.
#[derive(Debug, Clone)]
pub struct SpFleet {
    pub backend: SpBackend,
    /// Sidecar first, then the gimlets in index order.
    pub sps: Vec<Sp>,
}

impl SpFleet {
    /// Build the fleet for num_gimlets sleds and the sidecar, global gimlet
    /// indices 0 to num_gimlets.
    pub fn new(num_gimlets: usize, backend: SpBackend) -> Self {
        Self::for_gimlets(&(0..num_gimlets).collect::<Vec<_>>(), backend)
    }

    /// Build the fleet for explicit global gimlet indices and the sidecar, one
    /// rack of a multi-rack deployment. Identity derives from the global index.
    pub fn for_gimlets(gimlet_indices: &[usize], backend: SpBackend) -> Self {
        let n = gimlet_indices.len();
        let mut sps = Vec::with_capacity(n + 1);

        // Sidecar SP: base 33300, ereport 44400, fake-switch0, ignition 1.
        sps.push(Sp {
            role: SpRole::Sidecar,
            serial: SIDECAR_SERIAL.to_string(),
            part_number: Some(SIDECAR_PART_NUMBER.to_string()),
            root_cert_seed: ROOT_SEED.to_string(),
            device_id_seed: device_seed(0),
            base_port: SP_PORT_BASE,
            ereport_base: EREPORT_BASE,
            mgs_host: backend.mgs_host(),
            fake_interface: "fake-switch0".to_string(),
            ignition_target: 1,
            backend: backend.clone(),
        });

        // Gimlet SPs: ports keyed by the global index i, ignition target by the
        // position within this fleet.
        for (pos, &i) in gimlet_indices.iter().enumerate() {
            let role = SpRole::Gimlet(i);
            sps.push(Sp {
                role,
                // Matches the SMBIOS serial and the sprockets platform id.
                serial: format!("2FAKE{i:03}"),
                part_number: Some(GIMLET_PART_NUMBER.to_string()),
                root_cert_seed: ROOT_SEED.to_string(),
                device_id_seed: device_seed(i + 1),
                base_port: SP_PORT_BASE + PORT_STRIDE * (i as u16 + 1),
                ereport_base: EREPORT_BASE + PORT_STRIDE * (i as u16 + 1),
                mgs_host: backend.mgs_host(),
                fake_interface: format!("fake-sled{i}"),
                ignition_target: ((pos + 2) % (n + 1)) as u8,
                backend: backend.clone(),
            });
        }

        Self { backend, sps }
    }

    /// The loopback sp-sim fleet, the default.
    pub fn sim(num_gimlets: usize) -> Self {
        Self::new(num_gimlets, SpBackend::Sim)
    }

    /// The loopback sp-sim fleet for explicit global gimlet indices, one rack
    /// of a multi-rack deployment.
    pub fn sim_for_gimlets(gimlet_indices: &[usize]) -> Self {
        Self::for_gimlets(gimlet_indices, SpBackend::Sim)
    }

    /// A hybrid fleet: sp-sim by default, the SPs named in emu on sp-emu at
    /// addr. Selectors are sidecar or g<index>; unknown ones are ignored.
    pub fn sim_with_emu(
        gimlet_indices: &[usize],
        emu: &[String],
        addr: &str,
    ) -> Self {
        let mut fleet = Self::for_gimlets(gimlet_indices, SpBackend::Sim);
        for sp in &mut fleet.sps {
            if emu.iter().any(|sel| sp.matches_selector(sel)) {
                let backend = SpBackend::Emu { addr: addr.to_string() };
                sp.mgs_host = backend.mgs_host();
                sp.backend = backend;
            }
        }
        fleet
    }

    /// Whether any SP is emulator backed. Drives the MGS RPC timeouts and the
    /// host fleet launch.
    pub fn has_emu(&self) -> bool {
        self.sps.iter().any(|sp| matches!(sp.backend, SpBackend::Emu { .. }))
    }

    /// The emulator backed SPs in fleet order, one sp-emu process each.
    pub fn emu_sps(&self) -> Vec<&Sp> {
        self.sps
            .iter()
            .filter(|sp| matches!(sp.backend, SpBackend::Emu { .. }))
            .collect()
    }

    /// The sidecar SP, always present and first.
    pub fn sidecar(&self) -> &Sp {
        &self.sps[0]
    }

    /// The gimlet SPs, in sled-index order.
    pub fn gimlets(&self) -> &[Sp] {
        &self.sps[1..]
    }

    /// Render the sp-sim config for this fleet. Only the Sim backed SPs appear;
    /// emulated ones run real firmware and bind their own addresses.
    pub fn sp_sim_config(&self) -> String {
        let mut o = String::new();
        writeln!(o, "#").unwrap();
        writeln!(o, "# SP simulator config - generated by voxel-config.")
            .unwrap();
        writeln!(o, "#").unwrap();

        // The sidecar, emitted only when sp-sim backs it.
        let sidecar = self.sidecar();
        if sidecar.backend == SpBackend::Sim {
            render_sp_block(&mut o, "sidecar", sidecar);
        }

        // Gimlet SPs, sp-sim backed only.
        for sp in self.gimlets() {
            if sp.backend != SpBackend::Sim {
                continue;
            }
            render_sp_block(&mut o, "gimlet", sp);
            // The host CPU component is gimlet only.
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
        let v: toml::Value =
            toml::from_str(&s).expect("sp-sim renders valid TOML");

        let sidecars = v["simulated_sps"]["sidecar"].as_array().unwrap();
        assert_eq!(sidecars.len(), 1);
        assert_eq!(sidecars[0]["serial_number"].as_str(), Some("SimSidecar0"));
        assert_eq!(sidecars[0]["part_number"].as_str(), Some("SIDECAR-C"));

        let gimlets = v["simulated_sps"]["gimlet"].as_array().unwrap();
        assert_eq!(gimlets.len(), 4);
        assert_eq!(gimlets[0]["serial_number"].as_str(), Some("2FAKE000"));
        assert_eq!(gimlets[3]["serial_number"].as_str(), Some("2FAKE003"));
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
        // Identity fields use the global index, as SMBIOS and sprockets do.
        assert_eq!(g[0].serial, "2FAKE003");
        assert_eq!(g[2].serial, "2FAKE005");
        assert_eq!(g[0].base_port, 33340); // 33300 + 10*(3+1)
        assert_eq!(g[2].fake_interface, "fake-sled5");
        assert!(g[0].device_id_seed.ends_with("04")); // device_seed(3+1)
        // The SP slot is the global index: rack 1 sits in cubbies 3, 4, 5.
        assert_eq!(
            g[0].mgs_location(),
            "{ switch0 = [\"sled\", 3], switch1 = [\"sled\", 3] }"
        );
        // ignition-target is a per-rack permutation, pos + 2 mod n + 1: 2, 3,
        // 0. It never collides with the sidecar's target 1.
        assert_eq!(f.sidecar().ignition_target, 1);
        let targets: Vec<u8> = g.iter().map(|s| s.ignition_target).collect();
        assert_eq!(targets, vec![2, 3, 0]);
        assert!(!targets.contains(&1));
    }

    #[test]
    fn hybrid_emu_splits_providers() {
        // sidecar and g0 on the emulator, g1 to g3 on sp-sim.
        let host = crate::config::sp_host_addr(0);
        let f = SpFleet::sim_with_emu(
            &[0, 1, 2, 3],
            &["sidecar".into(), "g0".into()],
            &host,
        );
        assert!(f.has_emu());
        // The emu set is sidecar and g0, in fleet order.
        let emu: Vec<&str> =
            f.emu_sps().iter().map(|s| s.fake_interface.as_str()).collect();
        assert_eq!(emu, vec!["fake-switch0", "fake-sled0"]);
        // The sp-sim config omits the emulated SPs: no sidecar, g1 to g3 only.
        let v: toml::Value =
            toml::from_str(&f.sp_sim_config()).expect("valid TOML");
        assert!(
            v["simulated_sps"].get("sidecar").is_none(),
            "emu sidecar not in sp-sim"
        );
        assert_eq!(v["simulated_sps"]["gimlet"].as_array().unwrap().len(), 3);
        // Emulated SPs point at the host fleet, simulated ones at loopback.
        assert_eq!(f.sidecar().mgs_host, format!("[{host}]"));
        assert_eq!(f.gimlets()[0].mgs_host, format!("[{host}]"));
        assert_eq!(f.gimlets()[1].mgs_host, "[::1]");
    }

    #[test]
    fn all_sim_fleet_has_no_emu() {
        let f = SpFleet::sim(4);
        assert!(!f.has_emu());
        assert!(f.emu_sps().is_empty());
        // sp-sim still renders all four gimlets and the sidecar.
        let v: toml::Value = toml::from_str(&f.sp_sim_config()).unwrap();
        assert_eq!(v["simulated_sps"]["gimlet"].as_array().unwrap().len(), 4);
        assert!(v["simulated_sps"].get("sidecar").is_some());
    }

    #[test]
    fn new_is_for_gimlets_zero_to_n() {
        // The single rack constructor must match the explicit form exactly.
        assert_eq!(
            SpFleet::sim(4).sp_sim_config(),
            SpFleet::for_gimlets(&[0, 1, 2, 3], SpBackend::Sim).sp_sim_config()
        );
    }

    #[test]
    fn fleet_identities_are_per_sled_and_backend_independent() {
        // Identities and ports are invariant across backends; only the MGS
        // host changes.
        let sim = SpFleet::sim(4);
        let host = crate::config::sp_host_addr(0);
        let emu = SpFleet::new(4, SpBackend::Emu { addr: host.clone() });

        // Same fleet shape and identities.
        assert_eq!(sim.sps.len(), 5); // sidecar + 4 gimlets
        assert_eq!(sim.sidecar().serial, "SimSidecar0");
        assert_eq!(sim.gimlets()[0].base_port, emu.gimlets()[0].base_port);
        assert_eq!(
            sim.gimlets()[2].device_id_seed,
            emu.gimlets()[2].device_id_seed
        );

        // Only the MGS-facing host differs.
        assert_eq!(sim.sidecar().mgs_host, "[::1]");
        assert_eq!(emu.sidecar().mgs_host, format!("[{host}]"));
    }
}
