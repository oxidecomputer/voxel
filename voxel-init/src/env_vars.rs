//! Every environment variable the in-guest agent reads, in one place.

pub struct EnvVar(&'static str);

impl EnvVar {
    /// Unset and empty are the same thing here.
    pub fn get(&self) -> Option<String> {
        std::env::var(self.0).ok().filter(|v| !v.is_empty())
    }

    pub fn or(&self, default: &str) -> String {
        self.get().unwrap_or_else(|| default.to_string())
    }
}

/// External NIC to address, overriding the agent's own vioif probe.
pub const EXT_IF: EnvVar = EnvVar("EXT_IF");

/// Uplink the router NATs rack egress out of, overriding detection.
pub const UPSTREAM_IFACE: EnvVar = EnvVar("UPSTREAM_IFACE");

pub const OMICRON_REPO: EnvVar = EnvVar("OMICRON_REPO");

/// Image version recorded in the ready marker, one per install role.
pub const VOXEL_CP_VERSION: EnvVar = EnvVar("VOXEL_CP_VERSION");
pub const VOXEL_FRR_VERSION: EnvVar = EnvVar("VOXEL_FRR_VERSION");
pub const VOXEL_BUILDER_VERSION: EnvVar = EnvVar("VOXEL_BUILDER_VERSION");

pub const HOME: EnvVar = EnvVar("HOME");
pub const PATH: EnvVar = EnvVar("PATH");

pub fn home() -> String {
    HOME.or("/root")
}

pub const UNKNOWN_VERSION: &str = "unknown";
