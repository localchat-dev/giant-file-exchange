use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use reqwest::{Client, multipart};
use serde_json::Value;
use tokio::fs::File;
use tokio_util::io::ReaderStream;

use crate::{
    config::{authorization_header, normalize_token},
    logging::application_log,
};

pub const EXCHANGE_TYPE_EXTERNAL_TO_INTERNAL: u8 = 2;

#[derive(Clone)]
pub struct UploadOptions {
    pub api_base_url: String,
    pub token: String,
    pub receiver_users: Vec<String>,
    pub upload_file_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadResult {
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpendInfo {
    pub spend: f64,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub budget_reset_at: Option<String>,
    pub last_active: Option<String>,
}

#[derive(Clone)]
pub struct ExchangeApiClient {
    client: Client,
}

impl ExchangeApiClient {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::MAX)
            .build()
            .context("无法初始化 HTTP 客户端")?;
        Ok(Self { client })
    }

    pub async fn upload(
        &self,
        path: &Path,
        options: UploadOptions,
        on_progress: Arc<dyn Fn(u64, u64) + Send + Sync>,
    ) -> Result<UploadResult> {
        if options.receiver_users.is_empty() {
            bail!("没有可用的接收人");
        }
        let file = File::open(path).await.context("无法打开待上传文件")?;
        let total = file.metadata().await.context("无法读取文件大小")?.len();
        let mut sent = 0_u64;
        let progress = on_progress.clone();
        let stream = ReaderStream::new(file).inspect_ok(move |chunk| {
            sent = sent.saturating_add(chunk.len() as u64);
            progress(sent, total);
        });
        let body = reqwest::Body::wrap_stream(stream);
        let file_name = options.upload_file_name.clone().unwrap_or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("upload.bin")
                .to_owned()
        });
        let file_part = multipart::Part::stream_with_length(body, total)
            .file_name(file_name)
            .mime_str("application/octet-stream")?;

        let mut form = multipart::Form::new();
        for receiver in options.receiver_users {
            form = form.text("receiverUser", receiver);
        }
        form = form.part("file", file_part).text(
            "exchangeType",
            EXCHANGE_TYPE_EXTERNAL_TO_INTERNAL.to_string(),
        );

        let endpoint = format!(
            "{}/api/exchange/user/transfer/open/upload",
            options.api_base_url.trim_end_matches('/')
        );
        application_log(
            "UPLOAD",
            &format!(
                "开始上传：file={}; size={total}; exchangeType={}",
                path.display(),
                EXCHANGE_TYPE_EXTERNAL_TO_INTERNAL
            ),
        );
        let response = self
            .client
            .post(endpoint)
            .header("Authorization", authorization_header(&options.token)?)
            .multipart(form)
            .send()
            .await
            .context("上传请求失败")?;
        let status = response.status();
        let body = response.text().await.context("无法读取服务器响应")?;
        let parsed = parse_upload_response(status.as_u16(), status.is_success(), &body);
        application_log(
            "HTTP",
            &format!(
                "上传响应：status={}; success={}",
                status.as_u16(),
                parsed.is_ok()
            ),
        );
        parsed
    }

    pub async fn query_usage(&self, api_base_url: &str, api_key: &str) -> Result<SpendInfo> {
        let api_key = normalize_token(api_key);
        if api_key.is_empty() || api_key.contains(['\r', '\n']) {
            bail!("API Key 无效");
        }
        let endpoint = format!("{}/key/info", normalize_usage_api_base_url(api_base_url));
        application_log("USAGE", &format!("开始查询用量：endpoint={endpoint}"));
        let response = self
            .client
            .get(&endpoint)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
            .context("用量查询请求失败")?;
        let status = response.status();
        let body = response.text().await.context("无法读取用量查询响应")?;
        let parsed = parse_spend_response(status.as_u16(), status.is_success(), &body);
        application_log(
            "HTTP",
            &format!(
                "用量查询响应：status={}; success={}",
                status.as_u16(),
                parsed.is_ok()
            ),
        );
        parsed
    }
}

pub fn parse_upload_response(
    http_status: u16,
    http_success: bool,
    body: &str,
) -> Result<UploadResult> {
    let value: Value = serde_json::from_str(body).context("服务器返回了无法解析的响应")?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("请求失败");
    let message = safe_server_message(message);
    if !http_success {
        bail!("HTTP {http_status}: {message}");
    }
    let business_status = value
        .get("status")
        .and_then(|status| status.as_i64().or_else(|| status.as_str()?.parse().ok()));
    if business_status != Some(0) {
        bail!("服务端拒绝了请求：{message}");
    }

    let data = value.get("data");
    if data == Some(&Value::Bool(false)) {
        bail!("服务端报告上传失败：{message}");
    }
    let file_id = match data {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Array(ids)) => ids.first().and_then(Value::as_str).map(ToOwned::to_owned),
        Some(Value::Bool(true)) | None => None,
        Some(Value::Null) => None,
        _ => return Err(anyhow!("服务器响应中的 data 字段无效")),
    };
    Ok(UploadResult { file_id })
}

pub fn parse_spend_response(http_status: u16, http_success: bool, body: &str) -> Result<SpendInfo> {
    let value: Value = serde_json::from_str(body).context("用量接口返回了无法解析的响应")?;
    if !http_success {
        let message = extract_error_message(&value).unwrap_or_else(|| "请求失败".to_owned());
        bail!("HTTP {http_status}: {}", safe_server_message(&message));
    }

    let info = value.get("info").unwrap_or(&value);
    let spend = number_from_value(info.get("spend"))
        .ok_or_else(|| anyhow!("未获取到消费额信息，请检查用量查询地址和 API Key"))?;
    Ok(SpendInfo {
        spend,
        max_budget: number_from_value(info.get("max_budget")),
        budget_duration: string_from_value(info.get("budget_duration")),
        budget_reset_at: scalar_to_string(info.get("budget_reset_at")),
        last_active: scalar_to_string(info.get("last_active")),
    })
}

pub fn normalize_usage_api_base_url(value: &str) -> String {
    let mut base = value.trim().trim_end_matches('/').to_owned();
    for suffix in ["/v1/responses", "/v1/chat/completions", "/v1"] {
        if base.len() > suffix.len()
            && base[base.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            base.truncate(base.len() - suffix.len());
            base = base.trim_end_matches('/').to_owned();
            break;
        }
    }
    base
}

fn number_from_value(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.trim().is_empty() => text.trim().parse().ok(),
        _ => None,
    }
}

fn string_from_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        _ => None,
    }
}

fn scalar_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn extract_error_message(value: &Value) -> Option<String> {
    if let Some(error) = value.get("error") {
        if let Some(text) = error.as_str() {
            return Some(text.to_owned());
        }
        if let Some(text) = error.get("message").and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("msg").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn safe_server_message(value: &str) -> String {
    let value = value.replace(['\r', '\n'], " ");
    value.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    #[test]
    fn parses_supported_success_responses() {
        assert_eq!(
            parse_upload_response(200, true, r#"{"status":0,"data":["abc"]}"#).unwrap(),
            UploadResult {
                file_id: Some("abc".to_owned())
            }
        );
        assert_eq!(
            parse_upload_response(200, true, r#"{"status":"0","data":true}"#).unwrap(),
            UploadResult { file_id: None }
        );
    }

    #[test]
    fn rejects_http_and_business_failures() {
        let error =
            parse_upload_response(401, false, r#"{"message":"not find token"}"#).unwrap_err();
        assert_eq!(error.to_string(), "HTTP 401: not find token");
        assert!(parse_upload_response(200, true, r#"{"status":1,"message":"failed"}"#).is_err());
        assert!(parse_upload_response(200, true, r#"{"status":0,"data":false}"#).is_err());
        assert!(parse_upload_response(200, true, "not json").is_err());
    }

    #[test]
    fn parses_spend_response_and_normalizes_base_url() {
        let spend = parse_spend_response(
            200,
            true,
            r#"{"info":{"spend":"12.5","max_budget":20,"budget_duration":"monthly","budget_reset_at":1750000000,"last_active":"2026-07-21T10:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(spend.spend, 12.5);
        assert_eq!(spend.max_budget, Some(20.0));
        assert_eq!(spend.budget_duration.as_deref(), Some("monthly"));
        assert_eq!(spend.budget_reset_at.as_deref(), Some("1750000000"));
        assert_eq!(
            normalize_usage_api_base_url("https://example.com/v1/chat/completions"),
            "https://example.com"
        );
        assert_eq!(
            normalize_usage_api_base_url("https://example.com/v1/responses"),
            "https://example.com"
        );
    }

    #[test]
    fn rejects_invalid_spend_response() {
        assert!(parse_spend_response(200, true, r#"{"info":{}}"#).is_err());
        let error =
            parse_spend_response(401, false, r#"{"error":{"message":"bad key"}}"#).unwrap_err();
        assert_eq!(error.to_string(), "HTTP 401: bad key");
    }

    #[tokio::test]
    async fn sends_expected_multipart_request_and_reports_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_server = captured.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end;
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    header_end = position + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap();
            while request.len() < header_end + content_length {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            *captured_server.lock().unwrap() = request;
            let response_body = r#"{"status":0,"data":["id-1"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            ).unwrap();
        });

        let path = std::env::temp_dir().join(format!("gfe-api-test-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello upload").unwrap();
        let progress = Arc::new(AtomicU64::new(0));
        let progress_clone = progress.clone();
        let result = ExchangeApiClient::new()
            .unwrap()
            .upload(
                &path,
                UploadOptions {
                    api_base_url: format!("http://{address}"),
                    token: "secret-token".to_owned(),
                    receiver_users: vec!["alice".to_owned(), "bob".to_owned()],
                    upload_file_name: Some("message.txt".to_owned()),
                },
                Arc::new(move |sent, _| {
                    progress_clone.store(sent, Ordering::Relaxed);
                }),
            )
            .await
            .unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(result.file_id.as_deref(), Some("id-1"));
        assert_eq!(progress.load(Ordering::Relaxed), 12);
        let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_ascii_lowercase();
        assert!(request.contains("authorization: bear secret-token"));
        assert_eq!(request.matches("name=\"receiveruser\"").count(), 2);
        assert!(request.contains("alice"));
        assert!(request.contains("bob"));
        assert!(request.contains("name=\"exchangetype\""));
        assert!(request.contains("\r\n\r\n2\r\n"));
        assert!(request.contains("filename=\"message.txt\""));
        assert!(request.contains("hello upload"));
    }
}
