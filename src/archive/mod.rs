use std::io::{self, Write};
use tokio::sync::mpsc;

pub mod age;
pub mod tar_zstd;

/// 攒够这么多字节就往上传任务送一次，避免每个小写入都过一遍 channel
const CHUNK_FLUSH_BYTES: usize = 1024 * 1024;

/// 生产者侧统计信息，用于写 manifest
#[derive(Debug, Default, Clone, Copy)]
pub struct ProduceStats {
    pub entries: u64,
    pub plaintext_bytes: Option<u64>,
}

/// 把同步写入流切成块，阻塞地送进 tokio channel。
///
/// 只能在 `spawn_blocking` 线程里使用（`blocking_send` 不能在 async worker 上调用）。
pub struct ChunkSink {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    buf: Vec<u8>,
}

impl ChunkSink {
    pub fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx: Some(tx),
            buf: Vec::with_capacity(CHUNK_FLUSH_BYTES),
        }
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "上传任务已结束，停止写入"))?;
        let chunk = std::mem::take(&mut self.buf);
        tx.blocking_send(chunk)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "上传任务已结束，停止写入"))?;
        self.buf = Vec::with_capacity(CHUNK_FLUSH_BYTES);
        Ok(())
    }

    /// 刷出剩余缓冲并释放 sender，触发上传侧收尾
    pub fn finish(mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.tx.take();
        Ok(())
    }
}

impl Write for ChunkSink {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let total = bytes.len();
        while !bytes.is_empty() {
            let space = CHUNK_FLUSH_BYTES - self.buf.len();
            let take = space.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() >= CHUNK_FLUSH_BYTES {
                self.flush_buf()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sink_sends_buffered_chunks_on_finish() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        let mut sink = ChunkSink::new(tx);
        sink.write_all(b"hello").unwrap();
        sink.finish().unwrap();

        let chunk = rx.try_recv().expect("should have one chunk");
        assert_eq!(chunk, b"hello".to_vec());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sink_splits_large_writes_into_chunks() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        let mut sink = ChunkSink::new(tx);
        let data = vec![7u8; CHUNK_FLUSH_BYTES * 2 + 10];
        sink.write_all(&data).unwrap();
        sink.finish().unwrap();

        let first = rx.try_recv().unwrap();
        assert_eq!(first.len(), CHUNK_FLUSH_BYTES);
        let second = rx.try_recv().unwrap();
        assert_eq!(second.len(), CHUNK_FLUSH_BYTES);
        let third = rx.try_recv().unwrap();
        assert_eq!(third.len(), 10);
    }
}
