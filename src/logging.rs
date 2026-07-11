use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use crate::config::app_data_dir;

static LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn application_log(category: &str, message: &str) {
    write_log(app_data_dir().join("application.log"), category, message);
}

pub fn crash_log(message: &str) {
    write_log(app_data_dir().join("crash.log"), "PANIC", message);
}

fn write_log(path: PathBuf, category: &str, message: &str) {
    let _guard = LOG_LOCK.get_or_init(|| Mutex::new(())).lock().ok();
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let message = message.replace(['\r', '\n'], " ");
    let _ = writeln!(
        file,
        "[{}] [{}] {}",
        chrono::Local::now().to_rfc3339(),
        category,
        message
    );
}
