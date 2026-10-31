use crate::archive::{age, ChunkSink, ProduceStats};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// SQLite 热备：先 `sqlite3 .backup` 到临时文件，再顺序读入管道
///
/// 注意：`.backup` 只写文件，因此这一步会在系统临时目录留下未加密的快照副本，
/// 进程正常退出时由 `TempPath` 删除，被强杀时残留。见 README「已知边界」。
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
    // sqlite3 的 dot-command 用单引号包路径，路径内再出现单引号无法转义
    if database.to_string_lossy().contains('\'') {
        bail!(
            "SQLite 路径含单引号，无法安全传给 sqlite3：{}",
            database.display()
        );
    }

    let temp = tempfile::Builder::new()
        .suffix(".db")
        .tempfile()
        .context("创建临时备份文件失败")?;
    let snapshot = temp.into_temp_path();

    let output = Command::new(sqlite3_path)
        .arg(database)
        .arg(format!(".backup '{}'", snapshot.display()))
        .stdin(std::process::Stdio::null())
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
    let snapshot_len = file.metadata().map(|m| m.len()).ok();

    let age_writer = age::encrypt(sink, passphrase)?;
    let mut encoder = zstd::Encoder::new(age_writer, level).context("初始化 zstd 压缩器失败")?;
    std::io::copy(&mut file, &mut encoder).context("读取临时快照失败")?;

    let age_writer = encoder.finish().context("结束 zstd 压缩失败")?;
    let sink = age_writer.finish().context("结束 age 加密失败")?;
    sink.finish().context("刷新上传缓冲失败")?;

    Ok(ProduceStats {
        entries: 1,
        plaintext_bytes: snapshot_len,
    })
}
