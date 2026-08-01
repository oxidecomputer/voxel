#!/usr/bin/env python3
# Re-applied by build-cp.sh AFTER `git checkout <commit>` (which resets the tree).
#
# Nexus rack-init builds the "initial-infra" address lot as a single block from
# rack_network_config.infra_ip_first/last and lot-validates EVERY switch-port
# address against it. In Static mode that lot is a finite IPv4 range (the numbered
# /30 uplinks), so voxel's sidecar-interconnect ports (underlay, v6 addrconf)
# can't reserve -> handoff 400 "address not in lot". BGP mode already uses a v6
# (::) infra lot, where the same addrconf ports reserve fine. This adds a v6 block
# to the infra lot when it's v4, so the interconnect ports reserve in every mode.
#
# Switch-config preparation then reads the infra range back as the first block of
# the lot, from an unordered database load, so with the sentinel present it could
# publish ::/:: as the rack's infra range. The second edit makes it prefer the
# IPv4 block, falling back to first() for the all-v6 BGP lot.
import os
import sys

om = sys.argv[1] if len(sys.argv) > 1 else "."
path = om + "/nexus/src/app/rack.rs"
src = open(path).read()

MARKER = "voxel: add a v6 block"
if MARKER in src:
    print("nexus-infra-lot-v6: rack.rs already patched")
else:
    old = "        let blocks = vec![ipv4_block];"
    new = (
        "        // voxel: add a v6 block so Static-mode addrconf (interconnect) ports\n"
        "        // reserve in the infra lot (BGP mode already uses a v6 :: lot).\n"
        "        let mut blocks = vec![ipv4_block];\n"
        "        if first_address.is_ipv4() {\n"
        "            blocks.push(networking::AddressLotBlockCreate {\n"
        "                first_address: std::net::Ipv6Addr::UNSPECIFIED.into(),\n"
        "                last_address: std::net::Ipv6Addr::UNSPECIFIED.into(),\n"
        "            });\n"
        "        }"
    )
    n = src.count(old)
    if n != 1:
        sys.exit("nexus-infra-lot-v6: expected exactly 1 match of anchor, found %d" % n)
    open(path, "w").write(src.replace(old, new))
    print("nexus-infra-lot-v6: patched " + path)

path = om + "/nexus/switch-config/preparation/src/lib.rs"
if not os.path.exists(path):
    # The preparation crate postdates older supported pins (e.g. 43bb5af).
    # Those revisions read the infra range straight from rack.rs, so only
    # the first edit applies.
    print("nexus-infra-lot-v6: no preparation crate at this pin, skipping")
    sys.exit(0)
src = open(path).read()

MARKER = "voxel: prefer the IPv4 block"
if MARKER in src:
    print("nexus-infra-lot-v6: preparation already patched")
else:
    old = "    let (infra_ip_first, infra_ip_last) = match infra_blocks.first() {"
    new = (
        "    // voxel: prefer the IPv4 block. The rack.rs patch adds a v6 ::\n"
        "    // sentinel block to a v4 infra lot, and first() on the unordered\n"
        "    // load could otherwise publish :: as the infra range.\n"
        "    let (infra_ip_first, infra_ip_last) = match infra_blocks\n"
        "        .iter()\n"
        "        .find(|b| b.first_address.ip().is_ipv4())\n"
        "        .or_else(|| infra_blocks.first())\n"
        "    {"
    )
    n = src.count(old)
    if n != 1:
        sys.exit(
            "nexus-infra-lot-v6: expected exactly 1 match of preparation anchor, found %d"
            % n
        )
    open(path, "w").write(src.replace(old, new))
    print("nexus-infra-lot-v6: patched " + path)
