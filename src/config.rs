use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

pub const DEFAULT_API_BASE_URL: &str = "https://example.invalid";
pub const DEFAULT_HOTKEY: &str = "Ctrl+Alt+U";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub api_base_url: String,
    pub encrypted_token: String,
    pub receiver_users: Vec<String>,
    pub exchange_type: u8,
    pub hotkey: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            api_base_url: DEFAULT_API_BASE_URL.to_owned(),
            encrypted_token: String::new(),
            receiver_users: Vec::new(),
            exchange_type: 2,
            hotkey: DEFAULT_HOTKEY.to_owned(),
        }
    }
}

impl AppSettings {
    pub fn is_configured(&self) -> bool {
        self.validate_without_token().is_ok() && !self.encrypted_token.is_empty()
    }

    pub fn normalize_receivers(values: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        for value in values {
            let value = value.trim().to_owned();
            if !value.is_empty()
                && !result
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(&value))
            {
                result.push(value);
            }
        }
        result
    }

    pub fn validate_without_token(&self) -> Result<()> {
        let url = Url::parse(self.api_base_url.trim()).context("API 地址格式无效")?;
        if url.scheme() != "https" || url.host_str().is_none() {
            bail!("API 地址必须是有效的 HTTPS 地址");
        }
        if self.receiver_users.is_empty() {
            bail!("请至少填写一个接收人的域账号");
        }
        if !matches!(self.exchange_type, 1 | 2) {
            bail!("传输方向无效");
        }
        validate_hotkey_text(&self.hotkey)?;
        Ok(())
    }
}

pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> PathBuf {
        app_data_dir().join("settings.json")
    }

    pub fn load(&self) -> AppSettings {
        let Ok(data) = fs::read_to_string(&self.path) else {
            return AppSettings::default();
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    pub fn save(&self, settings: &AppSettings) -> Result<()> {
        settings.validate_without_token()?;
        if settings.encrypted_token.is_empty() {
            bail!("请填写个人令牌");
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).context("无法创建配置目录")?;
        }
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(settings)?).context("无法写入临时配置")?;
        replace_file(&temp, &self.path).context("无法保存配置")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    if !destination.exists() {
        return fs::rename(source, destination);
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let succeeded = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null(),
            ptr::null(),
        )
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)
}

pub fn app_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("GiantFileExchange")
}

pub fn normalize_token(value: &str) -> String {
    let value = value.trim();
    for prefix in ["bearer", "bear"] {
        if value.len() > prefix.len()
            && value[..prefix.len()].eq_ignore_ascii_case(prefix)
            && value.as_bytes()[prefix.len()].is_ascii_whitespace()
        {
            return value[prefix.len()..].trim().to_owned();
        }
    }
    value.to_owned()
}

pub fn authorization_header(token: &str) -> Result<String> {
    let token = normalize_token(token);
    if token.is_empty() || token.contains(['\r', '\n']) {
        bail!("个人令牌无效");
    }
    Ok(format!("Bear {token}"))
}

pub fn validate_hotkey_text(value: &str) -> Result<()> {
    let parts: Vec<_> = value
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        bail!("快捷键必须包含至少一个修饰键，例如 Ctrl+Alt+U");
    }
    let modifier_count = parts[..parts.len() - 1]
        .iter()
        .filter(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                "ctrl" | "alt" | "shift" | "win" | "super"
            )
        })
        .count();
    let valid_key = parts.last().is_some_and(|key| {
        let upper = key.to_ascii_uppercase();
        (upper.len() == 1 && upper.as_bytes()[0].is_ascii_alphanumeric())
            || upper
                .strip_prefix('F')
                .and_then(|number| number.parse::<u8>().ok())
                .is_some_and(|number| (1..=24).contains(&number))
    });
    if modifier_count != parts.len() - 1 || !valid_key {
        bail!("快捷键格式无效，请使用 Ctrl+Alt+U 这样的格式");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_receivers() {
        let values = AppSettings::normalize_receivers([
            " alice ".to_owned(),
            "ALICE".to_owned(),
            "".to_owned(),
            "bob".to_owned(),
        ]);
        assert_eq!(values, ["alice", "bob"]);
    }

    #[test]
    fn creates_company_authorization_header() {
        assert_eq!(authorization_header(" secret ").unwrap(), "Bear secret");
        assert_eq!(
            authorization_header("Bearer secret").unwrap(),
            "Bear secret"
        );
        assert_eq!(authorization_header("Bear secret").unwrap(), "Bear secret");
        assert!(authorization_header("bad\r\ntoken").is_err());
    }

    #[test]
    fn validates_settings() {
        let mut settings = AppSettings::default();
        assert!(settings.validate_without_token().is_err());
        settings.receiver_users.push("alice".to_owned());
        assert!(settings.validate_without_token().is_ok());
        settings.api_base_url = "http://example.com".to_owned();
        assert!(settings.validate_without_token().is_err());
    }

    #[test]
    fn saves_settings_more_than_once() {
        let directory = std::env::temp_dir().join(format!(
            "gfe-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("settings.json");
        let store = SettingsStore::new(path);
        let mut settings = AppSettings {
            encrypted_token: "encrypted".to_owned(),
            receiver_users: vec!["alice".to_owned()],
            ..Default::default()
        };
        store.save(&settings).unwrap();
        settings.receiver_users = vec!["bob".to_owned()];
        store.save(&settings).unwrap();
        assert_eq!(store.load().receiver_users, ["bob"]);
        let _ = fs::remove_dir_all(directory);
    }
}
