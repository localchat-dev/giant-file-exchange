use std::{fs, path::PathBuf, sync::mpsc, thread, time::Duration};

use anyhow::{Result, anyhow, bail};
use eframe::egui::{self, Color32, RichText};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

use crate::{
    api::UploadOptions,
    config::{AppSettings, SettingsStore, app_data_dir, normalize_token},
    model::{UploadStatus, format_bytes},
    queue::UploadQueue,
    windows::{
        TextCaptureResult, capture_selected_text, is_context_menu_registered, protect_token,
        register_context_menu, unprotect_token, unregister_context_menu,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Transfers,
    Settings,
}

struct SettingsForm {
    api_base_url: String,
    token: String,
    receivers: String,
    exchange_type: u8,
    hotkey: String,
}

impl From<&AppSettings> for SettingsForm {
    fn from(settings: &AppSettings) -> Self {
        Self {
            api_base_url: settings.api_base_url.clone(),
            token: String::new(),
            receivers: settings.receiver_users.join("\n"),
            exchange_type: settings.exchange_type,
            hotkey: settings.hotkey.clone(),
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
    banner: Option<(String, bool)>,
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
            banner: None,
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
            exchange_type: self.settings.exchange_type,
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
        } else {
            self.set_error(errors.join("\n"));
        }
    }

    fn save_settings(&mut self) {
        let receivers =
            AppSettings::normalize_receivers(self.form.receivers.lines().map(ToOwned::to_owned));
        let mut settings = AppSettings {
            api_base_url: self
                .form
                .api_base_url
                .trim()
                .trim_end_matches('/')
                .to_owned(),
            encrypted_token: self.settings.encrypted_token.clone(),
            receiver_users: receivers,
            exchange_type: self.form.exchange_type,
            hotkey: self.form.hotkey.trim().to_owned(),
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
        if let Err(error) = self.settings_store.save(&settings) {
            self.set_error(error.to_string());
            return;
        }
        self.settings = settings;
        self.form.token.clear();
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

    fn enqueue_text(&mut self, result: TextCaptureResult) {
        let options = match self.upload_options() {
            Ok(options) => options,
            Err(error) => {
                self.set_error(error.to_string());
                return;
            }
        };
        let directory = app_data_dir().join("Temp");
        if let Err(error) = fs::create_dir_all(&directory) {
            self.set_error(format!("无法创建文本临时目录：{error}"));
            return;
        }
        let name = format!(
            "selected-text-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")
        );
        let path = directory.join(name);
        if let Err(error) = fs::write(&path, result.text.as_bytes()) {
            self.set_error(format!("无法创建文本临时文件：{error}"));
            return;
        }
        let errors = self.queue.add_files([path], true, options);
        if let Some(error) = errors.first() {
            self.set_error(error.clone());
        } else if result.clipboard_restored {
            self.set_info("选中文本已加入上传队列");
        } else {
            self.set_error("文本已加入队列，但未能完整恢复原剪贴板");
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
        ui.horizontal(|ui| {
            if ui.button("添加文件").clicked()
                && let Some(files) = rfd::FileDialog::new()
                    .set_title("选择要上传的文件")
                    .pick_files()
            {
                self.accept_files(files);
            }
            let total = self.queue.tasks().len();
            let active = self.queue.active_count();
            ui.separator();
            ui.label(if total == 0 {
                "暂无任务".to_owned()
            } else {
                format!("共 {total} 个任务，{active} 个正在处理")
            });
        });
        ui.add_space(8.0);

        let mut action = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for task in self.queue.tasks() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_min_width(210.0);
                            ui.label(RichText::new(&task.file_name).strong());
                            ui.label(
                                RichText::new(format_bytes(task.file_size))
                                    .small()
                                    .color(Color32::GRAY),
                            );
                        });
                        ui.vertical(|ui| {
                            ui.set_min_width(300.0);
                            ui.add(egui::ProgressBar::new(task.progress()).show_percentage());
                            let progress = format!(
                                "{} / {}",
                                format_bytes(task.bytes_sent),
                                format_bytes(task.file_size)
                            );
                            let speed = if task.status == UploadStatus::Uploading {
                                format!(
                                    "  ·  {}/s",
                                    format_bytes(task.speed_bytes_per_second.max(0.0) as u64)
                                )
                            } else {
                                String::new()
                            };
                            ui.label(
                                RichText::new(format!("{progress}{speed}"))
                                    .small()
                                    .color(Color32::GRAY),
                            );
                        });
                        ui.vertical(|ui| {
                            ui.set_min_width(130.0);
                            let color = match task.status {
                                UploadStatus::Succeeded => Color32::from_rgb(22, 130, 70),
                                UploadStatus::Failed => Color32::from_rgb(185, 28, 28),
                                UploadStatus::Cancelled => Color32::GRAY,
                                _ => Color32::from_rgb(37, 99, 235),
                            };
                            ui.label(RichText::new(task.status.text()).strong().color(color));
                            if let Some(id) = &task.server_file_id {
                                ui.label(RichText::new(format!("ID: {id}")).small());
                            }
                            if !task.error.is_empty() {
                                ui.label(
                                    RichText::new(&task.error)
                                        .small()
                                        .color(Color32::from_rgb(185, 28, 28)),
                                );
                            }
                        });
                        ui.vertical(|ui| {
                            if ui
                                .add_enabled(task.status.is_active(), egui::Button::new("取消"))
                                .clicked()
                            {
                                action = Some(("cancel", task.id));
                            }
                            if ui
                                .add_enabled(task.status.can_retry(), egui::Button::new("重试"))
                                .clicked()
                            {
                                action = Some(("retry", task.id));
                            }
                            if ui
                                .add_enabled(!task.status.is_active(), egui::Button::new("移除"))
                                .clicked()
                            {
                                action = Some(("remove", task.id));
                            }
                        });
                    });
                });
                ui.add_space(6.0);
            }
        });
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
        if self.queue.tasks().is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("还没有上传任务");
                ui.label("可以点击“添加文件”，或在资源管理器右键菜单中上传文件。");
            });
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_max_width(680.0);
            ui.heading("上传设置");
            ui.add_space(10.0);
            ui.label("API 基地址");
            ui.text_edit_singleline(&mut self.form.api_base_url);
            ui.add_space(8.0);
            ui.label("个人令牌（留空表示不修改）");
            ui.add(egui::TextEdit::singleline(&mut self.form.token).password(true));
            ui.add_space(8.0);
            ui.label("默认接收人域账号（每行一个）");
            ui.add(egui::TextEdit::multiline(&mut self.form.receivers).desired_rows(5));
            ui.add_space(8.0);
            ui.label("默认传输方向");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.form.exchange_type, 1, "研发内网 → 办公网");
                ui.radio_value(&mut self.form.exchange_type, 2, "办公网 → 研发内网");
            });
            ui.add_space(8.0);
            ui.label("选中文本上传快捷键");
            ui.text_edit_singleline(&mut self.form.hotkey);
            if ui.button("保存设置").clicked() {
                self.save_settings();
            }
            ui.add_space(18.0);
            ui.separator();
            ui.add_space(12.0);
            ui.heading("资源管理器右键菜单");
            ui.label(if self.shell_registered {
                "右键菜单已注册到当前程序路径。"
            } else {
                "右键菜单未注册，或便携程序的位置已经变化。"
            });
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
            ui.add_space(12.0);
            ui.label(
                RichText::new(format!(
                    "配置文件：{}",
                    self.settings_store.path().display()
                ))
                .small()
                .color(Color32::GRAY),
            );
            ui.label(
                RichText::new("应用不会创建开机启动项；关闭窗口后仍会驻留托盘。")
                    .small()
                    .color(Color32::GRAY),
            );
        });
    }
}

impl eframe::App for FileExchangeApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_external_events(context);
        self.queue.poll();
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
        egui::CentralPanel::default().show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading("文件交换助手");
                ui.separator();
                ui.selectable_value(&mut self.page, Page::Transfers, "上传任务");
                ui.selectable_value(&mut self.page, Page::Settings, "设置");
            });
            ui.separator();
            if let Some((message, is_error)) = &self.banner {
                let color = if *is_error {
                    Color32::from_rgb(185, 28, 28)
                } else {
                    Color32::from_rgb(22, 130, 70)
                };
                let mut close_banner = false;
                ui.horizontal(|ui| {
                    ui.colored_label(color, message);
                    if ui.small_button("×").clicked() {
                        close_banner = true;
                    }
                });
                if close_banner {
                    self.banner = None;
                }
                ui.separator();
            }
            match self.page {
                Page::Transfers => self.transfers_ui(ui),
                Page::Settings => self.settings_ui(ui),
            }
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
    let mut rgba = vec![0_u8; 32 * 32 * 4];
    for y in 0..32 {
        for x in 0..32 {
            let index = (y * 32 + x) * 4;
            let inside = (3..29).contains(&x) && (3..29).contains(&y);
            if inside {
                rgba[index..index + 4].copy_from_slice(&[37, 99, 235, 255]);
                if (14..18).contains(&x) || ((10..22).contains(&x) && (17..21).contains(&y)) {
                    rgba[index..index + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
    }
    Icon::from_rgba(rgba, 32, 32).map_err(|error| anyhow!(error.to_string()))
}

fn configure_fonts(context: &egui::Context) {
    let windows = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let candidates = ["msyh.ttc", "msyhbd.ttc", "simhei.ttf"];
    for candidate in candidates {
        let path = windows.join("Fonts").join(candidate);
        let Ok(bytes) = fs::read(path) else { continue };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "windows-cjk".to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, "windows-cjk".to_owned());
        }
        context.set_fonts(fonts);
        return;
    }
}
