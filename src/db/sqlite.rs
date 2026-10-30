use crate::archive::{age, ChunkSink, ProduceStats};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// SQLite 热备：先 `sqlite3 .backup` 到临时文件，再顺序读入管道
pub fn produce(
    sink: ChunkSink,
    database: &Path,
    level: i32,
    passphrase: &str,
    sqlite3_path: &str,
) -> Result<ProduceStats> {
    if !database.exists() {
        bail!("SQLite 数据库不存在：{}", database.display());
    }

    let temp = tempfile::Builder::new()
        .suffix(".db")
        .tempfile()
        .context("创建临时备份文件失败")?;
    let snapshot = temp.into_temp_path();

    let output = Command::new(sqlite3_path)
        .arg(database)
        .arg(format!(".backup '{}'", snapshot.display()))
        .output()
        .with_context(|| format!("启动命令失败：{sqlite3_path}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "sqlite3 执行失败（{}）：{}",
            output.status,
            crate::db::tail_lines(&stderr, 20)
        );
    }

    let mut file = std::fs::File::open(&snapshot)
        .with_context(|| format!("读取临时快照失败：{}", snapshot.display()))?;

    let age_writer = age::encrypt(sink, passphrase)?;
    let mut encoder = zstd::Encoder::new(age_writer, level).context("初始化 zstd 压缩器失败")?;
    std::io::copy(&mut file, &mut encoder).context("读取临时快照失败")?;

    let age_writer = encoder.finish().context("结束 zstd 压缩失败")?;
    let sink = age_writer.finish().context("结束 age 加密失败")?;
    sink.finish().context("刷新上传缓冲失败")?;

    Ok(ProduceStats {
        entries: 1,
        plaintext_bytes: None,
    })
}
