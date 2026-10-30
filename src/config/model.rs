use serde::{Deserialize, Deserializer};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub global: Global,
    #[serde(default)]
    pub destination: Vec<Destination>,
    #[serde(default)]
    pub backup: Vec<Backup>,
}

#[derive(Debug, Clone, Deserialize, Default)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct Destination {
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub internal: bool,
    pub bucket: String,
    pub prefix: Option<String>,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub retain_count: Option<usize>,
    pub zstd_level: Option<i32>,
    pub part_size_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Backup {
    pub name: String,
    pub destination: String,
    pub retain_count: Option<usize>,
    pub zstd_level: Option<i32>,
    pub part_size_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub age_passphrase: Option<String>,
    pub age_passphrase_file: Option<PathBuf>,
    pub file: Option<FileCfg>,
    pub mysql: Option<MysqlCfg>,
    pub pgsql: Option<PgsqlCfg>,
    pub sqlite: Option<SqliteCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileCfg {
    #[serde(deserialize_with = "one_or_many")]
    pub local_path: Vec<PathBuf>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MysqlCfg {
    pub host: String,
    #[serde(default = "default_mysql_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(deserialize_with = "one_or_many")]
    pub database: Vec<String>,
    pub ssl_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PgsqlCfg {
    pub host: String,
    #[serde(default = "default_pgsql_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(deserialize_with = "one_or_many")]
    pub database: Vec<String>,
    pub ssl_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SqliteCfg {
    #[serde(deserialize_with = "one_or_many")]
    pub database: Vec<PathBuf>,
}

fn default_mysql_port() -> u16 {
    3306
}

fn default_pgsql_port() -> u16 {
    5432
}

/// 允许配置写单个值或数组，统一解析为 `Vec<T>`。
fn one_or_many<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }

    Ok(match OneOrMany::<T>::deserialize(deserializer)? {
        OneOrMany::One(v) => vec![v],
        OneOrMany::Many(v) => v,
    })
}
