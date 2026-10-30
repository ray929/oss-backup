use crate::naming;
use crate::storage::OssStore;
use anyhow::{Context, Result};
use chrono::{Duration, Local, NaiveDateTime};
use std::collections::BTreeMap;
use tracing::{debug, info, warn};

/// 没有 manifest 的运行目录，超过这个时长才回收
const ORPHAN_GRACE_HOURS: i64 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub keys: Vec<String>,
    pub has_manifest: bool,
}

/// 把 `{prefix}/{run}/...` 的 key 列表按运行目录分组
pub fn group_runs(keys: &[String], prefix: &str) -> BTreeMap<String, RunInfo> {
    let mut runs: BTreeMap<String, RunInfo> = BTreeMap::new();
    for key in keys {
        let Some(rest) = key.strip_prefix(prefix) else {
            continue;
        };
        let Some((run, file)) = rest.split_once('/') else {
            continue;
        };
        if run.is_empty() {
            continue;
        }
        let entry = runs.entry(run.to_string()).or_insert(RunInfo {
            keys: Vec::new(),
            has_manifest: false,
        });
        if file == naming::MANIFEST_OBJECT {
            entry.has_manifest = true;
        }
        entry.keys.push(key.clone());
    }
    runs
}

/// 决定要删除哪些运行目录：保留最新 `retain` 个完整运行，并回收超期孤儿
pub fn select_deletions(
    runs: &BTreeMap<String, RunInfo>,
    retain: usize,
    now: NaiveDateTime,
) -> Vec<String> {
    let mut deletions = Vec::new();

    let mut complete: Vec<&String> = runs
        .iter()
        .filter(|(_, info)| info.has_manifest)
        .map(|(name, _)| name)
        .collect();
    // 目录名是 `%Y%m%d_%H%M%S`，字典序倒序即时间倒序
    complete.sort_by(|a, b| b.cmp(a));
    for name in complete.into_iter().skip(retain) {
        deletions.push(name.clone());
    }

    for (name, info) in runs {
        if info.has_manifest {
            continue;
        }
        match parse_timestamp(name) {
            Some(ts) if now - ts > Duration::hours(ORPHAN_GRACE_HOURS) => {
                deletions.push(name.clone());
            }
            Some(_) => {}
            None => warn!(run = %name, "运行目录名无法解析为时间戳，跳过清理"),
        }
    }

    deletions
}

pub async fn apply(store: &OssStore, backup_prefix: &str, retain: usize) -> Result<()> {
    if retain == 0 {
        debug!("retain_count 为 0，跳过保留策略");
        return Ok(());
    }

    let keys = store
        .list(backup_prefix)
        .await
        .with_context(|| format!("列举备份前缀失败：{backup_prefix}"))?;
    let runs = group_runs(&keys, backup_prefix);
    let deletions = select_deletions(&runs, retain, Local::now().naive_local());

    let complete_count = runs.values().filter(|r| r.has_manifest).count();
    if deletions.is_empty() {
        debug!(runs = complete_count, retain, "无需清理");
        return Ok(());
    }

    info!(
        runs = complete_count,
        retain,
        delete_dirs = deletions.len(),
        "清理旧备份"
    );

    for name in &deletions {
        let Some(info) = runs.get(name) else { continue };
        for key in &info.keys {
            if let Err(err) = store.delete(key).await {
                warn!(key = %key, "删除失败：{err}");
            }
        }
        info!(run = %name, objects = info.keys.len(), "已删除旧运行目录");
    }

    Ok(())
}

fn parse_timestamp(name: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(name, naming::TIMESTAMP_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(runs: &[(&str, bool)]) -> Vec<String> {
        let mut out = Vec::new();
        for (name, has_manifest) in runs {
            out.push(format!("backups/docs/{name}/files.tar.zst.age"));
            if *has_manifest {
                out.push(format!("backups/docs/{name}/manifest.json"));
            }
        }
        out
    }

    #[test]
    fn groups_by_run_directory() {
        let list = keys(&[("20260101_000000", true), ("20260102_000000", false)]);
        let runs = group_runs(&list, "backups/docs/");
        assert_eq!(runs.len(), 2);
        assert!(runs["20260101_000000"].has_manifest);
        assert!(!runs["20260102_000000"].has_manifest);
        assert_eq!(runs["20260101_000000"].keys.len(), 2);
    }

    #[test]
    fn keeps_newest_complete_runs() {
        let list = keys(&[
            ("20260101_000000", true),
            ("20260102_000000", true),
            ("20260103_000000", true),
        ]);
        let runs = group_runs(&list, "backups/docs/");
        let now = NaiveDateTime::parse_from_str("20260104_000000", "%Y%m%d_%H%M%S").unwrap();
        let deletions = select_deletions(&runs, 2, now);
        assert_eq!(deletions, vec!["20260101_000000".to_string()]);
    }

    #[test]
    fn deletes_stale_orphans_only() {
        let list = keys(&[("20260101_000000", true), ("20260102_000000", false)]);
        let runs = group_runs(&list, "backups/docs/");
        let now = NaiveDateTime::parse_from_str("20260105_000000", "%Y%m%d_%H%M%S").unwrap();
        let mut deletions = select_deletions(&runs, 5, now);
        deletions.sort();
        assert_eq!(deletions, vec!["20260102_000000".to_string()]);
    }

    #[test]
    fn keeps_fresh_orphans() {
        let list = keys(&[("20260104_230000", false)]);
        let runs = group_runs(&list, "backups/docs/");
        let now = NaiveDateTime::parse_from_str("20260105_000000", "%Y%m%d_%H%M%S").unwrap();
        assert!(select_deletions(&runs, 5, now).is_empty());
    }

    #[test]
    fn ignores_keys_outside_prefix() {
        let list = vec![
            "backups/other/20260101_000000/x.zst.age".to_string(),
            "backups/docs/20260101_000000/files.tar.zst.age".to_string(),
        ];
        let runs = group_runs(&list, "backups/docs/");
        assert_eq!(runs.len(), 1);
    }
}
