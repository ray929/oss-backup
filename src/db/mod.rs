use crate::archive::{age, ChunkSink, ProduceStats};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub mod mysql;
pub mod pgsql;
pub mod sqlite;

/// `"db.table"` / `"db.*"` / `"db"` 解析为 (库名, 可选表名)
pub fn parse_db_table(entry: &str) -> (&str, Option<&str>) {
    match entry.find('.') {
        Some(pos) => {
            let db = &entry[..pos];
            let table = &entry[pos + 1..];
            if table == "*" {
                (db, None)
            } else {
                (db, Some(table))
            }
        }
        None => (entry, None),
    }
}

/// 把配置项按库分组，保留每个库的表清单
pub fn group_databases(entries: &[String]) -> BTreeMap<String, Vec<Option<String>>> {
    let mut grouped: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    for entry in entries {
        let (db, table) = parse_db_table(entry);
        grouped
            .entry(db.to_string())
            .or_default()
            .push(table.map(|t| t.to_string()));
    }
    grouped
}

/// 启动外部导出命令，把 stdout 依次送进 zstd → age → sink。
///
/// 先确认子进程成功退出，再收尾压缩/加密，避免把失败的半截 dump 当成完整对象提交。
///
/// `timeout` 覆盖整个导出过程：子进程在锁等待或网络中断时可能永远不返回，而同步
/// 阻塞的读写链路自身没有任何超时点，只能靠旁边的看门狗到点 kill。
pub(crate) fn stream_command(
    mut cmd: Command,
    sink: ChunkSink,
    level: i32,
    passphrase: &str,
    timeout: Duration,
) -> Result<ProduceStats> {
    let program = cmd.get_program().to_string_lossy().to_string();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("启动命令失败：{program}"))?;

    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        })
    });

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("无法读取 {program} 的输出"))?;

    let child = Arc::new(Mutex::new(child));
    let timed_out = Arc::new(AtomicBool::new(false));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let watchdog = {
        let child = Arc::clone(&child);
        let timed_out = Arc::clone(&timed_out);
        std::thread::spawn(move || {
            if stop_rx.recv_timeout(timeout).is_err() {
                timed_out.store(true, Ordering::SeqCst);
                if let Ok(mut handle) = child.lock() {
                    let _ = handle.kill();
                }
            }
        })
    };

    let pump_result = pump(&mut stdout, sink, level, passphrase);
    drop(stdout);

    let _ = stop_tx.send(());
    let _ = watchdog.join();

    if timed_out.load(Ordering::SeqCst) {
        kill_and_reap(&child);
        bail!(
            "{program} 超过 {} 秒未结束，已终止（检查数据库是否锁等待或网络不通）",
            timeout.as_secs()
        );
    }

    if let Err(err) = pump_result {
        // 读写链路已断，子进程继续跑没有意义，还会攥着表锁
        kill_and_reap(&child);
        return Err(err);
    }

    let status = match child.lock() {
        Ok(mut handle) => handle
            .wait()
            .with_context(|| format!("等待 {program} 退出失败"))?,
        Err(_) => bail!("{program} 的子进程句柄锁已中毒"),
    };
    let stderr_text = stderr_handle
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();

    if !status.success() {
        bail!(
            "{program} 执行失败（{status}）：{}",
            tail_lines(&stderr_text, 20)
        );
    }

    Ok(ProduceStats {
        entries: 1,
        plaintext_bytes: None,
    })
}

/// 把导出流压进 zstd → age → sink，并完成各级收尾
fn pump<R: Read>(mut reader: R, sink: ChunkSink, level: i32, passphrase: &str) -> Result<()> {
    let age_writer = age::encrypt(sink, passphrase)?;
    let mut encoder = zstd::Encoder::new(age_writer, level).context("初始化 zstd 压缩器失败")?;
    std::io::copy(&mut reader, &mut encoder).context("读取导出流失败")?;
    let age_writer = encoder.finish().context("结束 zstd 压缩失败")?;
    let sink = age_writer.finish().context("结束 age 加密失败")?;
    sink.finish().context("刷新上传缓冲失败")?;
    Ok(())
}

fn kill_and_reap(child: &Arc<Mutex<Child>>) {
    if let Ok(mut handle) = child.lock() {
        let _ = handle.kill();
        let _ = handle.wait();
    }
}

pub(crate) fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_db_entries() {
        assert_eq!(parse_db_table("blog.posts"), ("blog", Some("posts")));
        assert_eq!(parse_db_table("blog.*"), ("blog", None));
        assert_eq!(parse_db_table("blog"), ("blog", None));
    }

    #[test]
    fn groups_tables_by_database() {
        let entries = vec![
            "blog.posts".to_string(),
            "blog.comments".to_string(),
            "shop".to_string(),
        ];
        let grouped = group_databases(&entries);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped["blog"].len(), 2);
        assert_eq!(grouped["shop"], vec![None]);
    }

    #[test]
    fn tail_lines_keeps_last_lines() {
        let text = (1..=30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_lines(&text, 5);
        assert_eq!(tail.lines().count(), 5);
        assert!(tail.ends_with("line30"));
    }

    /// 用 `sleep` 模拟挂死的导出进程；只在 unix 上跑（CI 是 ubuntu）
    #[cfg(unix)]
    #[test]
    fn stream_command_kills_hung_process() {
        use crate::archive::ChunkSink;
        use std::time::Instant;

        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);

        let started = Instant::now();
        let err = stream_command(cmd, ChunkSink::new(tx), 3, "pw", Duration::from_millis(300))
            .unwrap_err();

        assert!(err.to_string().contains("未结束"), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "看门狗没有及时终止子进程"
        );
    }
}
