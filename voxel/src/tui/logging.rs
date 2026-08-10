use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
};

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
}
