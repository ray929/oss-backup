use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::fs::OpenOptions;
use std::path::Path;

/// 进程级防重叠锁，随进程退出自动释放
pub struct RunLock {
    _file: std::fs::File,
}

pub fn acquire(config_path: &Path) -> Result<RunLock> {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let path = dir.join(".oss-backup.lock");

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("打开锁文件失败：{}", path.display()))?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(RunLock { _file: file }),
        Err(_) => bail!("另一个 oss-backup 正在运行（锁文件：{}）", path.display()),
    }
}
