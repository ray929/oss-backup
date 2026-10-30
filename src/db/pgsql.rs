use crate::archive::{ChunkSink, ProduceStats};
use crate::config::PgsqlCfg;
use crate::db::stream_command;
use anyhow::Result;
use std::process::Command;

pub fn produce(
    sink: ChunkSink,
    cfg: &PgsqlCfg,
    database: &str,
    level: i32,
    passphrase: &str,
    dump_path: &str,
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

    cmd.arg(database);

    stream_command(cmd, sink, level, passphrase)
}
