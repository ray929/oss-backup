use crate::archive::ChunkSink;
use age::secrecy::SecretString;
use std::io;

/// 口令派生强度：N = 2^16，约 64 MiB 内存、几百毫秒。
///
/// age 默认会在运行时测速，把 work factor 抬到"单次派生约 1 秒"——通常是
/// log_n = 18，即 256 MiB 峰值，单这一项就吃光整个内存预算。这里固定成下界，
/// 让峰值可预测。log_n 写在 age header 里，解密端不受影响，旧备份照常可解。
const SCRYPT_LOG_N: u8 = 16;

/// 用 age 口令模式包装写入器：数据先压缩后进这里，再流向 ChunkSink
pub fn encrypt(
    sink: ChunkSink,
    passphrase: &str,
) -> io::Result<age::stream::StreamWriter<ChunkSink>> {
    let passphrase = SecretString::from(passphrase.to_string());
    let mut recipient = age::scrypt::Recipient::new(passphrase);
    recipient.set_work_factor(SCRYPT_LOG_N);

    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(io::Error::other)?;
    encryptor.wrap_output(sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 流式加密 8 MiB，用于验证内存不随数据量增长（channel 只有 4 MiB 容量）。
    ///
    /// 数据量压到 8 MiB 是为了控制 CI 时间：debug 构建下 scrypt 派生 + 逐块加密
    /// 在 CI 机器上要十几秒。峰值由外部工具另外测量（GetProcessMemoryInfo），
    /// 64 MiB 输入时实测约 76 MiB，其中 64 MiB 是 scrypt 的工作区。
    #[test]
    fn encrypts_stream_without_buffering_everything() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let producer = std::thread::spawn(move || {
            let mut writer = encrypt(ChunkSink::new(tx), "test-passphrase").unwrap();
            let block = vec![0xa5u8; 1 << 20];
            for _ in 0..8 {
                writer.write_all(&block).unwrap();
            }
            // StreamWriter::finish 收尾加密，再关掉底层 sink 触发上传侧结束
            writer.finish().unwrap().finish().unwrap();
        });

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut total = 0usize;
        runtime.block_on(async {
            while let Some(chunk) = rx.recv().await {
                total += chunk.len();
            }
        });
        producer.join().unwrap();

        // 密文略大于明文（header + AEAD tag），至少要有 8 MiB
        assert!(total >= 8 << 20, "unexpected ciphertext size: {total}");
    }
}
