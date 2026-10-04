use anyhow::Context;
use camino::Utf8PathBuf;
use std::ffi::OsString;
use std::path::PathBuf;
use voxel_config::{VoxelConfig, config as vcfg};

use super::operation::LogLevel;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicCommand {
    Launch,
    Route,
    Destroy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) current_dir: PathBuf,
    pub(crate) env: Vec<(OsString, OsString)>,
}

#[derive(Clone, Debug)]
pub(crate) struct TuiContext {
    pub(crate) config_path: Utf8PathBuf,
    pub(crate) workdir: Utf8PathBuf,
    pub(crate) name: String,
    pub(crate) dataset: String,
    pub(crate) build_root: Utf8PathBuf,
    pub(crate) config: VoxelConfig,
    executable: PathBuf,
    effective_env: Vec<(OsString, OsString)>,
}

impl TuiContext {
    pub(crate) fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config_path: Utf8PathBuf,
        workdir: Utf8PathBuf,
        name: String,
        dataset: String,
        build_root: Utf8PathBuf,
        config: VoxelConfig,
        executable: PathBuf,
        effective_env: Vec<(OsString, OsString)>,
    ) -> Self {
        Self {
            config_path,
            workdir,
            name,
            dataset,
            build_root,
            config,
            executable,
            effective_env,
        }
    }

    /// Moves an unconfigured external network to isolated mode. LAN mode
    /// reaches the rack only when the LAN's DHCP subnet is on-link for the
    /// host; otherwise the host route to the rack, and Nexus with it, cannot
    /// be installed. The choice is written to the config so launch, route,
    /// and destroy all agree on the segment. An operator's explicit mode is
    /// left alone. Returns a log message describing any decision made.
    pub(crate) fn default_isolated_external(
        &mut self,
        default_link: Option<String>,
    ) -> anyhow::Result<Option<(LogLevel, String)>> {
        let text = if self.config_path.exists() {
            std::fs::read_to_string(&self.config_path)
                .with_context(|| format!("read {}", self.config_path))?
        } else {
            VoxelConfig::default().to_toml()
        };
        let get = |key| vcfg::get(&text, key).map_err(anyhow::Error::msg);
        if get("external.mode")?.is_some() {
            return Ok(None);
        }
        let (text, uplink) = match (get("external.uplink")?, default_link) {
            (Some(uplink), _) => (text.clone(), uplink),
            (None, Some(link)) => (
                vcfg::set(&text, "external.uplink", &link)
                    .map_err(anyhow::Error::msg)?,
                link,
            ),
            (None, None) => {
                return Ok(Some((
                    LogLevel::Warning,
                    "external network: no default route to NAT an isolated \
                     segment out of; staying in lan mode"
                        .into(),
                )));
            }
        };
        let text = vcfg::set(&text, "external.mode", "isolated")
            .map_err(anyhow::Error::msg)?;
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {parent}"))?;
        }
        std::fs::write(&self.config_path, &text)
            .with_context(|| format!("write {}", self.config_path))?;
        self.config = VoxelConfig::from_toml(&text)
            .with_context(|| format!("parse {}", self.config_path))?;
        Ok(Some((
            LogLevel::Info,
            format!(
                "external network: {} now uses isolated mode out of {uplink}; \
                 set external.mode = \"lan\" to opt out",
                self.config_path
            ),
        )))
    }

    pub(crate) fn command_spec(&self, command: PublicCommand) -> CommandSpec {
        let subcommand = match command {
            PublicCommand::Launch => "launch",
            PublicCommand::Route => "route",
            PublicCommand::Destroy => "destroy",
        };
        let args = [
            OsString::from("--config"),
            self.config_path.as_os_str().to_owned(),
            OsString::from("--workdir"),
            self.workdir.as_os_str().to_owned(),
            OsString::from("--name"),
            OsString::from(&self.name),
            OsString::from("--dataset"),
            OsString::from(&self.dataset),
            OsString::from("--build-root"),
            self.build_root.as_os_str().to_owned(),
            OsString::from(subcommand),
        ]
        .into_iter()
        .collect();

        CommandSpec {
            program: self.executable.clone(),
            args,
            current_dir: self.workdir.clone().into_std_path_buf(),
            env: self.effective_env.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PublicCommand, TuiContext};
    use camino::Utf8PathBuf;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use voxel_config::VoxelConfig;

    fn context() -> TuiContext {
        TuiContext {
            config_path: Utf8PathBuf::from("/cfg/voxel.toml"),
            workdir: Utf8PathBuf::from("/work/voxel"),
            name: "demo".to_string(),
            dataset: "rpool/falcon".to_string(),
            build_root: Utf8PathBuf::from("/build/voxel"),
            config: VoxelConfig::default(),
            executable: PathBuf::from("/bin/voxel"),
            effective_env: vec![
                (
                    OsString::from("FALCON_DATASET"),
                    OsString::from("rpool/falcon"),
                ),
                (OsString::from("BUILD_ROOT"), OsString::from("/build/voxel")),
                (
                    OsString::from("VOXEL_OMICRON_SRC"),
                    OsString::from("/build/voxel/omicron-main"),
                ),
            ],
        }
    }

    fn expected_args(command: &str) -> Vec<OsString> {
        [
            "--config",
            "/cfg/voxel.toml",
            "--workdir",
            "/work/voxel",
            "--name",
            "demo",
            "--dataset",
            "rpool/falcon",
            "--build-root",
            "/build/voxel",
            command,
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    #[test]
    fn constructs_exact_public_command_specs() {
        let context = context();

        for (command, subcommand) in [
            (PublicCommand::Launch, "launch"),
            (PublicCommand::Route, "route"),
            (PublicCommand::Destroy, "destroy"),
        ] {
            let spec = context.command_spec(command);
            assert_eq!(spec.program, PathBuf::from("/bin/voxel"));
            assert_eq!(spec.args, expected_args(subcommand));
            assert_eq!(spec.current_dir, PathBuf::from("/work/voxel"));
            assert_eq!(spec.env, context.effective_env);
        }
    }

    fn context_with_config(name: &str, text: Option<&str>) -> TuiContext {
        let dir = std::env::temp_dir()
            .join(format!("voxel-tui-external-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = Utf8PathBuf::try_from(dir.join("voxel.toml")).unwrap();
        if let Some(text) = text {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        TuiContext { config_path: path, ..context() }
    }

    #[test]
    fn unconfigured_external_network_defaults_to_isolated() {
        let mut context = context_with_config("default", None);
        let (_, note) = context
            .default_isolated_external(Some("ixgbe0".into()))
            .unwrap()
            .unwrap();
        assert!(note.contains("isolated mode out of ixgbe0"), "{note}");
        assert!(context.config.external.isolated());
        assert_eq!(context.config.external.uplink.as_deref(), Some("ixgbe0"));
        let written = VoxelConfig::from_toml(
            &std::fs::read_to_string(&context.config_path).unwrap(),
        )
        .unwrap();
        assert!(written.external.isolated());
        // Once written, the mode is explicit and is not revisited.
        assert_eq!(
            context.default_isolated_external(Some("igb0".into())).unwrap(),
            None
        );
    }

    #[test]
    fn explicit_external_choices_are_kept() {
        let mut lan =
            context_with_config("lan", Some("[external]\nmode = \"lan\"\n"));
        assert_eq!(
            lan.default_isolated_external(Some("ixgbe0".into())).unwrap(),
            None
        );
        assert!(!lan.config.external.isolated());

        let mut uplink = context_with_config(
            "uplink",
            Some("[external]\nuplink = \"igb1\"\n"),
        );
        uplink.default_isolated_external(Some("ixgbe0".into())).unwrap();
        assert_eq!(uplink.config.external.uplink.as_deref(), Some("igb1"));
        assert!(uplink.config.external.isolated());

        let mut no_route = context_with_config("no-route", Some(""));
        let (level, note) =
            no_route.default_isolated_external(None).unwrap().unwrap();
        assert_eq!(level, super::LogLevel::Warning);
        assert!(note.contains("staying in lan mode"), "{note}");
        assert!(!no_route.config.external.isolated());
    }

    #[test]
    fn launch_does_not_enable_optional_flags() {
        let args = context().command_spec(PublicCommand::Launch).args;

        for flag in ["--no-progress", "--no-route", "--emu", "--sp-firmware"] {
            assert!(!args.contains(&OsString::from(flag)));
        }
    }
}
