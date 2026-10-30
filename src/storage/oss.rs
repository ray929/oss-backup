use crate::config::Destination;
use anyhow::{Context, Result};
use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::services::Oss;
use opendal::Operator;
use std::time::Duration;

/// OSS 访问封装：只暴露本项目需要的操作
pub struct OssStore {
    op: Operator,
}

impl OssStore {
    pub fn new(destination: &Destination, timeout: Duration) -> Result<Self> {
        let endpoint = normalize_endpoint(&destination.endpoint, destination.internal);
        let builder = Oss::default()
            .endpoint(&endpoint)
            .bucket(&destination.bucket)
            .access_key_id(&destination.access_key_id)
            .access_key_secret(&destination.access_key_secret);

        let op = Operator::new(builder)
            .with_context(|| {
                format!(
                    "创建 OSS 客户端失败（destination={} endpoint={}）",
                    destination.name, endpoint
                )
            })?
            .layer(RetryLayer::new().with_max_times(5).with_jitter())
            .layer(TimeoutLayer::new().with_io_timeout(timeout));

        Ok(Self { op })
    }

    pub fn operator(&self) -> &Operator {
        &self.op
    }

    /// 轻量连通性检查：列举前缀下最多一个对象
    pub async fn probe(&self, prefix: &str) -> Result<()> {
        self.op
            .list_with(prefix)
            .recursive(true)
            .limit(1)
            .await
            .with_context(|| format!("列举 OSS 前缀失败：{prefix}"))?;
        Ok(())
    }

    pub async fn write_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.op
            .write(key, bytes)
            .await
            .with_context(|| format!("写入 OSS 对象失败：{key}"))?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.op
            .delete(key)
            .await
            .with_context(|| format!("删除 OSS 对象失败：{key}"))?;
        Ok(())
    }

    /// 递归列出前缀下的对象 key（不含目录项）
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let entries = self
            .op
            .list_with(prefix)
            .recursive(true)
            .await
            .with_context(|| format!("列举 OSS 前缀失败：{prefix}"))?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry.metadata().mode() == opendal::EntryMode::FILE)
            .map(|entry| entry.path().to_string())
            .collect())
    }
}

/// 统一 endpoint 形态；`internal = true` 时改用内网域名
pub fn normalize_endpoint(endpoint: &str, internal: bool) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    if internal {
        with_scheme
            .replace("-internal.aliyuncs.com", ".aliyuncs.com")
            .replace(".aliyuncs.com", "-internal.aliyuncs.com")
    } else {
        with_scheme.replace("-internal.aliyuncs.com", ".aliyuncs.com")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_scheme_when_missing() {
        assert_eq!(
            normalize_endpoint("oss-cn-hangzhou.aliyuncs.com", false),
            "https://oss-cn-hangzhou.aliyuncs.com"
        );
    }

    #[test]
    fn keeps_existing_scheme() {
        assert_eq!(
            normalize_endpoint("https://oss-cn-hangzhou.aliyuncs.com/", false),
            "https://oss-cn-hangzhou.aliyuncs.com"
        );
    }

    #[test]
    fn switches_to_internal() {
        assert_eq!(
            normalize_endpoint("https://oss-cn-hangzhou.aliyuncs.com", true),
            "https://oss-cn-hangzhou-internal.aliyuncs.com"
        );
    }

    #[test]
    fn internal_is_idempotent() {
        assert_eq!(
            normalize_endpoint("https://oss-cn-hangzhou-internal.aliyuncs.com", true),
            "https://oss-cn-hangzhou-internal.aliyuncs.com"
        );
        assert_eq!(
            normalize_endpoint("https://oss-cn-hangzhou-internal.aliyuncs.com", false),
            "https://oss-cn-hangzhou.aliyuncs.com"
        );
    }
}
