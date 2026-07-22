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
import sys

om = sys.argv[1] if len(sys.argv) > 1 else "."
path = om + "/nexus/src/app/rack.rs"
src = open(path).read()

MARKER = "voxel: add a v6 block"
if MARKER in src:
    print("nexus-infra-lot-v6: already patched")
    sys.exit(0)

old = "        let blocks = vec![ipv4_block];"
new = (
    "        // voxel: add a v6 block so Static-mode addrconf (interconnect) ports\n"
    "        // reserve in the infra lot; BGP mode already uses a v6 :: lot.\n"
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
