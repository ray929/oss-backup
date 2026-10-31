use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_subscriber::fmt::time::LocalTime;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug)]
struct FileWriter(Mutex<std::fs::File>);

impl FileWriter {
    fn new(path: &Path) -> Result<Self> {
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

/// `log_dir` 只影响 `--background`：日志写在配置文件旁边，多份配置共用同一个
/// 工作目录时不会混写同一个文件。
pub fn init(log_level: Option<&str>, background: bool, log_dir: &Path) -> Result<()> {
    // 只认配置里的 log_level，不读 RUST_LOG：本工具的原则是不从环境变量取设置。
    let filter = match log_level {
        Some(level) => EnvFilter::new(level),
        None => EnvFilter::new("oss_backup=info"),
    };

    let layer = fmt::layer()
        .with_target(false)
        .with_level(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_timer(LocalTime::rfc_3339())
        .compact();

    if background {
        let path: PathBuf = log_dir.join("oss-backup.log");
        let writer = FileWriter::new(&path)?;
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
