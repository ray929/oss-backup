use crate::archive::{ChunkSink, ProduceStats};
use crate::config::MysqlCfg;
use crate::db::stream_command;
use anyhow::Result;
use std::process::Command;
use std::time::Duration;

/// mysqldump 的厂商。两者参数不完全兼容，硬塞会直接报 unknown option。
#[derive(Debug, Clone, Copy)]
enum Flavor {
    Mysql,
    Mariadb,
    Unknown,
}

fn detect_flavor(dump_path: &str) -> Flavor {
    let Ok(output) = Command::new(dump_path).arg("--version").output() else {
        return Flavor::Unknown;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if text.contains("mariadb") {
        Flavor::Mariadb
    } else if text.contains("mysql") {
        Flavor::Mysql
    } else {
        Flavor::Unknown
    }
}

#[allow(clippy::too_many_arguments)]
pub fn produce(
    sink: ChunkSink,
    cfg: &MysqlCfg,
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
        .arg(format!("--user={}", cfg.username))
        .arg("--single-transaction")
        .arg("--routines")
        .arg("--triggers")
        .env("MYSQL_PWD", &cfg.password);

    // MariaDB 的 mysqldump 没有这两个选项，传了会直接失败；探测不到厂商时也不加，
    // 保持原有行为。MySQL 侧的最低版本要求见 docs/ops-notes.md。
    if matches!(detect_flavor(dump_path), Flavor::Mysql) {
        // 备份账号通常没有 PROCESS 权限，不加会报 Access denied; you need the PROCESS privilege
        cmd.arg("--no-tablespaces");
        // 否则 dump 里会带 GTID_PURGED，恢复到未开 GTID 的实例会失败
        cmd.arg("--set-gtid-purged=OFF");
    }

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

    stream_command(cmd, sink, level, passphrase, timeout)
}
