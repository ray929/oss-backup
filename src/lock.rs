use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::fs::OpenOptions;
use std::path::PathBuf;

/// 进程级防重叠锁，随进程退出自动释放
pub struct RunLock {
    _file: std::fs::File,
}

/// 全局锁：同一用户共用一把，换配置文件也不能并行
///
/// 不能放在配置文件旁边——那样不同配置各自拿到一把锁，两个实例会同时上传。
fn lock_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".oss-backup.lock")
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn acquire() -> Result<RunLock> {
    let path = lock_path();

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("打开锁文件失败：{}", path.display()))?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(RunLock { _file: file }),
        Err(_) => bail!(
            "另一个 oss-backup 正在运行（全局锁：{}）\n同一时间只允许一个实例，不同配置文件也不行",
            path.display()
        ),
    }
}
