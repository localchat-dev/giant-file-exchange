use std::{collections::HashMap, fs, path::PathBuf, sync::mpsc, thread, time::Duration};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local, TimeZone};
use eframe::egui::{self, Color32, RichText};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

use crate::{
    api::{ExchangeApiClient, SpendInfo, UploadOptions},
    branding::{ICON_SIZE, icon_rgba},
    config::{AppSettings, SettingsStore, app_data_dir, normalize_token},
    logging::application_log,
    model::{UploadStatus, format_bytes},
    queue::UploadQueue,
    windows::{
        SystemNotificationKind, TextCaptureResult, capture_selected_text,
        is_context_menu_registered, protect_token, register_context_menu, show_tray_notification,
        unprotect_token, unregister_context_menu,
    },
};

const APP_BACKGROUND: Color32 = Color32::from_rgb(244, 247, 251);
const SIDEBAR_BACKGROUND: Color32 = Color32::from_rgb(20, 30, 48);
const SIDEBAR_SELECTED: Color32 = Color32::from_rgb(37, 99, 235);
const CARD_BACKGROUND: Color32 = Color32::from_rgb(255, 255, 255);
const BORDER_COLOR: Color32 = Color32::from_rgb(224, 230, 239);
const TEXT_PRIMARY: Color32 = Color32::from_rgb(24, 35, 52);
const TEXT_MUTED: Color32 = Color32::from_rgb(102, 116, 139);
const ACCENT: Color32 = Color32::from_rgb(37, 99, 235);
const SUCCESS: Color32 = Color32::from_rgb(22, 130, 70);
const DANGER: Color32 = Color32::from_rgb(185, 28, 28);
const WORKBENCH_CARD_HEIGHT: f32 = 430.0;
const WORKBENCH_ACTION_ROW_HEIGHT: f32 = 38.0;
const MANUAL_TEXT_EDITOR_HEIGHT: f32 = 176.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Transfers,
    Usage,
    Settings,
}

struct SettingsForm {
    api_base_url: String,
    token: String,
    receivers: String,
    hotkey: String,
    usage_api_base_url: String,
    usage_api_key: String,
}

impl From<&AppSettings> for SettingsForm {
    fn from(settings: &AppSettings) -> Self {
        Self {
            api_base_url: settings.api_base_url.clone(),
            token: String::new(),
            receivers: settings.receiver_users.join("\n"),
            hotkey: settings.hotkey.clone(),
            usage_api_base_url: settings.usage_api_base_url.clone(),
            usage_api_key: String::new(),
        }
    }
}

struct TrayState {
    _icon: TrayIcon,
    open_id: MenuId,
    quit_id: MenuId,
}

pub struct FileExchangeApp {
    queue: UploadQueue,
    settings_store: SettingsStore,
    settings: AppSettings,
    form: SettingsForm,
    page: Page,
    pending_files: Vec<PathBuf>,
    ipc_rx: mpsc::Receiver<Vec<PathBuf>>,
    hotkey_manager: Option<GlobalHotKeyManager>,
    hotkey: Option<HotKey>,
    tray: Option<TrayState>,
    capture_rx: Option<mpsc::Receiver<Result<TextCaptureResult, String>>>,
    usage_rx: Option<mpsc::Receiver<Result<SpendInfo, String>>>,
    task_statuses: HashMap<u64, UploadStatus>,
    banner: Option<(String, bool)>,
    manual_text: String,
    querying_usage: bool,
    usage_info: Option<SpendInfo>,
    usage_error: Option<String>,
    usage_last_fetched_at: Option<DateTime<Local>>,
    shell_registered: bool,
    confirm_exit: bool,
    exiting: bool,
}

impl FileExchangeApp {
    pub fn new(
        context: &eframe::CreationContext<'_>,
        startup_files: Vec<PathBuf>,
        ipc_rx: mpsc::Receiver<Vec<PathBuf>>,
    ) -> Result<Self> {
        configure_fonts(&context.egui_ctx);
        let settings_store = SettingsStore::new(SettingsStore::default_path());
        let settings = settings_store.load();
        let form = SettingsForm::from(&settings);
        let (tray, tray_error) = match create_tray() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(format!("无法创建系统托盘：{error}"))),
        };
        let mut app = Self {
            queue: UploadQueue::new()?,
            settings_store,
            settings,
            form,
            page: Page::Transfers,
            pending_files: Vec::new(),
            ipc_rx,
            hotkey_manager: None,
            hotkey: None,
            tray,
            capture_rx: None,
            usage_rx: None,
            task_statuses: HashMap::new(),
            banner: None,
            manual_text: String::new(),
            querying_usage: false,
            usage_info: None,
            usage_error: None,
            usage_last_fetched_at: None,
            shell_registered: is_context_menu_registered(),
            confirm_exit: false,
            exiting: false,
        };
        if let Some(error) = tray_error {
            app.set_error(error);
        }
        if !app.shell_registered {
            if let Err(error) = register_context_menu() {
                app.set_error(format!("右键菜单注册失败：{error}"));
            }
            app.shell_registered = is_context_menu_registered();
        }
        app.register_hotkey();
        if !startup_files.is_empty() {
            app.accept_files(startup_files);
            if app.settings.is_configured() {
                context
                    .egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        } else if !app.settings.is_configured() {
            app.page = Page::Settings;
        }
        Ok(app)
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.banner = Some((message.into(), true));
    }

    fn set_info(&mut self, message: impl Into<String>) {
        self.banner = Some((message.into(), false));
    }

    fn notify_system(&self, title: &str, message: &str, kind: SystemNotificationKind) {
        let Some(tray) = &self.tray else { return };
        if let Err(error) = show_tray_notification(tray._icon.window_handle(), title, message, kind)
        {
            application_log("NOTIFY", &format!("系统通知发送失败：{error}"));
        }
    }

    fn upload_options(&self) -> Result<UploadOptions> {
        self.settings.validate_without_token()?;
        let token = unprotect_token(&self.settings.encrypted_token)?;
        if token.is_empty() {
            bail!("请填写个人令牌");
        }
        Ok(UploadOptions {
            api_base_url: self.settings.api_base_url.clone(),
            token,
            receiver_users: self.settings.receiver_users.clone(),
            upload_file_name: None,
        })
    }

    fn accept_files(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<_> = paths.into_iter().filter(|path| path.is_file()).collect();
        if paths.is_empty() {
            return;
        }
        let Ok(options) = self.upload_options() else {
            self.pending_files.extend(paths);
            self.page = Page::Settings;
            self.set_error("请先配置个人令牌和默认接收人，文件将在保存后自动加入队列");
            return;
        };
        let count = paths.len();
        let errors = self.queue.add_files(paths, false, options);
        if errors.is_empty() {
            self.set_info(format!("已加入 {count} 个文件，将按顺序上传"));
            self.notify_system(
                "文件已加入队列",
                &format!("已加入 {count} 个文件，将按顺序上传"),
                SystemNotificationKind::Info,
            );
        } else {
            self.set_error(errors.join("\n"));
        }
    }

    fn save_settings(&mut self) {
        let receivers =
            AppSettings::normalize_receivers(self.form.receivers.lines().map(ToOwned::to_owned));
        let usage_api_base_url = self
            .form
            .usage_api_base_url
            .trim()
            .trim_end_matches('/')
            .to_owned();
        let usage_config_changed = self.settings.usage_api_base_url != usage_api_base_url
            || !self.form.usage_api_key.trim().is_empty();
        let mut settings = AppSettings {
            api_base_url: self
                .form
                .api_base_url
                .trim()
                .trim_end_matches('/')
                .to_owned(),
            encrypted_token: self.settings.encrypted_token.clone(),
            receiver_users: receivers,
            hotkey: self.form.hotkey.trim().to_owned(),
            usage_api_base_url: usage_api_base_url.clone(),
            encrypted_usage_api_key: if usage_api_base_url.is_empty() {
                String::new()
            } else {
                self.settings.encrypted_usage_api_key.clone()
            },
        };
        if !self.form.token.trim().is_empty() {
            let token = normalize_token(&self.form.token);
            if token.is_empty() {
                self.set_error("个人令牌无效");
                return;
            }
            match protect_token(&token) {
                Ok(encrypted) => settings.encrypted_token = encrypted,
                Err(error) => {
                    self.set_error(error.to_string());
                    return;
                }
            }
        }
        if !usage_api_base_url.is_empty() && !self.form.usage_api_key.trim().is_empty() {
            let api_key = normalize_token(&self.form.usage_api_key);
            if api_key.is_empty() {
                self.set_error("用量 API Key 无效");
                return;
            }
            match protect_token(&api_key) {
                Ok(encrypted) => settings.encrypted_usage_api_key = encrypted,
                Err(error) => {
                    self.set_error(error.to_string());
                    return;
                }
            }
        }
        if let Err(error) = self.settings_store.save(&settings) {
            self.set_error(error.to_string());
            return;
        }
        self.settings = settings;
        self.form.token.clear();
        self.form.usage_api_key.clear();
        if usage_config_changed || self.settings.usage_api_base_url.is_empty() {
            self.usage_info = None;
            self.usage_error = None;
            self.usage_last_fetched_at = None;
        }
        self.register_hotkey();
        self.set_info("设置已保存");
        if !self.pending_files.is_empty() {
            let files = std::mem::take(&mut self.pending_files);
            self.accept_files(files);
            self.page = Page::Transfers;
        }
    }

    fn register_hotkey(&mut self) {
        if let (Some(manager), Some(hotkey)) = (&self.hotkey_manager, self.hotkey.take()) {
            let _ = manager.unregister(hotkey);
        }
        if self.hotkey_manager.is_none() {
            match GlobalHotKeyManager::new() {
                Ok(manager) => self.hotkey_manager = Some(manager),
                Err(error) => {
                    self.set_error(format!("无法初始化全局快捷键：{error}"));
                    return;
                }
            }
        }
        let hotkey: HotKey = match self.settings.hotkey.parse() {
            Ok(hotkey) => hotkey,
            Err(error) => {
                self.set_error(format!("快捷键格式无效：{error}"));
                return;
            }
        };
        if let Some(manager) = &self.hotkey_manager {
            match manager.register(hotkey) {
                Ok(()) => self.hotkey = Some(hotkey),
                Err(error) => {
                    self.set_error(format!("快捷键 {} 已被占用：{error}", self.settings.hotkey))
                }
            }
        }
    }

    fn begin_text_capture(&mut self) {
        if self.capture_rx.is_some() {
            return;
        }
        if let Err(error) = self.upload_options() {
            self.page = Page::Settings;
            self.set_error(error.to_string());
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.capture_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(capture_selected_text());
        });
    }

    fn poll_external_events(&mut self, context: &egui::Context) {
        while let Ok(paths) = self.ipc_rx.try_recv() {
            if paths.is_empty() {
                show_window(context);
            } else {
                self.accept_files(paths);
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(tray) = &self.tray {
                if event.id == tray.open_id {
                    show_window(context);
                } else if event.id == tray.quit_id {
                    self.request_exit(context);
                }
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_window(context);
            }
        }
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state == HotKeyState::Pressed
                && self.hotkey.is_some_and(|hotkey| hotkey.id() == event.id)
            {
                self.begin_text_capture();
            }
        }

        let capture_result = self.capture_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = capture_result {
            self.capture_rx = None;
            match result {
                Ok(result) => self.enqueue_text(result),
                Err(error) => self.set_error(error),
            }
        }
    }

    fn enqueue_text_upload(&mut self, text: String) -> Result<()> {
        let mut options = self.upload_options()?;
        options.upload_file_name = Some("message.txt".to_owned());
        let directory = app_data_dir().join("Temp");
        fs::create_dir_all(&directory).map_err(|error| anyhow!("无法创建文本临时目录：{error}"))?;
        let name = format!(
            "selected-text-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")
        );
        let path = directory.join(name);
        fs::write(&path, text.as_bytes())
            .map_err(|error| anyhow!("无法创建文本临时文件：{error}"))?;
        let errors = self.queue.add_files([path], true, options);
        if let Some(error) = errors.first() {
            bail!(error.clone());
        }
        Ok(())
    }

    fn enqueue_manual_text(&mut self) {
        if self.manual_text.trim().is_empty() {
            self.set_error("请输入要发送的文本内容");
            return;
        }
        let text = self.manual_text.clone();
        match self.enqueue_text_upload(text) {
            Ok(()) => {
                self.manual_text.clear();
                self.set_info("手动输入的文本已加入上传队列");
                self.notify_system(
                    "文本已加入队列",
                    "message.txt 将按顺序上传",
                    SystemNotificationKind::Info,
                );
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn usage_query_options(&self) -> Result<(String, String)> {
        let base_url = self.settings.usage_api_base_url.trim();
        if base_url.is_empty() {
            bail!("请先在设置中填写用量查询地址");
        }
        let api_key = unprotect_token(&self.settings.encrypted_usage_api_key)?;
        if api_key.trim().is_empty() {
            bail!("请先在设置中填写用量 API Key");
        }
        Ok((base_url.to_owned(), api_key))
    }

    fn begin_usage_query(&mut self) {
        if self.querying_usage {
            return;
        }
        let (api_base_url, api_key) = match self.usage_query_options() {
            Ok(options) => options,
            Err(error) => {
                self.page = Page::Settings;
                self.set_error(error.to_string());
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        self.querying_usage = true;
        self.usage_error = None;
        self.usage_rx = Some(rx);
        thread::spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => match ExchangeApiClient::new() {
                    Ok(api) => runtime
                        .block_on(api.query_usage(&api_base_url, &api_key))
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(format!("无法创建用量查询运行时：{error}")),
            };
            let _ = tx.send(result);
        });
    }

    fn poll_usage_query(&mut self) {
        let Some(rx) = &self.usage_rx else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.usage_rx = None;
                self.querying_usage = false;
                match result {
                    Ok(info) => {
                        self.usage_info = Some(info);
                        self.usage_error = None;
                        self.usage_last_fetched_at = Some(Local::now());
                        self.set_info("用量信息已更新");
                    }
                    Err(error) => {
                        self.usage_error = Some(error.clone());
                        self.set_error(format!("用量查询失败：{error}"));
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.usage_rx = None;
                self.querying_usage = false;
                self.usage_error = Some("后台查询线程已断开".to_owned());
                self.set_error("用量查询失败：后台查询线程已断开");
            }
        }
    }

    fn enqueue_text(&mut self, result: TextCaptureResult) {
        match self.enqueue_text_upload(result.text) {
            Ok(()) if result.clipboard_restored => {
                self.set_info("选中文本已加入上传队列");
                self.notify_system(
                    "文本已加入队列",
                    "message.txt 将按顺序上传",
                    SystemNotificationKind::Info,
                );
            }
            Ok(()) => {
                self.set_error("文本已加入队列，但未能完整恢复原剪贴板");
                self.notify_system(
                    "文本已加入队列",
                    "message.txt 已加入，但未能完整恢复原剪贴板",
                    SystemNotificationKind::Warning,
                );
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn poll_task_notifications(&mut self) {
        let mut notifications = Vec::new();
        for task in self.queue.tasks() {
            let previous = self.task_statuses.insert(task.id, task.status);
            if previous == Some(task.status) {
                continue;
            }
            match task.status {
                UploadStatus::Succeeded => notifications.push((
                    "上传成功".to_owned(),
                    format!("{} 已上传完成", task.file_name),
                    SystemNotificationKind::Info,
                )),
                UploadStatus::Failed => notifications.push((
                    "上传失败".to_owned(),
                    if task.error.is_empty() {
                        format!("{} 上传失败", task.file_name)
                    } else {
                        format!("{}：{}", task.file_name, task.error)
                    },
                    SystemNotificationKind::Error,
                )),
                _ => {}
            }
        }
        self.task_statuses
            .retain(|id, _| self.queue.tasks().iter().any(|task| task.id == *id));
        for (title, message, kind) in notifications {
            self.notify_system(&title, &message, kind);
        }
    }

    fn request_exit(&mut self, context: &egui::Context) {
        if self.queue.active_count() > 0 {
            self.confirm_exit = true;
        } else {
            self.exiting = true;
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn transfers_ui(&mut self, ui: &mut egui::Ui) {
        let total = self.queue.tasks().len();
        let active = self.queue.active_count();
        let succeeded = self
            .queue
            .tasks()
            .iter()
            .filter(|task| task.status == UploadStatus::Succeeded)
            .count();
        let failed = self
            .queue
            .tasks()
            .iter()
            .filter(|task| task.status == UploadStatus::Failed)
            .count();
        let manual_text_lines = if self.manual_text.is_empty() {
            0
        } else {
            self.manual_text.lines().count()
        };
        let manual_text_bytes = self.manual_text.as_bytes().len() as u64;
        let manual_text_chars = self.manual_text.chars().count();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("传输工作台")
                                .size(26.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.label(
                            RichText::new("统一管理文件上传与文本投递")
                                .size(13.0)
                                .color(TEXT_MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let add_file_button = egui::Button::new(
                            RichText::new("添加文件").strong().color(Color32::WHITE),
                        )
                        .fill(ACCENT)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(8);
                        if ui.add_sized([110.0, 38.0], add_file_button).clicked()
                            && let Some(files) = rfd::FileDialog::new()
                                .set_title("选择要上传的文件")
                                .pick_files()
                        {
                            self.accept_files(files);
                        }
                    });
                });
                ui.add_space(18.0);

                ui.columns(4, |columns| {
                    metric_card(&mut columns[0], "任务总数", total, "当前会话", ACCENT);
                    metric_card(
                        &mut columns[1],
                        "正在处理",
                        active,
                        "顺序上传",
                        Color32::from_rgb(180, 83, 9),
                    );
                    metric_card(&mut columns[2], "已成功", succeeded, "已完成上传", SUCCESS);
                    metric_card(&mut columns[3], "失败任务", failed, "可重试", DANGER);
                });
                ui.add_space(18.0);
                section_card(ui, |ui| {
                    self.manual_text_card(
                        ui,
                        manual_text_lines,
                        manual_text_chars,
                        manual_text_bytes,
                    );
                });
                ui.add_space(18.0);

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("传输列表")
                            .size(16.0)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                    if total > 0 {
                        ui.label(
                            RichText::new(format!("{active} 个活动任务"))
                                .small()
                                .color(TEXT_MUTED),
                        );
                    }
                });
                ui.add_space(10.0);

                if self.queue.tasks().is_empty() {
                    egui::Frame::new()
                        .fill(CARD_BACKGROUND)
                        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
                        .corner_radius(12)
                        .inner_margin(32)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.vertical_centered(|ui| {
                                ui.add_space(18.0);
                                ui.label(
                                    RichText::new("当前还没有上传任务")
                                        .size(18.0)
                                        .strong()
                                        .color(TEXT_PRIMARY),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("可以添加文件，或在上方手动输入文本后发送到队列")
                                        .color(TEXT_MUTED),
                                );
                                ui.add_space(18.0);
                            });
                        });
                    return;
                }

                let mut action = None;
                for task in self.queue.tasks() {
                    egui::Frame::new()
                        .fill(CARD_BACKGROUND)
                        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
                        .corner_radius(10)
                        .inner_margin(16)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new(&task.file_name)
                                            .size(15.0)
                                            .strong()
                                            .color(TEXT_PRIMARY),
                                    );
                                    let source_text = if task.temporary {
                                        "来源：手动文本 / 临时内容".to_owned()
                                    } else {
                                        format!("来源：{}", task.path.display())
                                    };
                                    ui.label(RichText::new(source_text).small().color(TEXT_MUTED));
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let (foreground, background) = status_colors(task.status);
                                        egui::Frame::new()
                                            .fill(background)
                                            .corner_radius(8)
                                            .inner_margin(egui::Margin::symmetric(10, 5))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    RichText::new(task.status.text())
                                                        .small()
                                                        .strong()
                                                        .color(foreground),
                                                );
                                            });
                                    },
                                );
                            });
                            ui.add_space(12.0);
                            ui.add(
                                egui::ProgressBar::new(task.progress())
                                    .desired_height(8.0)
                                    .fill(status_colors(task.status).0)
                                    .animate(task.status == UploadStatus::Uploading),
                            );
                            ui.add_space(8.0);
                            ui.horizontal_wrapped(|ui| {
                                let progress = format!(
                                    "{} / {}",
                                    format_bytes(task.bytes_sent),
                                    format_bytes(task.file_size)
                                );
                                ui.label(RichText::new(progress).small().color(TEXT_MUTED));
                                if task.status == UploadStatus::Uploading {
                                    ui.label(
                                        RichText::new(format!(
                                            "{}/s",
                                            format_bytes(
                                                task.speed_bytes_per_second.max(0.0) as u64
                                            )
                                        ))
                                        .small()
                                        .color(TEXT_MUTED),
                                    );
                                }
                                if let Some(id) = &task.server_file_id {
                                    ui.label(
                                        RichText::new(format!("文件 ID：{id}"))
                                            .small()
                                            .color(TEXT_MUTED),
                                    );
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(
                                                !task.status.is_active(),
                                                egui::Button::new("移除"),
                                            )
                                            .clicked()
                                        {
                                            action = Some(("remove", task.id));
                                        }
                                        if ui
                                            .add_enabled(
                                                task.status.can_retry(),
                                                egui::Button::new("重试"),
                                            )
                                            .clicked()
                                        {
                                            action = Some(("retry", task.id));
                                        }
                                        if ui
                                            .add_enabled(
                                                task.status.is_active(),
                                                egui::Button::new("取消"),
                                            )
                                            .clicked()
                                        {
                                            action = Some(("cancel", task.id));
                                        }
                                    },
                                );
                            });
                            if !task.error.is_empty() {
                                ui.add_space(6.0);
                                ui.label(RichText::new(&task.error).small().color(DANGER));
                            }
                        });
                    ui.add_space(10.0);
                }

                if let Some((name, id)) = action {
                    match name {
                        "cancel" => self.queue.cancel(id),
                        "retry" => {
                            if let Err(error) = self.queue.retry(id) {
                                self.set_error(error.to_string());
                            }
                        }
                        "remove" => self.queue.remove(id),
                        _ => {}
                    }
                }
            });
    }

    fn manual_text_card(
        &mut self,
        ui: &mut egui::Ui,
        manual_text_lines: usize,
        manual_text_chars: usize,
        manual_text_bytes: u64,
    ) {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), WORKBENCH_CARD_HEIGHT),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("手动输入文本")
                                .size(18.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.label(
                            RichText::new(
                                "适合临时说明、链接、口令或纯文本片段，统一以 message.txt 发送",
                            )
                            .small()
                            .color(TEXT_MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::Frame::new()
                            .fill(Color32::from_rgb(247, 250, 253))
                            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(223, 231, 240)))
                            .corner_radius(8)
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} 行 / {} 字",
                                        manual_text_lines, manual_text_chars
                                    ))
                                    .small()
                                    .color(TEXT_PRIMARY),
                                );
                            });
                    });
                });
                ui.add_space(12.0);

                ui.horizontal_wrapped(|ui| {
                    for (label, value) in [
                        ("字符数", manual_text_chars.to_string()),
                        ("估算大小", format_bytes(manual_text_bytes)),
                        (
                            "状态",
                            if self.manual_text.trim().is_empty() {
                                "待输入".to_owned()
                            } else {
                                "可发送".to_owned()
                            },
                        ),
                    ] {
                        egui::Frame::new()
                            .fill(Color32::from_rgb(248, 250, 252))
                            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(228, 234, 241)))
                            .corner_radius(8)
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(label).small().color(TEXT_MUTED));
                                    ui.label(
                                        RichText::new(value).small().strong().color(TEXT_PRIMARY),
                                    );
                                });
                            });
                    }
                });
                ui.add_space(12.0);

                egui::Frame::new()
                    .fill(Color32::from_rgb(249, 251, 253))
                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(222, 230, 239)))
                    .corner_radius(10)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        let editor = egui::TextEdit::multiline(&mut self.manual_text)
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .hint_text("在这里输入需要投递的文本内容…");
                        ui.add_sized([ui.available_width(), MANUAL_TEXT_EDITOR_HEIGHT], editor);
                    });
                ui.add_space(14.0);
                ui.add_space((ui.available_height() - WORKBENCH_ACTION_ROW_HEIGHT).max(0.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;

                    if ui
                        .add_enabled(
                            !self.manual_text.trim().is_empty(),
                            sized_primary_button("发送文本", 104.0, 38.0),
                        )
                        .clicked()
                    {
                        self.enqueue_manual_text();
                    }

                    if ui
                        .add_sized([96.0, 38.0], secondary_button("清空内容"))
                        .clicked()
                    {
                        self.manual_text.clear();
                    }
                });
            },
        );
    }

    fn usage_ui(&mut self, ui: &mut egui::Ui) {
        let has_config = self.settings.has_usage_query_config();
        let spend_value = self
            .usage_info
            .as_ref()
            .map(|info| format_usage_currency(Some(info.spend)))
            .unwrap_or_else(|| "--".to_owned());
        let budget_value = self
            .usage_info
            .as_ref()
            .map(|info| format_usage_currency(info.max_budget))
            .unwrap_or_else(|| "--".to_owned());
        let budget_cycle = self
            .usage_info
            .as_ref()
            .and_then(|info| info.budget_duration.as_deref())
            .unwrap_or("未返回")
            .to_owned();
        let budget_reset = self
            .usage_info
            .as_ref()
            .map(|info| format_usage_date(info.budget_reset_at.as_deref()))
            .unwrap_or_else(|| "尚未执行查询".to_owned());
        let last_active = self
            .usage_info
            .as_ref()
            .map(|info| format_usage_date(info.last_active.as_deref()))
            .unwrap_or_else(|| "尚未执行查询".to_owned());
        let last_refresh = self
            .usage_last_fetched_at
            .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "尚无记录".to_owned());
        let (state_text, state_foreground, state_background, state_caption) = if !has_config {
            (
                "未配置",
                Color32::from_rgb(146, 64, 14),
                Color32::from_rgb(255, 247, 237),
                "请先到设置页补全地址与 API Key",
            )
        } else if self.querying_usage {
            (
                "查询中",
                ACCENT,
                Color32::from_rgb(239, 246, 255),
                "正在请求最新用量数据",
            )
        } else if self.usage_error.is_some() {
            (
                "查询失败",
                DANGER,
                Color32::from_rgb(254, 242, 242),
                "接口返回异常，请检查地址或凭证",
            )
        } else if self.usage_info.is_some() {
            (
                "已更新",
                SUCCESS,
                Color32::from_rgb(236, 253, 245),
                "最近一次查询已成功完成",
            )
        } else {
            (
                "待查询",
                TEXT_MUTED,
                Color32::from_rgb(241, 245, 249),
                "点击右上角按钮获取最新用量数据",
            )
        };
        let has_query_result = self.usage_info.is_some()
            || self.usage_error.is_some()
            || self.usage_last_fetched_at.is_some();
        let detail_label = if self.usage_error.is_some() {
            "错误信息"
        } else {
            "接口状态"
        };
        let detail_value = self
            .usage_error
            .clone()
            .unwrap_or_else(|| state_caption.to_owned());

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let content_width = ui.available_width().min(860.0);
                ui.set_width(content_width);
                ui.set_max_width(content_width);

                let mut trigger_query = false;
                let mut clear_result = false;
                let mut open_settings = false;
                let compact_header = content_width < 760.0;

                if compact_header {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("用量查询")
                                .size(26.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.label(
                            RichText::new("查看预算、花费、活跃状态与最近刷新时间")
                                .size(13.0)
                                .color(TEXT_MUTED),
                        );
                    });
                    ui.add_space(12.0);
                    ui.horizontal_wrapped(|ui| {
                        let (query, clear, settings) = usage_toolbar_buttons(
                            ui,
                            has_config,
                            self.querying_usage,
                            has_query_result,
                        );
                        trigger_query = query;
                        clear_result = clear;
                        open_settings = settings;
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new("用量查询")
                                    .size(26.0)
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            );
                            ui.label(
                                RichText::new("查看预算、花费、活跃状态与最近刷新时间")
                                    .size(13.0)
                                    .color(TEXT_MUTED),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (query, clear, settings) = usage_toolbar_buttons(
                                ui,
                                has_config,
                                self.querying_usage,
                                has_query_result,
                            );
                            trigger_query = query;
                            clear_result = clear;
                            open_settings = settings;
                        });
                    });
                }

                if trigger_query {
                    self.begin_usage_query();
                }
                if clear_result {
                    self.usage_info = None;
                    self.usage_error = None;
                    self.usage_last_fetched_at = None;
                }
                if open_settings {
                    self.page = Page::Settings;
                }

                ui.add_space(18.0);

                if !has_config {
                    section_card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.vertical_centered(|ui| {
                            ui.add_space(10.0);
                            ui.label(
                                RichText::new("当前还没有用量查询配置")
                                    .size(18.0)
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            );
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(
                                    "请先到设置页填写查询地址和 API Key，保存后再返回此页。",
                                )
                                .size(13.0)
                                .color(TEXT_MUTED),
                            );
                            ui.add_space(16.0);
                            if ui
                                .add_sized([108.0, 38.0], secondary_button("前往设置"))
                                .clicked()
                            {
                                self.page = Page::Settings;
                            }
                            ui.add_space(6.0);
                        });
                    });
                    return;
                }

                if content_width >= 620.0 {
                    ui.columns(2, |columns| {
                        usage_value_metric_card(
                            &mut columns[0],
                            "累计花费",
                            &spend_value,
                            "本次查询返回",
                            ACCENT,
                        );
                        usage_value_metric_card(
                            &mut columns[1],
                            "预算上限",
                            &budget_value,
                            "接口预算限制",
                            Color32::from_rgb(14, 116, 144),
                        );
                    });
                    ui.add_space(12.0);
                    ui.columns(2, |columns| {
                        usage_value_metric_card(
                            &mut columns[0],
                            "预算周期",
                            &budget_cycle,
                            "预算窗口长度",
                            Color32::from_rgb(180, 83, 9),
                        );
                        usage_status_metric_card(
                            &mut columns[1],
                            "查询状态",
                            state_text,
                            state_caption,
                            state_foreground,
                            state_background,
                        );
                    });
                } else {
                    usage_value_metric_card(ui, "累计花费", &spend_value, "本次查询返回", ACCENT);
                    ui.add_space(12.0);
                    usage_value_metric_card(
                        ui,
                        "预算上限",
                        &budget_value,
                        "接口预算限制",
                        Color32::from_rgb(14, 116, 144),
                    );
                    ui.add_space(12.0);
                    usage_value_metric_card(
                        ui,
                        "预算周期",
                        &budget_cycle,
                        "预算窗口长度",
                        Color32::from_rgb(180, 83, 9),
                    );
                    ui.add_space(12.0);
                    usage_status_metric_card(
                        ui,
                        "查询状态",
                        state_text,
                        state_caption,
                        state_foreground,
                        state_background,
                    );
                }

                ui.add_space(18.0);
                section_card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("查询概览")
                                .size(16.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.add_space(8.0);
                        usage_status_badge(ui, state_text, state_foreground, state_background);
                    });
                    ui.label(
                        RichText::new("最近一次查询结果与预算状态")
                            .size(13.0)
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(16.0);

                    if ui.available_width() >= 620.0 {
                        ui.columns(2, |columns| {
                            usage_detail_tile(&mut columns[0], "最近活跃", &last_active);
                            usage_detail_tile(&mut columns[1], "上次刷新", &last_refresh);
                        });
                        ui.add_space(12.0);
                        ui.columns(2, |columns| {
                            usage_detail_tile(&mut columns[0], "预算重置", &budget_reset);
                            usage_detail_tile(&mut columns[1], detail_label, &detail_value);
                        });
                    } else {
                        usage_detail_tile(ui, "最近活跃", &last_active);
                        ui.add_space(12.0);
                        usage_detail_tile(ui, "上次刷新", &last_refresh);
                        ui.add_space(12.0);
                        usage_detail_tile(ui, "预算重置", &budget_reset);
                        ui.add_space(12.0);
                        usage_detail_tile(ui, detail_label, &detail_value);
                    }
                });
            });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("系统设置")
                .size(26.0)
                .strong()
                .color(TEXT_PRIMARY),
        );
        ui.label(
            RichText::new("配置上传接口、用量查询能力与资源管理器集成")
                .size(13.0)
                .color(TEXT_MUTED),
        );
        ui.add_space(18.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let card_width = ui.available_width().min(820.0);
                let content_width = (card_width - 40.0).max(260.0);
                ui.set_width(card_width);
                ui.set_max_width(card_width);

                section_card(ui, |ui| {
                    ui.set_width(content_width);
                    ui.set_max_width(content_width);
                    ui.label(
                        RichText::new("上传配置")
                            .size(17.0)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new("用于文件上传与文本投递，默认接收人会自动用于每个任务")
                            .small()
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(16.0);

                    form_label(ui, "API 地址", "需为 HTTPS 地址");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.api_base_url)
                            .desired_width(content_width)
                            .hint_text("https://example.invalid"),
                    );
                    ui.add_space(12.0);

                    form_label(ui, "个人令牌", "留空表示继续使用已保存的令牌");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.token)
                            .desired_width(content_width)
                            .password(true)
                            .hint_text("输入新的上传令牌"),
                    );
                    ui.add_space(12.0);

                    form_label(ui, "默认接收人", "每行一个域账号，可同时发送给多人");
                    ui.add(
                        egui::TextEdit::multiline(&mut self.form.receivers)
                            .desired_width(content_width)
                            .desired_rows(5)
                            .hint_text("zhangsan\nlisi"),
                    );
                    ui.add_space(12.0);

                    form_label(ui, "选中文本快捷键", "例如 Ctrl+Alt+U");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.hotkey)
                            .desired_width(content_width)
                            .hint_text("Ctrl+Alt+U"),
                    );
                });
                ui.add_space(14.0);

                section_card(ui, |ui| {
                    ui.set_width(content_width);
                    ui.set_max_width(content_width);
                    ui.label(
                        RichText::new("用量查询配置")
                            .size(17.0)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new(
                            "仅参考接口协议，不复用原项目界面；支持直接填写带 /v1 的地址",
                        )
                        .small()
                        .color(TEXT_MUTED),
                    );
                    ui.add_space(16.0);

                    form_label(
                        ui,
                        "用量查询地址",
                        "支持 HTTP/HTTPS，以及 /v1、/v1/responses、/v1/chat/completions",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.usage_api_base_url)
                            .desired_width(content_width)
                            .hint_text("http://your-api-host/v1"),
                    );
                    ui.add_space(12.0);

                    form_label(ui, "用量 API Key", "留空表示继续使用已保存的 Key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.usage_api_key)
                            .desired_width(content_width)
                            .password(true)
                            .hint_text("sk-..."),
                    );
                    ui.add_space(10.0);
                    let status_text = if self.settings.has_usage_query_config() {
                        "已存在可用的用量查询配置"
                    } else {
                        "当前尚未完成用量查询配置"
                    };
                    ui.label(RichText::new(status_text).small().color(
                        if self.settings.has_usage_query_config() {
                            SUCCESS
                        } else {
                            TEXT_MUTED
                        },
                    ));
                });
                ui.add_space(14.0);

                section_card(ui, |ui| {
                    ui.set_width(content_width);
                    ui.set_max_width(content_width);
                    ui.label(
                        RichText::new("资源管理器集成")
                            .size(17.0)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new(if self.shell_registered {
                            "右键菜单已注册，可将文件快速送入上传队列"
                        } else {
                            "右键菜单未注册，可能是首次安装或系统环境发生变化"
                        })
                        .small()
                        .color(if self.shell_registered {
                            SUCCESS
                        } else {
                            DANGER
                        }),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("注册 / 修复").clicked() {
                            match register_context_menu() {
                                Ok(()) => self.set_info("右键菜单已注册"),
                                Err(error) => self.set_error(error.to_string()),
                            }
                            self.shell_registered = is_context_menu_registered();
                        }
                        if ui.button("移除菜单").clicked() {
                            match unregister_context_menu() {
                                Ok(()) => self.set_info("右键菜单已移除"),
                                Err(error) => self.set_error(error.to_string()),
                            }
                            self.shell_registered = is_context_menu_registered();
                        }
                    });
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!(
                            "配置文件：{}",
                            self.settings_store.path().display()
                        ))
                        .small()
                        .color(TEXT_MUTED),
                    );
                    ui.label(
                        RichText::new("关闭窗口后应用仍会驻留系统托盘，不会中断当前上传任务")
                            .small()
                            .color(TEXT_MUTED),
                    );
                    ui.label(
                        RichText::new("传输方向固定为：办公外网 → 内网")
                            .small()
                            .color(TEXT_MUTED),
                    );
                });
                ui.add_space(18.0);

                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [132.0, 40.0],
                            egui::Button::new(
                                RichText::new("保存全部配置").strong().color(Color32::WHITE),
                            )
                            .fill(ACCENT)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(8),
                        )
                        .clicked()
                    {
                        self.save_settings();
                    }
                    ui.label(
                        RichText::new("保存后会立即应用快捷键与新配置")
                            .small()
                            .color(TEXT_MUTED),
                    );
                });
            });
    }
}

impl eframe::App for FileExchangeApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_external_events(context);
        self.queue.poll();
        self.poll_usage_query();
        self.poll_task_notifications();
        if let Some(tray) = &self.tray {
            let count = self.queue.active_count();
            let tooltip = if count == 0 {
                "文件交换助手".to_owned()
            } else {
                format!("文件交换助手 - {count} 个活动任务")
            };
            let _ = tray._icon.set_tooltip(Some(tooltip));
        }

        if context.input(|input| input.viewport().close_requested()) && !self.exiting {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        context.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = root.ctx().clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(APP_BACKGROUND))
            .show(root, |ui| {
                let available_size = ui.available_size();
                ui.allocate_ui_with_layout(
                    available_size,
                    egui::Layout::left_to_right(egui::Align::Min),
                    |ui| {
                        let available_height = ui.available_height();
                        egui::Frame::new()
                            .fill(SIDEBAR_BACKGROUND)
                            .inner_margin(18)
                            .show(ui, |ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(176.0, (available_height - 36.0).max(0.0)),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.set_width(176.0);
                                        ui.set_min_height((available_height - 36.0).max(0.0));
                                        ui.horizontal(|ui| {
                                            egui::Frame::new()
                                                .fill(ACCENT)
                                                .corner_radius(9)
                                                .inner_margin(8)
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        RichText::new("↑")
                                                            .size(18.0)
                                                            .strong()
                                                            .color(Color32::WHITE),
                                                    );
                                                });
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new("文件交换")
                                                        .size(17.0)
                                                        .strong()
                                                        .color(Color32::WHITE),
                                                );
                                                ui.label(
                                                    RichText::new("外网上传助手")
                                                        .small()
                                                        .color(Color32::from_rgb(148, 163, 184)),
                                                );
                                            });
                                        });
                                        ui.add_space(28.0);

                                        if sidebar_button(
                                            ui,
                                            "↑",
                                            "上传任务",
                                            self.page == Page::Transfers,
                                        ) {
                                            self.page = Page::Transfers;
                                        }
                                        ui.add_space(6.0);
                                        if sidebar_button(
                                            ui,
                                            "◔",
                                            "用量查询",
                                            self.page == Page::Usage,
                                        ) {
                                            self.page = Page::Usage;
                                        }
                                        ui.add_space(6.0);
                                        if sidebar_button(
                                            ui,
                                            "⚙",
                                            "设置",
                                            self.page == Page::Settings,
                                        ) {
                                            self.page = Page::Settings;
                                        }
                                    },
                                );
                            });

                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width());
                            ui.add_space(16.0);
                            if let Some((message, is_error)) = &self.banner {
                                let foreground = if *is_error { DANGER } else { SUCCESS };
                                let background = if *is_error {
                                    Color32::from_rgb(254, 242, 242)
                                } else {
                                    Color32::from_rgb(240, 253, 244)
                                };
                                let mut close_banner = false;
                                egui::Frame::new()
                                    .fill(background)
                                    .stroke(egui::Stroke::new(1.0, foreground.gamma_multiply(0.25)))
                                    .corner_radius(8)
                                    .inner_margin(egui::Margin::symmetric(12, 8))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(message).color(foreground));
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui.small_button("×").clicked() {
                                                        close_banner = true;
                                                    }
                                                },
                                            );
                                        });
                                    });
                                if close_banner {
                                    self.banner = None;
                                }
                                ui.add_space(10.0);
                            }
                            egui::Frame::new()
                                .inner_margin(egui::Margin::symmetric(18, 4))
                                .show(ui, |ui| match self.page {
                                    Page::Transfers => self.transfers_ui(ui),
                                    Page::Usage => self.usage_ui(ui),
                                    Page::Settings => self.settings_ui(ui),
                                });
                        });
                    },
                );
            });

        if self.confirm_exit {
            egui::Window::new("确认退出")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&context, |ui| {
                    ui.label("仍有上传任务未完成，退出将取消这些任务。确定退出吗？");
                    ui.horizontal(|ui| {
                        if ui.button("继续上传").clicked() {
                            self.confirm_exit = false;
                        }
                        if ui
                            .button(RichText::new("退出").color(Color32::from_rgb(185, 28, 28)))
                            .clicked()
                        {
                            self.queue.cancel_all();
                            self.exiting = true;
                            self.confirm_exit = false;
                            context.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }
    }
}

fn sidebar_button(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool) -> bool {
    let fill = if selected {
        SIDEBAR_SELECTED
    } else {
        Color32::TRANSPARENT
    };
    let foreground = if selected {
        Color32::WHITE
    } else {
        Color32::from_rgb(203, 213, 225)
    };
    ui.add_sized(
        [ui.available_width(), 40.0],
        egui::Button::new(
            RichText::new(format!("{icon}   {label}"))
                .strong()
                .color(foreground),
        )
        .fill(fill)
        .stroke(egui::Stroke::NONE)
        .corner_radius(8),
    )
    .clicked()
}

fn metric_card(ui: &mut egui::Ui, label: &str, value: usize, caption: &str, accent: Color32) {
    egui::Frame::new()
        .fill(CARD_BACKGROUND)
        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
        .corner_radius(10)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).small().color(TEXT_MUTED));
                    ui.label(
                        RichText::new(value.to_string())
                            .size(25.0)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::Frame::new()
                        .fill(accent.gamma_multiply(0.1))
                        .corner_radius(8)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.label(RichText::new("●").color(accent));
                        });
                });
            });
            ui.label(RichText::new(caption).small().color(TEXT_MUTED));
        });
}

fn status_colors(status: UploadStatus) -> (Color32, Color32) {
    match status {
        UploadStatus::Succeeded => (SUCCESS, Color32::from_rgb(236, 253, 245)),
        UploadStatus::Failed => (DANGER, Color32::from_rgb(254, 242, 242)),
        UploadStatus::Cancelled => (TEXT_MUTED, Color32::from_rgb(241, 245, 249)),
        UploadStatus::Processing => (
            Color32::from_rgb(126, 34, 206),
            Color32::from_rgb(250, 245, 255),
        ),
        _ => (ACCENT, Color32::from_rgb(239, 246, 255)),
    }
}

fn section_card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD_BACKGROUND)
        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
        .corner_radius(12)
        .inner_margin(20)
        .show(ui, add_contents)
        .inner
}

fn primary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(label)
            .size(13.0)
            .strong()
            .color(Color32::WHITE),
    )
    .fill(ACCENT)
    .stroke(egui::Stroke::NONE)
    .corner_radius(8)
}

fn sized_primary_button(label: &str, width: f32, height: f32) -> egui::Button<'_> {
    primary_button(label).min_size(egui::vec2(width, height))
}

fn secondary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).size(13.0).strong().color(TEXT_PRIMARY))
        .fill(Color32::from_rgb(245, 248, 252))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(211, 220, 231)))
        .corner_radius(8)
}

fn usage_toolbar_buttons(
    ui: &mut egui::Ui,
    has_config: bool,
    querying_usage: bool,
    has_query_result: bool,
) -> (bool, bool, bool) {
    let mut trigger_query = false;
    let mut clear_result = false;
    let mut open_settings = false;

    ui.spacing_mut().item_spacing.x = 10.0;
    ui.spacing_mut().item_spacing.y = 10.0;

    if ui
        .add_enabled(
            !querying_usage && has_config,
            sized_primary_button(
                if querying_usage {
                    "查询中"
                } else {
                    "立即查询"
                },
                108.0,
                38.0,
            ),
        )
        .clicked()
    {
        trigger_query = true;
    }

    if ui
        .add_enabled(
            has_query_result,
            secondary_button("清除结果").min_size(egui::vec2(100.0, 38.0)),
        )
        .clicked()
    {
        clear_result = true;
    }

    if !has_config
        && ui
            .add_sized([100.0, 38.0], secondary_button("前往设置"))
            .clicked()
    {
        open_settings = true;
    }

    (trigger_query, clear_result, open_settings)
}

fn usage_value_metric_card(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    caption: &str,
    accent: Color32,
) {
    egui::Frame::new()
        .fill(CARD_BACKGROUND)
        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
        .corner_radius(10)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(104.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).size(12.0).color(TEXT_MUTED));
                    ui.add_space(6.0);
                    ui.label(RichText::new(value).size(24.0).strong().color(TEXT_PRIMARY));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::Frame::new()
                        .fill(accent.gamma_multiply(0.10))
                        .corner_radius(8)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.label(RichText::new("●").color(accent));
                        });
                });
            });
            ui.add_space(10.0);
            ui.label(RichText::new(caption).size(12.0).color(TEXT_MUTED));
        });
}

fn usage_status_metric_card(
    ui: &mut egui::Ui,
    label: &str,
    status: &str,
    caption: &str,
    foreground: Color32,
    background: Color32,
) {
    egui::Frame::new()
        .fill(CARD_BACKGROUND)
        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
        .corner_radius(10)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(104.0);
            ui.label(RichText::new(label).size(12.0).color(TEXT_MUTED));
            ui.add_space(10.0);
            usage_status_badge(ui, status, foreground, background);
            ui.add_space(10.0);
            ui.label(RichText::new(caption).size(12.0).color(TEXT_MUTED));
        });
}

fn usage_status_badge(ui: &mut egui::Ui, label: &str, foreground: Color32, background: Color32) {
    egui::Frame::new()
        .fill(background)
        .corner_radius(9)
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(12.5).strong().color(foreground));
        });
}

fn usage_detail_tile(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(248, 250, 252))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(228, 234, 241)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(72.0);
            ui.label(RichText::new(label).size(12.0).color(TEXT_MUTED));
            ui.add_space(8.0);
            ui.label(RichText::new(value).size(14.0).strong().color(TEXT_PRIMARY));
        });
}

fn format_usage_currency(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("${value:.2}"),
        None => "未返回".to_owned(),
    }
}

fn format_usage_date(value: Option<&str>) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return "未返回".to_owned();
    };
    if let Ok(timestamp) = value.parse::<i64>() {
        let local = if value.len() > 10 || timestamp.abs() > 10_000_000_000 {
            Local.timestamp_millis_opt(timestamp).single()
        } else {
            Local.timestamp_opt(timestamp, 0).single()
        };
        if let Some(local) = local {
            return local.format("%Y-%m-%d %H:%M:%S").to_string();
        }
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return parsed
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
    }
    value.to_owned()
}

fn form_label(ui: &mut egui::Ui, label: &str, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).strong().color(TEXT_PRIMARY));
        ui.label(RichText::new(hint).small().color(TEXT_MUTED));
    });
    ui.add_space(4.0);
}

fn show_window(context: &egui::Context) {
    context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    context.send_viewport_cmd(egui::ViewportCommand::Focus);
}

fn create_tray() -> Result<TrayState> {
    let menu = Menu::new();
    let open = MenuItem::new("打开传输中心", true, None);
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::new("退出", true, None);
    menu.append_items(&[&open, &separator, &quit])?;
    let icon = make_icon()?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("文件交换助手")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;
    Ok(TrayState {
        _icon: tray,
        open_id: open.id().clone(),
        quit_id: quit.id().clone(),
    })
}

fn make_icon() -> Result<Icon> {
    Icon::from_rgba(icon_rgba(), ICON_SIZE, ICON_SIZE).map_err(|error| anyhow!(error.to_string()))
}

fn configure_fonts(context: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = APP_BACKGROUND;
    visuals.window_fill = CARD_BACKGROUND;
    visuals.extreme_bg_color = Color32::from_rgb(248, 250, 252);
    visuals.widgets.inactive.corner_radius = 7.into();
    visuals.widgets.hovered.corner_radius = 7.into();
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(206, 216, 228));
    visuals.widgets.active.corner_radius = 7.into();
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(191, 219, 254));
    context.set_visuals(visuals);

    let mut style = (*context.style_of(egui::Theme::Light)).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size.y = 36.0;
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(12.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(13.0, egui::FontFamily::Monospace),
    );
    context.set_style_of(egui::Theme::Light, style);

    let windows = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let fonts_dir = windows.join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded_fonts = Vec::new();

    for (name, file) in [
        ("windows-yahei", "msyh.ttc"),
        ("windows-yahei-light", "msyhl.ttc"),
        ("windows-deng", "Deng.ttf"),
        ("windows-segoe", "segoeui.ttf"),
    ] {
        let path = fonts_dir.join(file);
        let Ok(bytes) = fs::read(path) else { continue };
        fonts
            .font_data
            .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
        loaded_fonts.push(name.to_owned());
    }

    if loaded_fonts.is_empty() {
        return;
    }

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let entry = fonts.families.entry(family).or_default();
        for name in loaded_fonts.iter().rev() {
            entry.insert(0, name.clone());
        }
    }
    context.set_fonts(fonts);
}
