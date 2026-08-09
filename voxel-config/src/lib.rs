//! On-the-fly topology config generation for Project Voxel.
//!
//! [`VoxelConfig`] (`voxel.toml`) is the single source of truth `voxel` renders
//! every per-node config from, replacing a4x2's static per-node files. This crate
//! is pure Rust (no libfalcon), so it builds and tests on any host.
//!
//! The per-node renderers live in submodules:
//!
//! - [`sled`]: sled-agent config
//! - [`frr`]: customer-router `frr.conf`, unnumbered BGP
//! - [`mgs`] / [`sp`]: switch-zone MGS-sim + sp-sim configs
//!
//! The RSS config-rss.toml is built in voxel's rss_request module through
//! omicron's own types (the rack-init-config crate), pinned to an omicron commit.

pub mod config;
pub mod frr;
pub mod mgs;
pub mod sled;
pub mod sp;

pub use config::{
    External, ExternalMode, Falcon, Image, Network, RecoverySiloCfg,
    RouterMode, SLED_SERIAL_PREFIX, SledDataLinksSchema, SledDesc,
    SledDisksSchema, SpCfg, Topology, UplinkCfg, UplinkPort, VoxelConfig,
};
pub use frr::{FrrNeighbor, FrrRouter, StaticUplink};
pub use sled::SledAgentConfig;
