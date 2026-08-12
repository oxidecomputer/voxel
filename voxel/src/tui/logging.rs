use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use slog::warn;

    use super::*;

    #[test]
    fn falcon_logger_writes_to_durable_log() {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "voxel-tui-falcon-log-{}-{nonce}",
            std::process::id()
        ));
        let durable = DurableLog::open(&path).unwrap();

        warn!(durable.falcon_logger(), "serial retry"; "node" => "g0");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("WARN serial retry"), "{contents:?}");
        std::fs::remove_file(path).unwrap();
    }
}
