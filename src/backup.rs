use crate::archive::tar_zstd;
use crate::config::{Backup, BackupSettings, Config, MysqlCfg, PgsqlCfg};
use crate::db;
use crate::manifest::{self, Manifest, ManifestObject};
use crate::naming::{self, DbKind};
use crate::retention;
use crate::storage::{upload_stream, OssStore};
use anyhow::{bail, Context, Result};
use chrono::Local;
use std::time::Duration;
use tracing::{info, warn};
use walkdir::WalkDir;

pub struct RunSummary {
    pub objects: usize,
    pub total_bytes: u64,
}

pub async fn run_backup(cfg: &Config, backup: &Backup, dry_run: bool) -> Result<RunSummary> {
    let settings = cfg.settings(backup)?;
    let destination = cfg.destination(backup)?;
    let store = OssStore::new(destination, Duration::from_secs(settings.timeout_secs))?;

    let prefix = destination.prefix.as_deref();
    let run_prefix = naming::run_prefix(prefix);
    let started_at = Local::now();
    let timestamp = started_at.format(naming::TIMESTAMP_FORMAT).to_string();
    let run_dir = naming::run_dir(prefix, &timestamp);
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    if dry_run {
        check_local_inputs(cfg, backup)?;
        store
            .probe(&run_prefix)
            .await
            .context("dry-run 连通性检查失败")?;
        for (object, source) in planned_objects(backup) {
            info!(
                key = %format!("{run_dir}/{object}"),
                source = source.as_deref().unwrap_or("-"),
                "计划写入"
            );
        }
        if !backup.files.is_empty() {
            let (count, bytes) = estimate_file(&backup.files)?;
            let parts = ensure_part_limit(bytes, part_size)?;
            info!(files = count, bytes, parts, "待备份文件预估");
        }
        return Ok(RunSummary {
            objects: planned_objects(backup).len(),
            total_bytes: 0,
        });
    }

    // 正式路径同样先估算：分片超限要在这里拒绝，而不是等 OSS 传到一半才报错，
    // 那时已上传的分片只能靠 abort 清理。
    if !backup.files.is_empty() {
        let (_, bytes) = estimate_file(&backup.files)?;
        ensure_part_limit(bytes, part_size)?;
    }

    let dump_timeout = Duration::from_secs(settings.timeout_secs);
    let mut objects = Vec::new();
    if !backup.files.is_empty() {
        objects.extend(run_file(&store, &backup.files, &settings, &run_dir).await?);
    }
    if let Some(mysql) = &backup.mysql {
        let dump_path = cfg.mysqldump_path().to_string();
        objects
            .extend(run_mysql(&store, mysql, &settings, &run_dir, &dump_path, dump_timeout).await?);
    }
    if let Some(pgsql) = &backup.pgsql {
        let dump_path = cfg.pg_dump_path().to_string();
        objects
            .extend(run_pgsql(&store, pgsql, &settings, &run_dir, &dump_path, dump_timeout).await?);
    }
    if !backup.sqlite.is_empty() {
        let sqlite3_path = cfg.sqlite3_path().to_string();
        objects
            .extend(run_sqlite(&store, &backup.sqlite, &settings, &run_dir, &sqlite3_path).await?);
    }
    if objects.is_empty() {
        bail!("backup '{}' 没有可执行的备份类型", backup.destination);
    }

    let finished_at = Local::now();
    let total_bytes: u64 = objects.iter().map(|o| o.size).sum();
    let kind = kinds_of(backup);
    let manifest = Manifest::new(
        &kind,
        &backup.destination,
        started_at.to_rfc3339(),
        finished_at.to_rfc3339(),
        settings.zstd_level,
        inputs_of(backup),
        objects.clone(),
        (finished_at - started_at).num_milliseconds().max(0) as u64,
    );

    let manifest_key = format!("{run_dir}/{}", naming::MANIFEST_OBJECT);
    manifest::upload(&store, &manifest_key, &manifest)
        .await
        .context("上传 manifest 失败")?;

    info!(
        destination = %destination.name,
        objects = objects.len(),
        bytes = total_bytes,
        run = %run_dir,
        "对象上传完成，manifest 已写入"
    );

    // 清理失败只影响存储占用，不该把已经成功的备份判成失败
    if settings.retain_count > 0 {
        if let Err(err) = retention::apply(&store, &run_prefix, settings.retain_count).await {
            warn!("清理旧备份失败（本次备份已成功）：{err:#}");
        }
    } else if let Err(err) = retention::reap_orphans(&store, &run_prefix).await {
        // retain_count = 0 表示不删备份，但失败运行留下的孤儿目录仍要回收
        warn!("回收孤儿目录失败（本次备份已成功）：{err:#}");
    }

    Ok(RunSummary {
        objects: objects.len() + 1,
        total_bytes,
    })
}

async fn run_file(
    store: &OssStore,
    file: &[crate::config::FileCfg],
    settings: &BackupSettings,
    run_dir: &str,
) -> Result<Vec<ManifestObject>> {
    let key = format!("{run_dir}/{}", naming::FILE_OBJECT);
    let sources: Vec<tar_zstd::Source> = file
        .iter()
        .map(|spec| tar_zstd::Source {
            root: spec.local_path.clone(),
            exclude: spec.exclude.clone(),
        })
        .collect();
    let level = settings.zstd_level;
    let passphrase = settings.passphrase.clone();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    let stats = upload_stream(store.operator(), &key, part_size, move |sink| {
        tar_zstd::produce(sink, &sources, level, &passphrase)
    })
    .await
    .context("文件备份失败")?;

    info!(key = %key, bytes = stats.bytes, entries = stats.entries, "已上传文件归档");

    Ok(vec![ManifestObject {
        key,
        source: None,
        size: stats.bytes,
        sha256: stats.sha256,
        plaintext_bytes: stats.plaintext_bytes,
        entries: stats.entries,
    }])
}

async fn run_mysql(
    store: &OssStore,
    mysql: &MysqlCfg,
    settings: &BackupSettings,
    run_dir: &str,
    dump_path: &str,
    timeout: Duration,
) -> Result<Vec<ManifestObject>> {
    let grouped = db::group_databases(&mysql.database);
    let mut objects = Vec::new();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    for (database, tables) in grouped {
        let object = naming::db_object_name(DbKind::Mysql, &database);
        let key = format!("{run_dir}/{object}");
        let cfg = mysql.clone();
        let tables_owned = tables.clone();
        let db_name = database.clone();
        let dump = dump_path.to_string();
        let level = settings.zstd_level;
        let passphrase = settings.passphrase.clone();

        let stats = upload_stream(store.operator(), &key, part_size, move |sink| {
            db::mysql::produce(
                sink,
                &cfg,
                &db_name,
                &tables_owned,
                level,
                &passphrase,
                &dump,
                timeout,
            )
        })
        .await
        .with_context(|| format!("备份 MySQL 库 {database} 失败"))?;

        info!(key = %key, bytes = stats.bytes, "已上传 MySQL 导出");
        objects.push(ManifestObject {
            key,
            source: Some(database),
            size: stats.bytes,
            sha256: stats.sha256,
            plaintext_bytes: stats.plaintext_bytes,
            entries: stats.entries,
        });
    }

    Ok(objects)
}

async fn run_pgsql(
    store: &OssStore,
    pgsql: &PgsqlCfg,
    settings: &BackupSettings,
    run_dir: &str,
    dump_path: &str,
    timeout: Duration,
) -> Result<Vec<ManifestObject>> {
    let grouped = db::group_databases(&pgsql.database);
    let mut objects = Vec::new();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    for (database, tables) in grouped {
        let object = naming::db_object_name(DbKind::Pgsql, &database);
        let key = format!("{run_dir}/{object}");
        let cfg = pgsql.clone();
        let tables_owned = tables.clone();
        let db_name = database.clone();
        let dump = dump_path.to_string();
        let level = settings.zstd_level;
        let passphrase = settings.passphrase.clone();

        let stats = upload_stream(store.operator(), &key, part_size, move |sink| {
            db::pgsql::produce(
                sink,
                &cfg,
                &db_name,
                &tables_owned,
                level,
                &passphrase,
                &dump,
                timeout,
            )
        })
        .await
        .with_context(|| format!("备份 PgSQL 库 {database} 失败"))?;

        info!(key = %key, bytes = stats.bytes, "已上传 PgSQL 导出");
        objects.push(ManifestObject {
            key,
            source: Some(database),
            size: stats.bytes,
            sha256: stats.sha256,
            plaintext_bytes: stats.plaintext_bytes,
            entries: stats.entries,
        });
    }

    Ok(objects)
}

async fn run_sqlite(
    store: &OssStore,
    sqlite: &[std::path::PathBuf],
    settings: &BackupSettings,
    run_dir: &str,
    sqlite3_path: &str,
) -> Result<Vec<ManifestObject>> {
    let mut objects = Vec::new();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    for database in sqlite {
        let object = naming::sqlite_object_name(database);
        let key = format!("{run_dir}/{object}");
        let db_path = database.clone();
        let sqlite3 = sqlite3_path.to_string();
        let level = settings.zstd_level;
        let passphrase = settings.passphrase.clone();

        let stats = upload_stream(store.operator(), &key, part_size, move |sink| {
            db::sqlite::produce(sink, &db_path, level, &passphrase, &sqlite3)
        })
        .await
        .with_context(|| format!("备份 SQLite 文件失败：{}", database.display()))?;

        info!(key = %key, bytes = stats.bytes, "已上传 SQLite 快照");
        objects.push(ManifestObject {
            key,
            source: Some(database.display().to_string()),
            size: stats.bytes,
            sha256: stats.sha256,
            plaintext_bytes: stats.plaintext_bytes,
            entries: stats.entries,
        });
    }

    Ok(objects)
}

/// dry-run 阶段就检查本地输入：SQLite 文件是否存在、导出命令是否可执行。
/// 这些错了正式跑一样会失败，提前暴露比传到一半再报错好。
fn check_local_inputs(cfg: &Config, backup: &Backup) -> Result<()> {
    for db in &backup.sqlite {
        if !db.exists() {
            bail!("SQLite 数据库不存在：{}", db.display());
        }
    }
    if backup.mysql.is_some() {
        ensure_command(cfg.mysqldump_path())?;
    }
    if backup.pgsql.is_some() {
        ensure_command(cfg.pg_dump_path())?;
    }
    if !backup.sqlite.is_empty() {
        ensure_command(cfg.sqlite3_path())?;
    }
    Ok(())
}

/// 只跑 `--version` 验证可执行，不连接数据库
fn ensure_command(program: &str) -> Result<()> {
    let status = std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => bail!("{program} 无法正常执行（--version 返回 {status}）"),
        Err(err) => bail!("找不到可执行文件 {program}：{err}"),
    }
}

/// OSS 单对象最多 10000 个分片；超限会在上传中途被拒绝
fn ensure_part_limit(bytes: u64, part_size: usize) -> Result<u64> {
    let parts = bytes / part_size as u64 + 1;
    if parts > 10000 {
        bail!("预估约 {parts} 个分片，超过 OSS 的 10000 片上限，请调大 part_size_mb");
    }
    Ok(parts)
}

pub fn kinds_of(backup: &Backup) -> String {
    let mut kinds = Vec::new();
    if !backup.files.is_empty() {
        kinds.push("file");
    }
    if backup.mysql.is_some() {
        kinds.push("mysql");
    }
    if backup.pgsql.is_some() {
        kinds.push("pgsql");
    }
    if !backup.sqlite.is_empty() {
        kinds.push("sqlite");
    }
    kinds.join("+")
}

fn inputs_of(backup: &Backup) -> Vec<String> {
    let mut out = Vec::new();
    for spec in &backup.files {
        out.push(spec.local_path.display().to_string());
        for pattern in &spec.exclude {
            out.push(format!("exclude:{}:{pattern}", spec.local_path.display()));
        }
    }
    if let Some(mysql) = &backup.mysql {
        out.extend(mysql.database.iter().cloned());
    }
    if let Some(pgsql) = &backup.pgsql {
        out.extend(pgsql.database.iter().cloned());
    }
    out.extend(backup.sqlite.iter().map(|p| p.display().to_string()));
    out
}

fn planned_objects(backup: &Backup) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    if !backup.files.is_empty() {
        out.push((naming::FILE_OBJECT.to_string(), None));
    }
    if let Some(mysql) = &backup.mysql {
        out.extend(
            db::group_databases(&mysql.database)
                .into_keys()
                .map(|db| (naming::db_object_name(DbKind::Mysql, &db), Some(db))),
        );
    }
    if let Some(pgsql) = &backup.pgsql {
        out.extend(
            db::group_databases(&pgsql.database)
                .into_keys()
                .map(|db| (naming::db_object_name(DbKind::Pgsql, &db), Some(db))),
        );
    }
    out.extend(
        backup
            .sqlite
            .iter()
            .map(|p| (naming::sqlite_object_name(p), Some(p.display().to_string()))),
    );
    out
}

fn estimate_file(file: &[crate::config::FileCfg]) -> Result<(u64, u64)> {
    let mut count = 0u64;
    let mut bytes = 0u64;

    for spec in file {
        let root = &spec.local_path;
        if !root.exists() {
            bail!("备份路径不存在：{}", root.display());
        }
        let exclude = tar_zstd::exclude_set(&spec.exclude)?;
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry.with_context(|| format!("遍历目录失败：{}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if exclude.is_match(&rel) {
                continue;
            }
            count += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or_default();
        }
    }

    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backup, Destination, FileCfg, Global};
    use std::path::PathBuf;

    fn config_with_file() -> Config {
        Config {
            global: Global {
                age_passphrase: Some("pw".into()),
                ..Default::default()
            },
            destination: vec![Destination {
                name: "aliyun".into(),
                endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
                bucket: "b".into(),
                prefix: Some("backups".into()),
                access_key_id: "id".into(),
                access_key_secret: "sec".into(),
                retain_count: None,
                zstd_level: None,
                part_size_mb: None,
                timeout_secs: None,
            }],
            backup: vec![],
        }
    }

    #[test]
    fn planned_objects_for_sqlite_follow_paths() {
        let backup = Backup {
            destination: "aliyun".into(),
            retain_count: None,
            zstd_level: None,
            part_size_mb: None,
            timeout_secs: None,
            age_passphrase: None,
            age_passphrase_file: None,
            files: vec![],
            mysql: None,
            pgsql: None,
            sqlite: vec![PathBuf::from(r"D:\a\app.db"), PathBuf::from(r"D:\b\app.db")],
        };
        let planned = planned_objects(&backup);
        assert_eq!(planned.len(), 2);
        assert_ne!(planned[0].0, planned[1].0);
    }

    #[test]
    fn kinds_combine_file_and_sqlite() {
        let mut cfg = config_with_file();
        let backup = Backup {
            destination: "aliyun".into(),
            retain_count: None,
            zstd_level: None,
            part_size_mb: None,
            timeout_secs: None,
            age_passphrase: None,
            age_passphrase_file: None,
            files: vec![FileCfg {
                local_path: PathBuf::from(r"D:\docs"),
                exclude: vec!["*.tmp".into()],
            }],
            mysql: None,
            pgsql: None,
            sqlite: vec![PathBuf::from(r"D:\a\app.db")],
        };
        cfg.backup.push(backup);
        assert_eq!(kinds_of(&cfg.backup[0]), "file+sqlite");
        assert_eq!(planned_objects(&cfg.backup[0]).len(), 2);
    }

    #[test]
    fn inputs_carry_excludes_per_source() {
        let backup = Backup {
            destination: "aliyun".into(),
            retain_count: None,
            zstd_level: None,
            part_size_mb: None,
            timeout_secs: None,
            age_passphrase: None,
            age_passphrase_file: None,
            files: vec![FileCfg {
                local_path: PathBuf::from(r"D:\docs"),
                exclude: vec!["*.tmp".into()],
            }],
            mysql: None,
            pgsql: None,
            sqlite: vec![],
        };
        let inputs = inputs_of(&backup);
        assert_eq!(inputs[0], r"D:\docs");
        assert_eq!(inputs[1], r"exclude:D:\docs:*.tmp");
    }
}
