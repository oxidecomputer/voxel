use std::{collections::VecDeque, io, process::ExitStatus};

use tokio::{
    io::{AsyncRead, AsyncReadExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
};

use super::{
    context::CommandSpec, logging::DurableLog, operation::OutputStream,
};

pub(crate) const STDERR_SUMMARY_LINES: usize = 128;
pub(crate) const MAX_OUTPUT_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputLine {
    pub(crate) stream: OutputStream,
    pub(crate) text: String,
}

#[derive(Debug)]
pub(crate) struct ChildResult {
    pub(crate) status: ExitStatus,
    pub(crate) stderr_summary: Vec<String>,
    pub(crate) output_errors: Vec<(OutputStream, DrainFailure)>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ChildWaitFailure {
    pub(crate) message: String,
}

impl From<io::Error> for ChildWaitFailure {
    fn from(error: io::Error) -> Self {
        Self { message: error.to_string() }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DrainFailure {
    Output(String),
    DurableLog(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ForceStopResult {
    Killed,
    AlreadyExited,
    Failed(String),
}

struct DrainResult {
    stderr: VecDeque<String>,
    errors: Vec<DrainFailure>,
}

pub(crate) struct ChildSupervisor {
    child: Child,
    stdout: JoinHandle<DrainResult>,
    stderr: JoinHandle<DrainResult>,
}

impl ChildSupervisor {
    pub(crate) fn spawn(
        spec: CommandSpec,
        durable: DurableLog,
        ui: mpsc::Sender<OutputLine>,
    ) -> io::Result<Self> {
        let mut command = Command::new(spec.program);
        command
            .args(spec.args)
            .current_dir(spec.current_dir)
            .envs(spec.env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("configured stdout pipe");
        let stderr = child.stderr.take().expect("configured stderr pipe");
        Ok(Self {
            child,
            stdout: tokio::spawn(drain(
                stdout,
                OutputStream::Stdout,
                durable.clone(),
                ui.clone(),
            )),
            stderr: tokio::spawn(drain(
                stderr,
                OutputStream::Stderr,
                durable,
                ui,
            )),
        })
    }

    pub(crate) async fn force_stop(&mut self) -> ForceStopResult {
        match self.child.try_wait() {
            Ok(Some(_)) => ForceStopResult::AlreadyExited,
            Ok(None) => match self.child.kill().await {
                Ok(()) => ForceStopResult::Killed,
                Err(error) => match self.child.try_wait() {
                    Ok(Some(_)) => ForceStopResult::AlreadyExited,
                    _ => ForceStopResult::Failed(error.to_string()),
                },
            },
            Err(error) => ForceStopResult::Failed(error.to_string()),
        }
    }

    pub(crate) fn is_settled(&mut self) -> Result<bool, ChildWaitFailure> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(ChildWaitFailure::from)
    }

    pub(crate) async fn wait(
        mut self,
    ) -> Result<ChildResult, ChildWaitFailure> {
        let status = self.child.wait().await.map_err(ChildWaitFailure::from)?;
        let stdout = join_drain(self.stdout, OutputStream::Stdout).await;
        let stderr = join_drain(self.stderr, OutputStream::Stderr).await;
        let mut output_errors = Vec::new();
        output_errors.extend(
            stdout
                .errors
                .into_iter()
                .map(|error| (OutputStream::Stdout, error)),
        );
        output_errors.extend(
            stderr
                .errors
                .into_iter()
                .map(|error| (OutputStream::Stderr, error)),
        );
        Ok(ChildResult {
            status,
            stderr_summary: stderr.stderr.into(),
            output_errors,
        })
    }
}

async fn join_drain(
    task: JoinHandle<DrainResult>,
    stream: OutputStream,
) -> DrainResult {
    task.await.unwrap_or_else(|error| DrainResult {
        stderr: VecDeque::new(),
        errors: vec![DrainFailure::Output(format!(
            "{stream:?} drain task failed: {error}"
        ))],
    })
}

async fn drain(
    pipe: impl AsyncRead + Unpin,
    stream: OutputStream,
    durable: DurableLog,
    ui: mpsc::Sender<OutputLine>,
) -> DrainResult {
    let mut reader = BufReader::new(pipe);
    let mut bytes = [0; MAX_OUTPUT_CHUNK_BYTES];
    let mut pending = Vec::with_capacity(MAX_OUTPUT_CHUNK_BYTES);
    let mut stderr = VecDeque::with_capacity(STDERR_SUMMARY_LINES);
    let mut errors = Vec::new();
    loop {
        match reader.read(&mut bytes).await {
            Ok(0) => break,
            Ok(read) => {
                for byte in &bytes[..read] {
                    if *byte == b'\n' {
                        if pending.last() == Some(&b'\r') {
                            pending.pop();
                        }
                        emit_line(
                            &pending,
                            stream,
                            &durable,
                            &ui,
                            &mut stderr,
                            &mut errors,
                        );
                        pending.clear();
                    } else {
                        if pending.len() == MAX_OUTPUT_CHUNK_BYTES {
                            emit_line(
                                &pending,
                                stream,
                                &durable,
                                &ui,
                                &mut stderr,
                                &mut errors,
                            );
                            pending.clear();
                        }
                        pending.push(*byte);
                    }
                }
            }
            Err(error) => {
                errors.push(DrainFailure::Output(error.to_string()));
                return DrainResult { stderr, errors };
            }
        }
    }
    if !pending.is_empty() {
        emit_line(&pending, stream, &durable, &ui, &mut stderr, &mut errors);
    }
    DrainResult { stderr, errors }
}

fn emit_line(
    bytes: &[u8],
    stream: OutputStream,
    durable: &DurableLog,
    ui: &mpsc::Sender<OutputLine>,
    stderr: &mut VecDeque<String>,
    errors: &mut Vec<DrainFailure>,
) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if let Err(error) = durable.write_line(&text)
        && !errors
            .iter()
            .any(|failure| matches!(failure, DrainFailure::DurableLog(_)))
    {
        errors.push(DrainFailure::DurableLog(error.to_string()));
    }
    if stream == OutputStream::Stderr {
        if stderr.len() == STDERR_SUMMARY_LINES {
            stderr.pop_front();
        }
        stderr.push_back(text.clone());
    }
    let _ = ui.try_send(OutputLine { stream, text });
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        io::{self, Write},
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    use tokio::{sync::mpsc, time::timeout};

    use super::*;
    use crate::tui::{context::CommandSpec, logging::DurableLog};

    const TWO_MIB: usize = 2 * 1024 * 1024;

    #[test]
    fn process_helper() {
        let Ok(mode) = std::env::var("VOXEL_TUI_PROCESS_HELPER") else {
            return;
        };
        match mode.as_str() {
            "both-full" => {
                let out = vec![b'o'; TWO_MIB];
                let err = vec![b'e'; TWO_MIB];
                let stdout = std::thread::spawn(move || {
                    io::stdout().write_all(&out).unwrap()
                });
                let stderr = std::thread::spawn(move || {
                    io::stderr().write_all(&err).unwrap()
                });
                stdout.join().unwrap();
                stderr.join().unwrap();
            }
            "exit-7" => {
                for n in 0..300 {
                    eprintln!("stderr-{n:03}");
                }
                std::process::exit(7);
            }
            "wait" => {
                println!("started");
                io::stdout().flush().unwrap();
                std::thread::sleep(Duration::from_millis(600));
                println!("settled");
            }
            "unterminated" => {
                io::stdout().write_all(b"last line").unwrap();
                io::stdout().flush().unwrap();
            }
            "split-line" => {
                let mut stdout = io::stdout();
                stdout.write_all(b"rack external ").unwrap();
                stdout.flush().unwrap();
                std::thread::sleep(Duration::from_millis(100));
                stdout.write_all(b"route: timed out\n").unwrap();
                stdout.flush().unwrap();
            }
            "invalid-utf8" => {
                io::stdout().write_all(b"bad \xff text\n").unwrap()
            }
            other => panic!("unknown helper mode {other}"),
        }
        std::process::exit(0);
    }

    fn paths(mode: &str) -> (CommandSpec, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let log = std::env::temp_dir().join(format!(
            "voxel-tui-{mode}-{}-{nonce}.log",
            std::process::id()
        ));
        let test_name = "tui::process::tests::process_helper";
        let spec = CommandSpec {
            program: std::env::current_exe().unwrap(),
            args: ["--exact", test_name, "--nocapture"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            current_dir: std::env::current_dir().unwrap(),
            env: vec![(
                OsString::from("VOXEL_TUI_PROCESS_HELPER"),
                OsString::from(mode),
            )],
        };
        (spec, log)
    }

    async fn spawn(
        mode: &str,
        capacity: usize,
    ) -> (ChildSupervisor, mpsc::Receiver<OutputLine>, PathBuf) {
        let (spec, path) = paths(mode);
        let (tx, rx) = mpsc::channel(capacity);
        let child =
            ChildSupervisor::spawn(spec, DurableLog::open(&path).unwrap(), tx)
                .unwrap();
        (child, rx, path)
    }

    #[tokio::test]
    async fn drains_large_stdout_and_stderr_without_deadlock() {
        let (child, _rx, path) = spawn("both-full", 1).await;
        let result = timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(result.status.success());
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.iter().filter(|b| **b == b'o').count() >= TWO_MIB);
        assert!(bytes.iter().filter(|b| **b == b'e').count() >= TWO_MIB);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn segments_newline_free_output_into_bounded_chunks() {
        let (child, mut rx, path) = spawn("both-full", 16).await;
        let result = timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(result.status.success());
        while let Ok(line) = rx.try_recv() {
            assert!(line.text.len() <= MAX_OUTPUT_CHUNK_BYTES);
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'o').count(), TWO_MIB);
        assert!(bytes.iter().filter(|byte| **byte == b'e').count() >= TWO_MIB);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn child_wait_errors_are_structured() {
        let failure = ChildWaitFailure::from(io::Error::other("wait broke"));
        assert_eq!(failure.message, "wait broke");
    }

    #[test]
    fn drain_failures_distinguish_pipe_and_durable_log() {
        assert_ne!(
            DrainFailure::Output("read broke".into()),
            DrainFailure::DurableLog("disk full".into())
        );
    }

    #[tokio::test]
    async fn captures_final_unterminated_line() {
        let (child, mut rx, path) = spawn("unterminated", 8).await;
        child.wait().await.unwrap();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("last line\n")
        );
        let mut found = false;
        while let Ok(line) = rx.try_recv() {
            found |= line.text == "last line";
        }
        assert!(found);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn assembles_a_line_written_in_multiple_flushed_pieces() {
        let (child, mut rx, path) = spawn("split-line", 8).await;
        child.wait().await.unwrap();
        let lines: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|line| line.stream == OutputStream::Stdout)
            .map(|line| line.text)
            .collect();
        assert!(
            lines.iter().any(|line| line == "rack external route: timed out")
        );
        assert!(!lines.iter().any(|line| line == "rack external "));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn lossily_preserves_invalid_utf8() {
        let (child, _rx, path) = spawn("invalid-utf8", 8).await;
        child.wait().await.unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("bad � text"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn nonzero_exit_keeps_bounded_stderr_summary() {
        let (child, _rx, path) = spawn("exit-7", 1).await;
        let result = child.wait().await.unwrap();
        assert_eq!(result.status.code(), Some(7));
        assert_eq!(result.stderr_summary.len(), STDERR_SUMMARY_LINES);
        assert_eq!(result.stderr_summary.last().unwrap(), "stderr-299");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn waiting_does_not_terminate_child() {
        let (child, _rx, path) = spawn("wait", 8).await;
        let result = child.wait().await.unwrap();
        assert!(result.status.success());
        assert!(std::fs::read_to_string(&path).unwrap().contains("settled"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn force_stop_settles_direct_child() {
        let (mut child, _rx, path) = spawn("wait", 8).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let kill = child.force_stop().await;
        let result = child.wait().await.unwrap();
        assert!(matches!(
            kill,
            ForceStopResult::Killed | ForceStopResult::AlreadyExited
        ));
        assert!(!result.status.success());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn closed_ui_receiver_does_not_block_drain() {
        let (child, rx, path) = spawn("both-full", 1).await;
        drop(rx);
        timeout(Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() >= (TWO_MIB * 2) as u64
        );
        std::fs::remove_file(path).unwrap();
    }
}
