use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;

pub mod model;

pub use model::*;

pub const DEFAULT_RETAIN_COUNT: usize = 0;
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const DEFAULT_PART_SIZE_MB: u64 = 16;
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// 单个 backup 项解析后的有效设置（已完成三层继承）
#[derive(Debug, Clone)]
pub struct BackupSettings {
    pub retain_count: usize,
    pub zstd_level: i32,
    pub part_size_mb: u64,
    pub timeout_secs: u64,
    pub passphrase: String,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            bail!(
                "配置文件不存在：{}\n请参考 config.example.toml 创建",
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("解析配置文件失败：{}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.destination.is_empty() {
            bail!("配置缺少 [[destination]]");
        }
        if self.backup.is_empty() {
            bail!("配置缺少 [[backup]]");
        }

        let mut dest_names = HashSet::new();
        for (idx, dest) in self.destination.iter().enumerate() {
            if !crate::naming::is_valid_name(&dest.name) {
                bail!(
                    "destination[{idx}] 名称 '{}' 非法（只允许字母、数字、点、下划线、短横线）",
                    dest.name
                );
            }
            if !dest_names.insert(dest.name.as_str()) {
                bail!("destination 名称重复：{}", dest.name);
            }
            for (field, value) in [
                ("endpoint", &dest.endpoint),
                ("bucket", &dest.bucket),
                ("access_key_id", &dest.access_key_id),
                ("access_key_secret", &dest.access_key_secret),
            ] {
                if value.trim().is_empty() {
                    bail!("destination[{}].{} 不能为空", idx, field);
                }
            }
            if let Some(prefix) = &dest.prefix {
                if prefix.starts_with('/') {
                    bail!("destination[{}].prefix 不能以 '/' 开头", idx);
                }
            }
        }

        let mut backup_destinations = HashSet::new();
        for (idx, backup) in self.backup.iter().enumerate() {
            if !self
                .destination
                .iter()
                .any(|d| d.name == backup.destination)
            {
                bail!(
                    "backup[{idx}] 引用了不存在的 destination '{}'",
                    backup.destination
                );
            }
            if !backup_destinations.insert(backup.destination.as_str()) {
                bail!(
                    "destination '{}' 被多个 [[backup]] 引用；运行目录按 destination 分段，一个 destination 只能对应一个 [[backup]]",
                    backup.destination
                );
            }

            let kinds = [
                !backup.file.is_empty(),
                backup.mysql.is_some(),
                backup.pgsql.is_some(),
                !backup.sqlite.is_empty(),
            ];
            let count = kinds.iter().filter(|v| **v).count();
            if count == 0 {
                bail!("backup[{idx}] 必须包含 file / mysql / pgsql / sqlite 至少一种");
            }

            for (spec_idx, file) in backup.file.iter().enumerate() {
                if file.local_path.as_os_str().is_empty() {
                    bail!("backup[{idx}].file[{spec_idx}].local_path 不能为空");
                }
            }
            if let Some(mysql) = &backup.mysql {
                if mysql.database.is_empty() {
                    bail!("backup[{idx}].mysql.database 不能为空");
                }
            }
            if let Some(pgsql) = &backup.pgsql {
                if pgsql.database.is_empty() {
                    bail!("backup[{idx}].pgsql.database 不能为空");
                }
            }

            self.settings(backup)
                .with_context(|| format!("backup[{idx}] 设置解析失败"))?;
        }

        Ok(())
    }

    pub fn destination(&self, backup: &Backup) -> Result<&Destination> {
        self.destination
            .iter()
            .find(|d| d.name == backup.destination)
            .with_context(|| format!("找不到 destination '{}'", backup.destination))
    }

    pub fn settings(&self, backup: &Backup) -> Result<BackupSettings> {
        let dest = self.destination(backup)?;

        let zstd_level = backup
            .zstd_level
            .or(dest.zstd_level)
            .or(self.global.zstd_level)
            .unwrap_or(DEFAULT_ZSTD_LEVEL);
        if !(-7..=22).contains(&zstd_level) {
            bail!("zstd_level 必须在 -7..=22 之间，当前为 {zstd_level}");
        }

        let part_size_mb = backup
            .part_size_mb
            .or(dest.part_size_mb)
            .or(self.global.part_size_mb)
            .unwrap_or(DEFAULT_PART_SIZE_MB);
        if part_size_mb == 0 {
            bail!("part_size_mb 必须大于 0");
        }

        let timeout_secs = backup
            .timeout_secs
            .or(dest.timeout_secs)
            .or(self.global.timeout_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        if timeout_secs == 0 {
            bail!("timeout_secs 必须大于 0");
        }

        let retain_count = backup
            .retain_count
            .or(dest.retain_count)
            .or(self.global.retain_count)
            .unwrap_or(DEFAULT_RETAIN_COUNT);

        let passphrase = self.resolve_passphrase(backup)?;

        Ok(BackupSettings {
            retain_count,
            zstd_level,
            part_size_mb,
            timeout_secs,
            passphrase,
        })
    }

    pub fn mysqldump_path(&self) -> &str {
        self.global.mysqldump_path.as_deref().unwrap_or("mysqldump")
    }

    pub fn pg_dump_path(&self) -> &str {
        self.global.pg_dump_path.as_deref().unwrap_or("pg_dump")
    }

    pub fn sqlite3_path(&self) -> &str {
        self.global.sqlite3_path.as_deref().unwrap_or("sqlite3")
    }

    pub fn resolve_passphrase(&self, backup: &Backup) -> Result<String> {
        if let Some(path) = backup
            .age_passphrase_file
            .as_ref()
            .or(self.global.age_passphrase_file.as_ref())
        {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("读取 age 口令文件失败：{}", path.display()))?;
            let passphrase = text.trim_end_matches(['\r', '\n']).to_string();
            if passphrase.is_empty() {
                bail!("age 口令文件为空：{}", path.display());
            }
            return Ok(passphrase);
        }

        match backup
            .age_passphrase
            .as_ref()
            .or(self.global.age_passphrase.as_ref())
        {
            Some(p) if !p.is_empty() => {
                tracing::warn!("使用配置文件内的明文 age 口令，建议改用 age_passphrase_file");
                Ok(p.clone())
            }
            _ => bail!("未配置 age 口令（age_passphrase_file 或 age_passphrase）"),
        }
    }
}

/// 配置文件权限校验。
///
/// Linux 下强制 600；Windows 下用 `icacls` 探测过宽的 ACL 并给出告警。
pub fn check_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("读取配置文件属性失败：{}", path.display()))?
            .permissions()
            .mode();
        if !is_private_mode(mode) {
            bail!(
                "配置文件权限过宽（{:o}），其中包含 AK/SK 与口令，请执行：chmod 600 {}",
                mode & 0o777,
                path.display()
            );
        }
    }

    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("icacls").arg(path).output() {
            let text = String::from_utf8_lossy(&output.stdout);
            if text.contains("Everyone") || text.contains("BUILTIN\\Users") {
                tracing::warn!(
                    "配置文件可被其他用户读取，其中包含 AK/SK 与口令，建议执行：icacls \"{}\" /inheritance:r /grant:r \"%USERNAME%\":F",
                    path.display()
                );
            }
        }
    }

    Ok(())
}

/// 纯函数：Unix 权限是否只允许属主访问
#[cfg_attr(not(unix), allow(dead_code))]
pub fn is_private_mode(mode: u32) -> bool {
    mode & 0o077 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn minimal() -> Config {
        let text = r#"
[global]
age_passphrase = "pw"

[[destination]]
name = "aliyun"
endpoint = "oss-cn-hangzhou.aliyuncs.com"
bucket = "b"
access_key_id = "id"
access_key_secret = "sec"

[[backup]]
destination = "aliyun"

[[backup.file]]
local_path = "D:/docs"
exclude = ["node_modules", "*.tmp"]
"#;
        toml::from_str(text).expect("config should parse")
    }

    #[test]
    fn parses_file_sources_with_own_excludes() {
        let cfg = minimal();
        assert_eq!(cfg.backup[0].file.len(), 1);
        assert_eq!(cfg.backup[0].file[0].local_path, PathBuf::from("D:/docs"));
        assert_eq!(cfg.backup[0].file[0].exclude.len(), 2);
    }

    #[test]
    fn parses_multiple_file_sources() {
        let cfg: Config = toml::from_str(
            r#"
[global]
age_passphrase = "pw"

[[destination]]
name = "aliyun"
endpoint = "e"
bucket = "b"
access_key_id = "id"
access_key_secret = "sec"

[[backup]]
destination = "aliyun"

[[backup.file]]
local_path = "D:/a"
exclude = ["*.tmp"]

[[backup.file]]
local_path = "D:/b"
"#,
        )
        .unwrap();
        assert_eq!(cfg.backup[0].file.len(), 2);
        assert!(cfg.backup[0].file[1].exclude.is_empty());
    }

    #[test]
    fn inheritance_prefers_backup_then_destination_then_global() {
        let mut cfg = minimal();
        cfg.global.zstd_level = Some(3);
        cfg.global.retain_count = Some(5);
        cfg.destination[0].zstd_level = Some(6);
        cfg.destination[0].retain_count = Some(4);
        cfg.backup[0].retain_count = Some(2);

        let settings = cfg.settings(&cfg.backup[0]).unwrap();
        assert_eq!(settings.zstd_level, 6);
        assert_eq!(settings.retain_count, 2);
        assert_eq!(settings.part_size_mb, DEFAULT_PART_SIZE_MB);
    }

    #[test]
    fn rejects_unknown_destination() {
        let mut cfg = minimal();
        cfg.backup[0].destination = "nope".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn allows_multiple_payloads() {
        let mut cfg = minimal();
        cfg.backup[0].sqlite = vec![PathBuf::from("a.db")];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_no_payload() {
        let mut cfg = minimal();
        cfg.backup[0].file.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_destination() {
        let mut cfg = minimal();
        let mut second = cfg.backup[0].clone();
        second.file.clear();
        second.sqlite = vec![PathBuf::from("a.db")];
        cfg.backup.push(second);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_zstd_level() {
        let mut cfg = minimal();
        cfg.global.zstd_level = Some(30);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn missing_passphrase_is_error() {
        let mut cfg = minimal();
        cfg.global.age_passphrase = None;
        assert!(cfg.resolve_passphrase(&cfg.backup[0]).is_err());
    }

    #[test]
    fn private_mode_check() {
        assert!(is_private_mode(0o600));
        assert!(!is_private_mode(0o644));
        assert!(!is_private_mode(0o640));
        assert!(is_private_mode(0o400));
    }

    #[test]
    fn passphrase_file_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pw.txt");
        std::fs::write(&p, "from-file\n").unwrap();
        let mut cfg = minimal();
        cfg.global.age_passphrase = Some("literal".into());
        cfg.global.age_passphrase_file = Some(p);
        assert_eq!(cfg.resolve_passphrase(&cfg.backup[0]).unwrap(), "from-file");
    }
}
