use crate::archive::{ChunkSink, ProduceStats};
use crate::config::PgsqlCfg;
use crate::db::stream_command;
use anyhow::Result;
use std::process::Command;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub fn produce(
    sink: ChunkSink,
    cfg: &PgsqlCfg,
    database: &str,
    tables: &[Option<String>],
    level: i32,
    passphrase: &str,
    dump_path: &str,
    timeout: Duration,
) -> Result<ProduceStats> {
    let mut cmd = Command::new(dump_path);
    cmd.arg(format!("--host={}", cfg.host))
        .arg(format!("--port={}", cfg.port))
        .arg(format!("--username={}", cfg.username))
        .arg("--no-password")
        .env("PGPASSWORD", &cfg.password);

    if let Some(ref ssl_mode) = cfg.ssl_mode {
        cmd.env("PGSSLMODE", ssl_mode);
    }

    // 与 mysql 一致：全部指定了表名才按表导出，任一项为整库则忽略表名
    if !tables.iter().any(|t| t.is_none()) {
        for table in tables.iter().flatten() {
            cmd.arg("-t").arg(table);
        }
    }

    cmd.arg(database);

    stream_command(cmd, sink, level, passphrase, timeout)
}
