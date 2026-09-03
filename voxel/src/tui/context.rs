use camino::Utf8PathBuf;
use std::ffi::OsString;
use std::path::PathBuf;
use voxel_config::VoxelConfig;

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

    #[test]
    fn launch_does_not_enable_optional_flags() {
        let args = context().command_spec(PublicCommand::Launch).args;

        for flag in ["--no-progress", "--no-route", "--emu", "--sp-firmware"] {
            assert!(!args.contains(&OsString::from(flag)));
        }
    }
}
