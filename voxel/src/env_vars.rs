//! Every environment variable voxel reads, in one place.

pub(crate) struct EnvVar(&'static str);

impl EnvVar {
    pub(crate) const fn name(&self) -> &'static str {
        self.0
    }

    /// Unset and empty are the same thing here.
    pub(crate) fn get(&self) -> Option<String> {
        std::env::var(self.0).ok().filter(|v| !v.is_empty())
    }

    pub(crate) fn or(&self, default: &str) -> String {
        self.get().unwrap_or_else(|| default.to_string())
    }

    pub(crate) fn or_else(&self, default: impl FnOnce() -> String) -> String {
        self.get().unwrap_or_else(default)
    }

    pub(crate) fn is_set(&self) -> bool {
        self.get().is_some()
    }

    /// Callers must be single-threaded: setenv races concurrent getenv.
    pub(crate) unsafe fn set(&self, value: &str) {
        unsafe { std::env::set_var(self.0, value) }
    }
}

/// Defaults to falcon's own rpool/falcon.
pub(crate) const FALCON_DATASET: EnvVar = EnvVar("FALCON_DATASET");

/// Defaults to $HOME/voxel-builds.
pub(crate) const BUILD_ROOT: EnvVar = EnvVar("BUILD_ROOT");

/// Omicron checkout the cp image was built from, derived from image.cp when unset.
pub(crate) const VOXEL_OMICRON_SRC: EnvVar = EnvVar("VOXEL_OMICRON_SRC");

/// Used when voxel's source tree is not found relative to the binary.
pub(crate) const VOXEL_REPO_ROOT: EnvVar = EnvVar("VOXEL_REPO_ROOT");

/// Host link every node's external NIC attaches to, overriding the config.
pub(crate) const EXT_INTERFACE: EnvVar = EnvVar("EXT_INTERFACE");

/// "<cidr> <gateway>" for an isolated-mode builder.
pub(crate) const VOXEL_BUILDER_NET: EnvVar = EnvVar("VOXEL_BUILDER_NET");

pub(crate) const OMICRON_REPO: EnvVar = EnvVar("OMICRON_REPO");

/// Overrides the pinned revision.
pub(crate) const SIDECAR_LITE_REV: EnvVar = EnvVar("SIDECAR_LITE_REV");

/// Gimlet SP count baked into the build-time smf configs.
pub(crate) const GIMLETS: EnvVar = EnvVar("GIMLETS");

/// humility binary used against an emulated SP.
pub(crate) const VOXEL_HUMILITY: EnvVar = EnvVar("VOXEL_HUMILITY");

pub(crate) const VOXEL_SKIP_MEM_PREFLIGHT: EnvVar = EnvVar("VOXEL_SKIP_MEM_PREFLIGHT");

/// Omicron build flags, when the caller has already set them.
pub(crate) const RUSTFLAGS: EnvVar = EnvVar("RUSTFLAGS");

pub(crate) const HOME: EnvVar = EnvVar("HOME");
pub(crate) const PATH: EnvVar = EnvVar("PATH");

pub(crate) fn home() -> String {
    HOME.or("/root")
}
