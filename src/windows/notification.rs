use std::{io, mem};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{HWND, RECT, S_OK},
    UI::Shell::{
        NIF_INFO, NIIF_ERROR, NIIF_INFO, NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW,
        NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
    },
};

#[derive(Clone, Copy)]
pub enum SystemNotificationKind {
    Info,
    Warning,
    Error,
}

pub fn show_tray_notification(
    tray_window: HWND,
    title: &str,
    message: &str,
    kind: SystemNotificationKind,
) -> Result<()> {
    let tray_id = find_tray_id(tray_window).context("无法定位系统托盘图标")?;
    let mut data = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: tray_window,
        uID: tray_id,
        uFlags: NIF_INFO,
        dwInfoFlags: match kind {
            SystemNotificationKind::Info => NIIF_INFO,
            SystemNotificationKind::Warning => NIIF_WARNING,
            SystemNotificationKind::Error => NIIF_ERROR,
        },
        ..Default::default()
    };
    copy_utf16(title, &mut data.szInfoTitle);
    copy_utf16(message, &mut data.szInfo);
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
        return Err(io::Error::last_os_error()).context("无法发送 Windows 系统通知");
    }
    Ok(())
}

fn find_tray_id(tray_window: HWND) -> Result<u32> {
    for id in 1..=64 {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: tray_window,
            uID: id,
            ..Default::default()
        };
        let mut rect = RECT::default();
        if unsafe { Shell_NotifyIconGetRect(&identifier, &mut rect) } == S_OK {
            return Ok(id);
        }
    }
    bail!("系统托盘图标尚未就绪")
}

fn copy_utf16<const N: usize>(value: &str, target: &mut [u16; N]) {
    target.fill(0);
    for (slot, code_unit) in target
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(value.encode_utf16())
    {
        *slot = code_unit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_is_null_terminated_and_truncated() {
        let mut target = [1_u16; 8];
        copy_utf16("123456789", &mut target);
        assert_eq!(target, [49, 50, 51, 52, 53, 54, 55, 0]);
    }
}
