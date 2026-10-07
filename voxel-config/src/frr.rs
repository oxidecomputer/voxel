// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FRR `frr.conf` generation for voxel routers.
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

use std::fmt;

/// An unnumbered eBGP neighbor reachable over an interface (no peer IP).
#[derive(Debug, Clone)]
pub struct FrrNeighbor {
    pub interface: String,
    pub description: String,
}

impl FrrNeighbor {
    pub fn new(
        interface: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
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
    /// egress needs. Matches a4x2's router `frr-bgp.txt`.
    pub peer_asn: u32,
    /// Rack prefix reached via `peer`, e.g. `198.51.100.0/24`.
    pub route: String,
}

/// The documentation prefix (RFC 5737 TEST-NET-1) an unnumbered rack-facing
/// link is numbered from.
///
/// pimd needs a [primary IPv4 address] for a rack-facing multicast VIF.
/// The `/32`s here provide one on each link.
///
/// [primary IPv4 address]: https://github.com/FRRouting/frr/blob/85cf1ed576deed121751e16a64970f8a652a9e1e/pimd/pim_iface.c#L973-L981
pub const PIM_LINK_PREFIX: [u8; 3] = [192, 0, 2];

/// The `/32` assigned to a router's nth unnumbered rack-facing link.
///
/// Both the router and the link index matter here to avoid
/// address collision. Addresses come from [`PIM_LINK_PREFIX`].
///
/// # Examples
///
/// ```
/// use voxel_config::frr::pim_link_addr;
///
/// assert_eq!(pim_link_addr(0, 0, 2), "192.0.2.1/32");
/// assert_eq!(pim_link_addr(1, 1, 2), "192.0.2.4/32");
/// ```
pub fn pim_link_addr(
    router: usize,
    link: usize,
    links_per_router: usize,
) -> String {
    let [a, b, c] = PIM_LINK_PREFIX;
    format!("{a}.{b}.{c}.{}/32", router * links_per_router + link + 1)
}

/// The prefix list naming the range pimd treats as source-specific.
pub const SSM_PREFIX_LIST: &str = "voxel-ssm";

/// The configuration declaring the SSM range, one vtysh command per element.
///
/// This is shared between the rendered `frr.conf` and
/// `voxel network multicast up`, which reasserts on every run.
///
/// A missing [`SSM_PREFIX_LIST`], or one without a matching entry,
/// treats no group as source-specific. In that case, everything falls back to
/// ASM.
///
/// The local network control block (224.0.0.0/24, RFC 5771) stays outside
/// the emulated SSM range, as the 224.0.0.0/4 permit is an emulation artifact
/// only.
///
/// # Examples
///
/// ```
/// let cmds = voxel_config::frr::ssm_range_cmds();
///
/// assert_eq!(cmds[0], "ip prefix-list voxel-ssm seq 1 deny 224.0.0.0/24 le 32");
/// assert_eq!(cmds[2], "router pim");
/// ```
pub fn ssm_range_cmds() -> [String; 5] {
    [
        format!(
            "ip prefix-list {SSM_PREFIX_LIST} seq 1 deny 224.0.0.0/24 le 32"
        ),
        format!(
            "ip prefix-list {SSM_PREFIX_LIST} seq 5 permit 224.0.0.0/4 le 32"
        ),
        "router pim".to_string(),
        format!("ssm prefix-list {SSM_PREFIX_LIST}"),
        "exit".to_string(),
    ]
}

/// An interface that PIM holds a passive multicast VIF on.
///
/// `address` is `Some` only when the interface carries no IPv4 address
/// (unnumbered BGP ~ RFC 5549). The `/32` comes from [`pim_link_addr`].
///
/// Every VIF is passive. Nothing on the other end of any link speaks PIM.
#[derive(Debug, Clone)]
pub struct PimIface {
    pub interface: String,
    pub address: Option<String>,
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
    /// Links carrying a multicast VIF.
    ///
    /// This is empty on a router that forwards no multicast.
    pub pim: Vec<PimIface>,
}

impl fmt::Display for FrrRouter {
    /// Render a complete `frr.conf`. Writing to a `Formatter` is infallible for
    /// the `String` backing `render`, but `Display` lets every writeln propagate
    /// via `?` instead of `.unwrap()`.
    fn fmt(&self, o: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.static_uplinks.is_empty() {
            self.render_bgp(o)
        } else {
            self.render_static(o)
        }
    }
}

impl FrrRouter {
    /// Render a complete `frr.conf`.
    pub fn render(&self) -> String {
        self.to_string()
    }

    fn write_header(&self, o: &mut impl fmt::Write) -> fmt::Result {
        writeln!(o, "frr defaults datacenter")?;
        writeln!(o, "hostname {}", self.hostname)?;
        writeln!(o, "!")
    }

    /// `interface`/`description` blocks for the unnumbered (ce) neighbors.
    fn write_neighbor_interfaces(
        &self,
        o: &mut impl fmt::Write,
    ) -> fmt::Result {
        for n in &self.neighbors {
            writeln!(o, "interface {}", n.interface)?;
            writeln!(o, " description {}", n.description)?;
            writeln!(o, "!")?;
        }
        Ok(())
    }

    /// Treat 224.0.0.0/4 as source-specific (except 224.0.0.0/24).
    ///
    /// Voxel's assignments name a sender, making every entry an `(S,G)`, but
    /// pimd judges that by address range.
    ///
    /// For ASM groups, pimd can add the register VIF and receive packet
    /// copies from the kernel. Without a rendezvous point, those copies are
    /// discarded before PIM Register encapsulation.
    ///
    /// SSM [skips adding the register VIF].
    ///
    /// [skips adding the register VIF]: https://github.com/FRRouting/frr/blob/85cf1ed576deed121751e16a64970f8a652a9e1e/pimd/pim_register.c#L37-L50
    fn write_ssm_range(&self, o: &mut impl fmt::Write) -> fmt::Result {
        if self.pim.is_empty() {
            return Ok(());
        }
        for line in ssm_range_cmds() {
            writeln!(o, "{line}")?;
        }
        writeln!(o, "!")
    }

    /// This configures PIM on the links an externally sourced group crosses.
    ///
    /// `voxel network multicast up` adds the per-group assignments at runtime,
    /// since the groups are not known at launch.
    fn write_pim(&self, o: &mut impl fmt::Write) -> fmt::Result {
        for p in &self.pim {
            writeln!(o, "interface {}", p.interface)?;
            if let Some(addr) = &p.address {
                writeln!(o, " ip address {addr}")?;
            }
            writeln!(o, " ip pim passive")?;
            writeln!(o, "!")?;
        }
        Ok(())
    }

    /// `router bgp` open, `no ebgp-requires-policy`, and the unnumbered eBGP
    /// neighbors (toward ce). Callers append numbered peers + address-families.
    fn write_bgp_open(&self, o: &mut impl fmt::Write) -> fmt::Result {
        writeln!(o, "router bgp {}", self.asn)?;
        writeln!(o, " no bgp ebgp-requires-policy")?;
        for n in &self.neighbors {
            writeln!(
                o,
                " neighbor {} interface remote-as external",
                n.interface
            )?;
            writeln!(o, " neighbor {} timers connect 1", n.interface)?;
        }
        Ok(())
    }

    fn render_bgp(&self, o: &mut impl fmt::Write) -> fmt::Result {
        self.write_header(o)?;
        self.write_neighbor_interfaces(o)?;
        self.write_pim(o)?;
        self.write_ssm_range(o)?;
        self.write_bgp_open(o)?;
        writeln!(o, " !")?;
        self.render_afi(o, "ipv4", &self.originate4, false)?;
        writeln!(o, " !")?;
        self.render_afi(o, "ipv6", &self.originate6, false)?;
        writeln!(o, "!")
    }

    fn render_static(&self, o: &mut impl fmt::Write) -> fmt::Result {
        self.write_header(o)?;
        self.write_neighbor_interfaces(o)?;
        // Numbered /30 toward each sidecar.
        for s in &self.static_uplinks {
            writeln!(o, "interface {}", s.interface)?;
            writeln!(o, " ip address {}", s.address)?;
            writeln!(o, "!")?;
        }
        self.write_pim(o)?;
        self.write_ssm_range(o)?;

        // Single-hop BFD sessions to the sidecars (only when BFD-tracking).
        if self.track_bfd {
            writeln!(o, "bfd")?;
            for s in &self.static_uplinks {
                writeln!(o, " peer {}", s.peer)?;
                writeln!(o, "  no shutdown")?;
            }
            writeln!(o, "!")?;
        }

        // eBGP toward ce plus a numbered peer per sidecar (see StaticUplink.peer_asn).
        if !self.neighbors.is_empty() || !self.static_uplinks.is_empty() {
            self.write_bgp_open(o)?;
            for s in &self.static_uplinks {
                writeln!(o, " neighbor {} remote-as {}", s.peer, s.peer_asn)?;
                writeln!(o, " neighbor {} timers connect 1", s.peer)?;
            }
            writeln!(o, " !")?;
            self.render_afi(o, "ipv4", &self.originate4, true)?;
            writeln!(o, "!")?;
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
                writeln!(o, "ip route {} {} bfd", s.route, s.peer)?;
                writeln!(o, "ip route {} {} 250", s.route, s.peer)?;
            } else {
                writeln!(o, "ip route {} {}", s.route, s.peer)?;
            }
        }
        Ok(())
    }

    fn render_afi(
        &self,
        o: &mut impl fmt::Write,
        afi: &str,
        originate: &[String],
        redistribute_static: bool,
    ) -> fmt::Result {
        writeln!(o, " address-family {afi} unicast")?;
        if redistribute_static && afi == "ipv4" {
            writeln!(o, "  redistribute static")?;
        }
        for p in originate {
            writeln!(o, "  network {p}")?;
        }
        for n in &self.neighbors {
            writeln!(o, "  neighbor {} activate", n.interface)?;
        }
        // Numbered sidecar peers are IPv4; only activate in the v4 AF.
        if afi == "ipv4" {
            for s in &self.static_uplinks {
                writeln!(o, "  neighbor {} activate", s.peer)?;
            }
        }
        writeln!(o, " exit-address-family")
    }
}
