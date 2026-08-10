use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use super::TuiContext;

const VERSION: u32 = 1;
const CLAIM_ENV: &str = "VOXEL_TUI_SESSION_CLAIM";
const ELEVATED_ENV: &str = "VOXEL_TUI_RESUME_ELEVATED";
const SYSTEM_DIRECTORY: &str = "/var/run/voxel/tui-sessions";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DetachedSession {
    version: u32,
    detached_at_ns: u128,
    executable: PathBuf,
    config_path: PathBuf,
    workdir: PathBuf,
    name: String,
    dataset: String,
    build_root: PathBuf,
}

impl DetachedSession {
    fn from_context(context: &TuiContext) -> anyhow::Result<Self> {
        Ok(Self {
            version: VERSION,
            detached_at_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock precedes Unix epoch")?
                .as_nanos(),
            executable: context.executable().to_owned(),
            config_path: context.config_path.clone().into_std_path_buf(),
            workdir: context.workdir.clone().into_std_path_buf(),
            name: context.name.clone(),
            dataset: context.dataset.clone(),
            build_root: context.build_root.clone().into_std_path_buf(),
        })
    }

    fn arguments(&self) -> Vec<OsString> {
        [
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
            OsString::from("tui"),
        ]
        .into_iter()
        .collect()
    }

    pub(crate) fn reattach_command(&self) -> anyhow::Result<String> {
        let mut words = vec!["pfexec".to_owned(), quote(&self.executable)?];
        for argument in self.arguments() {
            words.push(crate::util::shell_quote(
                argument.to_str().ok_or_else(|| {
                    anyhow!("recorded TUI argument is not valid UTF-8")
                })?,
            ));
        }
        Ok(words.join(" "))
    }
}

fn quote(path: &Path) -> anyhow::Result<String> {
    Ok(crate::util::shell_quote(
        path.to_str()
            .ok_or_else(|| anyhow!("recorded TUI path is not valid UTF-8"))?,
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Stored {
    path: PathBuf,
    session: DetachedSession,
}

struct Store {
    directory: PathBuf,
}

impl Store {
    fn system() -> Self {
        Self { directory: SYSTEM_DIRECTORY.into() }
    }

    #[cfg(test)]
    fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn write(&self, session: &DetachedSession) -> anyhow::Result<PathBuf> {
        ensure_private_dir(&self.directory)?;
        let path = self.directory.join(format!(
            "{:032}-{}.json",
            session.detached_at_ns,
            std::process::id()
        ));
        let temporary = path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(&serde_json::to_vec_pretty(session)?)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        Ok(path)
    }

    fn load(&self) -> anyhow::Result<(Vec<Stored>, Vec<String>)> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((vec![], vec![]));
            }
            Err(error) => return Err(error.into()),
        };
        let mut records = vec![];
        let mut warnings = vec![];
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            match read_private(&path) {
                Ok(session) if session.version == VERSION => {
                    records.push(Stored { path, session })
                }
                Ok(session) => warnings.push(format!(
                    "skip {}: unsupported session schema {}",
                    path.display(),
                    session.version
                )),
                Err(error) => {
                    warnings.push(format!("skip {}: {error:#}", path.display()))
                }
            }
        }
        records.sort_by(|a, b| {
            b.session
                .detached_at_ns
                .cmp(&a.session.detached_at_ns)
                .then_with(|| b.path.cmp(&a.path))
        });
        Ok((records, warnings))
    }
}

fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if path == Path::new(SYSTEM_DIRECTORY) {
            let mut parent_builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                parent_builder.mode(0o700);
            }
            match parent_builder.create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(anyhow!("TUI session directory is not private"));
        }
    }
    Ok(())
}

fn read_private(path: &Path) -> anyhow::Result<DetachedSession> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(anyhow!("session record is not private"));
        }
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub(crate) fn prepare_direct(
    context: &TuiContext,
) -> anyhow::Result<(DetachedSession, String)> {
    let session = DetachedSession::from_context(context)?;
    let command = session.reattach_command()?;
    Ok((session, command))
}

pub(crate) fn detach(session: &DetachedSession) -> anyhow::Result<PathBuf> {
    Store::system().write(session)
}

pub(crate) struct SessionClaim {
    path: PathBuf,
    _file: fs::File,
}

impl SessionClaim {
    pub(crate) fn finish(self) -> anyhow::Result<()> {
        fs::remove_file(&self.path).with_context(|| {
            format!("remove resumed session {}", self.path.display())
        })
    }
}

pub(crate) fn claim_from_environment() -> anyhow::Result<Option<SessionClaim>> {
    let Some(path) = std::env::var_os(CLAIM_ENV).map(PathBuf::from) else {
        return Ok(None);
    };
    if path.parent() != Some(Path::new(SYSTEM_DIRECTORY)) {
        return Err(anyhow!("invalid detached TUI session claim path"));
    }
    read_private(&path)?;
    let file =
        OpenOptions::new().read(true).write(true).open(&path).with_context(
            || format!("open session claim {}", path.display()),
        )?;
    #[cfg(unix)]
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
        != 0
    {
        return Err(io::Error::last_os_error())
            .context("detached TUI session is already being resumed");
    }
    Ok(Some(SessionClaim { path, _file: file }))
}

pub(crate) fn resume(choose: bool) -> anyhow::Result<()> {
    elevate(choose)?;
    let (records, warnings) = Store::system().load()?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    if records.is_empty() {
        return Err(anyhow!("no detached Voxel TUI session found"));
    }
    let Some(record) = select(&records, choose)? else {
        return Ok(());
    };
    let status = Command::new(&record.session.executable)
        .args(record.session.arguments())
        .env(CLAIM_ENV, &record.path)
        .env_remove(ELEVATED_ENV)
        .status()
        .context("spawn detached TUI")?;
    if !status.success() {
        return Err(anyhow!("resumed TUI exited with status {status}"));
    }
    Ok(())
}

fn select(records: &[Stored], choose: bool) -> anyhow::Result<Option<&Stored>> {
    if records.is_empty() {
        return Ok(None);
    }
    if !choose {
        return Ok(records.first());
    }
    for (index, record) in records.iter().enumerate() {
        eprintln!(
            "  {}. {}  {}",
            index + 1,
            record.session.name,
            record.session.workdir.display()
        );
    }
    eprint!("Select session [1-{}, blank to cancel]: ", records.len());
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim().is_empty() {
        return Ok(None);
    }
    let selected: usize =
        input.trim().parse().context("session selection must be a number")?;
    records
        .get(selected.saturating_sub(1))
        .map(Some)
        .ok_or_else(|| anyhow!("session selection {selected} is out of range"))
}

#[cfg(unix)]
fn elevate(choose: bool) -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    if std::env::var_os(ELEVATED_ENV).is_some() {
        return Err(anyhow!("pfexec did not run Voxel with effective UID 0"));
    }
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("/usr/bin/pfexec");
    command
        .arg(std::env::current_exe()?)
        .args(["tui", "resume"])
        .env(ELEVATED_ENV, "1");
    if choose {
        command.arg("--choose");
    }
    Err(command.exec()).context("elevate detached TUI session resume")
}

#[cfg(not(unix))]
fn elevate(_: bool) -> anyhow::Result<()> {
    Err(anyhow!("resuming detached TUI sessions requires Unix"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record(at: u128, name: &str) -> DetachedSession {
        DetachedSession {
            version: VERSION,
            detached_at_ns: at,
            executable: "/opt/voxel bin/voxel".into(),
            config_path: "/cfg/v.toml".into(),
            workdir: "/work".into(),
            name: name.into(),
            dataset: "pool/falcon".into(),
            build_root: "/build".into(),
        }
    }
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voxel-session-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn round_trip_and_newest_selection_preserve_exact_values() {
        let directory = directory();
        let store = Store::at(directory.clone());
        store.write(&record(1, "old")).unwrap();
        let expected = record(2, "new");
        store.write(&expected).unwrap();
        let (records, warnings) = store.load().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(select(&records, false).unwrap().unwrap().session, expected);
        assert!(!expected.arguments().iter().any(|arg| arg == "--rss-gen"));
        assert!(
            expected
                .reattach_command()
                .unwrap()
                .starts_with("pfexec '/opt/voxel bin/voxel'")
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
