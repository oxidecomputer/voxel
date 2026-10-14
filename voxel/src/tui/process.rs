// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Supervises a lifecycle child process: spawns it with piped output, drains
//! stdout and stderr into bounded lines without deadlocking, keeps a stderr
//! summary for failures, and force stops it on request.

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
        time::Duration,
    };

    use tempfile::TempPath;
    use tokio::{sync::mpsc, time::timeout};

    use super::*;
    use crate::tui::{context::CommandSpec, logging::DurableLog};

    const TWO_MIB: usize = 2 * 1024 * 1024;

    /// The child's first stdout line. libtest exits 0 having run nothing when
    /// `--exact` names no test, so without this a renamed `process_helper`
    /// would turn every parent test into a silent pass. libtest may print
    /// `test <name> ... ` on the same line first, so parents look for it with
    /// `contains`.
    const HELPER_RAN: &str = "voxel-tui process helper ran";

    #[test]
    fn process_helper() {
        let Ok(mode) = std::env::var("VOXEL_TUI_PROCESS_HELPER") else {
            return;
        };
        println!("{HELPER_RAN}");
        match mode.as_str() {
            // Both streams end without a newline, so their final chunks are
            // unterminated.
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
            "wait" => {
                std::thread::sleep(Duration::from_millis(200));
                println!("settled");
            }
            other => panic!("unknown helper mode {other}"),
        }
        std::process::exit(0);
    }

    /// Re-executes this test binary as a child running `process_helper` in
    /// `mode`.
    fn spawn(
        mode: &str,
        capacity: usize,
    ) -> (ChildSupervisor, mpsc::Receiver<OutputLine>, TempPath) {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let spec = CommandSpec {
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "tui::process::tests::process_helper",
                "--nocapture",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            current_dir: std::env::current_dir().unwrap(),
            env: vec![(
                OsString::from("VOXEL_TUI_PROCESS_HELPER"),
                OsString::from(mode),
            )],
        };
        let (tx, rx) = mpsc::channel(capacity);
        let child =
            ChildSupervisor::spawn(spec, DurableLog::open(&path).unwrap(), tx)
                .unwrap();
        (child, rx, path)
    }

    /// Reads the durable log of a settled child, failing if the helper never
    /// ran.
    fn helper_log(path: &TempPath) -> String {
        let log =
            String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        assert!(log.contains(HELPER_RAN), "helper did not run: {log:.400}");
        log
    }

    /// Total length of the log lines made only of `byte`. libtest's own
    /// output lines carry stray letters, so they are excluded rather than
    /// counted.
    fn run_bytes(log: &str, byte: char) -> usize {
        log.lines()
            .filter(|line| !line.is_empty() && line.chars().all(|c| c == byte))
            .map(str::len)
            .sum()
    }

    #[tokio::test]
    async fn drains_full_pipes_into_bounded_chunks_without_loss_or_deadlock() {
        // Both pipes and the bounded UI channel fill long before the child
        // finishes writing.
        let (child, _rx, path) = spawn("both-full", 16);
        timeout(Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
        // The UI receives the same chunks, so bounding the log bounds both.
        let log = helper_log(&path);
        assert!(
            log.lines().all(|line| line.len() <= MAX_OUTPUT_CHUNK_BYTES),
            "durable log holds an unbounded line"
        );
        assert_eq!(
            (run_bytes(&log, 'o'), run_bytes(&log, 'e')),
            (TWO_MIB, TWO_MIB),
            "stdout and stderr bytes in log"
        );
    }

    #[tokio::test]
    async fn force_stop_settles_direct_child() {
        let (mut child, mut rx, _log) = spawn("wait", 8);
        // Kill only once the helper is known to be running, so a helper that
        // never ran cannot pass as a killed one.
        timeout(Duration::from_secs(5), async {
            while let Some(line) = rx.recv().await {
                if line.text.contains(HELPER_RAN) {
                    return;
                }
            }
            panic!("helper exited without running");
        })
        .await
        .unwrap();
        assert_eq!(child.force_stop().await, ForceStopResult::Killed);
        assert!(!child.wait().await.unwrap().status.success());
    }
}
