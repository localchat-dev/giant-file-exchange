use std::{mem, thread, time::Duration};

use clipboard_win::{Clipboard, Getter, formats, raw};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

#[derive(Debug)]
pub struct TextCaptureResult {
    pub text: String,
    pub clipboard_restored: bool,
}

struct ClipboardSnapshot {
    formats: Vec<(u32, Vec<u8>)>,
    complete: bool,
}

pub fn capture_selected_text() -> Result<TextCaptureResult, String> {
    let snapshot = snapshot_clipboard();
    wait_for_modifiers_released();
    let before = raw::seq_num();
    send_copy_shortcut()?;

    let mut selected = None;
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(50));
        if raw::seq_num() == before {
            continue;
        }
        if let Ok(text) = clipboard_win::get_clipboard_string() {
            selected = Some(text);
            break;
        }
    }
    let restored = snapshot.as_ref().is_some_and(restore_clipboard);
    let text = selected
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "未获取到选中文本，请先选择一段可复制的文本".to_owned())?;
    Ok(TextCaptureResult {
        text,
        clipboard_restored: restored,
    })
}

fn snapshot_clipboard() -> Option<ClipboardSnapshot> {
    let _clipboard = Clipboard::new_attempts(10).ok()?;
    let ids: Vec<u32> = raw::EnumFormats::new().collect();
    let mut values = Vec::new();
    let mut complete = true;
    for id in ids {
        let mut bytes = Vec::new();
        match formats::RawData(id).read_clipboard(&mut bytes) {
            Ok(_) => values.push((id, bytes)),
            Err(_) => complete = false,
        }
    }
    Some(ClipboardSnapshot {
        formats: values,
        complete,
    })
}

fn restore_clipboard(snapshot: &ClipboardSnapshot) -> bool {
    let Ok(_clipboard) = Clipboard::new_attempts(10) else {
        return false;
    };
    if raw::empty().is_err() {
        return false;
    }
    let mut complete = snapshot.complete;
    for (format, data) in &snapshot.formats {
        if raw::set_without_clear(*format, data).is_err() {
            complete = false;
        }
    }
    complete
}

fn wait_for_modifiers_released() {
    for _ in 0..20 {
        let pressed = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
            .into_iter()
            .any(|key| unsafe { GetAsyncKeyState(key as i32) } < 0);
        if !pressed {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn send_copy_shortcut() -> Result<(), String> {
    fn key_input(key: u16, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    wScan: 0,
                    dwFlags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(b'C' as u16, false),
        key_input(b'C' as u16, true),
        key_input(VK_CONTROL, true),
    ];
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            mem::size_of::<INPUT>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        return Err("无法向当前程序发送复制快捷键".to_owned());
    }
    Ok(())
}
