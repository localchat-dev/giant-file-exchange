use std::{env, io, path::PathBuf};

use anyhow::{Context, Result};
use winreg::{RegKey, enums::HKEY_CURRENT_USER};

const MENU_PATH: &str = r"Software\Classes\*\shell\GiantFileExchange";

fn executable_path() -> Result<PathBuf> {
    env::current_exe().context("无法确定程序路径")
}

pub fn command_for(executable: &std::path::Path) -> String {
    format!(r#""{}" "%1""#, executable.display())
}

pub fn register_context_menu() -> Result<()> {
    let executable = executable_path()?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (menu, _) = hkcu
        .create_subkey(MENU_PATH)
        .context("无法创建右键菜单注册表项")?;
    menu.set_value("", &"上传到文件交换系统")?;
    menu.set_value("MultiSelectModel", &"Player")?;
    menu.set_value("Icon", &executable.to_string_lossy().as_ref())?;
    let (command, _) = menu.create_subkey("command")?;
    command.set_value("", &command_for(&executable))?;
    Ok(())
}

pub fn unregister_context_menu() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.delete_subkey_all(MENU_PATH) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("无法移除右键菜单"),
    }
}

pub fn is_context_menu_registered() -> bool {
    let Ok(executable) = executable_path() else {
        return false;
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(command) = hkcu.open_subkey(format!(r"{MENU_PATH}\command")) else {
        return false;
    };
    command
        .get_value::<String, _>("")
        .is_ok_and(|value| value.eq_ignore_ascii_case(&command_for(&executable)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_command_paths() {
        let command = command_for(std::path::Path::new(
            r"C:\Program Files\GiantFileExchange.exe",
        ));
        assert_eq!(command, r#""C:\Program Files\GiantFileExchange.exe" "%1""#);
    }
}
