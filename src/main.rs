mod archive;
mod backup;
mod cli;
mod config;
mod db;
mod lock;
mod logger;
mod manifest;
mod naming;
mod notify;
mod retention;
mod storage;

use clap::Parser;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    let code = run().await;
    std::process::exit(code);
}

async fn run() -> i32 {
    let started = std::time::Instant::now();
    let args = cli::Cli::parse();

    let cfg = match config::Config::load(&args.config) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err:#}");
            return 2;
        }
    };

    if let Err(err) = logger::init(cfg.global.log_level.as_deref(), args.background) {
        eprintln!("初始化日志失败：{err:#}");
        return 2;
    }

    if let Err(err) = cfg.validate() {
        error!("配置校验失败：{err:#}");
        return 2;
    }
    if let Err(err) = config::check_permissions(&args.config) {
        error!("{err:#}");
        return 2;
    }

    let _lock = if args.dry_run {
        None
    } else {
        match lock::acquire(&args.config) {
            Ok(lock) => Some(lock),
            Err(err) => {
                error!("{err:#}");
                return 3;
            }
        }
    };

    let mut failures = Vec::new();
    for backup in &cfg.backup {
        match backup::run_backup(&cfg, backup, args.dry_run).await {
            Ok(summary) => info!(
                destination = %backup.destination,
                objects = summary.objects,
                bytes = summary.total_bytes,
                "备份项完成"
            ),
            Err(err) => {
                error!(destination = %backup.destination, "备份失败：{err:#}");
                failures.push(notify::NotifyFailure {
                    name: backup.destination.clone(),
                    error: format!("{err:#}"),
                });
            }
        }
    }

    let payload = notify::NotifyPayload {
        status: if failures.is_empty() {
            "success"
        } else {
            "partial_failure"
        },
        backup_count: cfg.backup.len(),
        failures,
        duration_ms: started.elapsed().as_millis() as u64,
    };

    if !args.dry_run {
        if let Some(url) = cfg.global.webhook_url.as_deref() {
            notify::send(url, &payload).await;
        }
    }

    if payload.failures.is_empty() {
        info!("全部备份项完成");
        0
    } else {
        error!("{} 个备份项失败", payload.failures.len());
        1
    }
}
