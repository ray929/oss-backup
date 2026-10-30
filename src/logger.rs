use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use tracing_subscriber::fmt::time::LocalTime;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug)]
struct FileWriter(Mutex<std::fs::File>);

impl FileWriter {
    fn new(path: &str) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self(Mutex::new(file)))
    }
}

impl<'a> fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriterGuard(self.0.lock().unwrap())
    }
}

struct FileWriterGuard<'a>(std::sync::MutexGuard<'a, std::fs::File>);

impl Write for FileWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

pub fn init(log_level: Option<&str>, background: bool) -> Result<()> {
    let filter = match log_level {
        Some(level) => EnvFilter::new(level),
        None => EnvFilter::from_default_env().add_directive(
            "oss_backup=info"
                .parse()
                .expect("static directive should parse"),
        ),
    };

    let layer = fmt::layer()
        .with_target(false)
        .with_level(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_timer(LocalTime::rfc_3339())
        .compact();

    if background {
        let writer = FileWriter::new("oss-backup.log")?;
        tracing_subscriber::registry()
            .with(filter)
            .with(layer.with_writer(writer))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .init();
    }

    Ok(())
}
