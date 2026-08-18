use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, FontFamily, FontId, Frame, Id, Layout, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextStyle, UiBuilder, pos2, vec2,
};
use egui_phosphor::regular as icons;
use ksni::blocking::{Handle as TrayHandle, TrayMethods};
use open_mouse_memory::access;
use open_mouse_memory::device::{
    LogicalDevice, discover, dpi_capabilities, rate_capabilities, read_battery, read_dpi,
    read_onboard_profiles, read_onboard_status, read_rate, select_device, set_onboard_active_profile,
    set_onboard_current_dpi_index, set_onboard_mode, write_onboard_profile,
};
use open_mouse_memory::error::{AppError, Result};
use open_mouse_memory::hid::refresh_api;
use open_mouse_memory::profile::{
    ButtonAction, MAX_DPI, MAX_DPI_POINTS, MIN_DPI, MouseButton, Profile, ProfileLibrary, REPORT_RATES,
};
use serde::{Deserialize, Serialize};
use winit::platform::x11::EventLoopBuilderExtX11;

const ACCENT: Color32 = Color32::from_rgb(31, 173, 255);
const ACCENT_DARK: Color32 = Color32::from_rgb(8, 115, 176);
const SURFACE: Color32 = Color32::from_rgb(27, 30, 34);
const SURFACE_HIGH: Color32 = Color32::from_rgb(38, 42, 47);
const BACKGROUND: Color32 = Color32::from_rgb(8, 10, 12);
const MUTED: Color32 = Color32::from_rgb(146, 155, 166);
const WARNING: Color32 = Color32::from_rgb(255, 187, 80);
const ACCENT_TEXT: Color32 = Color32::BLACK;
const NOTICE_DURATION: Duration = Duration::from_secs(8);
const CACHE_SCHEMA_VERSION: u8 = 1;
const SETTINGS_SCHEMA_VERSION: u8 = 1;
const APP_SLUG: &str = "open-mouse-memory";
const AUTOSTART_MARKER: &str = "X-OpenMouseMemory-Autostart=true";
const WINDOW_BACKEND_ENV: &str = "OPEN_MOUSE_MEMORY_BACKEND";
const REFRESH_INTERVALS: [u64; 4] = [30, 60, 300, 900];
const NAV_CONTROL_SIZE: f32 = 50.0;
const NAV_ICON_SIZE: f32 = 24.0;

fn main() {
    let argument = std::env::args_os().nth(1);
    if argument.as_deref() == Some(std::ffi::OsStr::new("__install-access-rule")) {
        match access::install_rule_as_root() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("ERROR: {error}");
                std::process::exit(error.exit_code().into());
            }
        }
    }
    let start_hidden =
        matches!(argument.as_deref(), Some(value) if value == "--tray" || value == "--tray-only");

    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("open-mouse-memory")
            .with_title("Open Mouse Memory")
            .with_inner_size([1_280.0, 800.0])
            .with_min_inner_size([960.0, 640.0])
            .with_decorations(false)
            .with_icon(Arc::new(app_window_icon(64)))
            .with_visible(true),
        ..Default::default()
    };
    let window_hiding_supported = configure_window_backend(&mut options);
    options.viewport = options
        .viewport
        .with_visible(!(start_hidden && window_hiding_supported));
    if let Err(error) = eframe::run_native(
        "Open Mouse Memory",
        options,
        Box::new(move |context| {
            Ok(Box::new(OpenMouseMemoryApp::new(
                context,
                start_hidden,
                window_hiding_supported,
            )))
        }),
    ) {
        eprintln!("ERROR: {error}");
    }
}

fn configure_window_backend(options: &mut eframe::NativeOptions) -> bool {
    let use_x11 = should_use_x11_backend(
        std::env::var_os("DISPLAY").as_deref(),
        std::env::var_os(WINDOW_BACKEND_ENV).as_deref(),
    );
    if use_x11 {
        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
    use_x11
}

fn should_use_x11_backend(display: Option<&std::ffi::OsStr>, preference: Option<&std::ffi::OsStr>) -> bool {
    let x11_available = display.is_some_and(|value| !value.is_empty());
    let preference = preference
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match preference.as_deref() {
        Some("wayland") => false,
        Some("x11") => x11_available,
        _ => x11_available,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Sensitivity,
    Assignments,
    Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceSnapshot {
    name: String,
    battery: Option<u8>,
    battery_status: Option<String>,
    dpi: Option<u16>,
    dpi_min: u16,
    dpi_max: u16,
    dpi_step: u16,
    report_rate: Option<u32>,
    report_rates: Vec<u32>,
    onboard: Option<bool>,
    onboard_profiles: Option<ProfileLibrary>,
}

#[derive(Debug)]
enum DeviceState {
    Loading(String),
    Ready(DeviceSnapshot),
    Permission,
    Error(String),
}

#[derive(Debug)]
enum DeviceTask {
    Refresh { load_settings: bool },
    RefreshStatus { snapshot: DeviceSnapshot },
    SaveOnboard { slot: u8, profile: Profile },
    GrantAccess,
}

#[derive(Debug)]
enum TrayCommand {
    ShowWindow,
    Refresh,
    Quit,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeCache {
    schema_version: u8,
    snapshot: DeviceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    schema_version: u8,
    launch_on_startup: bool,
    start_in_tray: bool,
    close_to_tray: bool,
    minimize_to_tray: bool,
    auto_refresh: bool,
    refresh_interval_seconds: u64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            launch_on_startup: false,
            start_in_tray: true,
            close_to_tray: false,
            minimize_to_tray: false,
            auto_refresh: true,
            refresh_interval_seconds: 60,
        }
    }
}

struct MouseTray {
    sender: Sender<TrayCommand>,
    repaint: egui::Context,
    connected: bool,
    busy: bool,
    device_name: String,
    battery: Option<u8>,
    dpi: Option<u16>,
    polling_rate: Option<u32>,
}

impl MouseTray {
    fn new(sender: Sender<TrayCommand>, repaint: egui::Context) -> Self {
        Self {
            sender,
            repaint,
            connected: false,
            busy: true,
            device_name: "Logitech mouse".to_owned(),
            battery: None,
            dpi: None,
            polling_rate: None,
        }
    }

    fn send(&self, command: TrayCommand) {
        let _ = self.sender.send(command);
        self.repaint.request_repaint();
    }

    fn current_settings(&self) -> String {
        let mut settings = Vec::new();
        if let Some(dpi) = self.dpi {
            settings.push(format!("{dpi} DPI"));
        }
        if let Some(rate) = self.polling_rate {
            settings.push(format!("{rate} Hz"));
        }
        if let Some(battery) = self.battery {
            settings.push(format!("{battery}% battery"));
        }
        if settings.is_empty() {
            "Refreshing mouse".to_owned()
        } else {
            settings.join(" · ")
        }
    }
}

impl ksni::Tray for MouseTray {
    fn id(&self) -> String {
        "open-mouse-memory".to_owned()
    }

    fn title(&self) -> String {
        match self.battery {
            Some(battery) => format!("{} · {battery}% battery", self.device_name),
            None => self.device_name.clone(),
        }
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn status(&self) -> ksni::Status {
        if !self.connected || self.battery.is_some_and(|battery| battery <= 20) {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![mouse_tray_icon(22), mouse_tray_icon(32)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: self.icon_pixmap(),
            title: self.device_name.clone(),
            description: self.current_settings(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::ShowWindow);
    }

    fn menu_about_to_show(&mut self) {
        if !self.busy {
            self.send(TrayCommand::Refresh);
        }
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        vec![
            StandardItem {
                label: self.device_name.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Current · {}", self.current_settings()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Open Mouse Memory".to_owned(),
                icon_name: "preferences-system".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::ShowWindow)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Refresh mouse".to_owned(),
                icon_name: "view-refresh".to_owned(),
                enabled: !self.busy,
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Refresh)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".to_owned(),
                icon_name: "application-exit".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[derive(Debug)]
struct TaskResult {
    state: DeviceState,
    notice: Option<String>,
    load_settings: bool,
}

struct OpenMouseMemoryApp {
    view: View,
    settings: AppSettings,
    profiles: ProfileLibrary,
    saved_profiles: ProfileLibrary,
    dirty: bool,
    device: DeviceState,
    worker: Option<Receiver<TaskResult>>,
    tray: Option<TrayHandle<MouseTray>>,
    tray_receiver: Receiver<TrayCommand>,
    notice: Option<String>,
    notice_expires_at: Option<Instant>,
    selected_button: MouseButton,
    assignment_search: String,
    custom_key: String,
    custom_macro: String,
    last_device_refresh: Instant,
    startup_hidden_frames: u8,
    force_quit: bool,
    window_hiding_supported: bool,
}

impl OpenMouseMemoryApp {
    fn new(context: &eframe::CreationContext<'_>, start_hidden: bool, window_hiding_supported: bool) -> Self {
        configure_style(&context.egui_ctx);
        let settings = load_app_settings();
        let cached = load_runtime_cache();
        let profiles = cached
            .as_ref()
            .and_then(|snapshot| snapshot.onboard_profiles.clone())
            .unwrap_or_default();
        let device = cached
            .map(DeviceState::Ready)
            .unwrap_or_else(|| DeviceState::Loading("Looking for a Logitech mouse".to_owned()));
        let (tray_sender, tray_receiver) = mpsc::channel();
        let tray_item = MouseTray::new(tray_sender, context.egui_ctx.clone());
        let tray = tray_item.spawn().ok();
        let startup_hidden_frames = u8::from(start_hidden && tray.is_some() && window_hiding_supported) * 3;
        let mut app = Self {
            view: View::Sensitivity,
            settings,
            saved_profiles: profiles.clone(),
            profiles,
            dirty: false,
            device,
            worker: None,
            tray,
            tray_receiver,
            notice_expires_at: None,
            notice: None,
            selected_button: MouseButton::Primary,
            assignment_search: String::new(),
            custom_key: String::new(),
            custom_macro: String::new(),
            last_device_refresh: Instant::now(),
            startup_hidden_frames,
            force_quit: false,
            window_hiding_supported,
        };
        if start_hidden && !app.can_hide_to_tray() {
            let message = if app.tray.is_none() {
                "Tray-only mode needs a compatible system tray"
            } else {
                "Tray-only mode needs X11 or XWayland"
            };
            app.set_notice(message);
        }
        app.sync_tray(false);
        app.start_task(DeviceTask::Refresh { load_settings: true });
        app
    }

    fn start_task(&mut self, task: DeviceTask) {
        if self.worker.is_some() {
            return;
        }
        let message = match &task {
            DeviceTask::Refresh { .. } => "Refreshing device",
            DeviceTask::RefreshStatus { .. } => "Refreshing mouse status",
            DeviceTask::SaveOnboard { .. } => "Saving to onboard memory",
            DeviceTask::GrantAccess => "Requesting device access",
        };
        let keep_ready =
            matches!(&task, DeviceTask::Refresh { .. }) && matches!(self.device, DeviceState::Ready(_));
        if !keep_ready {
            self.device = DeviceState::Loading(message.to_owned());
        }
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = run_device_task(task);
            let _ = sender.send(result);
        });
        self.worker = Some(receiver);
        self.sync_tray(true);
    }

    fn poll_worker(&mut self, context: &egui::Context) {
        let Some(receiver) = &self.worker else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                let cache_snapshot = match &result.state {
                    DeviceState::Ready(snapshot) => Some(snapshot.clone()),
                    _ => None,
                };
                if result.load_settings {
                    if let DeviceState::Ready(snapshot) = &result.state {
                        self.load_current_settings(snapshot);
                    }
                }
                self.device = result.state;
                if let Some(snapshot) = cache_snapshot {
                    save_runtime_cache(&snapshot);
                }
                if let Some(notice) = result.notice {
                    self.set_notice(notice);
                }
                self.worker = None;
                self.last_device_refresh = Instant::now();
                self.sync_tray(false);
            }
            Err(mpsc::TryRecvError::Empty) => context.request_repaint_after(Duration::from_millis(80)),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.device = DeviceState::Error("The device worker stopped unexpectedly".to_owned());
                self.worker = None;
                self.last_device_refresh = Instant::now();
                self.sync_tray(false);
            }
        }
    }

    fn poll_tray(&mut self, context: &egui::Context) {
        while let Ok(command) = self.tray_receiver.try_recv() {
            match command {
                TrayCommand::ShowWindow => {
                    context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    context.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayCommand::Refresh => {
                    if self.worker.is_none() {
                        self.start_status_refresh();
                    }
                }
                TrayCommand::Quit => {
                    self.force_quit = true;
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn tray_is_available(&self) -> bool {
        self.tray.as_ref().is_some_and(|tray| !tray.is_closed())
    }

    fn can_hide_to_tray(&self) -> bool {
        self.window_hiding_supported && self.tray_is_available()
    }

    fn hide_window(&self, context: &egui::Context) {
        context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn request_close(&mut self, context: &egui::Context) {
        if self.settings.close_to_tray && self.can_hide_to_tray() {
            self.hide_window(context);
        } else {
            self.force_quit = true;
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn request_minimize(&self, context: &egui::Context) {
        if self.settings.minimize_to_tray && self.can_hide_to_tray() {
            self.hide_window(context);
        } else {
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }

    fn handle_window_lifecycle(&mut self, context: &egui::Context) {
        if self.tray.as_ref().is_some_and(TrayHandle::is_closed) {
            self.tray = None;
            self.startup_hidden_frames = 0;
            context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            self.set_notice("The tray service disconnected");
        }
        if self.startup_hidden_frames > 0 {
            self.startup_hidden_frames -= 1;
            self.hide_window(context);
            context.request_repaint_after(Duration::from_millis(16));
        }
        let close_requested = context.input(|input| input.viewport().close_requested());
        if close_requested && !self.force_quit && self.settings.close_to_tray && self.can_hide_to_tray() {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(context);
        }
        let minimized = context.input(|input| input.viewport().minimized == Some(true));
        if minimized && self.settings.minimize_to_tray && self.can_hide_to_tray() {
            self.hide_window(context);
        }
    }

    fn poll_auto_refresh(&mut self, context: &egui::Context) {
        if !self.settings.auto_refresh || self.worker.is_some() {
            return;
        }
        let interval = Duration::from_secs(self.settings.refresh_interval_seconds);
        let elapsed = self.last_device_refresh.elapsed();
        if elapsed >= interval {
            self.start_status_refresh();
        } else {
            context.request_repaint_after(interval - elapsed);
        }
    }

    fn start_status_refresh(&mut self) {
        match &self.device {
            DeviceState::Ready(snapshot) => self.start_task(DeviceTask::RefreshStatus {
                snapshot: snapshot.clone(),
            }),
            _ => self.start_task(DeviceTask::Refresh { load_settings: false }),
        }
    }

    fn sync_tray(&self, busy: bool) {
        let Some(handle) = &self.tray else {
            return;
        };
        let connected = matches!(self.device, DeviceState::Ready(_));
        let (device_name, battery, dpi, polling_rate) = match &self.device {
            DeviceState::Ready(device) => (
                device.name.clone(),
                device.battery,
                device.dpi,
                device.report_rate,
            ),
            _ => ("Logitech mouse".to_owned(), None, None, None),
        };
        handle.update(move |tray| {
            tray.connected = connected;
            tray.busy = busy;
            tray.device_name = device_name;
            tray.battery = battery;
            tray.dpi = dpi;
            tray.polling_rate = polling_rate;
        });
    }

    fn save_profiles(&mut self) {
        if self.worker.is_some() || !matches!(self.device, DeviceState::Ready(_)) {
            return;
        }
        self.start_task(DeviceTask::SaveOnboard {
            slot: self.profiles.selected as u8 + 1,
            profile: self.profiles.selected().clone(),
        });
    }

    fn reset_changes(&mut self) {
        self.profiles = self.saved_profiles.clone();
        self.dirty = false;
        self.set_notice("Unsaved profile changes were reset");
    }

    fn apply_app_settings(&mut self, mut updated: AppSettings) {
        updated.schema_version = SETTINGS_SCHEMA_VERSION;
        if !REFRESH_INTERVALS.contains(&updated.refresh_interval_seconds) {
            updated.refresh_interval_seconds = AppSettings::default().refresh_interval_seconds;
        }
        let previous = self.settings.clone();
        let autostart_changed = updated.launch_on_startup != previous.launch_on_startup
            || (updated.launch_on_startup && updated.start_in_tray != previous.start_in_tray);
        if autostart_changed {
            if let Err(error) = configure_autostart(updated.launch_on_startup, updated.start_in_tray) {
                updated.launch_on_startup = previous.launch_on_startup;
                updated.start_in_tray = previous.start_in_tray;
                self.set_notice(error);
            }
        }
        self.settings = updated;
        match save_app_settings(&self.settings) {
            Ok(()) if self.notice.is_none() => self.set_notice("Application settings saved"),
            Ok(()) => {}
            Err(error) => self.set_notice(error),
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        let mut updated = self.settings.clone();
        let tray_available = self.tray_is_available();
        let can_hide_to_tray = self.can_hide_to_tray();
        let settings_path = app_settings_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Configuration path unavailable".to_owned());
        let cache_path = runtime_cache_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Cache path unavailable".to_owned());
        let mut hide_now = false;
        let mut clear_cache = false;
        let mut restore_defaults = false;
        let available = ui.available_width();
        let width = available.min(780.0);
        ui.horizontal(|ui| {
            ui.add_space(((available - width) / 2.0).max(0.0));
            ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.heading("Settings");
                ui.label(
                    RichText::new(
                        "Control how Open Mouse Memory runs in the background and starts with Linux",
                    )
                    .color(MUTED),
                );
                ui.add_space(18.0);
                settings_card(
                    ui,
                    "Background and tray",
                    "The hidden mode keeps this process running without a visible window",
                    |ui| {
                        let close_to_tray_enabled = can_hide_to_tray || updated.close_to_tray;
                        let minimize_to_tray_enabled = can_hide_to_tray || updated.minimize_to_tray;
                        setting_toggle(
                            ui,
                            &mut updated.launch_on_startup,
                            "Launch at login",
                            "Use the desktop's standard per-user autostart folder",
                            true,
                        );
                        ui.separator();
                        setting_toggle(
                            ui,
                            &mut updated.start_in_tray,
                            "Start hidden in tray",
                            "Show only the mouse tray icon when started at login",
                            true,
                        );
                        ui.separator();
                        setting_toggle(
                            ui,
                            &mut updated.close_to_tray,
                            "Close to tray",
                            if self.window_hiding_supported {
                                "The close button hides the window instead of stopping the backend"
                            } else {
                                "Requires X11 or XWayland"
                            },
                            close_to_tray_enabled,
                        );
                        ui.separator();
                        setting_toggle(
                            ui,
                            &mut updated.minimize_to_tray,
                            "Minimize to tray",
                            if self.window_hiding_supported {
                                "Remove the window from the taskbar when it is minimized"
                            } else {
                                "Requires X11 or XWayland"
                            },
                            minimize_to_tray_enabled,
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let (status, color) = if can_hide_to_tray {
                                ("Tray service available", ACCENT)
                            } else if tray_available {
                                ("Window hiding needs X11 or XWayland", WARNING)
                            } else {
                                ("No compatible tray host detected", WARNING)
                            };
                            ui.label(RichText::new(status).color(color));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                hide_now = ui
                                    .add_enabled(
                                        can_hide_to_tray,
                                        accent_button(format!("{}  Hide to tray now", icons::EYE_SLASH)),
                                    )
                                    .clicked();
                            });
                        });
                    },
                );
                ui.add_space(14.0);
                settings_card(
                    ui,
                    "Background refresh",
                    "Refreshes use short hardware reads and release the mouse immediately",
                    |ui| {
                        setting_toggle(
                            ui,
                            &mut updated.auto_refresh,
                            "Refresh current settings automatically",
                            "Keep tray battery, DPI, and polling rate current while running",
                            true,
                        );
                        ui.separator();
                        ui.add_enabled_ui(updated.auto_refresh, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new("Refresh interval").strong().color(Color32::WHITE),
                                    );
                                    ui.label(
                                        RichText::new("Longer intervals use fewer device reads").color(MUTED),
                                    );
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    egui::ComboBox::from_id_salt("background_refresh_interval")
                                        .selected_text(refresh_interval_label(
                                            updated.refresh_interval_seconds,
                                        ))
                                        .width(150.0)
                                        .show_ui(ui, |ui| {
                                            for interval in REFRESH_INTERVALS {
                                                ui.selectable_value(
                                                    &mut updated.refresh_interval_seconds,
                                                    interval,
                                                    refresh_interval_label(interval),
                                                );
                                            }
                                        });
                                });
                            });
                        });
                    },
                );
                ui.add_space(14.0);
                settings_card(
                    ui,
                    "Storage",
                    "Settings and cache are kept in standard Linux user folders",
                    |ui| {
                        ui.label(RichText::new("SETTINGS FILE").small().color(MUTED));
                        ui.label(RichText::new(settings_path).color(Color32::WHITE));
                        ui.add_space(10.0);
                        ui.label(RichText::new("DEVICE CACHE").small().color(MUTED));
                        ui.label(RichText::new(cache_path).color(Color32::WHITE));
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            clear_cache = ui
                                .button(format!("{}  Clear device cache", icons::TRASH))
                                .clicked();
                            restore_defaults = ui
                                .button(format!(
                                    "{}  Restore application defaults",
                                    icons::ARROW_COUNTER_CLOCKWISE
                                ))
                                .clicked();
                        });
                        ui.label(
                            RichText::new("The device cache is recreated after the next successful refresh")
                                .small()
                                .color(MUTED),
                        );
                    },
                );
                ui.add_space(24.0);
            });
        });
        if restore_defaults {
            self.apply_app_settings(AppSettings::default());
        } else if updated != self.settings {
            self.apply_app_settings(updated);
        }
        if clear_cache {
            match clear_runtime_cache() {
                Ok(()) => self.set_notice("Device cache cleared"),
                Err(error) => self.set_notice(error),
            }
        }
        if hide_now {
            self.hide_window(ui.ctx());
        }
    }

    fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
        self.notice_expires_at = Some(Instant::now() + NOTICE_DURATION);
    }

    fn update_notice(&mut self, context: &egui::Context) {
        let Some(expires_at) = self.notice_expires_at else {
            return;
        };
        let now = Instant::now();
        if now >= expires_at {
            self.notice = None;
            self.notice_expires_at = None;
        } else {
            context.request_repaint_after(expires_at - now);
        }
    }

    fn notification_toast(&mut self, context: &egui::Context) {
        let Some(message) = self.notice.clone() else {
            return;
        };
        egui::Area::new(Id::new("notification_toast"))
            .anchor(Align2::RIGHT_TOP, vec2(-20.0, 108.0))
            .order(egui::Order::Foreground)
            .show(context, |ui| {
                Frame::new()
                    .fill(SURFACE_HIGH)
                    .stroke(Stroke::new(1.0, Color32::from_rgb(70, 77, 86)))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(message).size(16.0).strong().color(Color32::WHITE));
                            if ui.small_button("×").on_hover_text("dismiss message").clicked() {
                                self.notice = None;
                                self.notice_expires_at = None;
                            }
                        });
                    });
            });
    }

    fn load_current_settings(&mut self, snapshot: &DeviceSnapshot) {
        if let Some(profiles) = &snapshot.onboard_profiles {
            self.profiles = profiles.clone();
            self.saved_profiles = profiles.clone();
            self.dirty = false;
            self.set_notice(format!(
                "Loaded {} profiles from onboard memory",
                self.profiles.profiles.len()
            ));
            return;
        }
        let profile = self.profiles.selected_mut();
        if let Some(dpi) = snapshot.dpi {
            profile.select_dpi_value(dpi);
        }
        if let Some(report_rate) = snapshot.report_rate {
            profile.report_rate = report_rate;
        }
        self.saved_profiles = self.profiles.clone();
        self.dirty = false;
        if snapshot.dpi.is_some() || snapshot.report_rate.is_some() {
            let source = if snapshot.onboard == Some(true) {
                "onboard"
            } else {
                "device"
            };
            self.set_notice(format!("Loaded the current {source} DPI and report rate"));
        }
    }

    fn profile_toolbar(&mut self, ui: &mut egui::Ui) {
        let pending = match &self.device {
            DeviceState::Loading(_) => Some("Connecting to mouse"),
            DeviceState::Permission => Some("Device access required"),
            DeviceState::Error(_) => Some("Mouse unavailable"),
            DeviceState::Ready(_) => None,
        };
        if let Some(label) = pending {
            header_status(ui, label);
            return;
        }
        let old_selected = self.profiles.selected;
        self.profile_selector(ui);
        if old_selected != self.profiles.selected {
            self.dirty = self.profiles != self.saved_profiles;
        }
    }

    fn profile_selector(&mut self, ui: &mut egui::Ui) {
        let selected_name = profile_display_name(&self.profiles.selected().name);
        let (rect, response) = ui.allocate_exact_size(vec2(220.0, 32.0), Sense::click());
        let visuals = ui.style().interact(&response);
        ui.painter().rect_filled(rect, 3.0, visuals.bg_fill);
        ui.painter()
            .rect_stroke(rect, 3.0, visuals.bg_stroke, StrokeKind::Inside);
        ui.painter().text(
            pos2(rect.left() + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            selected_name,
            FontId::proportional(14.0),
            visuals.text_color(),
        );
        let arrow_center = pos2(rect.right() - 15.0, rect.center().y);
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                arrow_center + vec2(-5.0, -2.5),
                arrow_center + vec2(5.0, -2.5),
                arrow_center + vec2(0.0, 3.5),
            ],
            visuals.fg_stroke.color,
            Stroke::NONE,
        ));
        let names = self
            .profiles
            .profiles
            .iter()
            .map(|profile| profile_display_name(&profile.name))
            .collect::<Vec<_>>();
        egui::Popup::menu(&response).width(rect.width()).show(|ui| {
            ui.set_min_width(rect.width() - 16.0);
            for (index, name) in names.into_iter().enumerate() {
                let selected = self.profiles.selected == index;
                if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::selectable(selected, selection_text(name, selected)),
                    )
                    .clicked()
                {
                    self.profiles.selected = index;
                }
            }
        });
    }

    fn window_title_bar(&mut self, context: &egui::Context) {
        let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
        let title = self.window_title();
        egui::TopBottomPanel::top("window_title_bar")
            .exact_height(38.0)
            .frame(Frame::new().fill(Color32::from_rgb(30, 33, 37)))
            .show(context, |ui| {
                let rect = ui.max_rect();
                let controls_width = 138.0;
                let drag_rect =
                    Rect::from_min_max(rect.min, pos2(rect.right() - controls_width, rect.bottom()));
                let drag = ui.interact(drag_rect, Id::new("window_drag"), Sense::click_and_drag());
                if drag.double_clicked() {
                    context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                } else if drag.drag_started() {
                    context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    title,
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );

                let controls_rect =
                    Rect::from_min_max(pos2(rect.right() - controls_width, rect.top()), rect.max);
                let mut controls = ui.new_child(
                    UiBuilder::new()
                        .id_salt("window_controls")
                        .max_rect(controls_rect)
                        .layout(Layout::right_to_left(Align::Center)),
                );
                controls.spacing_mut().item_spacing.x = 0.0;
                if window_control(&mut controls, WindowControl::Close, maximized).clicked() {
                    self.request_close(context);
                }
                if window_control(&mut controls, WindowControl::Maximize, maximized).clicked() {
                    context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if window_control(&mut controls, WindowControl::Minimize, maximized).clicked() {
                    self.request_minimize(context);
                }
            });
    }

    fn window_title(&self) -> String {
        match &self.device {
            DeviceState::Ready(device) => {
                let name = if device.name == "PRO X 2" {
                    "PRO X2"
                } else {
                    &device.name
                };
                let battery = device
                    .battery
                    .map(|percentage| format!("{percentage}% BATTERY"))
                    .unwrap_or_else(|| "BATTERY --".to_owned());
                format!("OPEN MOUSE MEMORY  ·  {name}  -  {battery}")
            }
            DeviceState::Loading(message) => {
                format!("OPEN MOUSE MEMORY  ·  {}", message.to_ascii_uppercase())
            }
            DeviceState::Permission => "OPEN MOUSE MEMORY  ·  DEVICE ACCESS REQUIRED".to_owned(),
            DeviceState::Error(_) => "OPEN MOUSE MEMORY  ·  NO DEVICE".to_owned(),
        }
    }

    fn header(&mut self, context: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(56.0)
            .frame(
                Frame::new()
                    .fill(Color32::BLACK)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(context, |ui| {
                if self.view == View::Settings {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.heading("Application settings");
                            ui.label(RichText::new("Saved automatically").color(MUTED));
                        },
                    );
                } else {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        Layout::right_to_left(Align::Center),
                        |ui| self.profile_toolbar(ui),
                    );
                }
            });
    }

    fn action_bar(&mut self, context: &egui::Context) {
        egui::TopBottomPanel::bottom("action_bar")
            .exact_height(62.0)
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(17, 19, 22))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(46, 50, 56)))
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(context, |ui| {
                if self.view == View::Settings {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new("Application settings are saved automatically").color(MUTED),
                            );
                        },
                    );
                    return;
                }
                ui.allocate_ui_with_layout(ui.available_size(), Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            self.dirty
                                && self.worker.is_none()
                                && matches!(self.device, DeviceState::Ready(_)),
                            accent_button(format!(
                                "{}  {}",
                                icons::FLOPPY_DISK,
                                if self.dirty { "Save *" } else { "Save" }
                            ))
                            .min_size(vec2(110.0, 36.0)),
                        )
                        .on_hover_text("write these settings to onboard memory")
                        .clicked()
                    {
                        self.save_profiles();
                    }
                    if ui
                        .add_enabled_ui(self.dirty, |ui| {
                            ui.add_sized(
                                [110.0, 36.0],
                                egui::Button::new(format!("{}  Reset", icons::ARROW_COUNTER_CLOCKWISE)),
                            )
                        })
                        .inner
                        .on_hover_text("discard unsaved changes")
                        .clicked()
                    {
                        self.reset_changes();
                    }
                    ui.label(
                        RichText::new(if self.dirty {
                            "Unsaved profile changes"
                        } else {
                            "Profile is up to date"
                        })
                        .color(MUTED),
                    );
                });
            });
    }

    fn navigation(&mut self, context: &egui::Context) {
        egui::SidePanel::left("navigation")
            .exact_width(88.0)
            .resizable(false)
            .frame(Frame::new().fill(Color32::from_rgb(13, 15, 18)).inner_margin(12))
            .show(context, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    app_mark(ui);
                    ui.add_space(28.0);
                    if nav_button(
                        ui,
                        self.view == View::Sensitivity,
                        icons::SLIDERS_HORIZONTAL,
                        "sensitivity",
                    )
                    .clicked()
                    {
                        self.view = View::Sensitivity;
                    }
                    ui.add_space(10.0);
                    if nav_button(
                        ui,
                        self.view == View::Assignments,
                        icons::MOUSE_LEFT_CLICK,
                        "assignments",
                    )
                    .clicked()
                    {
                        self.view = View::Assignments;
                    }
                    ui.allocate_ui_with_layout(ui.available_size(), Layout::bottom_up(Align::Center), |ui| {
                        if refresh_icon_button(ui).clicked() {
                            self.start_task(DeviceTask::Refresh { load_settings: true });
                        }
                        ui.add_space(10.0);
                        if nav_button(
                            ui,
                            self.view == View::Settings,
                            icons::GEAR_SIX,
                            "application settings",
                        )
                        .clicked()
                        {
                            self.view = View::Settings;
                        }
                    });
                });
            });
    }

    fn device_state_view(&mut self, ui: &mut egui::Ui) {
        let height = ui.available_height().clamp(360.0, 520.0);
        let mut grant_access = false;
        let mut retry = false;
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), height),
            Layout::top_down(Align::Center),
            |ui| {
                ui.add_space(((height - 210.0) / 2.0).max(40.0));
                Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0, Color32::from_rgb(49, 54, 61)))
                    .corner_radius(14)
                    .inner_margin(24)
                    .show(ui, |ui| {
                        ui.set_min_width(320.0);
                        ui.set_max_width(320.0);
                        ui.vertical_centered(|ui| match &self.device {
                            DeviceState::Loading(message) => {
                                ui.add(egui::Spinner::new().size(28.0).color(ACCENT));
                                ui.add_space(10.0);
                                ui.heading(if message.starts_with("Saving") {
                                    "Saving to onboard memory"
                                } else {
                                    "Connecting to your mouse"
                                });
                                ui.label(RichText::new(message).color(MUTED));
                            }
                            DeviceState::Permission => {
                                ui.heading("Device access required");
                                ui.label(
                                    RichText::new("Allow access to load your onboard settings").color(MUTED),
                                );
                                ui.add_space(12.0);
                                grant_access = ui
                                    .add(accent_button(format!(
                                        "{}  Grant device access",
                                        icons::LOCK_OPEN
                                    )))
                                    .clicked();
                            }
                            DeviceState::Error(error) => {
                                ui.heading("Mouse unavailable");
                                ui.label(RichText::new(error).color(WARNING));
                                ui.add_space(12.0);
                                retry = ui
                                    .button(format!("{}  Try again", icons::ARROW_CLOCKWISE))
                                    .clicked();
                            }
                            DeviceState::Ready(_) => {}
                        });
                    });
            },
        );
        if grant_access {
            self.start_task(DeviceTask::GrantAccess);
        } else if retry {
            self.start_task(DeviceTask::Refresh { load_settings: true });
        }
    }

    fn sensitivity_view(&mut self, ui: &mut egui::Ui) {
        let (minimum, maximum, step) = match &self.device {
            DeviceState::Ready(device) => (device.dpi_min, device.dpi_max, device.dpi_step),
            _ => (MIN_DPI, MAX_DPI, 50),
        };
        let left_width = (ui.available_width() * 0.31).clamp(300.0, 350.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_min_width(left_width);
                ui.set_max_width(left_width);
                Frame::new()
                    .fill(SURFACE)
                    .corner_radius(14)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        let content_width = left_width - 32.0;
                        ui.set_min_width(content_width);
                        ui.set_max_width(content_width);
                        ui.spacing_mut().item_spacing.y = 5.0;
                        ui.heading("Sensitivity");
                        ui.label(
                            RichText::new(profile_display_name(&self.profiles.selected().name))
                                .size(18.0)
                                .strong()
                                .color(Color32::WHITE),
                        );
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("DPI STAGES").small().color(MUTED));
                        });
                        ui.add_space(4.0);
                        let colors = dpi_colors();
                        let points = self.profiles.selected().dpi_points.clone();
                        let gap = ui.spacing().item_spacing.x;
                        let columns = if content_width >= 92.0 * 3.0 + gap * 2.0 {
                            3
                        } else {
                            2
                        };
                        for row_start in (0..points.len()).step_by(columns) {
                            let row_end = (row_start + columns).min(points.len());
                            let row_width = (row_end - row_start) as f32 * 92.0
                                + (row_end - row_start).saturating_sub(1) as f32 * gap;
                            ui.allocate_ui_with_layout(
                                vec2(content_width, 34.0),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    ui.set_max_width(content_width);
                                    ui.add_space(((content_width - row_width) / 2.0).max(0.0));
                                    for index in row_start..row_end {
                                        let value = points[index];
                                        let selected = self.profiles.selected().active_dpi == index;
                                        let text_color = if selected { ACCENT_TEXT } else { colors[index] };
                                        let fill = if selected { ACCENT } else { SURFACE_HIGH };
                                        let text = RichText::new(format!("{} DPI", format_dpi(value)))
                                            .color(text_color)
                                            .strong();
                                        if ui
                                            .add_sized([92.0, 34.0], egui::Button::new(text).fill(fill))
                                            .clicked()
                                        {
                                            self.profiles.selected_mut().active_dpi = index;
                                            self.dirty = true;
                                        }
                                    }
                                },
                            );
                        }
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("POLLING RATE").small().color(MUTED));
                        });
                        ui.add_space(3.0);
                        let available_rates = match &self.device {
                            DeviceState::Ready(device) if !device.report_rates.is_empty() => {
                                device.report_rates.clone()
                            }
                            _ => REPORT_RATES.to_vec(),
                        };
                        let selected_rate = self.profiles.selected().report_rate;
                        if let Some(rate) = polling_rate_picker(ui, selected_rate, &available_rates) {
                            self.profiles.selected_mut().report_rate = rate;
                            self.dirty = true;
                        }
                        ui.add_space(14.0);
                        self.device_controls(ui);
                        ui.add_space(10.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 34.0],
                                egui::Button::new("Restore sensitivity defaults"),
                            )
                            .clicked()
                        {
                            let defaults = Profile::default();
                            let profile = self.profiles.selected_mut();
                            profile.dpi_points = defaults.dpi_points;
                            profile.active_dpi = defaults.active_dpi;
                            profile.shift_dpi = defaults.shift_dpi;
                            profile.report_rate = defaults.report_rate;
                            self.dirty = true;
                        }
                    });
            });
            ui.add_space(20.0);
            ui.vertical(|ui| {
                ui.set_min_width(ui.available_width());
                ui.heading("DPI speeds");
                ui.label(RichText::new("Add up to five points and drag them along the range").color(MUTED));
                ui.label(
                    RichText::new(format!(
                        "Mouse range  {}–{} DPI  ·  {} DPI steps",
                        format_dpi(minimum),
                        format_dpi(maximum),
                        step
                    ))
                    .small()
                    .color(MUTED),
                );
                ui.add_space(20.0);
                let changed = dpi_track(ui, self.profiles.selected_mut(), minimum, maximum, step);
                self.dirty |= changed;
                ui.add_space(16.0);
                let changed = dpi_point_editor(ui, self.profiles.selected_mut(), minimum, maximum, step);
                self.dirty |= changed;
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.profiles.selected().dpi_points.len() < MAX_DPI_POINTS,
                            egui::Button::new(format!("{}  Add DPI point", icons::PLUS)).fill(SURFACE_HIGH),
                        )
                        .clicked()
                    {
                        let next = suggested_dpi(self.profiles.selected(), maximum, step);
                        self.profiles.selected_mut().add_dpi_point(next);
                        self.dirty = true;
                    }
                });
                ui.add_space(14.0);
                Frame::new()
                    .fill(Color32::from_rgb(38, 31, 20))
                    .corner_radius(8)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Save writes these settings directly to onboard memory")
                                .color(WARNING),
                        );
                    });
            });
        });
    }

    fn device_controls(&mut self, ui: &mut egui::Ui) {
        match &self.device {
            DeviceState::Ready(device) => {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("CURRENT SETTINGS").small().color(MUTED));
                });
                ui.add_space(2.0);
                let card_width = ui.available_width();
                Frame::new()
                    .fill(SURFACE_HIGH)
                    .corner_radius(10)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        let content_width = card_width - 20.0;
                        ui.set_min_width(content_width);
                        ui.set_max_width(content_width);
                        let tile_width = ((content_width - ui.spacing().item_spacing.x) / 2.0).max(100.0);
                        ui.horizontal(|ui| {
                            current_setting_tile(
                                ui,
                                tile_width,
                                "DPI",
                                device
                                    .dpi
                                    .map(|dpi| format!("{} DPI", format_dpi(dpi)))
                                    .unwrap_or_else(|| "-- DPI".to_owned()),
                            );
                            current_setting_tile(
                                ui,
                                tile_width,
                                "POLLING RATE",
                                device
                                    .report_rate
                                    .map(format_rate)
                                    .unwrap_or_else(|| "-- Hz".to_owned()),
                            );
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("BATTERY").small().color(MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(
                                        device
                                            .battery
                                            .map(|percentage| format!("{percentage}%"))
                                            .unwrap_or_else(|| "--".to_owned()),
                                    )
                                    .size(18.0)
                                    .strong()
                                    .color(Color32::WHITE),
                                );
                                if let Some(status) = &device.battery_status {
                                    ui.label(RichText::new(friendly_status(status)).size(13.0).color(MUTED));
                                }
                            });
                        });
                        battery_bar(ui, device.battery);
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("MEMORY").small().color(MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (label, color) = match device.onboard {
                                    Some(true) => ("Onboard", ACCENT),
                                    Some(false) => ("Ready on save", WARNING),
                                    None => ("Unknown", MUTED),
                                };
                                ui.label(RichText::new(label).strong().color(color));
                            });
                        });
                    });
            }
            DeviceState::Permission => {
                ui.label(RichText::new("Permission is needed to open the mouse").color(WARNING));
                if ui
                    .button(format!("{}  Grant device access", icons::LOCK_OPEN))
                    .clicked()
                {
                    self.start_task(DeviceTask::GrantAccess);
                }
            }
            DeviceState::Error(error) => {
                ui.label(RichText::new(error).small().color(WARNING));
                if ui
                    .button(format!("{}  Try again", icons::ARROW_CLOCKWISE))
                    .clicked()
                {
                    self.start_task(DeviceTask::Refresh { load_settings: true });
                }
            }
            DeviceState::Loading(message) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(message);
                });
            }
        }
    }

    fn assignments_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(350.0);
                Frame::new()
                    .fill(SURFACE)
                    .corner_radius(14)
                    .inner_margin(20)
                    .show(ui, |ui| {
                        ui.set_width(310.0);
                        ui.heading("Assignments");
                        ui.label(
                            RichText::new(format!("Editing {} button", self.selected_button.label()))
                                .color(ACCENT)
                                .strong(),
                        );
                        ui.add_space(12.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.assignment_search)
                                .hint_text("search actions")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(12.0);
                        ScrollArea::vertical().max_height(410.0).show(ui, |ui| {
                            let query = self.assignment_search.to_ascii_lowercase();
                            for group in action_groups() {
                                let matches: Vec<_> = group
                                    .actions
                                    .into_iter()
                                    .filter(|(_, action)| {
                                        query.is_empty()
                                            || action.label().to_ascii_lowercase().contains(&query)
                                    })
                                    .collect();
                                if matches.is_empty() {
                                    continue;
                                }
                                ui.label(RichText::new(group.name).small().color(MUTED).strong());
                                for (label, action) in matches {
                                    if ui.add_sized([270.0, 30.0], egui::Button::new(label)).clicked() {
                                        self.profiles.selected_mut().assign(self.selected_button, action);
                                        self.dirty = true;
                                    }
                                }
                                ui.add_space(10.0);
                            }
                        });
                        ui.separator();
                        ui.label(RichText::new("CUSTOM KEY").small().color(MUTED));
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.custom_key).desired_width(175.0));
                            ui.add_enabled(false, egui::Button::new("Assign"))
                                .on_hover_text("onboard key encoding is not supported yet");
                        });
                        ui.label(RichText::new("MACRO NAME").small().color(MUTED));
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.custom_macro).desired_width(175.0));
                            ui.add_enabled(false, egui::Button::new("Assign"))
                                .on_hover_text("onboard macro sectors are not supported yet");
                        });
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(
                                "Mouse and DPI actions are stored onboard  ·  free-form keys and macros are not supported yet",
                            )
                            .small()
                            .color(WARNING),
                        );
                    });
            });
            ui.add_space(20.0);
            ui.vertical(|ui| {
                ui.heading("Mouse buttons");
                ui.label(
                    RichText::new("Select a button then choose its action from the library").color(MUTED),
                );
                ui.add_space(8.0);
                if let Some(button) = mouse_canvas(ui, self.profiles.selected(), self.selected_button) {
                    self.selected_button = button;
                }
            });
        });
    }
}

impl eframe::App for OpenMouseMemoryApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker(context);
        self.poll_tray(context);
        self.handle_window_lifecycle(context);
        self.poll_auto_refresh(context);
        self.update_notice(context);
        self.window_title_bar(context);
        self.header(context);
        self.navigation(context);
        self.action_bar(context);
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BACKGROUND).inner_margin(24))
            .show(context, |ui| {
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if self.view == View::Settings {
                        self.settings_view(ui);
                    } else if matches!(self.device, DeviceState::Ready(_)) {
                        match self.view {
                            View::Sensitivity => self.sensitivity_view(ui),
                            View::Assignments => self.assignments_view(ui),
                            View::Settings => unreachable!(),
                        }
                    } else {
                        self.device_state_view(ui);
                    }
                });
            });
        self.notification_toast(context);
        window_resize_handles(context);
    }
}

#[derive(Clone, Copy)]
enum WindowControl {
    Minimize,
    Maximize,
    Close,
}

fn window_control(ui: &mut egui::Ui, control: WindowControl, maximized: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(46.0, 38.0), Sense::click());
    let hovered = response.hovered();
    let fill = match (control, hovered) {
        (WindowControl::Close, true) => Color32::from_rgb(196, 43, 56),
        (_, true) => Color32::from_rgb(58, 63, 70),
        _ => Color32::TRANSPARENT,
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    let stroke = Stroke::new(1.6, Color32::WHITE);
    let center = rect.center();
    match control {
        WindowControl::Minimize => {
            ui.painter().line_segment(
                [
                    pos2(center.x - 6.0, center.y + 4.0),
                    pos2(center.x + 6.0, center.y + 4.0),
                ],
                stroke,
            );
        }
        WindowControl::Maximize if maximized => {
            let back = Rect::from_center_size(center + vec2(2.0, -2.0), vec2(11.0, 9.0));
            let front = Rect::from_center_size(center + vec2(-2.0, 2.0), vec2(11.0, 9.0));
            ui.painter().rect_stroke(back, 0.0, stroke, StrokeKind::Inside);
            ui.painter().rect_filled(front, 0.0, fill);
            ui.painter().rect_stroke(front, 0.0, stroke, StrokeKind::Inside);
        }
        WindowControl::Maximize => {
            let icon = Rect::from_center_size(center, vec2(12.0, 10.0));
            ui.painter().rect_stroke(icon, 0.0, stroke, StrokeKind::Inside);
        }
        WindowControl::Close => {
            ui.painter()
                .line_segment([center + vec2(-5.0, -5.0), center + vec2(5.0, 5.0)], stroke);
            ui.painter()
                .line_segment([center + vec2(5.0, -5.0), center + vec2(-5.0, 5.0)], stroke);
        }
    }
    response.on_hover_text(match control {
        WindowControl::Minimize => "minimize",
        WindowControl::Maximize => {
            if maximized {
                "restore"
            } else {
                "maximize"
            }
        }
        WindowControl::Close => "close",
    })
}

fn window_resize_handles(context: &egui::Context) {
    if context.input(|input| input.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let rect = context.screen_rect();
    let edge = 5.0;
    let corner = 11.0;
    let handles = [
        (
            egui::ResizeDirection::North,
            Rect::from_min_max(
                pos2(rect.left() + corner, rect.top()),
                pos2(rect.right() - corner, rect.top() + edge),
            ),
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::ResizeDirection::South,
            Rect::from_min_max(
                pos2(rect.left() + corner, rect.bottom() - edge),
                pos2(rect.right() - corner, rect.bottom()),
            ),
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::ResizeDirection::West,
            Rect::from_min_max(
                pos2(rect.left(), rect.top() + corner),
                pos2(rect.left() + edge, rect.bottom() - corner),
            ),
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::ResizeDirection::East,
            Rect::from_min_max(
                pos2(rect.right() - edge, rect.top() + corner),
                pos2(rect.right(), rect.bottom() - corner),
            ),
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::ResizeDirection::NorthWest,
            Rect::from_min_size(rect.left_top(), vec2(corner, corner)),
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::ResizeDirection::NorthEast,
            Rect::from_min_size(pos2(rect.right() - corner, rect.top()), vec2(corner, corner)),
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::ResizeDirection::SouthWest,
            Rect::from_min_size(pos2(rect.left(), rect.bottom() - corner), vec2(corner, corner)),
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::ResizeDirection::SouthEast,
            Rect::from_min_size(
                pos2(rect.right() - corner, rect.bottom() - corner),
                vec2(corner, corner),
            ),
            egui::CursorIcon::ResizeNwSe,
        ),
    ];
    for (index, (direction, handle, cursor)) in handles.into_iter().enumerate() {
        egui::Area::new(Id::new(("window_resize", index)))
            .order(egui::Order::Foreground)
            .fixed_pos(handle.min)
            .movable(false)
            .show(context, |ui| {
                let response = ui
                    .allocate_response(handle.size(), Sense::drag())
                    .on_hover_cursor(cursor);
                if response.drag_started() {
                    context.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                }
            });
    }
}

fn accent_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.into()).strong().color(ACCENT_TEXT)).fill(ACCENT)
}

fn header_status(ui: &mut egui::Ui, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(220.0, 32.0), Sense::hover());
    ui.painter().rect_filled(rect, 4.0, SURFACE_HIGH);
    ui.painter().rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_rgb(58, 64, 72)),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(14.0),
        MUTED,
    );
}

fn selection_text(text: impl Into<String>, selected: bool) -> RichText {
    let text = RichText::new(text.into());
    if selected {
        text.strong().color(ACCENT_TEXT)
    } else {
        text
    }
}

fn profile_display_name(name: &str) -> String {
    let Some(slot) = name.strip_prefix("Onboard Slot ") else {
        return name.to_owned();
    };
    if slot == "1" {
        "Onboard Memory".to_owned()
    } else {
        format!("Onboard Memory {slot}")
    }
}

fn format_rate(rate: u32) -> String {
    format!("{} Hz", format_rate_value(rate))
}

fn format_rate_value(rate: u32) -> String {
    if rate >= 1_000 {
        format!("{},{:03}", rate / 1_000, rate % 1_000)
    } else {
        rate.to_string()
    }
}

fn polling_rate_picker(ui: &mut egui::Ui, selected_rate: u32, rates: &[u32]) -> Option<u32> {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let fill = if response.hovered() {
        Color32::from_rgb(48, 53, 59)
    } else {
        SURFACE_HIGH
    };
    ui.painter().rect_filled(rect, 5.0, fill);
    ui.painter().rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, Color32::from_rgb(66, 72, 80)),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        format_rate(selected_rate),
        FontId::proportional(16.0),
        Color32::WHITE,
    );
    let arrow = pos2(rect.right() - 18.0, rect.center().y);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            arrow + vec2(-5.0, -2.5),
            arrow + vec2(5.0, -2.5),
            arrow + vec2(0.0, 3.5),
        ],
        MUTED,
        Stroke::NONE,
    ));

    let mut picked = None;
    egui::Popup::menu(&response)
        .align(egui::RectAlign::BOTTOM_START)
        .align_alternatives(&[])
        .width(rect.width())
        .frame(
            Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, Color32::from_rgb(66, 72, 80)))
                .corner_radius(6)
                .inner_margin(6),
        )
        .show(|ui| {
            ui.set_min_width(rect.width() - 12.0);
            for rate in rates.iter().copied() {
                let selected = rate == selected_rate;
                let row = egui::Button::new(
                    RichText::new(format_rate(rate))
                        .size(15.0)
                        .strong()
                        .color(Color32::WHITE),
                )
                .fill(if selected { ACCENT_DARK } else { SURFACE })
                .stroke(Stroke::NONE);
                if ui.add_sized([ui.available_width(), 31.0], row).clicked() {
                    picked = Some(rate);
                }
            }
        });
    picked
}

fn friendly_status(status: &str) -> &'static str {
    match status {
        "discharging" => "Discharging",
        "recharging" => "Charging",
        "almost-full" => "Almost full",
        "full" => "Full",
        "slow-recharge" => "Charging slowly",
        "invalid-battery" => "Battery unavailable",
        "thermal-error" => "Temperature warning",
        _ => "Status unavailable",
    }
}

fn mouse_tray_icon(size: i32) -> ksni::Icon {
    let size = size.max(16);
    let rgba = app_icon_rgba(size as u32);
    let mut data = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        data.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

fn app_window_icon(size: u32) -> egui::IconData {
    egui::IconData {
        rgba: app_icon_rgba(size),
        width: size,
        height: size,
    }
}

fn app_icon_rgba(size: u32) -> Vec<u8> {
    let mut data = vec![0; size as usize * size as usize * 4];
    let dimension = size as f32;
    for y in 0..size {
        for x in 0..size {
            let px = (x as f32 + 0.5) / dimension;
            let py = (y as f32 + 0.5) / dimension;
            let offset = ((y * size + x) * 4) as usize;
            let corner_x = px.clamp(0.22, 0.78);
            let corner_y = py.clamp(0.22, 0.78);
            let rounded_distance = ((px - corner_x).powi(2) + (py - corner_y).powi(2)).sqrt();
            if rounded_distance <= 0.18 {
                let shade = (29.0 - py * 21.0).round() as u8;
                data[offset..offset + 4].copy_from_slice(&[
                    shade,
                    shade.saturating_add(3),
                    shade.saturating_add(7),
                    255,
                ]);
            }
            let mouse_x = (px - 0.5) / 0.29;
            let mouse_y = (py - 0.51) / 0.39;
            if mouse_x * mouse_x + mouse_y * mouse_y <= 1.0 {
                data[offset..offset + 4].copy_from_slice(&[230, 235, 241, 255]);
                if (py - 0.5).abs() <= 0.012 || (px - 0.5).abs() <= 0.012 && py < 0.5 {
                    data[offset..offset + 4].copy_from_slice(&[21, 25, 30, 255]);
                }
                if (px - 0.5).abs() <= 0.055 && (0.23..=0.42).contains(&py) {
                    data[offset..offset + 4].copy_from_slice(&[21, 25, 30, 255]);
                }
                if (px - 0.5).abs() <= 0.022 && (0.27..=0.37).contains(&py) {
                    data[offset..offset + 4].copy_from_slice(&[31, 173, 255, 255]);
                }
                for dot_x in [0.41_f32, 0.5, 0.59] {
                    if (px - dot_x).powi(2) + (py - 0.68).powi(2) <= 0.025_f32.powi(2) {
                        data[offset..offset + 4].copy_from_slice(&[31, 173, 255, 255]);
                    }
                }
            }
        }
    }
    data
}

fn runtime_cache_path() -> Option<PathBuf> {
    Some(cache_home()?.join(APP_SLUG).join("device.json"))
}

fn cache_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

fn app_settings_path() -> Option<PathBuf> {
    Some(config_home()?.join(APP_SLUG).join("settings.json"))
}

fn autostart_path() -> Option<PathBuf> {
    Some(
        config_home()?
            .join("autostart")
            .join(format!("{APP_SLUG}.desktop")),
    )
}

fn load_app_settings() -> AppSettings {
    let mut settings = app_settings_path()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<AppSettings>(&bytes).ok())
        .filter(|settings| settings.schema_version == SETTINGS_SCHEMA_VERSION)
        .unwrap_or_default();
    if !REFRESH_INTERVALS.contains(&settings.refresh_interval_seconds) {
        settings.refresh_interval_seconds = AppSettings::default().refresh_interval_seconds;
    }
    settings.launch_on_startup = managed_autostart_entry();
    settings
}

fn save_app_settings(settings: &AppSettings) -> std::result::Result<(), String> {
    let path = app_settings_path().ok_or_else(|| "cannot locate the configuration folder".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "cannot locate the configuration folder".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("cannot create the configuration folder: {error}"))?;
    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("cannot encode application settings: {error}"))?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&temporary, bytes).map_err(|error| format!("cannot write application settings: {error}"))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(temporary);
        return Err(format!("cannot replace application settings: {error}"));
    }
    Ok(())
}

fn managed_autostart_entry() -> bool {
    autostart_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|entry| autostart_contents_managed(&entry))
}

fn autostart_contents_managed(entry: &str) -> bool {
    entry.contains(AUTOSTART_MARKER)
}

fn desktop_exec_path(path: &std::path::Path) -> std::result::Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| "the executable path is not valid UTF-8".to_owned())?;
    if path.contains('\n') || path.contains('\r') {
        return Err("the executable path contains an invalid line break".to_owned());
    }
    let escaped = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    Ok(format!("\"{escaped}\""))
}

fn autostart_entry(executable: &std::path::Path, start_in_tray: bool) -> std::result::Result<String, String> {
    let executable = desktop_exec_path(executable)?;
    let tray_argument = if start_in_tray { " --tray" } else { "" };
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Open Mouse Memory Tray\nComment=Keep Logitech mouse status available\nExec={executable}{tray_argument}\nIcon=open-mouse-memory\nTerminal=false\nCategories=Settings;HardwareSettings;\nX-GNOME-Autostart-enabled=true\nX-OpenMouseMemory-Autostart=true\n"
    ))
}

fn preferred_autostart_executable(
    appimage: Option<&std::ffi::OsStr>,
    current_executable: PathBuf,
) -> PathBuf {
    appimage
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or(current_executable)
}

fn autostart_executable() -> std::result::Result<PathBuf, String> {
    let current_executable = std::env::current_exe()
        .map_err(|error| format!("cannot locate the Open Mouse Memory executable: {error}"))?;
    Ok(preferred_autostart_executable(
        std::env::var_os("APPIMAGE").as_deref(),
        current_executable,
    ))
}

fn configure_autostart(enabled: bool, start_in_tray: bool) -> std::result::Result<(), String> {
    let path = autostart_path().ok_or_else(|| "cannot locate the autostart folder".to_owned())?;
    if !enabled {
        if !path.exists() {
            return Ok(());
        }
        let entry = fs::read_to_string(&path)
            .map_err(|error| format!("cannot inspect the autostart entry: {error}"))?;
        if !autostart_contents_managed(&entry) {
            return Err("the existing autostart entry is not managed by Open Mouse Memory".to_owned());
        }
        fs::remove_file(path).map_err(|error| format!("cannot remove the autostart entry: {error}"))?;
        return Ok(());
    }
    if path.exists() {
        let entry = fs::read_to_string(&path)
            .map_err(|error| format!("cannot inspect the autostart entry: {error}"))?;
        if !autostart_contents_managed(&entry) {
            return Err("the existing autostart entry is not managed by Open Mouse Memory".to_owned());
        }
    }
    let executable = autostart_executable()?;
    let entry = autostart_entry(&executable, start_in_tray)?;
    let parent = path
        .parent()
        .ok_or_else(|| "cannot locate the autostart folder".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("cannot create the autostart folder: {error}"))?;
    let temporary = path.with_extension(format!("desktop.tmp-{}", std::process::id()));
    fs::write(&temporary, entry).map_err(|error| format!("cannot write the autostart entry: {error}"))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(temporary);
        return Err(format!("cannot replace the autostart entry: {error}"));
    }
    Ok(())
}

fn clear_runtime_cache() -> std::result::Result<(), String> {
    let Some(path) = runtime_cache_path() else {
        return Err("cannot locate the device cache".to_owned());
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot clear the device cache: {error}")),
    }
}

fn load_runtime_cache() -> Option<DeviceSnapshot> {
    load_runtime_cache_from(runtime_cache_path()?)
}

fn load_runtime_cache_from(path: PathBuf) -> Option<DeviceSnapshot> {
    let bytes = fs::read(path).ok()?;
    let mut cache: RuntimeCache = serde_json::from_slice(&bytes).ok()?;
    if cache.schema_version != CACHE_SCHEMA_VERSION || !valid_cached_snapshot(&cache.snapshot) {
        return None;
    }
    cache.snapshot.battery = None;
    cache.snapshot.battery_status = None;
    Some(cache.snapshot)
}

fn save_runtime_cache(snapshot: &DeviceSnapshot) {
    if !valid_cached_snapshot(snapshot) {
        return;
    }
    let Some(path) = runtime_cache_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut snapshot = snapshot.clone();
    snapshot.battery = None;
    snapshot.battery_status = None;
    let cache = RuntimeCache {
        schema_version: CACHE_SCHEMA_VERSION,
        snapshot,
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    if fs::write(&temporary, bytes).is_ok() && fs::rename(&temporary, &path).is_ok() {
        return;
    }
    let _ = fs::remove_file(temporary);
}

fn valid_cached_snapshot(snapshot: &DeviceSnapshot) -> bool {
    if snapshot.name.trim().is_empty()
        || snapshot.dpi_min == 0
        || snapshot.dpi_min > snapshot.dpi_max
        || snapshot.dpi_step == 0
    {
        return false;
    }
    let Some(library) = &snapshot.onboard_profiles else {
        return false;
    };
    if library.profiles.is_empty() || library.selected >= library.profiles.len() {
        return false;
    }
    library.profiles.iter().all(|profile| {
        !profile.dpi_points.is_empty()
            && profile.dpi_points.len() <= MAX_DPI_POINTS
            && profile.active_dpi < profile.dpi_points.len()
            && profile
                .shift_dpi
                .is_none_or(|index| index < profile.dpi_points.len())
            && REPORT_RATES.contains(&profile.report_rate)
    })
}

fn current_setting_tile(ui: &mut egui::Ui, width: f32, title: &str, value: String) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 70.0), Sense::hover());
    ui.painter().rect_filled(rect, 8.0, BACKGROUND);
    ui.painter().text(
        pos2(rect.center().x, rect.top() + 22.0),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(13.0),
        MUTED,
    );
    ui.painter().text(
        pos2(rect.center().x, rect.bottom() - 23.0),
        Align2::CENTER_CENTER,
        value,
        FontId::proportional(19.0),
        Color32::WHITE,
    );
}

fn battery_bar(ui: &mut egui::Ui, percentage: Option<u8>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, BACKGROUND);
    if let Some(percentage) = percentage {
        let fraction = f32::from(percentage.min(100)) / 100.0;
        let fill = Rect::from_min_size(rect.min, vec2(rect.width() * fraction, rect.height()));
        let color = if percentage <= 20 { WARNING } else { ACCENT };
        ui.painter().rect_filled(fill, 3.0, color);
    }
}

fn configure_style(context: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    context.set_fonts(fonts);

    let mut style = (*context.style()).clone();
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::new(25.0, FontFamily::Proportional));
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(16.0, FontFamily::Proportional));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::new(13.0, FontFamily::Proportional));
    style.spacing.item_spacing = vec2(10.0, 8.0);
    style.spacing.button_padding = vec2(12.0, 7.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BACKGROUND;
    style.visuals.window_fill = SURFACE;
    style.visuals.widgets.inactive.bg_fill = SURFACE_HIGH;
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(49, 55, 62);
    style.visuals.widgets.active.bg_fill = ACCENT;
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, ACCENT_TEXT);
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);
    style.visuals.hyperlink_color = ACCENT;
    context.set_style(style);
}

fn app_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(NAV_CONTROL_SIZE, NAV_CONTROL_SIZE), Sense::hover());
    ui.painter().rect_filled(rect, 10.0, SURFACE_HIGH);
    ui.painter().rect_stroke(
        rect,
        10.0,
        Stroke::new(1.0, Color32::from_rgb(55, 62, 70)),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icons::MOUSE,
        FontId::proportional(NAV_ICON_SIZE),
        ACCENT,
    );
}

fn nav_button(ui: &mut egui::Ui, selected: bool, icon: &str, hint: &str) -> egui::Response {
    let fill = if selected { ACCENT } else { SURFACE_HIGH };
    let icon_color = if selected { ACCENT_TEXT } else { Color32::WHITE };
    ui.add_sized(
        [NAV_CONTROL_SIZE, NAV_CONTROL_SIZE],
        egui::Button::new(RichText::new(icon).size(NAV_ICON_SIZE).color(icon_color))
            .fill(fill)
            .corner_radius(10),
    )
    .on_hover_text(hint)
}

fn refresh_icon_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add_sized(
        [NAV_CONTROL_SIZE, NAV_CONTROL_SIZE],
        egui::Button::new(
            RichText::new(icons::ARROW_CLOCKWISE)
                .size(NAV_ICON_SIZE)
                .color(Color32::WHITE),
        )
        .corner_radius(10),
    )
    .on_hover_text("refresh device")
}

fn settings_card(ui: &mut egui::Ui, title: &str, description: &str, contents: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, Color32::from_rgb(49, 54, 61)))
        .corner_radius(12)
        .inner_margin(20)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading(title);
            ui.label(RichText::new(description).color(MUTED));
            ui.add_space(14.0);
            contents(ui);
        });
}

fn setting_toggle(ui: &mut egui::Ui, value: &mut bool, title: &str, description: &str, enabled: bool) {
    ui.add_enabled_ui(enabled, |ui| {
        let width = ui.available_width();
        ui.horizontal(|ui| {
            ui.allocate_ui(vec2((width - 62.0).max(220.0), 48.0), |ui| {
                ui.label(RichText::new(title).strong().color(Color32::WHITE));
                ui.label(RichText::new(description).color(MUTED));
            });
            settings_switch(ui, value);
        });
    });
}

fn settings_switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(48.0, 26.0), Sense::click());
    if response.clicked() {
        *value = !*value;
    }
    let fill = if *value {
        ACCENT
    } else if response.hovered() {
        Color32::from_rgb(72, 78, 86)
    } else {
        Color32::from_rgb(55, 60, 67)
    };
    ui.painter().rect_filled(rect, 13.0, fill);
    let knob_x = if *value {
        rect.right() - 13.0
    } else {
        rect.left() + 13.0
    };
    ui.painter().circle_filled(
        pos2(knob_x, rect.center().y),
        9.0,
        if *value { ACCENT_TEXT } else { Color32::WHITE },
    );
    response
}

fn refresh_interval_label(seconds: u64) -> &'static str {
    match seconds {
        30 => "Every 30 seconds",
        60 => "Every minute",
        300 => "Every 5 minutes",
        900 => "Every 15 minutes",
        _ => "Every minute",
    }
}

fn dpi_colors() -> [Color32; MAX_DPI_POINTS] {
    [
        Color32::from_rgb(220, 255, 0),
        Color32::from_rgb(45, 209, 255),
        Color32::from_rgb(255, 141, 31),
        Color32::from_rgb(255, 22, 145),
        Color32::from_rgb(132, 77, 255),
    ]
}

fn dpi_track(ui: &mut egui::Ui, profile: &mut Profile, minimum: u16, maximum: u16, step: u16) -> bool {
    let width = ui.available_width().max(420.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 165.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let track = Rect::from_min_max(
        pos2(rect.left() + 48.0, rect.center().y),
        pos2(rect.right() - 48.0, rect.center().y + 4.0),
    );
    painter.rect_filled(track, 2.0, Color32::from_rgb(72, 78, 85));
    painter.text(
        pos2(track.left(), track.bottom() + 22.0),
        Align2::LEFT_TOP,
        format_dpi(minimum),
        FontId::proportional(13.0),
        MUTED,
    );
    painter.text(
        pos2(track.right(), track.bottom() + 22.0),
        Align2::RIGHT_TOP,
        format_dpi(maximum),
        FontId::proportional(13.0),
        MUTED,
    );

    let colors = dpi_colors();
    let mut changed = false;
    for (index, color) in colors.iter().copied().enumerate().take(profile.dpi_points.len()) {
        let value = profile.dpi_points[index].clamp(minimum, maximum);
        let fraction = dpi_fraction(value, minimum, maximum);
        let position = pos2(egui::lerp(track.x_range(), fraction), track.center().y);
        let hit = Rect::from_center_size(position, vec2(28.0, 46.0));
        let response = ui.interact(hit, Id::new(("dpi_track", index)), Sense::click_and_drag());
        if response.clicked() {
            profile.active_dpi = index;
            changed = true;
        }
        if response.dragged() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let fraction = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
                let raw = dpi_from_fraction(fraction, minimum, maximum);
                let step = step.max(1) as f32;
                let snapped = ((raw / step).round() * step) as u16;
                profile.dpi_points[index] = snapped.clamp(minimum, maximum);
                changed = true;
            }
        }
        let selected = profile.active_dpi == index;
        painter.line_segment(
            [
                pos2(position.x, track.top() - 8.0),
                pos2(position.x, track.bottom() + 8.0),
            ],
            Stroke::new(if selected { 3.0 } else { 2.0 }, color),
        );
        painter.circle_filled(position, if selected { 9.0 } else { 7.0 }, color);
        painter.text(
            pos2(position.x, track.top() - 18.0),
            Align2::CENTER_BOTTOM,
            format_dpi(profile.dpi_points[index]),
            FontId::proportional(if selected { 17.0 } else { 15.0 }),
            color,
        );
        if profile.shift_dpi == Some(index) {
            let diamond = [
                pos2(position.x, track.bottom() + 15.0),
                pos2(position.x + 6.0, track.bottom() + 21.0),
                pos2(position.x, track.bottom() + 27.0),
                pos2(position.x - 6.0, track.bottom() + 21.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                diamond.to_vec(),
                Color32::WHITE,
                Stroke::NONE,
            ));
        }
    }
    changed
}

fn dpi_point_editor(ui: &mut egui::Ui, profile: &mut Profile, minimum: u16, maximum: u16, step: u16) -> bool {
    let colors = dpi_colors();
    let mut changed = false;
    let mut remove = None;
    Frame::new()
        .fill(SURFACE)
        .corner_radius(12)
        .inner_margin(14)
        .show(ui, |ui| {
            egui::Grid::new("dpi_point_editor_grid")
                .num_columns(6)
                .spacing(vec2(8.0, 8.0))
                .show(ui, |ui| {
                    for (index, color) in colors.iter().copied().enumerate().take(profile.dpi_points.len()) {
                        color_dot(ui, color);
                        ui.add_sized([58.0, 28.0], egui::Label::new(format!("Stage {}", index + 1)));
                        let mut value = profile.dpi_points[index];
                        let response = ui.add_sized(
                            [78.0, 28.0],
                            egui::DragValue::new(&mut value)
                                .range(minimum..=maximum)
                                .speed(step.max(1) as f64)
                                .suffix(" dpi"),
                        );
                        if response.changed() {
                            let step = step.max(1);
                            profile.dpi_points[index] = ((value / step) * step).clamp(minimum, maximum);
                            changed = true;
                        }
                        if ui
                            .add_sized(
                                [66.0, 28.0],
                                egui::Button::selectable(
                                    profile.active_dpi == index,
                                    selection_text("Active", profile.active_dpi == index),
                                ),
                            )
                            .clicked()
                        {
                            profile.active_dpi = index;
                            changed = true;
                        }
                        if ui
                            .add_sized(
                                [58.0, 28.0],
                                egui::Button::selectable(
                                    profile.shift_dpi == Some(index),
                                    selection_text("Shift", profile.shift_dpi == Some(index)),
                                ),
                            )
                            .clicked()
                        {
                            profile.shift_dpi = if profile.shift_dpi == Some(index) {
                                None
                            } else {
                                Some(index)
                            };
                            changed = true;
                        }
                        if ui
                            .add_enabled_ui(profile.dpi_points.len() > 1, |ui| {
                                ui.add_sized([32.0, 28.0], egui::Button::new("×"))
                            })
                            .inner
                            .on_hover_text("remove point")
                            .clicked()
                        {
                            remove = Some(index);
                        }
                        ui.end_row();
                    }
                });
        });
    if let Some(index) = remove {
        changed |= profile.remove_dpi_point(index);
    }
    changed
}

fn suggested_dpi(profile: &Profile, maximum: u16, step: u16) -> u16 {
    let highest = profile.dpi_points.iter().copied().max().unwrap_or(800);
    highest.saturating_add(step.max(50) * 8).min(maximum)
}

fn color_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

fn format_dpi(value: u16) -> String {
    if value >= 1_000 {
        format!("{},{:03}", value / 1_000, value % 1_000)
    } else {
        value.to_string()
    }
}

fn dpi_fraction(value: u16, minimum: u16, maximum: u16) -> f32 {
    let minimum = minimum.max(1) as f32;
    let maximum = (maximum as f32).max(minimum + 1.0);
    ((value.max(1) as f32).ln() - minimum.ln()) / (maximum.ln() - minimum.ln())
}

fn dpi_from_fraction(fraction: f32, minimum: u16, maximum: u16) -> f32 {
    let minimum = minimum.max(1) as f32;
    let maximum = (maximum as f32).max(minimum + 1.0);
    (minimum.ln() + fraction * (maximum.ln() - minimum.ln())).exp()
}

struct ActionGroup {
    name: &'static str,
    actions: Vec<(&'static str, ButtonAction)>,
}

fn action_groups() -> Vec<ActionGroup> {
    vec![
        ActionGroup {
            name: "MOUSE",
            actions: vec![
                ("Primary click", ButtonAction::PrimaryClick),
                ("Secondary click", ButtonAction::SecondaryClick),
                ("Middle click", ButtonAction::MiddleClick),
                ("Back", ButtonAction::Back),
                ("Forward", ButtonAction::Forward),
            ],
        },
        ActionGroup {
            name: "DPI",
            actions: vec![
                ("DPI up", ButtonAction::DpiUp),
                ("DPI down", ButtonAction::DpiDown),
                ("DPI cycle", ButtonAction::DpiCycle),
                ("DPI shift", ButtonAction::DpiShift),
            ],
        },
        ActionGroup {
            name: "SYSTEM",
            actions: vec![("Disable button", ButtonAction::Disabled)],
        },
    ]
}

fn mouse_canvas(ui: &mut egui::Ui, profile: &Profile, selected: MouseButton) -> Option<MouseButton> {
    let desired = vec2(ui.available_width().max(600.0), 540.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);
    let center_x = rect.center().x;
    let top = rect.top() + 55.0;
    let body = Rect::from_min_size(pos2(center_x - 105.0, top), vec2(210.0, 420.0));
    painter.rect_filled(body, 78.0, Color32::from_rgb(29, 32, 36));
    painter.rect_stroke(
        body,
        78.0,
        Stroke::new(2.0, Color32::from_rgb(75, 80, 87)),
        StrokeKind::Inside,
    );
    let split_y = top + 160.0;
    painter.line_segment(
        [
            pos2(body.left() + 8.0, split_y),
            pos2(body.right() - 8.0, split_y),
        ],
        Stroke::new(1.0, Color32::from_rgb(72, 76, 82)),
    );
    painter.line_segment(
        [pos2(center_x, top + 4.0), pos2(center_x, split_y)],
        Stroke::new(1.0, Color32::from_rgb(72, 76, 82)),
    );
    let wheel = Rect::from_center_size(pos2(center_x, top + 77.0), vec2(24.0, 62.0));
    painter.rect_filled(wheel, 10.0, Color32::from_rgb(9, 11, 13));
    painter.rect_stroke(wheel, 10.0, Stroke::new(2.0, MUTED), StrokeKind::Inside);
    painter.text(
        pos2(center_x, body.bottom() - 90.0),
        Align2::CENTER_CENTER,
        "LM",
        FontId::proportional(34.0),
        Color32::from_rgb(100, 105, 111),
    );

    let points = [
        (MouseButton::Primary, pos2(center_x - 53.0, top + 93.0), true),
        (MouseButton::Secondary, pos2(center_x + 53.0, top + 93.0), false),
        (MouseButton::Middle, pos2(center_x, top + 77.0), false),
        (MouseButton::Forward, pos2(body.left() - 2.0, top + 185.0), true),
        (MouseButton::Back, pos2(body.left() - 2.0, top + 238.0), true),
    ];
    let mut clicked = None;
    for (button, position, label_left) in points {
        let hit = Rect::from_center_size(position, vec2(30.0, 30.0));
        let response = ui.interact(hit, Id::new(("mouse_button", button)), Sense::click());
        if response.clicked() {
            clicked = Some(button);
        }
        let is_selected = selected == button;
        painter.circle_filled(position, if is_selected { 10.0 } else { 8.0 }, Color32::WHITE);
        painter.circle_stroke(
            position,
            12.0,
            Stroke::new(2.0, if is_selected { ACCENT } else { MUTED }),
        );
        let action = profile.action(button).label();
        if button == MouseButton::Middle {
            let anchor = pos2(position.x, top - 18.0);
            painter.line_segment(
                [position, anchor],
                Stroke::new(1.5, Color32::from_rgb(100, 106, 114)),
            );
            painter.text(
                anchor,
                Align2::CENTER_BOTTOM,
                format!("{}  ·  {action}", button.label()),
                FontId::proportional(14.0),
                if is_selected { ACCENT } else { Color32::WHITE },
            );
            continue;
        }
        let anchor_x = if label_left {
            rect.left() + 30.0
        } else {
            rect.right() - 30.0
        };
        let elbow_x = if label_left {
            position.x - 48.0
        } else {
            position.x + 48.0
        };
        painter.line_segment(
            [position, pos2(elbow_x, position.y)],
            Stroke::new(1.5, Color32::from_rgb(100, 106, 114)),
        );
        painter.line_segment(
            [pos2(elbow_x, position.y), pos2(anchor_x, position.y)],
            Stroke::new(1.5, Color32::from_rgb(100, 106, 114)),
        );
        painter.text(
            pos2(anchor_x, position.y - 4.0),
            if label_left {
                Align2::LEFT_BOTTOM
            } else {
                Align2::RIGHT_BOTTOM
            },
            format!("{}  ·  {action}", button.label()),
            FontId::proportional(14.0),
            if is_selected { ACCENT } else { Color32::WHITE },
        );
    }
    clicked
}

fn run_device_task(task: DeviceTask) -> TaskResult {
    let result = match task {
        DeviceTask::Refresh { load_settings } => {
            load_device().map(|snapshot| (snapshot, None, load_settings))
        }
        DeviceTask::RefreshStatus { snapshot } => {
            refresh_device_status(snapshot).map(|snapshot| (snapshot, None, false))
        }
        DeviceTask::SaveOnboard { slot, profile } => save_onboard_profile(slot, &profile)
            .map(|snapshot| (snapshot, Some("Saved to onboard memory".to_owned()), true)),
        DeviceTask::GrantAccess => grant_access()
            .and_then(|()| load_device())
            .map(|snapshot| (snapshot, Some("Device access granted".to_owned()), true)),
    };
    match result {
        Ok((snapshot, notice, load_settings)) => TaskResult {
            state: DeviceState::Ready(snapshot),
            notice,
            load_settings,
        },
        Err(AppError::PermissionDenied { .. }) => TaskResult {
            state: DeviceState::Permission,
            notice: None,
            load_settings: false,
        },
        Err(error) => TaskResult {
            state: DeviceState::Error(error.to_string()),
            notice: None,
            load_settings: false,
        },
    }
}

fn discover_device() -> Result<(hidapi::HidApi, LogicalDevice)> {
    let api = refresh_api()?;
    let discovery = discover(&api, false);
    let device = match select_device(&discovery.devices, None) {
        Err(AppError::NoDevice) if !discovery.permission_denied_paths.is_empty() => {
            return Err(AppError::PermissionDenied {
                path: discovery.permission_denied_paths[0].clone(),
            });
        }
        result => result?,
    };
    Ok((api, device.clone()))
}

fn load_device() -> Result<DeviceSnapshot> {
    let (api, device) = discover_device()?;
    let mut transport = device.open(&api, false)?;
    snapshot_from(&mut transport, &device)
}

fn refresh_device_status(mut snapshot: DeviceSnapshot) -> Result<DeviceSnapshot> {
    let (api, device) = discover_device()?;
    let mut transport = device.open(&api, false)?;
    if device.name != snapshot.name {
        return snapshot_from(&mut transport, &device);
    }
    let battery = read_battery(&mut transport, &device.features).ok();
    snapshot.battery = battery.as_ref().and_then(|battery| battery.percentage);
    snapshot.battery_status = battery.map(|battery| battery.status);
    snapshot.dpi = read_dpi(&mut transport, &device.features).ok().map(|dpi| dpi.x);
    snapshot.report_rate = read_rate(&mut transport, &device.features)
        .ok()
        .map(|rate| rate.hz);
    snapshot.onboard = read_onboard_status(&mut transport, &device.features)
        .ok()
        .map(|status| status.mode_code == 1);
    Ok(snapshot)
}

fn save_onboard_profile(slot: u8, profile: &Profile) -> Result<DeviceSnapshot> {
    let (api, device) = discover_device()?;
    let mut transport = device.open(&api, false)?;
    write_onboard_profile(&mut transport, &device.features, slot, profile)?;
    set_onboard_mode(&mut transport, &device.features, false)?;
    set_onboard_mode(&mut transport, &device.features, true)?;
    set_onboard_active_profile(&mut transport, &device.features, slot)?;
    set_onboard_current_dpi_index(&mut transport, &device.features, profile.active_dpi as u8)?;
    snapshot_from(&mut transport, &device)
}

fn snapshot_from<I: open_mouse_memory::hid::transport::HidIo>(
    transport: &mut open_mouse_memory::hidpp::HidppTransport<I>,
    device: &LogicalDevice,
) -> Result<DeviceSnapshot> {
    let battery = read_battery(transport, &device.features).ok();
    let dpi = read_dpi(transport, &device.features).ok();
    let dpi_capabilities = dpi_capabilities(transport, &device.features).ok();
    let rate = read_rate(transport, &device.features).ok();
    let rate_capabilities = rate_capabilities(transport, &device.features).ok();
    let onboard = read_onboard_status(transport, &device.features).ok();
    let onboard_profiles = read_onboard_profiles(transport, &device.features)
        .ok()
        .map(|profiles| {
            let selected = profiles.iter().position(|profile| profile.active).unwrap_or(0);
            ProfileLibrary {
                profiles: profiles.into_iter().map(|profile| profile.profile).collect(),
                selected,
            }
        });
    let (dpi_min, dpi_max, dpi_step) = dpi_capabilities
        .as_ref()
        .map(|capabilities| {
            (
                capabilities.minimum,
                capabilities.maximum,
                capabilities.step.unwrap_or(50).max(1),
            )
        })
        .unwrap_or((MIN_DPI, MAX_DPI, 50));
    Ok(DeviceSnapshot {
        name: device.name.clone(),
        battery: battery.as_ref().and_then(|battery| battery.percentage),
        battery_status: battery.map(|battery| battery.status),
        dpi: dpi.map(|dpi| dpi.x),
        dpi_min,
        dpi_max,
        dpi_step,
        report_rate: rate.map(|rate| rate.hz),
        report_rates: rate_capabilities
            .map(|capabilities| capabilities.rates_hz)
            .unwrap_or_default(),
        onboard: onboard.map(|status| status.mode_code == 1),
        onboard_profiles,
    })
}

fn grant_access() -> Result<()> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::Other(format!("cannot locate open-mouse-memory-gui: {error}")))?;
    let status = Command::new("/usr/bin/pkexec")
        .arg(executable)
        .arg("__install-access-rule")
        .status()
        .map_err(|error| AppError::Other(format!("cannot start PolicyKit: {error}")))?;
    if status.success() {
        thread::sleep(Duration::from_millis(800));
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "device access was not granted{}",
            status
                .code()
                .map(|code| format!("  exit {code}"))
                .unwrap_or_default()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_snapshot() -> DeviceSnapshot {
        DeviceSnapshot {
            name: "PRO X 2".to_owned(),
            battery: None,
            battery_status: None,
            dpi: Some(800),
            dpi_min: 100,
            dpi_max: 44_000,
            dpi_step: 50,
            report_rate: Some(4_000),
            report_rates: REPORT_RATES.to_vec(),
            onboard: Some(true),
            onboard_profiles: Some(ProfileLibrary::default()),
        }
    }

    #[test]
    fn validates_safe_runtime_cache() {
        let snapshot = cached_snapshot();
        assert!(valid_cached_snapshot(&snapshot));
        let mut invalid = snapshot;
        invalid.onboard_profiles.as_mut().unwrap().selected = 10;
        assert!(!valid_cached_snapshot(&invalid));
    }

    #[test]
    fn builds_argb_mouse_tray_icon() {
        let icon = mouse_tray_icon(22);
        assert_eq!(icon.data.len(), 22 * 22 * 4);
        assert!(icon.data.chunks_exact(4).any(|pixel| pixel[0] == 255));
    }

    #[test]
    fn tray_summary_only_shows_current_values() {
        let (sender, _receiver) = mpsc::channel();
        let mut tray = MouseTray::new(sender, egui::Context::default());
        tray.dpi = Some(800);
        tray.polling_rate = Some(4_000);
        assert_eq!(tray.current_settings(), "800 DPI · 4000 Hz");
        tray.battery = Some(96);
        assert_eq!(tray.current_settings(), "800 DPI · 4000 Hz · 96% battery");
    }

    #[test]
    fn application_defaults_keep_existing_window_behavior() {
        let settings = AppSettings::default();
        assert!(!settings.launch_on_startup);
        assert!(settings.start_in_tray);
        assert!(!settings.close_to_tray);
        assert!(!settings.minimize_to_tray);
        assert!(settings.auto_refresh);
        assert!(REFRESH_INTERVALS.contains(&settings.refresh_interval_seconds));
    }

    #[test]
    fn builds_owned_tray_autostart_entry() {
        let entry = autostart_entry(
            std::path::Path::new("/opt/Open Mouse Memory/open-mouse-memory-gui"),
            true,
        )
        .unwrap();
        assert!(entry.contains("Exec=\"/opt/Open Mouse Memory/open-mouse-memory-gui\" --tray"));
        assert!(entry.contains(AUTOSTART_MARKER));
        assert!(!entry.contains("OnlyShowIn="));
    }

    #[test]
    fn recognizes_managed_autostart_entries() {
        assert!(autostart_contents_managed(AUTOSTART_MARKER));
        assert!(!autostart_contents_managed("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn rejects_line_breaks_in_autostart_executable() {
        assert!(desktop_exec_path(std::path::Path::new("/tmp/logi\nmemory")).is_err());
    }

    #[test]
    fn autostart_prefers_the_appimage_path() {
        let current = PathBuf::from("/tmp/.mount_open-mouse-memory/usr/bin/open-mouse-memory-gui");
        let appimage = std::ffi::OsStr::new("/opt/Open-Mouse-Memory.AppImage");
        assert_eq!(
            preferred_autostart_executable(Some(appimage), current),
            PathBuf::from("/opt/Open-Mouse-Memory.AppImage")
        );
    }

    #[test]
    fn selects_x11_when_it_is_available() {
        assert!(should_use_x11_backend(Some(std::ffi::OsStr::new(":0")), None));
        assert!(!should_use_x11_backend(
            Some(std::ffi::OsStr::new(":0")),
            Some(std::ffi::OsStr::new("wayland"))
        ));
        assert!(!should_use_x11_backend(None, Some(std::ffi::OsStr::new("x11"))));
    }
}
