use serde::Serialize;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize)]
pub struct NotifyFailure {
    pub name: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotifyPayload {
    pub status: &'static str,
    pub backup_count: usize,
    pub failures: Vec<NotifyFailure>,
    pub duration_ms: u64,
}

/// 推送告警；失败只记 warn，不影响退出码
pub async fn send(url: &str, payload: &NotifyPayload) {
    if url.is_empty() {
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!("创建 webhook 客户端失败：{err}");
            return;
        }
    };

    match client.post(url).json(payload).send().await {
        Ok(resp) if resp.status().is_success() => info!("webhook 推送成功"),
        Ok(resp) => warn!("webhook 返回异常状态：{}", resp.status()),
        Err(err) => warn!("webhook 推送失败：{err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_serializes() {
        let payload = NotifyPayload {
            status: "partial_failure",
            backup_count: 2,
            failures: vec![NotifyFailure {
                name: "docs".into(),
                error: "boom".into(),
            }],
            duration_ms: 100,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["status"], "partial_failure");
        assert_eq!(json["failures"][0]["name"], "docs");
    }
}
