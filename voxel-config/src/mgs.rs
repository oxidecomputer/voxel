// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MGS-simulator config (`mgs-config.toml`) generation for the switch zones.
//!
//! The single `voxel-cp` image bakes **switch0**'s MGS sim config into every
//! sled's switch zone. A rack with two scrimlets needs the second to present as
//! **switch1**, or RSS only ever discovers switch0 and the RSS->Nexus handoff
//! fails ("switch-port qsfp0 not found"). voxel owns the switch config here:
//! [`switch_config`] generates switch0 for `num_sleds` simulated sleds and emits
//! either it (slot 0) or switch1 (derived by the per-slot transform).
//!
//! The config is **generated**, not a fixed template, so it scales to any sled
//! count (the per-sled `[[switch.port]]` blocks + the `determination` interface
//! list grow with `num_sleds`). The port scheme matches [`crate::sp`]: the switch
//! binds `33300`/`44400`, sled `i` binds `333{i+1}0`/`444{i+1}0`. The two
//! `[[switch.location.description]]` entries' `local_sled` are the scrimlet sled
//! indices (where switch0/switch1's MGS run). `ignition-target` is `1` for the
//! switch and `(i+2) mod (num_sleds+1)` for sled `i` - a unique id per port that
//! reproduces the original a4x2 4-sled values exactly (regression-locked below).

use crate::sp::SpFleet;
use std::fmt::Write as _;

/// MGS per-attempt UDP RPC timeout. The default (baked into [`HEAD`]) is tight for
/// sp-sim; an emulator-backed fleet answers far slower, so [`switch0_config`]
/// widens it to [`SP_RPC_TIMEOUT_EMU_MS`]. Both ends of that rewrite source these
/// consts so the `replace` target can't drift from `HEAD`.
const SP_RPC_TIMEOUT_DEFAULT_MS: u32 = 2000;
/// Widened per-attempt RPC timeout for emulator-backed fleets; see [`SP_RPC_TIMEOUT_DEFAULT_MS`].
const SP_RPC_TIMEOUT_EMU_MS: u32 = 15000;

// Static comment/boilerplate blocks, verbatim from the known-good config. Only
// the description/determination/port *data* between them is generated.
const HEAD: &str = r#"#
# Oxide API: example configuration file
#

# Maximum number of host phase2 trampoline images we're willing to cache. Note
# that this value is specified in terms of _number of images_, not bytes, and
# our cache is in-memory. We expect this value to be small in production,
# potentially even 1 (i.e., only keep the most-recently-uploaded image).
host_phase2_recovery_image_cache_max_images = 1

[dropshot]
# We want to allow uploads of host phase 2 recovery images, which may be
# measured in the (small) hundreds of MiB. Set this to 512 MiB.
default_request_body_max_bytes = 1048576

[switch]
# Which interface is connected to our local sidecar SP (i.e., the SP that acts
# as our contact to the ignition controller)?
local_ignition_controller_interface = "fake-switch0"

[switch.rpc_retry_config]
# When sending UDP RPC packets to an SP (other than to reset it), how many total
# attempts do we make before giving up?
max_attempts_general = 5

# When sending UDP RPC packets to an SP (to reset it), how many total attempts
# do we make before giving up?
max_attempts_reset = 30

# sleep time between UDP RPC resends (up to `max_attempts_*`)
per_attempt_timeout_millis = 2000

# Possible locations where MGS could be running.
#
# The `name` of each location description will appear in logs and in the
# remainder of the `[switch.*]` configuration to define port mappings.
#
# The `local_sled` of each location description specifies the `slot` of the
# `SpIdentifier` for the sled on which this MGS is running. (The
# `SpIdentifier::typ` value is implicitly `SpType::Sled`.)
#
# `allow_local_sled_sp_reset` determines whether MGS will accept a request to
# perform an SP reset on its own local sled. This is dangerous during SP
# updates, because the "reset" operation involves a watchdog that requires MGS
# to send a "disarm the watchdog" message _after_ the reset, which it can't do
# if it just powered itself off. In production, we set this to `false` for both
# sleds; this means a scrimlet's SP can only be reset via MGS on the _other_
# scrimlet. (We allow this to be `true` in tests and other dev environments
# where rejecting reset attempts is too restrictive.)"#;

const DET_COMMENT: &str = r#"# `[[switch.location.determination]]` is a list of switch ports we should
# contact in order to determine our location; each port defines a subset of
# `[switch.location.names]` which are the possible location(s) of this MGS
# instance if the message was received on the given SP port. When MGS starts, it
# will send a discovery message on each port listed in this section, collect the
# responses, and determine its location via the intersection of the names listed
# below (for all ports which returned a successful response). This process can
# fail if too few SPs respond (leaving us with 2 or more possible locations) or
# if there is a miscabling that results in an unsolvable system (e.g.,
# determination 0 reports "switch0" and determination 1 reports "switch1")."#;

const PORT_COMMENT: &str = r#"# `[[switch.port.*]]` defines the local data link address (in RFD 250 terms, the
# interface configured to use VLAN tag  assigned to the given port) and the
# logical ID of the remote SP ("sled 7", "switch 1", etc.), which must have an
# entry for each member of `[[switch.location.description]]` above."#;

const FOOTER: &str = r#"[log]
# Show log messages of this level and more severe
level = "debug"

# Example output to a terminal (with colors)
mode = "stderr-terminal"

# Example output to a file, appending if it already exists.
#mode = "file"
#path = "logs/server.log"
#if_exists = "append"
"#;

/// Generate switch0's MGS-sim config from the SP `fleet`, with scrimlets at sled
/// indices `scrimlets` (exactly two: switch0/switch1's MGS run there). The
/// `[[switch.port]]` table - addresses, ports, fake-interfaces, ignition
/// targets - is derived from the fleet, so MGS and the SP provider agree by
/// construction (the sidecar SP is the switch port; each gimlet SP is a sled
/// port).
fn switch0_config(fleet: &SpFleet, scrimlets: &[usize]) -> String {
    assert_eq!(
        scrimlets.len(),
        2,
        "MGS sim models exactly two switches (scrimlets)"
    );
    let num_sleds = fleet.gimlets().len();
    let mut o = String::with_capacity(HEAD.len() + 200 * num_sleds);

    writeln!(o, "{HEAD}").unwrap();

    // Two switch location descriptions; local_sled = the scrimlet sled index.
    for (sw, &sled) in scrimlets.iter().enumerate() {
        writeln!(o, "[[switch.location.description]]").unwrap();
        writeln!(o, "name = \"switch{sw}\"").unwrap();
        writeln!(o, "local_sled = {sled}").unwrap();
        writeln!(o, "allow_local_sled_sp_reset = true").unwrap();
        writeln!(o).unwrap();
    }

    // Determination: contact every sled SP's fake-interface to locate ourselves.
    writeln!(o, "{DET_COMMENT}").unwrap();
    writeln!(o, "[[switch.location.determination]]").unwrap();
    let ifaces: Vec<String> = fleet
        .gimlets()
        .iter()
        .map(|sp| format!("\"{}\"", sp.fake_interface))
        .collect();
    writeln!(o, "interfaces = [{}]", ifaces.join(", ")).unwrap();
    writeln!(o, "sp_port_1 = [\"switch0\"]").unwrap();
    writeln!(o, "sp_port_2 = [\"switch1\"]").unwrap();
    writeln!(o).unwrap();

    // Ports: the sidecar SP (the switch port) then each gimlet SP (a sled port),
    // all from the fleet.
    writeln!(o, "{PORT_COMMENT}").unwrap();
    for sp in &fleet.sps {
        writeln!(o, "[[switch.port]]").unwrap();
        writeln!(o, "kind = \"simulated\"").unwrap();
        writeln!(o, "fake-interface = \"{}\"", sp.fake_interface).unwrap();
        writeln!(o, "addr = \"{}:{}\"", sp.mgs_host, sp.base_port).unwrap();
        writeln!(o, "ereport-addr = \"{}:{}\"", sp.mgs_host, sp.ereport_base)
            .unwrap();
        writeln!(o, "ignition-target = {}", sp.ignition_target).unwrap();
        writeln!(o, "location = {}", sp.mgs_location()).unwrap();
        writeln!(o).unwrap();
    }

    write!(o, "{FOOTER}").unwrap();

    // The emulator answers MGS far slower than sp-sim (RPCs take seconds), so when
    // any SP is emulator-backed, widen MGS's per-attempt RPC timeout or discovery
    // spuriously fails. The "SP online" gate (voxel-init) covers the long boot;
    // this only needs to cover a single slow RPC. sp-sim keeps the tight default.
    if fleet.has_emu() {
        o = o.replace(
            &format!(
                "per_attempt_timeout_millis = {SP_RPC_TIMEOUT_DEFAULT_MS}"
            ),
            &format!("per_attempt_timeout_millis = {SP_RPC_TIMEOUT_EMU_MS}"),
        );
    }
    o
}

/// Render the MGS-simulator config for the switch zone in `slot` (0 or 1) for the
/// given SP `fleet`, with scrimlets at `scrimlets` (two sled indices).
///
/// Slot 0 is switch0; any other slot derives its config by rewriting the
/// `fake-switch0` interface name and each simulated SP port's trailing instance
/// digit to the slot number (see the module docs).
pub fn switch_config(slot: u8, fleet: &SpFleet, scrimlets: &[usize]) -> String {
    let base = switch0_config(fleet, scrimlets);
    if slot == 0 {
        return base;
    }
    let digit = char::from_digit(slot as u32, 10).unwrap_or_else(|| {
        panic!("switch slot {slot} is not a single decimal digit")
    });

    let mut out = String::with_capacity(base.len());
    for line in base.lines() {
        // `fake-switch0` -> `fake-switch{slot}` (the local interface name; the
        // `switch0`/`switch1` location *labels* never appear as `fake-switch0`).
        let mut rewritten =
            line.replace("fake-switch0", &format!("fake-switch{slot}"));
        // Each simulated SP listens on per-switch ports; the trailing digit of
        // the port is the switch instance. Bump it on the address lines.
        let trimmed = rewritten.trim_start();
        if (trimmed.starts_with("addr =")
            || trimmed.starts_with("ereport-addr ="))
            && let Some(close) = rewritten.rfind('"')
            && close >= 1
            && rewritten.as_bytes()[close - 1].is_ascii_digit()
        {
            rewritten.replace_range(close - 1..close, &digit.to_string());
        }
        out.push_str(&rewritten);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original a4x2 4-sled switch0 config - generation must reproduce this
    /// byte-for-byte (the known-good baseline that brought up the live rack).
    const REFERENCE_4SLED: &str = r#"#
# Oxide API: example configuration file
#

# Maximum number of host phase2 trampoline images we're willing to cache. Note
# that this value is specified in terms of _number of images_, not bytes, and
# our cache is in-memory. We expect this value to be small in production,
# potentially even 1 (i.e., only keep the most-recently-uploaded image).
host_phase2_recovery_image_cache_max_images = 1

[dropshot]
# We want to allow uploads of host phase 2 recovery images, which may be
# measured in the (small) hundreds of MiB. Set this to 512 MiB.
default_request_body_max_bytes = 1048576

[switch]
# Which interface is connected to our local sidecar SP (i.e., the SP that acts
# as our contact to the ignition controller)?
local_ignition_controller_interface = "fake-switch0"

[switch.rpc_retry_config]
# When sending UDP RPC packets to an SP (other than to reset it), how many total
# attempts do we make before giving up?
max_attempts_general = 5

# When sending UDP RPC packets to an SP (to reset it), how many total attempts
# do we make before giving up?
max_attempts_reset = 30

# sleep time between UDP RPC resends (up to `max_attempts_*`)
per_attempt_timeout_millis = 2000

# Possible locations where MGS could be running.
#
# The `name` of each location description will appear in logs and in the
# remainder of the `[switch.*]` configuration to define port mappings.
#
# The `local_sled` of each location description specifies the `slot` of the
# `SpIdentifier` for the sled on which this MGS is running. (The
# `SpIdentifier::typ` value is implicitly `SpType::Sled`.)
#
# `allow_local_sled_sp_reset` determines whether MGS will accept a request to
# perform an SP reset on its own local sled. This is dangerous during SP
# updates, because the "reset" operation involves a watchdog that requires MGS
# to send a "disarm the watchdog" message _after_ the reset, which it can't do
# if it just powered itself off. In production, we set this to `false` for both
# sleds; this means a scrimlet's SP can only be reset via MGS on the _other_
# scrimlet. (We allow this to be `true` in tests and other dev environments
# where rejecting reset attempts is too restrictive.)
[[switch.location.description]]
name = "switch0"
local_sled = 0
allow_local_sled_sp_reset = true

[[switch.location.description]]
name = "switch1"
local_sled = 3
allow_local_sled_sp_reset = true

# `[[switch.location.determination]]` is a list of switch ports we should
# contact in order to determine our location; each port defines a subset of
# `[switch.location.names]` which are the possible location(s) of this MGS
# instance if the message was received on the given SP port. When MGS starts, it
# will send a discovery message on each port listed in this section, collect the
# responses, and determine its location via the intersection of the names listed
# below (for all ports which returned a successful response). This process can
# fail if too few SPs respond (leaving us with 2 or more possible locations) or
# if there is a miscabling that results in an unsolvable system (e.g.,
# determination 0 reports "switch0" and determination 1 reports "switch1").
[[switch.location.determination]]
interfaces = ["fake-sled0", "fake-sled1", "fake-sled2", "fake-sled3"]
sp_port_1 = ["switch0"]
sp_port_2 = ["switch1"]

# `[[switch.port.*]]` defines the local data link address (in RFD 250 terms, the
# interface configured to use VLAN tag  assigned to the given port) and the
# logical ID of the remote SP ("sled 7", "switch 1", etc.), which must have an
# entry for each member of `[[switch.location.description]]` above.
[[switch.port]]
kind = "simulated"
fake-interface = "fake-switch0"
addr = "[::1]:33300"
ereport-addr = "[::1]:44400"
ignition-target = 1
location = { switch0 = ["switch", 0], switch1 = ["switch", 1] }

[[switch.port]]
kind = "simulated"
fake-interface = "fake-sled0"
addr = "[::1]:33310"
ereport-addr = "[::1]:44410"
ignition-target = 2
location = { switch0 = ["sled", 0], switch1 = ["sled", 0] }

[[switch.port]]
kind = "simulated"
fake-interface = "fake-sled1"
addr = "[::1]:33320"
ereport-addr = "[::1]:44420"
ignition-target = 3
location = { switch0 = ["sled", 1], switch1 = ["sled", 1] }

[[switch.port]]
kind = "simulated"
fake-interface = "fake-sled2"
addr = "[::1]:33330"
ereport-addr = "[::1]:44430"
ignition-target = 4
location = { switch0 = ["sled", 2], switch1 = ["sled", 2] }

[[switch.port]]
kind = "simulated"
fake-interface = "fake-sled3"
addr = "[::1]:33340"
ereport-addr = "[::1]:44440"
ignition-target = 0
location = { switch0 = ["sled", 3], switch1 = ["sled", 3] }

[log]
# Show log messages of this level and more severe
level = "debug"

# Example output to a terminal (with colors)
mode = "stderr-terminal"

# Example output to a file, appending if it already exists.
#mode = "file"
#path = "logs/server.log"
#if_exists = "append"
"#;

    #[test]
    fn four_sled_switch0_is_byte_exact_to_reference() {
        // The generator must reproduce the live-validated 4-sled config exactly -
        // the default `sim` fleet keeps MGS on loopback, so nothing changes.
        assert_eq!(
            switch_config(0, &SpFleet::sim(4), &[0, 3]),
            REFERENCE_4SLED
        );
    }

    #[test]
    fn emulated_backend_repoints_mgs_off_loopback() {
        // The pluggable bit: an emulated fleet swaps the MGS address but keeps
        // the port scheme + structure (still valid, still byte-identical except
        // the host).
        let host = crate::config::sp_host_addr(0);
        let s = switch_config(
            0,
            &SpFleet::new(4, crate::sp::SpBackend::Emu { addr: host.clone() }),
            &[0, 3],
        );
        let _: toml::Value =
            toml::from_str(&s).expect("emulated switch0 valid TOML");
        assert!(s.contains(&format!("addr = \"[{host}]:33300\"")));
        assert!(s.contains(&format!("addr = \"[{host}]:33310\"")));
        assert!(!s.contains("[::1]:33300")); // no longer loopback
    }

    #[test]
    fn switch1_transform_unchanged() {
        let s1 = switch_config(1, &SpFleet::sim(4), &[0, 3]);
        let _: toml::Value = toml::from_str(&s1).expect("switch1 valid TOML");
        assert!(s1.contains(
            "local_ignition_controller_interface = \"fake-switch1\""
        ));
        assert!(s1.contains("fake-interface = \"fake-switch1\""));
        assert!(!s1.contains("fake-switch0"));
        assert!(s1.contains("addr = \"[::1]:33301\""));
        assert!(s1.contains("addr = \"[::1]:33311\""));
        assert!(!s1.contains(":33300\""));
        assert!(s1.contains("fake-interface = \"fake-sled0\""));
    }

    #[test]
    fn emu_fleet_widens_mgs_timeout() {
        // Any emulator-backed SP -> lenient per-attempt RPC timeout.
        let f = SpFleet::sim_with_emu(
            &[0, 1, 2, 3],
            &["sidecar".into()],
            &crate::config::sp_host_addr(0),
        );
        let s = switch_config(0, &f, &[0, 3]);
        assert!(s.contains("per_attempt_timeout_millis = 15000"));
        assert!(!s.contains("per_attempt_timeout_millis = 2000"));
        // All-sim keeps the tight default (byte-exact reference unaffected).
        let sim = switch_config(0, &SpFleet::sim(4), &[0, 3]);
        assert!(sim.contains("per_attempt_timeout_millis = 2000"));
    }

    #[test]
    fn scales_to_six_sleds() {
        let s = switch_config(0, &SpFleet::sim(6), &[0, 5]);
        let v: toml::Value = toml::from_str(&s).expect("6-sled valid TOML");
        // 1 switch port + 6 sled ports.
        assert_eq!(v["switch"]["port"].as_array().unwrap().len(), 7);
        // determination lists all 6 sled interfaces.
        let dets = v["switch"]["location"]["determination"].as_array().unwrap();
        assert_eq!(dets[0]["interfaces"].as_array().unwrap().len(), 6);
        // switch1 runs on the 2nd scrimlet (sled 5).
        let descs = v["switch"]["location"]["description"].as_array().unwrap();
        assert_eq!(descs[1]["local_sled"].as_integer(), Some(5));
        // sled5's ports follow the scheme (33360/44460), ignition (5+2)%7 = 0.
        assert!(s.contains("addr = \"[::1]:33360\""));
        assert!(s.contains("fake-interface = \"fake-sled5\""));
    }
}
