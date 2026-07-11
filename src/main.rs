#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("文件交换助手目前仅支持 Windows");

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::path::PathBuf;

    use giant_file_exchange::{
        app::FileExchangeApp, config::SettingsStore, logging::crash_log, windows,
    };
    use single_instance::SingleInstance;

    let startup_paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    std::panic::set_hook(Box::new(|info| crash_log(&info.to_string())));
    let instance = SingleInstance::new("Local\\GiantFileExchange.Singleton.7D25A9A0")?;
    if !instance.is_single() {
        windows::forward_paths(&startup_paths)?;
        return Ok(());
    }

    let (ipc_tx, ipc_rx) = std::sync::mpsc::channel();
    let _ipc_server = windows::start_pipe_server(ipc_tx)?;
    let start_hidden = !startup_paths.is_empty()
        && SettingsStore::new(SettingsStore::default_path())
            .load()
            .is_configured();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("文件交换助手")
            .with_inner_size([940.0, 640.0])
            .with_min_inner_size([760.0, 500.0])
            .with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native(
        "文件交换助手",
        options,
        Box::new(move |context| {
            Ok(Box::new(FileExchangeApp::new(
                context,
                startup_paths,
                ipc_rx,
            )?))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
