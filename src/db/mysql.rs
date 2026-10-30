use crate::archive::{ChunkSink, ProduceStats};
use crate::config::MysqlCfg;
use crate::db::stream_command;
use anyhow::Result;
use std::process::Command;

pub fn produce(
    sink: ChunkSink,
    cfg: &MysqlCfg,
    database: &str,
    tables: &[Option<String>],
    level: i32,
    passphrase: &str,
    dump_path: &str,
) -> Result<ProduceStats> {
    let mut cmd = Command::new(dump_path);
    cmd.arg(format!("--host={}", cfg.host))
        .arg(format!("--port={}", cfg.port))
        .arg(format!("--user={}", cfg.username))
        .arg("--single-transaction")
        .arg("--routines")
        .arg("--triggers")
        .env("MYSQL_PWD", &cfg.password);

    if let Some(ref ssl_mode) = cfg.ssl_mode {
        cmd.arg(format!("--ssl-mode={ssl_mode}"));
    }

    cmd.arg(database);

    // 任一表是 None（即整库备份）时忽略所有表名
    if !tables.iter().any(|t| t.is_none()) {
        for table in tables.iter().flatten() {
            cmd.arg(table);
        }
    }

    stream_command(cmd, sink, level, passphrase)
}
