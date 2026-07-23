//! FRR `frr.conf` generation for Voxel routers.
//!
//! Two forms per router. Unnumbered eBGP (default): `neighbor <iface> interface
//! remote-as external`, `no bgp ebgp-requires-policy`, both address-families with
//! `network` + `activate`; IPv4 prefixes ride the IPv6 link-local session via RFC
//! 5549 (FRR enables extended-nexthop automatically for interface peers). Static
//! (`RouterMode::Static`): numbered /30 toward each sidecar and static routes to
//! the rack pool, redistributed into any remaining eBGP neighbors (e.g. `ce`).
//! With `track_bfd`, the routes are BFD-tracked and single-hop BFD peers are
//! added (`bfdd` must be enabled in the image); off (the a4x2 default) renders
//! plain static routes.

use std::fmt::Write as _;

/// An unnumbered eBGP neighbor reachable over an interface (no peer IP).
#[derive(Debug, Clone)]
pub struct FrrNeighbor {
    pub interface: String,
    pub description: String,
}

impl FrrNeighbor {
    pub fn new(interface: impl Into<String>, description: impl Into<String>) -> Self {
        Self { interface: interface.into(), description: description.into() }
    }
}

/// A numbered /30 toward a sidecar with a BFD-tracked static route to the rack
/// pool. A non-empty `static_uplinks` list selects the static render.
#[derive(Debug, Clone)]
pub struct StaticUplink {
    pub interface: String,
    /// Router-side /30, e.g. `198.51.101.1/30`.
    pub address: String,
    /// Sidecar peer address, e.g. `198.51.101.2`.
    pub peer: String,
    /// ASN to peer with at `peer` (the sidecar's rack ASN). The session never
    /// establishes in static mode (sidecar runs no BGP), but bgpd's connect
    /// retries keep softnpu's neighbor for this gateway resolved, which static
    /// egress needs. Mirrors a4x2's router `frr-bgp.txt`.
    pub peer_asn: u32,
    /// Rack prefix reached via `peer`, e.g. `198.51.100.0/24`.
    pub route: String,
}

/// A router's FRR config. `static_uplinks` empty renders unnumbered eBGP;
/// non-empty renders static + BFD (eBGP neighbors, if any, are kept for `ce`).
#[derive(Debug, Clone)]
pub struct FrrRouter {
    pub hostname: String,
    pub asn: u32,
    pub neighbors: Vec<FrrNeighbor>,
    pub originate4: Vec<String>,
    pub originate6: Vec<String>,
    pub static_uplinks: Vec<StaticUplink>,
    /// Static mode: BFD-track the routes (`ip route ... bfd` + `bfd`/`peer`
    /// blocks). Off renders plain static routes (the a4x2 default).
    pub track_bfd: bool,
}

impl FrrRouter {
    /// Render a complete `frr.conf`.
    pub fn render(&self) -> String {
        if self.static_uplinks.is_empty() {
            self.render_bgp()
        } else {
            self.render_static()
        }
    }

    fn write_header(&self, o: &mut String) {
        writeln!(o, "frr defaults datacenter").unwrap();
        writeln!(o, "hostname {}", self.hostname).unwrap();
        writeln!(o, "!").unwrap();
    }

    /// `interface`/`description` blocks for the unnumbered (ce) neighbors.
    fn write_neighbor_interfaces(&self, o: &mut String) {
        for n in &self.neighbors {
            writeln!(o, "interface {}", n.interface).unwrap();
            writeln!(o, " description {}", n.description).unwrap();
            writeln!(o, "!").unwrap();
        }
    }

    /// `router bgp` open, `no ebgp-requires-policy`, and the unnumbered eBGP
    /// neighbors (toward ce). Callers append numbered peers + address-families.
    fn write_bgp_open(&self, o: &mut String) {
        writeln!(o, "router bgp {}", self.asn).unwrap();
        writeln!(o, " no bgp ebgp-requires-policy").unwrap();
        for n in &self.neighbors {
            writeln!(o, " neighbor {} interface remote-as external", n.interface).unwrap();
            writeln!(o, " neighbor {} timers connect 1", n.interface).unwrap();
        }
    }

    fn render_bgp(&self) -> String {
        let mut o = String::new();
        self.write_header(&mut o);
        self.write_neighbor_interfaces(&mut o);
        self.write_bgp_open(&mut o);
        writeln!(o, " !").unwrap();
        self.render_afi(&mut o, "ipv4", &self.originate4, false);
        writeln!(o, " !").unwrap();
        self.render_afi(&mut o, "ipv6", &self.originate6, false);
        writeln!(o, "!").unwrap();
        o
    }

    fn render_static(&self) -> String {
        let mut o = String::new();
        self.write_header(&mut o);
        self.write_neighbor_interfaces(&mut o);
        // Numbered /30 toward each sidecar.
        for s in &self.static_uplinks {
            writeln!(o, "interface {}", s.interface).unwrap();
            writeln!(o, " ip address {}", s.address).unwrap();
            writeln!(o, "!").unwrap();
        }

        // Single-hop BFD sessions to the sidecars (only when BFD-tracking).
        if self.track_bfd {
            writeln!(o, "bfd").unwrap();
            for s in &self.static_uplinks {
                writeln!(o, " peer {}", s.peer).unwrap();
                writeln!(o, "  no shutdown").unwrap();
            }
            writeln!(o, "!").unwrap();
        }

        // eBGP toward ce plus a numbered peer per sidecar (see StaticUplink.peer_asn).
        if !self.neighbors.is_empty() || !self.static_uplinks.is_empty() {
            self.write_bgp_open(&mut o);
            for s in &self.static_uplinks {
                writeln!(o, " neighbor {} remote-as {}", s.peer, s.peer_asn).unwrap();
                writeln!(o, " neighbor {} timers connect 1", s.peer).unwrap();
            }
            writeln!(o, " !").unwrap();
            self.render_afi(&mut o, "ipv4", &self.originate4, true);
            writeln!(o, "!").unwrap();
        }

        // Static routes to the rack pool via each sidecar. With BFD tracking we
        // also emit an unconditional floating static (admin distance 250) as a
        // backstop. FRR withdraws a `... bfd` route while its session is down,
        // and during RSS the sidecar side of BFD may not be up yet (early
        // networking programs the mgd peer best effort, with no retry), so
        // without a backstop the router loses its return route to the rack and
        // time sync deadlocks. The distance 250 static carries traffic until BFD
        // comes up, then the tracked route (distance 1) preempts. Matches a4x2's
        // floating static idiom.
        for s in &self.static_uplinks {
            if self.track_bfd {
                writeln!(o, "ip route {} {} bfd", s.route, s.peer).unwrap();
                writeln!(o, "ip route {} {} 250", s.route, s.peer).unwrap();
            } else {
                writeln!(o, "ip route {} {}", s.route, s.peer).unwrap();
            }
        }
        o
    }

    fn render_afi(&self, o: &mut String, afi: &str, originate: &[String], redistribute_static: bool) {
        writeln!(o, " address-family {afi} unicast").unwrap();
        if redistribute_static && afi == "ipv4" {
            writeln!(o, "  redistribute static").unwrap();
        }
        for p in originate {
            writeln!(o, "  network {p}").unwrap();
        }
        for n in &self.neighbors {
            writeln!(o, "  neighbor {} activate", n.interface).unwrap();
        }
        // Numbered sidecar peers are IPv4; only activate in the v4 AF.
        if afi == "ipv4" {
            for s in &self.static_uplinks {
                writeln!(o, "  neighbor {} activate", s.peer).unwrap();
            }
        }
        writeln!(o, " exit-address-family").unwrap();
    }
}
