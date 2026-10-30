use crate::storage::OssStore;
use anyhow::{Context, Result};
use serde::Serialize;

pub const MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct ManifestObject {
    pub key: String,
    /// 数据库类记录原始库名/文件路径，文件类为 null
    pub source: Option<String>,
    /// 上传的密文字节数
    pub size: u64,
    /// 密文字节的 SHA-256
    pub sha256: String,
    pub plaintext_bytes: Option<u64>,
    pub entries: u64,
}

#[derive(Debug, Serialize)]
pub struct Manifest<'a> {
    pub version: u32,
    pub kind: &'a str,
    pub destination: &'a str,
    pub started_at: String,
    pub finished_at: String,
    pub zstd_level: i32,
    pub encryption: &'a str,
    pub inputs: Vec<String>,
    pub objects: Vec<ManifestObject>,
    pub total_plaintext_bytes: Option<u64>,
    pub duration_ms: u64,
    pub tool_version: &'a str,
}

impl<'a> Manifest<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: &'a str,
        destination: &'a str,
        started_at: String,
        finished_at: String,
        zstd_level: i32,
        inputs: Vec<String>,
        objects: Vec<ManifestObject>,
        duration_ms: u64,
    ) -> Self {
        let total_plaintext_bytes = objects
            .iter()
            .map(|o| o.plaintext_bytes)
            .collect::<Option<Vec<u64>>>()
            .map(|values| values.iter().sum());

        Self {
            version: MANIFEST_VERSION,
            kind,
            destination,
            started_at,
            finished_at,
            zstd_level,
            encryption: "age-passphrase",
            inputs,
            objects,
            total_plaintext_bytes,
            duration_ms,
            tool_version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// 上传 manifest，作为“本次运行完整”的提交标记
pub async fn upload(store: &OssStore, key: &str, manifest: &Manifest<'_>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).context("序列化 manifest 失败")?;
    store.write_bytes(key, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(plaintext: Option<u64>) -> ManifestObject {
        ManifestObject {
            key: "k".into(),
            source: None,
            size: 10,
            sha256: "00".into(),
            plaintext_bytes: plaintext,
            entries: 1,
        }
    }

    #[test]
    fn total_plaintext_is_sum_when_all_known() {
        let manifest = Manifest::new(
            "file",
            "aliyun",
            "t0".into(),
            "t1".into(),
            3,
            vec!["D:/docs".into()],
            vec![object(Some(10)), object(Some(20))],
            5,
        );
        assert_eq!(manifest.total_plaintext_bytes, Some(30));
    }

    #[test]
    fn total_plaintext_is_none_when_any_unknown() {
        let manifest = Manifest::new(
            "mysql",
            "aliyun",
            "t0".into(),
            "t1".into(),
            3,
            vec![],
            vec![object(Some(10)), object(None)],
            5,
        );
        assert_eq!(manifest.total_plaintext_bytes, None);
    }

    #[test]
    fn serializes_to_expected_shape() {
        let manifest = Manifest::new(
            "file",
            "aliyun",
            "t0".into(),
            "t1".into(),
            3,
            vec![],
            vec![object(Some(1))],
            0,
        );
        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["version"], 2);
        assert_eq!(json["encryption"], "age-passphrase");
        assert!(json["objects"].is_array());
        assert!(json.get("backup").is_none());
    }
}
