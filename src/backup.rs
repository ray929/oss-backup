use crate::archive::tar_zstd;
use crate::config::{Backup, BackupSettings, Config, MysqlCfg, PgsqlCfg, SqliteCfg};
use crate::db;
use crate::manifest::{self, Manifest, ManifestObject};
use crate::naming::{self, DbKind};
use crate::retention;
use crate::storage::{upload_stream, OssStore};
use anyhow::{bail, Context, Result};
use chrono::Local;
use std::time::Duration;
use tracing::info;
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
    let backup_prefix = naming::backup_prefix(prefix, &backup.name);
    let started_at = Local::now();
    let timestamp = started_at.format(naming::TIMESTAMP_FORMAT).to_string();
    let run_dir = naming::run_dir(prefix, &backup.name, &timestamp);
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    if dry_run {
        store
            .probe(&backup_prefix)
            .await
            .context("dry-run 连通性检查失败")?;
        for (object, source) in planned_objects(backup) {
            info!(
                key = %format!("{run_dir}/{object}"),
                source = source.as_deref().unwrap_or("-"),
                "计划写入"
            );
        }
        if let Some(file) = &backup.file {
            let (count, bytes) = estimate_file(file)?;
            let parts = bytes as usize / part_size + 1;
            if parts > 10000 {
                bail!("预估约 {parts} 个分片，超过 OSS 的 10000 片上限，请调大 part_size_mb");
            }
            info!(files = count, bytes, parts, "待备份文件预估");
        }
        return Ok(RunSummary {
            objects: planned_objects(backup).len(),
            total_bytes: 0,
        });
    }

    let mut objects = Vec::new();
    if let Some(file) = &backup.file {
        objects.extend(run_file(&store, backup, file, &settings, &run_dir).await?);
    }
    if let Some(mysql) = &backup.mysql {
        let dump_path = cfg.mysqldump_path().to_string();
        objects.extend(run_mysql(&store, backup, mysql, &settings, &run_dir, &dump_path).await?);
    }
    if let Some(pgsql) = &backup.pgsql {
        let dump_path = cfg.pg_dump_path().to_string();
        objects.extend(run_pgsql(&store, backup, pgsql, &settings, &run_dir, &dump_path).await?);
    }
    if let Some(sqlite) = &backup.sqlite {
        let sqlite3_path = cfg.sqlite3_path().to_string();
        objects
            .extend(run_sqlite(&store, backup, sqlite, &settings, &run_dir, &sqlite3_path).await?);
    }
    if objects.is_empty() {
        bail!("backup '{}' 没有可执行的备份类型", backup.name);
    }

    let finished_at = Local::now();
    let total_bytes: u64 = objects.iter().map(|o| o.size).sum();
    let kind = kinds_of(backup);
    let manifest = Manifest::new(
        &backup.name,
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
        backup = %backup.name,
        objects = objects.len(),
        bytes = total_bytes,
        run = %run_dir,
        "对象上传完成，manifest 已写入"
    );

    if settings.retain_count > 0 {
        retention::apply(&store, &backup_prefix, settings.retain_count).await?;
    }

    Ok(RunSummary {
        objects: objects.len() + 1,
        total_bytes,
    })
}

async fn run_file(
    store: &OssStore,
    backup: &Backup,
    file: &crate::config::FileCfg,
    settings: &BackupSettings,
    run_dir: &str,
) -> Result<Vec<ManifestObject>> {
    let key = format!("{run_dir}/{}", naming::FILE_OBJECT);
    let paths = file.local_path.clone();
    let exclude = file.exclude.clone();
    let level = settings.zstd_level;
    let passphrase = settings.passphrase.clone();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    let stats = upload_stream(store.operator(), &key, part_size, move |sink| {
        tar_zstd::produce(sink, &paths, &exclude, level, &passphrase)
    })
    .await
    .with_context(|| format!("文件备份失败：{}", backup.name))?;

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
    backup: &Backup,
    mysql: &MysqlCfg,
    settings: &BackupSettings,
    run_dir: &str,
    dump_path: &str,
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

    let _ = backup;
    Ok(objects)
}

async fn run_pgsql(
    store: &OssStore,
    backup: &Backup,
    pgsql: &PgsqlCfg,
    settings: &BackupSettings,
    run_dir: &str,
    dump_path: &str,
) -> Result<Vec<ManifestObject>> {
    let grouped = db::group_databases(&pgsql.database);
    let mut objects = Vec::new();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    for database in grouped.keys() {
        let object = naming::db_object_name(DbKind::Pgsql, database);
        let key = format!("{run_dir}/{object}");
        let cfg = pgsql.clone();
        let db_name = database.clone();
        let dump = dump_path.to_string();
        let level = settings.zstd_level;
        let passphrase = settings.passphrase.clone();

        let stats = upload_stream(store.operator(), &key, part_size, move |sink| {
            db::pgsql::produce(sink, &cfg, &db_name, level, &passphrase, &dump)
        })
        .await
        .with_context(|| format!("备份 PgSQL 库 {database} 失败"))?;

        info!(key = %key, bytes = stats.bytes, "已上传 PgSQL 导出");
        objects.push(ManifestObject {
            key,
            source: Some(database.clone()),
            size: stats.bytes,
            sha256: stats.sha256,
            plaintext_bytes: stats.plaintext_bytes,
            entries: stats.entries,
        });
    }

    let _ = backup;
    Ok(objects)
}

async fn run_sqlite(
    store: &OssStore,
    backup: &Backup,
    sqlite: &SqliteCfg,
    settings: &BackupSettings,
    run_dir: &str,
    sqlite3_path: &str,
) -> Result<Vec<ManifestObject>> {
    let mut objects = Vec::new();
    let part_size = settings.part_size_mb as usize * 1024 * 1024;

    for database in &sqlite.database {
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

    let _ = backup;
    Ok(objects)
}

fn kinds_of(backup: &Backup) -> String {
    let mut kinds = Vec::new();
    if backup.file.is_some() {
        kinds.push("file");
    }
    if backup.mysql.is_some() {
        kinds.push("mysql");
    }
    if backup.pgsql.is_some() {
        kinds.push("pgsql");
    }
    if backup.sqlite.is_some() {
        kinds.push("sqlite");
    }
    kinds.join("+")
}

fn inputs_of(backup: &Backup) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(file) = &backup.file {
        out.extend(file.local_path.iter().map(|p| p.display().to_string()));
        out.extend(file.exclude.iter().map(|e| format!("exclude:{e}")));
    }
    if let Some(mysql) = &backup.mysql {
        out.extend(mysql.database.iter().cloned());
    }
    if let Some(pgsql) = &backup.pgsql {
        out.extend(pgsql.database.iter().cloned());
    }
    if let Some(sqlite) = &backup.sqlite {
        out.extend(sqlite.database.iter().map(|p| p.display().to_string()));
    }
    out
}

fn planned_objects(backup: &Backup) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    if backup.file.is_some() {
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
    if let Some(sqlite) = &backup.sqlite {
        out.extend(
            sqlite
                .database
                .iter()
                .map(|p| (naming::sqlite_object_name(p), Some(p.display().to_string()))),
        );
    }
    out
}

fn estimate_file(file: &crate::config::FileCfg) -> Result<(u64, u64)> {
    let exclude = tar_zstd::exclude_set(&file.exclude)?;
    let mut count = 0u64;
    let mut bytes = 0u64;

    for root in &file.local_path {
        if !root.exists() {
            bail!("备份路径不存在：{}", root.display());
        }
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
            name: "cache".into(),
            destination: "aliyun".into(),
            retain_count: None,
            zstd_level: None,
            part_size_mb: None,
            timeout_secs: None,
            age_passphrase: None,
            age_passphrase_file: None,
            file: None,
            mysql: None,
            pgsql: None,
            sqlite: Some(SqliteCfg {
                database: vec![PathBuf::from(r"D:\a\app.db"), PathBuf::from(r"D:\b\app.db")],
            }),
        };
        let planned = planned_objects(&backup);
        assert_eq!(planned.len(), 2);
        assert_ne!(planned[0].0, planned[1].0);
    }

    #[test]
    fn kind_is_detected() {
        let mut cfg = config_with_file();
        let backup = Backup {
            name: "docs".into(),
            destination: "aliyun".into(),
            retain_count: None,
            zstd_level: None,
            part_size_mb: None,
            timeout_secs: None,
            age_passphrase: None,
            age_passphrase_file: None,
            file: Some(FileCfg {
                local_path: vec![PathBuf::from(r"D:\docs")],
                exclude: vec![],
            }),
            mysql: None,
            pgsql: None,
            sqlite: None,
        };
        cfg.backup.push(backup);
        assert_eq!(kinds_of(&cfg.backup[0]), "file");
        assert_eq!(planned_objects(&cfg.backup[0]).len(), 1);
    }
}
