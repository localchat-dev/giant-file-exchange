use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use reqwest::{Client, multipart};
use serde_json::Value;
use tokio::fs::File;
use tokio_util::io::ReaderStream;

use crate::{config::authorization_header, logging::application_log};

#[derive(Clone)]
pub struct UploadOptions {
    pub api_base_url: String,
    pub token: String,
    pub receiver_users: Vec<String>,
    pub exchange_type: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadResult {
    pub file_id: Option<String>,
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
        if !matches!(options.exchange_type, 1 | 2) {
            bail!("传输方向无效");
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
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("upload.bin")
            .to_owned();
        let file_part = multipart::Part::stream_with_length(body, total)
            .file_name(file_name)
            .mime_str("application/octet-stream")?;

        let mut form = multipart::Form::new();
        for receiver in options.receiver_users {
            form = form.text("receiverUser", receiver);
        }
        form = form
            .part("file", file_part)
            .text("exchangeType", options.exchange_type.to_string());

        let endpoint = format!(
            "{}/api/exchange/user/transfer/open/upload",
            options.api_base_url.trim_end_matches('/')
        );
        application_log(
            "UPLOAD",
            &format!(
                "开始上传：file={}; size={total}; exchangeType={}",
                path.display(),
                options.exchange_type
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
                    exchange_type: 2,
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
        assert!(request.contains("hello upload"));
    }
}
