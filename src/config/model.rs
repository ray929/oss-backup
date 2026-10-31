use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;

/// 敏感值的展示形态：保留前 4 与后 4 个字符，中间固定 4 个星号。
///
/// 日志、错误信息与 `Debug` 输出都走这里，避免 AK/SK 与口令意外落到磁盘或终端。
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.is_empty() {
        return "<empty>".to_string();
    }
    if chars.len() <= 8 {
        return "*".repeat(8);
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

fn masked_opt(value: &Option<String>) -> Option<String> {
    value.as_ref().map(|s| mask_secret(s))
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub global: Global,
    #[serde(default)]
    pub destination: Vec<Destination>,
    #[serde(default)]
    pub backup: Vec<Backup>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Global {
    pub log_level: Option<String>,
    pub retain_count: Option<usize>,
    pub zstd_level: Option<i32>,
    pub part_size_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub age_passphrase: Option<String>,
    pub age_passphrase_file: Option<PathBuf>,
    pub webhook_url: Option<String>,
    pub mysqldump_path: Option<String>,
    pub pg_dump_path: Option<String>,
    pub sqlite3_path: Option<String>,
}

impl fmt::Debug for Global {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Global")
            .field("log_level", &self.log_level)
            .field("retain_count", &self.retain_count)
            .field("zstd_level", &self.zstd_level)
            .field("part_size_mb", &self.part_size_mb)
            .field("timeout_secs", &self.timeout_secs)
            .field("age_passphrase", &masked_opt(&self.age_passphrase))
            .field("age_passphrase_file", &self.age_passphrase_file)
            .field("webhook_url", &masked_opt(&self.webhook_url))
            .field("mysqldump_path", &self.mysqldump_path)
            .field("pg_dump_path", &self.pg_dump_path)
            .field("sqlite3_path", &self.sqlite3_path)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub name: String,
    pub endpoint: String,
    pub bucket: String,
    pub prefix: Option<String>,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub retain_count: Option<usize>,
    pub zstd_level: Option<i32>,
    pub part_size_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
}

impl fmt::Debug for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Destination")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &mask_secret(&self.access_key_id))
            .field("access_key_secret", &mask_secret(&self.access_key_secret))
            .field("retain_count", &self.retain_count)
            .field("zstd_level", &self.zstd_level)
            .field("part_size_mb", &self.part_size_mb)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    /// 同一 destination 被多个 backup 引用时必填，并作为 OSS 上的一级路径段
    pub name: Option<String>,
    /// 省略时使用配置里第一个 destination
    pub destination: Option<String>,
    pub retain_count: Option<usize>,
    pub zstd_level: Option<i32>,
    pub part_size_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub age_passphrase: Option<String>,
    pub age_passphrase_file: Option<PathBuf>,
    /// 文件源列表，每个源带自己的排除规则
    #[serde(default)]
    pub files: Vec<FileCfg>,
    pub mysql: Option<MysqlCfg>,
    pub pgsql: Option<PgsqlCfg>,
    /// SQLite 数据库文件列表
    #[serde(default)]
    pub sqlite: Vec<PathBuf>,
}

impl fmt::Debug for Backup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backup")
            .field("name", &self.name)
            .field("destination", &self.destination)
            .field("retain_count", &self.retain_count)
            .field("zstd_level", &self.zstd_level)
            .field("part_size_mb", &self.part_size_mb)
            .field("timeout_secs", &self.timeout_secs)
            .field("age_passphrase", &masked_opt(&self.age_passphrase))
            .field("age_passphrase_file", &self.age_passphrase_file)
            .field("files", &self.files)
            .field("mysql", &self.mysql)
            .field("pgsql", &self.pgsql)
            .field("sqlite", &self.sqlite)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCfg {
    pub local_path: PathBuf,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MysqlCfg {
    pub host: String,
    #[serde(default = "default_mysql_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: Vec<String>,
    pub ssl_mode: Option<String>,
}

impl fmt::Debug for MysqlCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MysqlCfg")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &mask_secret(&self.password))
            .field("database", &self.database)
            .field("ssl_mode", &self.ssl_mode)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgsqlCfg {
    pub host: String,
    #[serde(default = "default_pgsql_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: Vec<String>,
    pub ssl_mode: Option<String>,
}

impl fmt::Debug for PgsqlCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgsqlCfg")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &mask_secret(&self.password))
            .field("database", &self.database)
            .field("ssl_mode", &self.ssl_mode)
            .finish()
    }
}

fn default_mysql_port() -> u16 {
    3306
}

fn default_pgsql_port() -> u16 {
    5432
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_long_secrets_keeping_edges() {
        assert_eq!(mask_secret("AKIDEXAMPLE1234"), "AKID****1234");
    }

    #[test]
    fn masks_short_secrets_entirely() {
        assert_eq!(mask_secret("short"), "********");
        assert_eq!(mask_secret(""), "<empty>");
    }

    #[test]
    fn debug_output_never_contains_secret() {
        let dest = Destination {
            name: "aliyun".into(),
            endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
            bucket: "b".into(),
            prefix: None,
            access_key_id: "LTAIabcdefghijklmn".into(),
            access_key_secret: "supersecretvalue".into(),
            retain_count: None,
            zstd_level: None,
            part_size_mb: None,
            timeout_secs: None,
        };
        let text = format!("{dest:?}");
        assert!(!text.contains("supersecretvalue"));
        assert!(!text.contains("LTAIabcdefghijklmn"));
        assert!(text.contains("aliyun"));
    }

    #[test]
    fn debug_output_masks_database_password() {
        let cfg = MysqlCfg {
            host: "127.0.0.1".into(),
            port: 3306,
            username: "backup".into(),
            password: "db-password".into(),
            database: vec!["blog".into()],
            ssl_mode: None,
        };
        assert!(!format!("{cfg:?}").contains("db-password"));
    }
}
