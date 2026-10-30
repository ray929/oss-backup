use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "oss-backup", about = "把本地文件与数据库备份到阿里云 OSS")]
pub struct Cli {
    /// 配置文件路径
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    /// 只校验配置与连通性，打印计划写入的对象，不实际上传
    #[arg(long)]
    pub dry_run: bool,

    /// 日志写入 oss-backup.log
    #[arg(long)]
    pub background: bool,
}
