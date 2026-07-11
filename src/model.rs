use std::{path::PathBuf, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStatus {
    Waiting,
    Uploading,
    Processing,
    Succeeded,
    Failed,
    Cancelled,
}

impl UploadStatus {
    pub fn text(self) -> &'static str {
        match self {
            Self::Waiting => "等待上传",
            Self::Uploading => "上传中",
            Self::Processing => "服务器处理中",
            Self::Succeeded => "上传成功",
            Self::Failed => "上传失败",
            Self::Cancelled => "已取消",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Waiting | Self::Uploading | Self::Processing)
    }

    pub fn can_retry(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug)]
pub struct UploadTask {
    pub id: u64,
    pub path: PathBuf,
    pub file_name: String,
    pub file_size: u64,
    pub bytes_sent: u64,
    pub speed_bytes_per_second: f64,
    pub status: UploadStatus,
    pub error: String,
    pub server_file_id: Option<String>,
    pub temporary: bool,
    pub(crate) last_progress_at: Option<Instant>,
    pub(crate) last_progress_bytes: u64,
}

impl UploadTask {
    pub fn new(id: u64, path: PathBuf, temporary: bool) -> std::io::Result<Self> {
        let metadata = path.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "所选路径不是普通文件",
            ));
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("未命名文件")
            .to_owned();
        Ok(Self {
            id,
            path,
            file_name,
            file_size: metadata.len(),
            bytes_sent: 0,
            speed_bytes_per_second: 0.0,
            status: UploadStatus::Waiting,
            error: String::new(),
            server_file_id: None,
            temporary,
            last_progress_at: None,
            last_progress_bytes: 0,
        })
    }

    pub fn progress(&self) -> f32 {
        if self.file_size == 0 {
            return if matches!(self.status, UploadStatus::Succeeded) {
                1.0
            } else {
                0.0
            };
        }
        (self.bytes_sent as f64 / self.file_size as f64).clamp(0.0, 1.0) as f32
    }

    pub fn reset_for_retry(&mut self) {
        self.bytes_sent = 0;
        self.speed_bytes_per_second = 0.0;
        self.status = UploadStatus::Waiting;
        self.error.clear();
        self.server_file_id = None;
        self.last_progress_at = None;
        self.last_progress_bytes = 0;
    }
}

pub fn format_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if value == 0 {
        return "0 B".to_owned();
    }
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if amount >= 10.0 || unit == 0 {
        format!("{amount:.0} {}", UNITS[unit])
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(10 * 1024), "10 KB");
    }

    #[test]
    fn status_capabilities_are_stable() {
        assert!(UploadStatus::Waiting.is_active());
        assert!(!UploadStatus::Succeeded.is_active());
        assert!(UploadStatus::Failed.can_retry());
        assert!(!UploadStatus::Uploading.can_retry());
    }
}
