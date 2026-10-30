use serde::Deserialize;
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
    pub destination: String,
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

#[derive(Debug, Clone, Deserialize)]
pub struct FileCfg {
    pub local_path: PathBuf,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MysqlCfg {
    pub host: String,
    #[serde(default = "default_mysql_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
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
    pub database: Vec<String>,
    pub ssl_mode: Option<String>,
}

fn default_mysql_port() -> u16 {
    3306
}

fn default_pgsql_port() -> u16 {
    5432
}
