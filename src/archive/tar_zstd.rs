use crate::archive::{age, ChunkSink, ProduceStats};
use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};
use tar::Builder;
use walkdir::WalkDir;

/// 一个文件源及其专属排除规则
pub struct Source {
    pub root: PathBuf,
    pub exclude: Vec<String>,
}

/// 文件类备份：walkdir → exclude 过滤 → tar → zstd → age → sink
///
/// 每个源使用自己的排除规则；归档条目名带源路径前缀，多个源下的同名文件不会互相覆盖。
pub fn produce(
    sink: ChunkSink,
    sources: &[Source],
    level: i32,
    passphrase: &str,
) -> Result<ProduceStats> {
    for source in sources {
        if !source.root.exists() {
            anyhow::bail!("备份路径不存在：{}", source.root.display());
        }
    }

    let age_writer = age::encrypt(sink, passphrase)?;
    let encoder = zstd::Encoder::new(age_writer, level).context("初始化 zstd 压缩器失败")?;
    let mut tar = Builder::new(encoder);
    tar.follow_symlinks(false);

    let mut entries = 0u64;
    let mut plaintext_bytes = 0u64;

    for source in sources {
        let exclude_set = exclude_set(&source.exclude)?;
        for entry in WalkDir::new(&source.root).follow_links(false) {
            let entry =
                entry.with_context(|| format!("遍历目录失败：{}", source.root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(&source.root).unwrap_or(path);
            let rel = relative.to_string_lossy().replace('\\', "/");

            if exclude_set.is_match(&rel) {
                tracing::debug!(file = %rel, "已排除");
                continue;
            }

            plaintext_bytes += entry.metadata().map(|m| m.len()).unwrap_or_default();

            // 用根路径做前缀，避免多个 local_path 下同名文件互相覆盖
            let name = archive_name(&source.root, &rel);
            tar.append_path_with_name(path, &name)
                .with_context(|| format!("打包文件失败：{}", path.display()))?;
            entries += 1;
        }
    }

    let encoder = tar.into_inner().context("结束 tar 写入失败")?;
    let age_writer = encoder.finish().context("结束 zstd 压缩失败")?;
    let sink = age_writer.finish().context("结束 age 加密失败")?;
    sink.finish().context("刷新上传缓冲失败")?;

    Ok(ProduceStats {
        entries,
        plaintext_bytes: Some(plaintext_bytes),
    })
}

/// 把 `D:\Documents\a.txt` 变成 `D__Documents/a.txt`
fn archive_name(root: &Path, relative: &str) -> String {
    let prefix = root
        .to_string_lossy()
        .replace(['/', '\\', ':'], "_")
        .trim_matches('_')
        .to_string();
    if relative.is_empty() {
        prefix
    } else {
        format!("{}/{}", prefix, relative)
    }
}

pub(crate) fn exclude_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        for piece in pattern.split_whitespace() {
            if piece.is_empty() {
                continue;
            }
            builder.add(Glob::new(piece).with_context(|| format!("非法排除规则：{piece}"))?);
            // 目录式规则（如 node_modules）同时排除其下所有内容
            if let Ok(subtree) = Glob::new(&format!("{piece}/**")) {
                builder.add(subtree);
            }
        }
    }
    builder.build().context("构建排除规则失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ChunkSink;
    use ::age::secrecy::SecretString;
    use std::io::Read;
    use tokio::sync::mpsc;

    const PASSPHRASE: &str = "test-passphrase";

    fn produce_collect(sources: &[Source]) -> (ProduceStats, Vec<u8>) {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
        let stats = produce(ChunkSink::new(tx), sources, 3, PASSPHRASE).unwrap();
        let mut out = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        (stats, out)
    }

    fn source(root: &std::path::Path, exclude: &[&str]) -> Source {
        Source {
            root: root.to_path_buf(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn decrypt_and_list(ciphertext: &[u8]) -> Vec<String> {
        let passphrase = SecretString::from(PASSPHRASE.to_string());
        let identity = ::age::scrypt::Identity::new(passphrase);
        let decryptor = ::age::Decryptor::new(ciphertext).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn ::age::Identity))
            .unwrap();
        let decoder = zstd::Decoder::new(&mut reader).unwrap();
        let mut archive = tar::Archive::new(decoder);
        let mut names = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            names.push(entry.path().unwrap().to_string_lossy().to_string());
            // 读掉内容，确保解密链正常
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
        }
        names
    }

    #[test]
    fn produces_encrypted_tar_with_excludes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("drop.tmp"), b"bye").unwrap();
        std::fs::write(dir.path().join("node_modules/x.js"), b"js").unwrap();

        let (stats, ciphertext) =
            produce_collect(&[source(dir.path(), &["node_modules", "*.tmp"])]);
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.plaintext_bytes, Some(5));

        let names = decrypt_and_list(&ciphertext);
        assert_eq!(names.len(), 1);
        assert!(
            names[0].ends_with("keep.txt"),
            "unexpected name: {}",
            names[0]
        );
    }

    #[test]
    fn excludes_apply_per_source() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("keep.txt"), b"a").unwrap();
        std::fs::write(a.path().join("drop.tmp"), b"a").unwrap();
        std::fs::write(b.path().join("keep.tmp"), b"b").unwrap();

        // 同一个 *.tmp 规则只作用于源 a，源 b 的 keep.tmp 应保留
        let (stats, ciphertext) =
            produce_collect(&[source(a.path(), &["*.tmp"]), source(b.path(), &[])]);
        assert_eq!(stats.entries, 2);

        let names = decrypt_and_list(&ciphertext);
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n.ends_with("keep.tmp")));
    }

    #[test]
    fn multiple_roots_do_not_collide() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("same.txt"), b"a").unwrap();
        std::fs::write(b.path().join("same.txt"), b"b").unwrap();

        let (stats, ciphertext) = produce_collect(&[source(a.path(), &[]), source(b.path(), &[])]);
        assert_eq!(stats.entries, 2);

        let names = decrypt_and_list(&ciphertext);
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
    }

    #[test]
    fn empty_directory_produces_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let (stats, ciphertext) = produce_collect(&[source(dir.path(), &[])]);
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.plaintext_bytes, Some(0));
        assert!(decrypt_and_list(&ciphertext).is_empty());
    }

    #[test]
    fn archive_name_encodes_root() {
        let name = archive_name(Path::new(r"D:\Documents"), "a/b.txt");
        assert_eq!(name, "D__Documents/a/b.txt");
        let single = archive_name(Path::new(r"D:\config.ini"), "");
        assert_eq!(single, "D__config.ini");
    }

    #[test]
    fn missing_path_is_error() {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let err = produce(
            ChunkSink::new(tx),
            &[Source {
                root: PathBuf::from(r"D:\definitely-not-exists"),
                exclude: Vec::new(),
            }],
            3,
            PASSPHRASE,
        )
        .unwrap_err();
        assert!(err.to_string().contains("备份路径不存在"));
    }

    #[test]
    fn invalid_exclude_pattern_is_error() {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let dir = tempfile::tempdir().unwrap();
        let err = produce(
            ChunkSink::new(tx),
            &[source(dir.path(), &["["])],
            3,
            PASSPHRASE,
        )
        .unwrap_err();
        assert!(err.to_string().contains("非法排除规则"));
    }
}
