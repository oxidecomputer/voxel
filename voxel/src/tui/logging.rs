// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The durable `voxel-tui.log`, shared by the TUI and the Falcon runner,
//! which keeps a record after the terminal UI exits or detaches.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant},
};

use slog::Drain;

#[derive(Clone)]
pub(crate) struct DurableLog(Arc<Mutex<File>>);

impl DurableLog {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self(Arc::new(Mutex::new(file))))
    }

    pub(crate) fn write_line(&self, line: &str) -> io::Result<()> {
        let mut file =
            self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        writeln!(file, "{line}")?;
        file.flush()
    }

    /// Like [`write_line`](Self::write_line), but skips the line if the log
    /// stays busy for `wait`. The panic hook uses this: the panicking thread
    /// may itself hold the lock, and waiting on it would hang the process.
    pub(crate) fn write_line_within(
        &self,
        line: &str,
        wait: Duration,
    ) -> io::Result<()> {
        let deadline = Instant::now() + wait;
        let mut file = loop {
            match self.0.try_lock() {
                Ok(file) => break file,
                Err(TryLockError::Poisoned(poisoned)) => {
                    break poisoned.into_inner();
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TryLockError::WouldBlock) => return Ok(()),
            }
        };
        writeln!(file, "{line}")?;
        file.flush()
    }

    pub(crate) fn falcon_logger(&self) -> slog::Logger {
        slog::Logger::root(DurableDrain(self.clone()).fuse(), slog::o!())
    }
}

struct DurableDrain(DurableLog);

impl slog::Drain for DurableDrain {
    type Ok = ();
    type Err = io::Error;

    fn log(
        &self,
        record: &slog::Record<'_>,
        _values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        self.0.write_line(&format!("{} {}", record.level(), record.msg()))
    }
}

#[cfg(test)]
mod tests {
    use slog::warn;

    use super::*;

    #[test]
    fn falcon_logger_writes_to_durable_log() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let durable = DurableLog::open(&path).unwrap();

        warn!(durable.falcon_logger(), "serial retry"; "node" => "g0");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("WARN serial retry"), "{contents:?}");
    }

    #[test]
    fn bounded_write_gives_up_on_a_lock_its_own_thread_holds() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let durable = DurableLog::open(&path).unwrap();

        let held = durable.0.lock().unwrap();
        durable
            .write_line_within("skipped", Duration::from_millis(20))
            .unwrap();
        drop(held);
        durable
            .write_line_within("written", Duration::from_millis(20))
            .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "written\n");
    }
}
