use crate::archive::ChunkSink;
use crate::archive::ProduceStats;
use anyhow::{anyhow, Context, Result};
use opendal::options::WriteOptions;
use opendal::Operator;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

pub mod oss;

pub use oss::OssStore;

/// 单个对象上传完成后的统计
#[derive(Debug, Default, Clone)]
pub struct UploadStats {
    /// 已上传的密文字节数
    pub bytes: u64,
    /// 密文字节的 SHA-256（十六进制）
    pub sha256: String,
    pub entries: u64,
    pub plaintext_bytes: Option<u64>,
}

/// `produce` 在阻塞线程里把数据写进 channel，这里在 async 侧边算哈希边上传。
///
/// 任一环节失败都会 `abort()` 放弃 Multipart，OSS 上不会留下可见的半成品对象。
pub async fn upload_stream<F>(
    op: &Operator,
    key: &str,
    part_size: usize,
    produce: F,
) -> Result<UploadStats>
where
    F: FnOnce(ChunkSink) -> Result<ProduceStats> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
    let options = WriteOptions {
        chunk: Some(part_size.max(1024 * 1024)),
        concurrent: 1,
        ..Default::default()
    };
    let mut writer = op
        .writer_options(key, options)
        .await
        .with_context(|| format!("创建 OSS 写入器失败：{key}"))?;

    let handle = tokio::task::spawn_blocking(move || produce(ChunkSink::new(tx)));

    let mut hasher = Sha256::new();
    let mut bytes: u64 = 0;
    let mut write_error: Option<anyhow::Error> = None;

    while let Some(chunk) = rx.recv().await {
        hasher.update(&chunk);
        bytes += chunk.len() as u64;
        if let Err(err) = writer.write(chunk).await {
            write_error = Some(anyhow!(err).context(format!("上传数据块失败：{key}")));
            break;
        }
    }
    drop(rx);

    let produced = match handle.await {
        Ok(result) => result,
        Err(err) => Err(anyhow!("备份线程异常退出：{err}")),
    };

    if let Some(err) = write_error {
        if let Err(abort_err) = writer.abort().await {
            tracing::warn!("放弃分片上传失败（{key}）：{abort_err}");
        }
        return Err(err);
    }

    let stats = match produced {
        Ok(stats) => stats,
        Err(err) => {
            if let Err(abort_err) = writer.abort().await {
                tracing::warn!("放弃分片上传失败（{key}）：{abort_err}");
            }
            return Err(err);
        }
    };

    // close 失败同样要 abort：否则分片留在 OSS 上不可见但持续计费，
    // 与 README「失败即放弃分片上传」的承诺不符。
    if let Err(err) = writer.close().await {
        if let Err(abort_err) = writer.abort().await {
            tracing::warn!("放弃分片上传失败（{key}）：{abort_err}");
        }
        return Err(anyhow!(err).context(format!("完成上传失败：{key}")));
    }

    Ok(UploadStats {
        bytes,
        sha256: hex::encode(hasher.finalize()),
        entries: stats.entries,
        plaintext_bytes: stats.plaintext_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ProduceStats;
    use opendal::services::Memory;

    #[tokio::test]
    async fn uploads_stream_and_reports_hash() {
        let op = Operator::new(Memory::default()).unwrap();
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();

        let stats = upload_stream(&op, "a/b.bin", 1024 * 1024, move |mut sink| {
            use std::io::Write;
            sink.write_all(&payload)?;
            sink.finish()?;
            Ok(ProduceStats {
                entries: 1,
                plaintext_bytes: Some(payload.len() as u64),
            })
        })
        .await
        .unwrap();

        assert_eq!(stats.bytes, expected.len() as u64);
        assert_eq!(stats.entries, 1);
        let got = op.read("a/b.bin").await.unwrap().to_vec();
        assert_eq!(got, expected);

        let mut hasher = Sha256::new();
        hasher.update(&expected);
        assert_eq!(stats.sha256, hex::encode(hasher.finalize()));
    }

    #[tokio::test]
    async fn producer_error_leaves_no_object() {
        let op = Operator::new(Memory::default()).unwrap();
        let err = upload_stream(&op, "a/b.bin", 1024 * 1024, |mut sink| {
            use std::io::Write;
            sink.write_all(&vec![0u8; 2048])?;
            anyhow::bail!("模拟导出失败")
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("模拟导出失败"));
        assert!(!op.exists("a/b.bin").await.unwrap());
    }
}
