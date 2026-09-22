//! 常驻管理页及托盘。调度独立于渲染，隐藏窗口不会停止任务或归档。
use crate::{
    ipc,
    manager::Manager,
    protocol::{Backend, Profile, Profiles, Routing},
    runner, store, ui,
};
use anyhow::Result;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, IsWindowVisible, SW_HIDE, SW_RESTORE, SetForegroundWindow, ShowWindow,
};

/// App 决定新任务的执行配置，Harness 只提供工作内容。
#[derive(Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    dcr: crate::dcr::DcrConfig,
    backend: Backend,
    allow_edits: bool,
    #[serde(default = "default_clean_start")]
    clean_start: bool,
    #[serde(default = "default_max_parallel")]
    max_parallel: usize,
    timeout_seconds: u64,
    #[serde(default = "crate::protocol::default_retry_timeout_seconds")]
    retry_timeout_seconds: u64,
    #[serde(default = "default_archive_hours")]
    archive_hours: u64,
    #[serde(default)]
    floating: bool,
    #[serde(default = "default_floating_limit")]
    floating_limit: usize,
    #[serde(default = "default_floating_idle_minutes")]
    floating_idle_minutes: u64,
    #[serde(default)]
    main_size: Option<[f32; 2]>,
    #[serde(default)]
    floating_bottom: Option<[f32; 2]>,
    #[serde(default)]
    archived: bool,
    /// Antigravity CLI 启用出站代理。
    #[serde(default)]
    agy_proxy_enabled: bool,
    /// Antigravity CLI SOCKS5 代理地址，格式 ip:port。
    #[serde(default)]
    agy_proxy_socks5: String,
}

/// 首次启动及已有设置新增该选项时，默认不自动加载用户和项目指令文件。
fn default_clean_start() -> bool {
    true
}

/// 首次启动及旧设置默认最多同时执行三个任务，其余按提交顺序排队。
fn default_max_parallel() -> usize {
    3
}

/// 尚未设置归档时间时保留四小时。
fn default_archive_hours() -> u64 {
    4
}

/// 未保存数量设置时，悬浮窗默认最多展示四个对话。
fn default_floating_limit() -> usize {
    4
}

/// 悬浮监控默认在三十分钟无活动后隐藏，运行任务不受该期限影响。
fn default_floating_idle_minutes() -> u64 {
    30
}

/// 仅隐藏达到无活动期限的终态卡片；运行中保留，归档和删除仍由既有筛选处理。
#[cfg(test)]
fn floating_record_visible(record: &store::Record, now: i64, idle_minutes: u64) -> bool {
    record.running() || now.saturating_sub(record.last_activity) < (idle_minutes * 60_000) as i64
}

/// UI 编辑缓存与唯一管理器共享；无第二套执行状态机。
struct Dashboard {
    dcr: crate::dcr::DcrService,
    dcr_open: bool,
    dcr_error: String,
    main_hwnd: usize,
    floating_position: Arc<Mutex<FloatingPosition>>,
    floating: Arc<AtomicBool>,
    floating_ready: Arc<AtomicBool>,
    floating_native: Arc<Mutex<ui::OverlayWindow>>,
    logo: egui::TextureHandle,
    manager: Arc<Mutex<Manager>>,
    config: PathBuf,
    preferences: Preferences,
    editing_profiles: Profiles,
    model_choices: Vec<String>,
    model_variants: std::collections::BTreeMap<String, Vec<String>>,
    model_backend: Backend,
    model_refresh: Option<mpsc::Receiver<Result<crate::opencode::Catalog, String>>>,
    selected: Option<String>,
    detail_tab: u8,
    detail_content: String,
    archived: bool,
    error: String,
    confirm_exit: bool,
    exit_request: Arc<AtomicBool>,
    _tray: TrayIcon,
}

/// 底边坐标在拖动和伸缩后记忆；重置中心请求由悬浮视口按实际尺寸消费。
#[derive(Default)]
struct FloatingPosition {
    bottom: Option<[f32; 2]>,
    reset_center: Option<egui::Pos2>,
}

/// 恢复窗口无需等待隐藏窗口的绘制事件，避免托盘恢复死锁。
fn reveal(hwnd: usize) {
    unsafe {
        ShowWindow(hwnd as _, SW_RESTORE);
        SetForegroundWindow(hwnd as _);
    }
}

/// 左键释放时切换窗口；隐藏/最小化都视为待恢复，右键交给原生退出菜单。
fn tray_visibility(event: &TrayIconEvent, visible: bool, minimized: bool) -> Option<bool> {
    matches!(
        event,
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
    )
    .then_some(!visible || minimized)
}

impl Dashboard {
    /// 顶部设置仅管理 DCR 服务，日志统一展示在主页面会话中。
    fn dcr_window(&mut self, ctx: &egui::Context) {
        if !self.dcr_open { return; }
        let mut open = self.dcr_open;
        let before = serde_json::to_value(&self.preferences.dcr).unwrap();
        egui::Window::new("DCR · 网页端本机连接")
            .open(&mut open).default_size([560., 180.]).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(self.dcr.status());
                    if ui.add_enabled(!self.dcr.running(), egui::Button::new("启动")).clicked() {
                        self.dcr_error = self.dcr.start(&self.preferences.dcr)
                            .err().map(|e| format!("{e:#}")).unwrap_or_default();
                    }
                    if ui.add_enabled(self.dcr.running(), egui::Button::new("停止")).clicked() {
                        self.dcr_error = self.dcr.stop()
                            .err().map(|e| format!("{e:#}")).unwrap_or_default();
                    }
                    ui.checkbox(&mut self.preferences.dcr.auto_start, "随 AR 启动");
                });
                ui.add_enabled_ui(!self.dcr.running(), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("出站代理");
                        ui.text_edit_singleline(&mut self.preferences.dcr.proxy);
                    });
                    ui.horizontal(|ui| {
                        ui.label("工作目录");
                        let mut workdir = self.preferences.dcr.workdir.to_string_lossy().into_owned();
                        if ui.text_edit_singleline(&mut workdir).changed() {
                            self.preferences.dcr.workdir = PathBuf::from(workdir);
                        }
                    });
                });
                if !self.dcr_error.is_empty() {
                    ui.colored_label(egui::Color32::LIGHT_RED, &self.dcr_error);
                }
            });
        self.dcr_open = open;
        if before != serde_json::to_value(&self.preferences.dcr).unwrap()
            && let Err(e) = self.save_settings() {
            self.dcr_error = format!("保存 DCR 设置失败：{e:#}");
        }
    }

    /// 独立置顶视口实时读取管理器；关闭它只关闭监控，不影响执行或主窗口。
    fn floating_window(&self, ctx: &egui::Context) {
        if !self.floating.load(Ordering::Relaxed) {
            self.floating_native.lock().unwrap().hide();
            return;
        }
        let enabled = self.floating.clone();
        let ready = self.floating_ready.clone();
        let native = self.floating_native.clone();
        let logo = self.logo.clone();
        let main_hwnd = self.main_hwnd;
        let position = self.floating_position.clone();
        let manager = self.manager.clone();
        let dcr_monitor = self.dcr.monitor();
        let limit = self.preferences.floating_limit;
        let idle_minutes = self.preferences.floating_idle_minutes;
        let backend = self.preferences.backend;
        let cli = backend.short();
        let model_label = self
            .editing_profiles
            .get(backend.key())
            .map(|profile| {
                format!(
                    "{cli}-{}-{}",
                    profile
                        .model
                        .as_deref()
                        .unwrap_or("default")
                        .to_ascii_lowercase(),
                    profile.effort.as_deref().unwrap_or("default")
                )
            })
            .unwrap_or_else(|| format!("{cli}-未配置"));
        // Builder 保持初始尺寸不变，后续尺寸调整在子视口中同时维护底边位置。
        let size = ui::floating_size(0, None);
        let viewport = egui::ViewportId::from_hash_of("floating_monitor");
        ctx.show_viewport_deferred(
            viewport,
            egui::ViewportBuilder::default()
                .with_title("Agent Router · 悬浮监控")
                .with_inner_size(size)
                .with_resizable(false)
                .with_decorations(false)
                .with_transparent(true)
                .with_taskbar(false)
                .with_active(false)
                .with_visible(ready.load(Ordering::Relaxed))
                .with_window_level(egui::WindowLevel::AlwaysOnTop),
            move |ctx, _| {
                if !enabled.load(Ordering::Relaxed) || ctx.input(|i| i.viewport().close_requested())
                {
                    enabled.store(false, Ordering::Relaxed);
                    ready.store(false, Ordering::Relaxed);
                    native.lock().unwrap().hide();
                    ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Close);
                    ctx.request_repaint_of(egui::ViewportId::ROOT);
                    return;
                }
                let passthrough = unsafe {
                    IsWindowVisible(main_hwnd as _) == 0 || IsIconic(main_hwnd as _) != 0
                };
                let styled = match native.lock().unwrap().apply(ctx.pixels_per_point(), passthrough) {
                    Ok(styled) => styled,
                    Err(error) => {
                        manager.lock().unwrap().error = format!("悬浮窗原生样式失败：{error}");
                        enabled.store(false, Ordering::Relaxed);
                        // 错误路径也不留下空白原生表面。
                        ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Visible(false));
                        ctx.request_repaint_of(egui::ViewportId::ROOT);
                        return;
                    }
                };
                let now = runner::now_ms() as i64;
                let (running, router_records, archive_hours) = {
                    let manager = manager.lock().unwrap();
                    (manager.running_count(), manager.records.values().cloned().collect::<Vec<_>>(), manager.archive_hours)
                };
                let dcr = {
                    let mut session = dcr_monitor.lock().unwrap();
                    session.update_window(now, archive_hours);
                    session.clone()
                };
                let records = crate::dcr_view::recent(&router_records, &dcr, limit, now,
                    Some((idle_minutes * 60_000) as i64));
                let size = ui::floating_size(
                    records.len(),
                    ctx.input(|input| input.viewport().monitor_size.map(|size| size.y)),
                );
                if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                    let mut position = position.lock().unwrap();
                    let resized = (rect.size() - size).abs().max_elem() > 1.;
                    let target = if let Some(center) = position.reset_center.take() {
                        Some(center - size / 2.)
                    } else if !ready.load(Ordering::Relaxed) && position.bottom.is_some() {
                        position.bottom.map(|[x, bottom]| egui::pos2(x, bottom - size.y))
                    } else if resized {
                        Some(egui::pos2(rect.left(), rect.bottom() - size.y))
                    } else {
                        None
                    };
                    if let Some(target) = target {
                        position.bottom = Some([target.x, target.y + size.y]);
                        ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::OuterPosition(target));
                    } else {
                        position.bottom = Some([rect.left(), rect.bottom()]);
                    }
                    if resized {
                        ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::InnerSize(size));
                    }
                }
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(12, 19, 31, 208))
                            .stroke(egui::Stroke::new(
                                1.,
                                egui::Color32::from_rgba_unmultiplied(80, 117, 139, 130),
                            ))
                            .corner_radius(24)
                            .inner_margin(egui::Margin::same(8)),
                    )
                    .show(ctx, |ui| {
                        // 穿透由原生窗口处理；仅拦截残留操作，不用禁用样式改变亮度。
                        ui.style_mut().interaction.selectable_labels = false;
                        let (close, drag, clear) = ui::floating_header(ui, running + usize::from(dcr.busy()), &model_label, logo.id());
                        let areas = ctx.data(|data| data.get_temp::<[egui::Rect; 2]>(egui::Id::new("floating_button_rects"))).unwrap();
                        native.lock().unwrap().button_hit_regions(passthrough, areas, ctx.pixels_per_point());
                        if clear {
                            let mut session = dcr_monitor.lock().unwrap();
                            if !session.busy() { session.hide(); }
                            drop(session);
                            let mut manager = manager.lock().unwrap();
                            if let Err(error) = clear_finished(&mut manager) {
                                manager.error = error.to_string();
                            }
                            ctx.request_repaint_of(egui::ViewportId::ROOT);
                        }
                        if drag && !passthrough {
                            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::StartDrag);
                        }
                        if close {
                            enabled.store(false, Ordering::Relaxed);
                            ready.store(false, Ordering::Relaxed);
                            native.lock().unwrap().hide();
                            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Close);
                            ctx.request_repaint_of(egui::ViewportId::ROOT);
                            return;
                        }
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if records.is_empty() {
                                    ui.label("等待任务");
                                }
                                for record in &records {
                                    match record {
                                        crate::dcr_view::Card::Router(record) => ui::floating_summary(ui, record, now),
                                        crate::dcr_view::Card::Dcr(session) => crate::dcr_view::floating(ui, session, now),
                                    }
                                }
                            });
                    });
                // 首帧先在隐藏窗口绘制，下一次主视口更新才显示，避免空白帧和焦点切换。
                if !enabled.load(Ordering::Relaxed) {
                    return;
                }
                if styled && enabled.load(Ordering::Relaxed) && !ready.swap(true, Ordering::Relaxed)
                {
                    ctx.request_repaint_of(egui::ViewportId::ROOT);
                }
                ctx.request_repaint_after(Duration::from_millis(250));
            },
        );
    }
    /// 保存编辑配置，已运行任务使用其启动时快照不受影响。
    fn save_settings(&mut self) -> Result<()> {
        std::fs::write(
            &self.config,
            serde_json::to_vec_pretty(&self.editing_profiles)?,
        )?;
        let mut manager = self.manager.lock().unwrap();
        std::fs::write(
            manager.root.join("preferences.json"),
            serde_json::to_vec_pretty(&self.preferences)?,
        )?;
        manager.profiles = self.editing_profiles.clone();
        manager.archive_hours = self.preferences.archive_hours;
        manager.max_parallel = self.preferences.max_parallel;
        manager.routing = Routing {
            backend: self.preferences.backend,
            allow_edits: self.preferences.allow_edits,
            clean_start: self.preferences.clean_start,
            timeout_seconds: self.preferences.timeout_seconds,
            retry_timeout_seconds: self.preferences.retry_timeout_seconds,
            agy_proxy: if self.preferences.agy_proxy_enabled {
                self.preferences.agy_proxy_socks5.clone()
            } else { String::new() },
        };
        Ok(())
    }
    /// 手动刷新或切换 CLI 时只重读当前供应商的模型，不改变已保存的选择。
    fn refresh_models(&mut self) {
        self.model_backend = self.preferences.backend;
        self.model_choices.clear();
        self.model_variants.clear();
        let Some(profile) = self.editing_profiles.get(self.model_backend.key()) else {
            self.error = "请先配置 CLI 路径".into();
            return;
        };
        let program = profile.program.clone();
        let backend = self.model_backend;
        let (tx, rx) = mpsc::channel();
        self.model_refresh = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(
                (if backend == Backend::Opencode {
                    crate::opencode::catalog(&program)
                } else {
                    crate::model_catalog::discover(backend, &program).map(|models|
                        crate::opencode::Catalog { models, ..Default::default() })
                })
                    .map_err(|error| format!("模型刷新失败：{error:#}")),
            );
        });
    }
    /// 通过正常 Harness 工作契约提交测试，继承所选执行快照且独立留痕归档。
    fn test_model(&mut self, now: i64) -> Result<()> {
        let mut manager = self.manager.lock().unwrap();
        let work = crate::protocol::Work {
            task_id: format!("model-test-{}", uuid::Uuid::new_v4()),
            title: Some(format!("模型测试 · {}", self.editing_profiles[self.preferences.backend.key()].model.as_deref().unwrap_or("CLI 默认"))),
            group_path: vec!["模型测试".into(), "rs_agent_router".into(), "连通性测试".into()],
            workdir: manager.root.clone(),
            prompt: "这是 Harness 发起的模型连通性测试。请只回复 ROUTER_MODEL_OK，不调用工具，不读取或修改文件。".into(),
        };
        manager.submit(work, now)?;
        self.archived = false;
        Ok(())
    }
    /// 加载选中任务详情，不改变任务活动时间；日志显示尾部，完整文件仍保留。
    fn load_detail(&mut self) {
        let manager = self.manager.lock().unwrap();
        if let Some(record) = self
            .selected
            .as_ref()
            .and_then(|id| manager.records.get(id))
        {
            self.detail_content = match self.detail_tab {
                0 => serde_json::to_string_pretty(&record.task).unwrap(),
                1 => std::fs::read_to_string(record.run_directory().join("console.log"))
                    .map(|s| {
                        s.lines()
                            .rev()
                            .take(1000)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_else(|e| e.to_string()),
                _ => serde_json::to_string_pretty(&record.result).unwrap(),
            };
        }
    }
}

/// 清空仅复用已有逻辑删除：保留运行任务和磁盘日志，包含归档中的终态任务。
fn clear_finished(manager: &mut Manager) -> Result<()> {
    let ids: Vec<_> = manager
        .records
        .values()
        .filter(|r| !r.running() && !r.deleted)
        .map(|r| r.task.task_id.clone())
        .collect();
    for id in ids {
        manager.delete(&id)?;
    }
    Ok(())
}

impl eframe::App for Dashboard {
    /// 透明视口需要透明清屏；主界面的不透明面板仍覆盖其整个客户区。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.; 4]
    }
    /// 每帧只展示快照；所有活动更新来自后台调度，不随排序或选择改变。
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.dcr.poll();
        self.dcr_window(ctx);
        let now = runner::now_ms() as i64;
        let old_geometry = (self.preferences.main_size, self.preferences.floating_bottom);
        ctx.input(|input| {
            let viewport = input.viewport();
            if viewport.minimized != Some(true) && viewport.maximized != Some(true)
                && let Some(rect) = viewport.inner_rect
                && rect.width() >= 1000. && rect.height() >= 650.
            {
                self.preferences.main_size = Some([rect.width(), rect.height()]);
            }
        });
        self.preferences.floating_bottom = self.floating_position.lock().unwrap().bottom;
        if old_geometry != (self.preferences.main_size, self.preferences.floating_bottom) {
            let root = self.manager.lock().unwrap().root.clone();
            if let Err(error) = std::fs::write(root.join("preferences.json"),
                serde_json::to_vec_pretty(&self.preferences).unwrap()) {
                self.error = format!("窗口位置保存失败：{error}");
            }
        }
        if let Some(result) = self
            .model_refresh
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.model_refresh = None;
            match result {
                Ok(models) => {
                    self.model_choices = models.models;
                    self.model_variants = models.variants;
                    self.error.clear();
                }
                Err(error) => self.error = error,
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.confirm_exit {
                self.confirm_exit = false;
            } else {
                self.selected = None;
            }
        }
        let dcr = {
            let monitor = self.dcr.monitor();
            let mut session = monitor.lock().unwrap();
            session.update_window(now, self.preferences.archive_hours);
            session.clone()
        };
        let mut actions = Vec::new();
        let mut refresh_models = false;
        let mut test_model = false;
        let (records, running, shutting, error) = {
            let m = self.manager.lock().unwrap();
            (
                m.records.values().cloned().collect::<Vec<_>>(),
                m.running_count(),
                m.shutting_down,
                m.error.clone(),
            )
        };
        if !shutting
            && (ctx.input(|i| i.viewport().close_requested())
                || self.exit_request.swap(false, Ordering::Relaxed))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if running > 0 || dcr.busy() {
                self.confirm_exit = true;
            } else {
                self.manager.lock().unwrap().shutdown();
            }
        }
        if shutting && running == 0 {
            self.floating_native.lock().unwrap().hide();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let settings_before =
            serde_json::to_value((&self.preferences, &self.editing_profiles)).unwrap();
        egui::TopBottomPanel::top("settings")
            .frame(
                egui::Frame::new()
                    .fill(ui::SURFACE)
                    .inner_margin(egui::Margin::symmetric(10, 5))
                    .stroke(egui::Stroke::new(1., ui::BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.image((self.logo.id(), egui::vec2(30., 30.)));
                    ui.heading("Agent Router");
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .size(12.)
                            .color(ui::MUTED),
                    );
                    ui::badge(ui, &format!("{} 运行", running + usize::from(dcr.busy())), ui::ACCENT);
                    let queued = records.iter().filter(|record| record.state == "queued").count();
                    if queued > 0 {
                        ui::badge(ui, &format!("{queued} 排队"), ui::state_color("queued"));
                    }
                    ui.add_space(8.);
                    ui::legend(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("退出").clicked() {
                            self.exit_request.store(true, Ordering::Relaxed);
                        }
                        if ui.button("统计").clicked() {
                            let url = self
                                .manager
                                .lock()
                                .unwrap()
                                .statistics_url
                                .clone()
                                .expect("statistics listener initialized");
                            ctx.open_url(egui::OpenUrl::new_tab(url));
                        }
                        ui.checkbox(&mut self.archived, "显示归档");
                        if ui
                            .add_enabled(
                                records.iter().any(|r| !r.running() && !r.deleted) || (dcr.visible && !dcr.busy()),
                                egui::Button::new("清空已结束"),
                            )
                            .on_hover_text("清空所有已结束任务（含归档），保留运行任务和磁盘日志")
                            .clicked()
                        {
                            actions.push(ui::Action::ClearFinished);
                        }
                        ui.separator();
                        let mut floating = self.floating.load(Ordering::Relaxed);
                        if ui.checkbox(&mut floating, "悬浮窗").changed() {
                            self.floating_ready.store(false, Ordering::Relaxed);
                            self.floating.store(floating, Ordering::Relaxed);
                        }
                        ui.label("个");
                        ui.add(egui::DragValue::new(&mut self.preferences.floating_limit).range(1..=20))
                            .on_hover_text("悬浮窗最多展示的对话数量，按最近活动排序，修改后自动记忆");
                        ui.label("悬浮上限");
                        ui.label("min");
                        ui.add(egui::DragValue::new(&mut self.preferences.floating_idle_minutes)
                            .range(1..=10080))
                            .on_hover_text("运行中的任务不受无活动隐藏限制；修改后自动记忆");
                        ui.label("无活动隐藏");
                        if ui.button("悬浮位置重置").clicked() {
                            self.floating_position.lock().unwrap().reset_center =
                                ctx.input(|input| input.viewport().outer_rect.map(|rect| rect.center()));
                            self.floating.store(true, Ordering::Relaxed);
                        }
                    });
                });
                ui.add_space(3.);
                egui::ScrollArea::horizontal()
                    .id_salt("configuration_strip")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_height(26.);
                            ui.spacing_mut().interact_size.y = 24.;
                            ui.spacing_mut().button_padding.y = 3.;
                            ui.spacing_mut().item_spacing.x = 8.;
                            ui.label("入口");
                            ui.selectable_value(
                                &mut self.preferences.backend,
                                Backend::Claude,
                                "Claude",
                            );
                            ui.selectable_value(
                                &mut self.preferences.backend,
                                Backend::Codex,
                                "Codex",
                            );
                            ui.selectable_value(&mut self.preferences.backend, Backend::Opencode, "OpenCode");
                            ui.selectable_value(&mut self.preferences.backend, Backend::Agy, "Antigravity");
                            let key = self.preferences.backend.key();
                            let profile =
                                self.editing_profiles.entry(key.into()).or_insert(Profile {
                                    program: if self.preferences.backend == Backend::Opencode {
                                        std::env::var_os("APPDATA").map(PathBuf::from)
                                            .map(|path| path.join("npm/node_modules/opencode-ai/bin/opencode.exe"))
                                            .filter(|path| path.is_file()).unwrap_or_else(|| "opencode".into())
                                    } else { PathBuf::new() },
                                    model: None,
                                    effort: None,
                                });
                            let mut path = profile.program.display().to_string();
                            ui.add_space(8.);
                            ui.label("CLI");
                            if ui
                                .add(egui::TextEdit::singleline(&mut path).desired_width(260.))
                                .changed()
                            {
                                profile.program = PathBuf::from(path);
                            }
                            ui.add_space(8.);
                            ui.label("模型");
                            ui::model_picker(ui, &mut profile.model,
                                if self.model_backend == self.preferences.backend {
                                    &self.model_choices
                                } else { &[] });
                            refresh_models = ui
                                .add_enabled(
                                    self.model_refresh.is_none(),
                                    egui::Button::new(if self.model_refresh.is_some() {
                                        "刷新中…"
                                    } else {
                                        "刷新"
                                    }),
                                )
                                .on_hover_text("Claude 重读 CC Switch；Codex / OpenCode 查询各自 CLI 的可选模型列表")
                                .clicked();
                            test_model = ui
                                .add_enabled(!shutting, egui::Button::new("测试"))
                                .on_hover_text("用所选配置提交一次模型测试任务")
                                .clicked();
                            ui.add_space(8.);
                            if self.preferences.backend != Backend::Opencode {
                                crate::model_efforts::normalize(profile.model.as_deref(), &mut profile.effort);
                            } else if self.model_backend == Backend::Opencode && self.model_refresh.is_none()
                                && profile.effort.as_ref().is_some_and(|effort| !profile.model.as_ref()
                                    .and_then(|model| self.model_variants.get(model)).is_some_and(|levels| levels.contains(effort))) {
                                profile.effort = None;
                            }
                            let effort_rule =
                                crate::model_efforts::lookup(profile.model.as_deref());
                            let effort_label = format!(
                                "强度 {}", profile.effort.as_deref().unwrap_or("默认")
                            );
                            let effort_font = egui::TextStyle::Button.resolve(ui.style());
                            let label_size = ui.painter().layout_no_wrap(
                                effort_label.clone(), effort_font.clone(), ui.visuals().text_color()
                            ).size();
                            let effort_button = ui.add_sized(
                                [label_size.x + 36., 24.], egui::Button::new("")
                            );
                            let effort_color = ui.style().interact(&effort_button).text_color();
                            ui.painter().text(
                                effort_button.rect.left_center() + egui::vec2(6., 0.),
                                egui::Align2::LEFT_CENTER, effort_label, effort_font, effort_color,
                            );
                            // 与模型菜单一致，绘制几何三角形，避免字体缺字显示方框。
                            let arrow_center = egui::pos2(
                                effort_button.rect.right() - 12., effort_button.rect.center().y
                            );
                            ui.painter().add(egui::Shape::convex_polygon(
                                vec![arrow_center + egui::vec2(-4., -2.),
                                     arrow_center + egui::vec2(4., -2.),
                                     arrow_center + egui::vec2(0., 3.)],
                                effort_color, egui::Stroke::NONE,
                            ));
                            let effort_popup = ui.make_persistent_id("effort_popup");
                            if effort_button.clicked() {
                                ui.memory_mut(|memory| memory.toggle_popup(effort_popup));
                            }
                            egui::popup::popup_below_widget(
                                ui, effort_popup, &effort_button,
                                egui::PopupCloseBehavior::CloseOnClick, |ui| {
                                    let levels = if self.preferences.backend == Backend::Opencode {
                                        profile.model.as_ref().and_then(|model| self.model_variants.get(model))
                                            .map(Vec::as_slice).unwrap_or(&[])
                                    } else { effort_rule.map(|rule| rule.levels.as_slice()).unwrap_or(&[]) };
                                    // 在滚动区外设置弹层高度，支持同时显示十行选项。
                                    let row_height = 26.;
                                    let height = (levels.len() + 1).min(10) as f32
                                        * (row_height + ui.spacing().item_spacing.y);
                                    ui.set_height(height);
                                    egui::ScrollArea::vertical().max_height(height).show(ui, |ui| {
                                        ui.spacing_mut().interact_size.y = row_height;
                                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                                        ui.selectable_value(&mut profile.effort, None, "默认");
                                        for level in levels {
                                            ui.selectable_value(
                                                &mut profile.effort,
                                                Some(level.clone()),
                                                level,
                                            );
                                        }
                                    });
                                });
                            effort_button.on_hover_text(
                                    if self.preferences.backend == Backend::Opencode { "档位取 OpenCode 模型的原生 variant；未声明时使用默认。" } else { effort_rule.map(|rule| rule.note.as_str()).unwrap_or(
                                        if profile.model.is_none() {
                                            "请先指定模型，再选择其支持的强度。"
                                        } else {
                                            "本地表尚未收录该模型，使用默认强度。"
                                        },
                                    ) },
                                );
                        });
                    });
                ui.add_space(3.);
                egui::ScrollArea::horizontal()
                    .id_salt("execution_configuration_strip")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_height(26.);
                            ui.spacing_mut().interact_size.y = 24.;
                            ui.spacing_mut().item_spacing.x = 8.;
                            ui.checkbox(&mut self.preferences.allow_edits, "允许修改文件");
                            ui.add_space(8.);
                            if self.preferences.backend == Backend::Claude {
                                ui.checkbox(&mut self.preferences.clean_start, "纯净启动")
                                    .on_hover_text("新会话不自动加载用户和项目的 CLAUDE.md、技能及插件等定制；已有会话沿用创建时设置。");
                                ui.add_space(8.);
                            }
                            let timeout_label = ui.label("无输出超时");
                            ui.add(
                                egui::DragValue::new(&mut self.preferences.timeout_seconds)
                                    .range(1..=86400),
                            )
                            .labelled_by(timeout_label.id)
                            .on_hover_text("从启动或最近一次 CLI 非空输出开始计时；标准输出和错误日志均会重置，持续输出不限制任务总时长。");
                            ui.label("秒");
                            if self.preferences.backend == Backend::Claude {
                                ui.label("异常重试");
                                ui.add_sized([48., 24.], egui::DragValue::new(
                                    &mut self.preferences.retry_timeout_seconds).range(1..=86400))
                                    .on_hover_text("首次上游异常后允许 CLI 重试的总等待时间；重复报错不延长，恢复正常模型输出后解除，到期按超时终止。");
                                ui.label("秒");
                            }
                            ui.label("归档");
                            ui.add(
                                egui::DragValue::new(&mut self.preferences.archive_hours)
                                    .range(1..=8760),
                            );
                            ui.label("h");
                            ui.add_space(8.);
                            ui.label("并行");
                            ui.add_sized([48., 24.],
                                egui::DragValue::new(&mut self.preferences.max_parallel).range(1..=100))
                                .on_hover_text("最大同时执行数量，默认 3；超出上限按提交顺序排队，降低上限不会中断运行任务。");
                            ui.separator();
                            if ui.button("DCR 设置").clicked() {
                                self.dcr_open = true;
                            }
                            ui.label(self.dcr.status());
                            if self.preferences.backend == Backend::Agy {
                                ui.separator();
                                ui.checkbox(&mut self.preferences.agy_proxy_enabled, "启用代理");
                                if self.preferences.agy_proxy_enabled {
                                    ui.label("SOCKS5");
                                    ui.add(egui::TextEdit::singleline(&mut self.preferences.agy_proxy_socks5)
                                        .desired_width(140.)
                                        .hint_text("127.0.0.1:11808"));
                                }
                            }
                        });
                    });
                self.preferences.floating = self.floating.load(Ordering::Relaxed);
                self.preferences.archived = self.archived;
                if settings_before
                    != serde_json::to_value((&self.preferences, &self.editing_profiles)).unwrap()
                {
                    self.error = match self.save_settings() {
                        Ok(()) => String::new(),
                        Err(e) => e.to_string(),
                    };
                }
                if refresh_models || self.model_backend != self.preferences.backend {
                    self.refresh_models();
                }
                if test_model {
                    self.error = match self.save_settings().and_then(|_| self.test_model(now)) {
                        Ok(()) => String::new(),
                        Err(error) => format!("模型测试启动失败：{error:#}"),
                    };
                }
                if !self.error.is_empty() {
                    ui.colored_label(egui::Color32::LIGHT_RED, &self.error);
                }
                if !error.is_empty() {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if shutting {
                    ui.colored_label(egui::Color32::YELLOW, "正在终止全部任务并保存错误结果…");
                }
            });
        let previous = self.selected.clone();
        egui::SidePanel::left("tasks")
            .frame(
                egui::Frame::new()
                    .fill(ui::SURFACE)
                    .inner_margin(egui::Margin::same(8)),
            )
            .default_width(380.)
            .min_width(340.)
            .show(ctx, |ui| {
                ui.heading(if self.archived {
                    "归档任务"
                } else {
                    "任务树"
                });
                ui.label(
                    egui::RichText::new("来源 / APP / Feat")
                        .size(12.)
                        .color(ui::MUTED),
                );
                ui.add_space(4.);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let visible: Vec<_> = records
                        .iter()
                        .filter(|r| r.archived == self.archived && !r.deleted)
                        .collect();
                    crate::dcr_view::tree(ui, &visible, &dcr, self.archived,
                        &mut self.selected, now, &mut actions);
                    if visible.is_empty() && !(dcr.visible && dcr.archived == self.archived) {
                        ui.label("暂无任务");
                    }
                });
            });
        if self.selected != previous {
            self.detail_tab = 2;
            self.load_detail();
        }
        let router_recent = store::recent(records.clone().into_iter());
        let recent = crate::dcr_view::recent(&router_recent, &dcr, 3, now, None);
        egui::CentralPanel::default().show(ctx, |ui| {
            if recent.is_empty() {
                ui.centered_and_justified(|ui| ui.label("等待 Harness 下发工作"));
                return;
            }
            let area = ui.available_rect_before_wrap();
            let height = (area.height() - 6. * (recent.len() - 1) as f32) / recent.len() as f32;
            for (index, record) in recent.iter().enumerate() {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(area.left(), area.top() + index as f32 * (height + 6.)),
                    egui::vec2(area.width(), height),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.set_min_size(rect.size());
                    ui.set_max_size(rect.size());
                    match record {
                        crate::dcr_view::Card::Router(record) => {
                            if let Some(action) = ui::execution(ui, record, now) { actions.push(action); }
                        }
                        crate::dcr_view::Card::Dcr(session) => crate::dcr_view::execution(
                            ui, session, now, &mut self.selected, &mut actions),
                    }
                });
            }
            ui.allocate_rect(area, egui::Sense::hover());
        });
        if self.selected.as_deref() == Some(crate::dcr_view::DCR_ID) {
            let mut open = true;
            egui::Window::new("DCR 调用 · 会话详情").open(&mut open)
                .default_size([750., 550.]).show(ctx, |ui| {
                    ui.label(format!("{} · {} 个执行中调用", dcr.state_label(), dcr.active_tools.len()));
                    ui.label(egui::RichText::new(crate::dcr_view::timing(&dcr, now)).size(12.).color(ui::MUTED));
                    ui.small("所有网页端调用汇总于此；工具返回不代表其启动的后台进程已结束。");
                    crate::dcr_view::logs(ui, &dcr, "dcr_detail_logs", true);
                });
            if !open { self.selected = None; }
        } else if self.selected.is_some() {
            let mut open = true;
            egui::Window::new("任务详情")
                .open(&mut open)
                .default_size([750., 550.])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        for (tab, label) in [(0, "请求"), (1, "日志"), (2, "结果")] {
                            if ui
                                .selectable_value(&mut self.detail_tab, tab, label)
                                .clicked()
                            {
                                self.load_detail();
                            }
                        }
                        if ui.button("刷新").clicked() {
                            self.load_detail();
                        }
                    });
                    if let Some(record) = self
                        .manager
                        .lock()
                        .unwrap()
                        .records
                        .get(self.selected.as_ref().unwrap())
                    {
                        ui.label(record.directory.display().to_string());
                    }
                    egui::ScrollArea::both()
                        .max_height(ui.available_height().max(80.))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.detail_content)
                                    .text_color(match self.detail_tab {
                                        0 => ui::TextKind::Instruction.color(),
                                        1 => ui::TextKind::Diagnostic.color(),
                                        _ => ui::TextKind::Result.color(),
                                    })
                                    .desired_width(f32::INFINITY)
                                    .interactive(false),
                            );
                        });
                });
            if !open {
                self.selected = None;
            }
        }
        if self.confirm_exit {
            egui::Window::new("确认退出")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0., 0.])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "当前存在 {running} 个 CLI 任务，{} 个 DCR 调用。退出将终止运行中的工作。", dcr.active_tools.len()
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("继续运行").clicked() {
                            self.confirm_exit = false;
                        }
                        if ui.button("终止全部并退出").clicked() {
                            self.manager.lock().unwrap().shutdown();
                            self.confirm_exit = false;
                        }
                    });
                });
        }
        for action in actions {
            match &action {
                ui::Action::Archive(id, archived) if id == crate::dcr_view::DCR_ID => {
                    let monitor = self.dcr.monitor();
                    let mut session = monitor.lock().unwrap();
                    if !session.busy() { session.set_archived(*archived); }
                    continue;
                }
                ui::Action::Delete(id) if id == crate::dcr_view::DCR_ID => {
                    let monitor = self.dcr.monitor();
                    let mut session = monitor.lock().unwrap();
                    if !session.busy() {
                        session.hide();
                        if self.selected.as_deref() == Some(crate::dcr_view::DCR_ID) { self.selected = None; }
                    }
                    continue;
                }
                ui::Action::ClearFinished => {
                    let monitor = self.dcr.monitor();
                    let mut session = monitor.lock().unwrap();
                    if !session.busy() {
                        session.hide();
                        if self.selected.as_deref() == Some(crate::dcr_view::DCR_ID) { self.selected = None; }
                    }
                }
                _ => {}
            }
            let mut manager = self.manager.lock().unwrap();
            let result = match action {
                ui::Action::ClearFinished => {
                    let result = clear_finished(&mut manager);
                    if self
                        .selected
                        .as_ref()
                        .and_then(|id| manager.records.get(id))
                        .is_some_and(|r| r.deleted)
                    {
                        self.selected = None;
                    }
                    result
                }
                ui::Action::Cancel(id) => manager.cancel(&id),
                ui::Action::Archive(id, archived) => manager.archive(&id, archived),
                ui::Action::Delete(id) => {
                    let result = manager.delete(&id);
                    if result.is_ok() && self.selected.as_ref() == Some(&id) {
                        self.selected = None;
                    }
                    result
                }
                ui::Action::DeleteGroup(ids) => {
                    let result = manager.delete_group(&ids);
                    if self
                        .selected
                        .as_ref()
                        .and_then(|id| manager.records.get(id))
                        .is_some_and(|record| record.deleted)
                    {
                        self.selected = None;
                    }
                    result
                }
            };
            if let Err(error) = result {
                self.error = error.to_string();
            } else {
                self.error.clear();
            }
        }
        self.floating_window(ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// 尝试从 PATH 中定位 agy 可执行文件，找不到返回 None。
fn which_agy() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("agy.exe"))
            .find(|path| path.is_file())
    })
}

/// 创建管理页和托盘；调度线程在隐藏期间照常消费命令、输出和归档。
pub fn show(config: PathBuf, root: PathBuf, sid: String) -> Result<i32> {
    let loaded: Result<Profiles> = (|| Ok(serde_json::from_slice(&std::fs::read(&config)?)?))();
    let (mut profiles, config_error) = match loaded {
        Ok(profiles) => (profiles, String::new()),
        Err(error) => (
            Profiles::new(),
            format!("配置读取失败（{}）：{error:#}", config.display()),
        ),
    };
    let mut legacy_glm = false;
    let preferences = match std::fs::read(root.join("preferences.json")) {
        Ok(bytes) => {
            let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
            if value["backend"] == "glm" {
                legacy_glm = true;
                value["backend"] = serde_json::json!("claude");
            }
            serde_json::from_value(value)?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Preferences {
            dcr: Default::default(),
            backend: Backend::Claude,
            allow_edits: false,
            clean_start: default_clean_start(),
            max_parallel: default_max_parallel(),
            timeout_seconds: 300,
            retry_timeout_seconds: crate::protocol::default_retry_timeout_seconds(),
            archive_hours: 4,
            floating: false,
            floating_limit: default_floating_limit(),
            floating_idle_minutes: default_floating_idle_minutes(),
            main_size: None,
            floating_bottom: None,
            archived: false,
            agy_proxy_enabled: false,
            agy_proxy_socks5: String::new(),
        },
        Err(e) => return Err(e.into()),
    };
    if legacy_glm {
        if let Some(profile) = profiles.get("glm").cloned() {
            profiles.insert("claude".into(), profile);
        }
    }
    profiles.retain(|key, _| key == "claude" || key == "codex" || key == "opencode" || key == "agy");
    let manager = Arc::new(Mutex::new(Manager::new_with_archive(
        root,
        profiles.clone(),
        runner::now_ms() as i64,
        preferences.archive_hours,
    )?));
    manager.lock().unwrap().max_parallel = preferences.max_parallel;
    manager.lock().unwrap().routing = Routing {
        backend: preferences.backend,
        allow_edits: preferences.allow_edits,
        clean_start: preferences.clean_start,
        timeout_seconds: preferences.timeout_seconds,
        retry_timeout_seconds: preferences.retry_timeout_seconds,
        agy_proxy: if preferences.agy_proxy_enabled {
            preferences.agy_proxy_socks5.clone()
        } else { String::new() },
    };
    let (tx, rx) = mpsc::channel();
    let statistics = crate::webstats::WebStats::start(tx.clone())?;
    manager.lock().unwrap().statistics_url = Some(statistics.url.clone());
    ipc::serve(sid, tx)?;
    let quit = Arc::new(AtomicBool::new(false));
    let worker_slot = Arc::new(Mutex::new(None));
    let app_manager = manager.clone();
    let app_quit = quit.clone();
    let app_slot = worker_slot.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!(
                "../assets/window.png"
            ))?)
            .with_inner_size(preferences.main_size.unwrap_or([1440., 900.]))
            .with_transparent(true)
            .with_min_inner_size([1000., 650.]),
        ..Default::default()
    };
    let result = eframe::run_native(
        &format!("Agent Router {} · CLI 执行服务", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(move |cc| {
            ui::load_fonts(&cc.egui_ctx);
            ui::apply_theme(&cc.egui_ctx);
            let brand = eframe::icon_data::from_png_bytes(include_bytes!("../assets/window.png"))?;
            let logo = cc.egui_ctx.load_texture(
                "app-logo",
                egui::ColorImage::from_rgba_unmultiplied(
                    [brand.width as usize, brand.height as usize],
                    &brand.rgba,
                ),
                egui::TextureOptions::LINEAR,
            );
            let hwnd = match cc.window_handle()?.as_raw() {
                RawWindowHandle::Win32(w) => w.hwnd.get() as usize,
                _ => return Err("Windows required".into()),
            };
            let menu = Menu::new();
            let exit_item = MenuItem::new("退出", true, None);
            menu.append(&exit_item)?;
            let exit_id = exit_item.id().clone();
            let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/tray.png"))?;
            let tray = TrayIconBuilder::new()
                .with_tooltip("Agent Router · 任务管理")
                .with_icon(tray_icon::Icon::from_rgba(
                    icon.rgba,
                    icon.width,
                    icon.height,
                )?)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()?;
            let exit_request = Arc::new(AtomicBool::new(false));
            let pump_exit = exit_request.clone();
            let pump_manager = app_manager.clone();
            let context = cc.egui_ctx.clone();
            *app_slot.lock().unwrap() = Some(std::thread::spawn(move || {
                loop {
                    if unsafe { IsIconic(hwnd as _) } != 0 {
                        unsafe {
                            ShowWindow(hwnd as _, SW_HIDE);
                        }
                    }
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        if event.id == exit_id {
                            reveal(hwnd);
                            pump_exit.store(true, Ordering::Relaxed);
                        }
                    }
                    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                        if let Some(show) = tray_visibility(
                            &event,
                            unsafe { IsWindowVisible(hwnd as _) != 0 },
                            unsafe { IsIconic(hwnd as _) != 0 },
                        ) {
                            if show {
                                reveal(hwnd);
                            } else {
                                unsafe {
                                    ShowWindow(hwnd as _, SW_HIDE);
                                }
                            }
                        }
                    }
                    let mut reveal_requested = false;
                    {
                        let mut manager = pump_manager.lock().unwrap();
                        for envelope in rx.try_iter() {
                            reveal_requested |= manager.request(envelope, runner::now_ms() as i64);
                        }
                        if app_quit.load(Ordering::Relaxed) {
                            manager.shutdown();
                        }
                        if let Err(error) = manager.poll(runner::now_ms() as i64) {
                            manager.error = format!("{error:#}");
                            manager.shutdown();
                        }
                        if app_quit.load(Ordering::Relaxed) && manager.running_count() == 0 {
                            break;
                        }
                    }
                    if reveal_requested {
                        reveal(hwnd);
                    }
                    context.request_repaint();
                    std::thread::sleep(Duration::from_millis(100));
                }
            }));
            let mut dashboard = Dashboard {
                dcr: crate::dcr::DcrService::new(),
                dcr_open: false,
                dcr_error: String::new(),
                main_hwnd: hwnd,
                floating_position: Arc::new(Mutex::new(FloatingPosition {
                    bottom: preferences.floating_bottom,
                    reset_center: None,
                })),
                floating: Arc::new(AtomicBool::new(preferences.floating)),
                floating_ready: Arc::new(AtomicBool::new(false)),
                floating_native: Arc::new(Mutex::new(ui::OverlayWindow::default())),
                logo,
                manager: app_manager,
                config,
                model_backend: preferences.backend,
                model_choices: Vec::new(),
                model_variants: Default::default(),
                model_refresh: None,
                archived: preferences.archived,
                preferences,
                editing_profiles: profiles,
                selected: None,
                detail_tab: 2,
                detail_content: String::new(),
                error: config_error,
                confirm_exit: false,
                exit_request,
                _tray: tray,
            };
            let monitor_path = dashboard.manager.lock().unwrap().root.join("dcr-session.json");
            let history_path = PathBuf::from(std::env::var_os("USERPROFILE")
                .ok_or_else(|| anyhow::anyhow!("缺少 USERPROFILE，无法定位 DCR 历史"))?)
                .join(".claude-server-commander/tool-history.jsonl");
            dashboard.dcr.configure_monitor(monitor_path, history_path)?;
            dashboard.refresh_models();
            if dashboard.preferences.dcr.auto_start
                && let Err(error) = dashboard.dcr.start(&dashboard.preferences.dcr) {
                dashboard.dcr_error = format!("DCR 自动启动失败：{error:#}");
                dashboard.dcr_open = true;
            }
            Ok(Box::new(dashboard))
        }),
    );
    quit.store(true, Ordering::Relaxed);
    if let Some(worker) = worker_slot.lock().unwrap().take() {
        let _ = worker.join();
    }
    result.map_err(|e| anyhow::anyhow!("window failed: {e}"))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 到期终态隐藏但运行中不隐藏；新活动恢复显示，过滤不修改任务数据。
    #[test]
    fn floating_idle_only_hides_finished_records() {
        let mut record = store::tests::sample("idle");
        record.last_activity = 1000;
        assert!(floating_record_visible(&record, 1_800_999, 30));
        assert!(!floating_record_visible(&record, 1_801_000, 30));
        record.state = "running".into();
        assert!(floating_record_visible(&record, 9_000_000, 30));
        record.state = "failed".into();
        record.last_activity = 9_000_000;
        assert!(floating_record_visible(&record, 9_000_000, 30));
    }
    /// 所有顶部开关与时限完整往返，旧设置缺少归档字段时默认四小时。
    #[test]
    fn preferences_remember_header_settings() {
        let old: Preferences = serde_json::from_value(serde_json::json!({
            "backend": "claude", "allow_edits": false, "timeout_seconds": 300
        }))
        .unwrap();
        assert_eq!(old.archive_hours, 4);
        assert!(old.clean_start);
        assert_eq!(old.max_parallel, 3);
        assert!(!old.dcr.auto_start);
        assert_eq!(old.retry_timeout_seconds, 60);
        assert_eq!(old.floating_limit, 4);
        assert_eq!(old.floating_idle_minutes, 30);
        let settings = Preferences {
            dcr: crate::dcr::DcrConfig {
                auto_start: true,
                proxy: "http://127.0.0.1:11809".into(),
                workdir: PathBuf::from("C:/workspace"),
            },
            backend: Backend::Codex,
            allow_edits: true,
            clean_start: false,
            max_parallel: 5,
            timeout_seconds: 600,
            retry_timeout_seconds: 75,
            archive_hours: 6,
            floating: true,
            floating_limit: 7,
            floating_idle_minutes: 45,
            main_size: Some([1280., 800.]),
            floating_bottom: Some([100., 900.]),
            archived: true,
        };
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(restored.backend, Backend::Codex);
        assert!(restored.dcr.auto_start);
        assert_eq!(restored.dcr.proxy, settings.dcr.proxy);
        assert_eq!(restored.dcr.workdir, settings.dcr.workdir);
        assert!(!restored.clean_start);
        assert_eq!(restored.max_parallel, 5);
        assert_eq!(restored.retry_timeout_seconds, 75);
        assert_eq!((restored.timeout_seconds, restored.archive_hours), (600, 6));
        assert!(restored.allow_edits && restored.floating && restored.archived);
        assert_eq!(restored.floating_limit, 7);
        assert_eq!(restored.floating_idle_minutes, 45);
        assert_eq!(restored.main_size, Some([1280., 800.]));
        assert_eq!(restored.floating_bottom, Some([100., 900.]));
    }
    /// 清空终态与归档记录，但不删除执行中的任务或任何日志文件。
    #[test]
    fn clear_finished_preserves_running_and_logs() {
        let root = std::env::temp_dir().join(format!("router-clear-{}", runner::now_ms()));
        let mut manager = Manager::new(root.clone(), Profiles::new(), 0).unwrap();
        for (id, state, archived) in [
            ("live", "running", false),
            ("done", "succeeded", false),
            ("old", "failed", true),
        ] {
            let mut record = store::tests::sample(id);
            record.state = state.into();
            record.archived = archived;
            record.directory = root.join(id);
            std::fs::create_dir(&record.directory).unwrap();
            std::fs::write(record.directory.join("console.log"), "evidence").unwrap();
            manager.records.insert(id.into(), record);
        }
        clear_finished(&mut manager).unwrap();
        assert!(!manager.records["live"].deleted);
        assert!(manager.records["done"].deleted && manager.records["old"].deleted);
        for id in ["live", "done", "old"] {
            assert_eq!(
                std::fs::read_to_string(root.join(id).join("console.log")).unwrap(),
                "evidence"
            );
        }
        drop(manager);
        let restored = Manager::new(root, Profiles::new(), 1).unwrap();
        assert!(restored.records["done"].deleted && restored.records["old"].deleted);
    }
    /// 一次按下/释放只切换一次；右键和双击附加通知不会误切换，最小化可恢复。
    #[test]
    fn tray_click_sequence_toggles_once() {
        let click = |button, button_state| TrayIconEvent::Click {
            id: "test".into(),
            position: Default::default(),
            rect: Default::default(),
            button,
            button_state,
        };
        let mut visible = true;
        for state in [MouseButtonState::Down, MouseButtonState::Up] {
            if let Some(next) = tray_visibility(&click(MouseButton::Left, state), visible, false) {
                visible = next;
            }
        }
        assert!(!visible);
        assert_eq!(
            tray_visibility(
                &click(MouseButton::Left, MouseButtonState::Up),
                visible,
                false
            ),
            Some(true)
        );
        assert_eq!(
            tray_visibility(&click(MouseButton::Left, MouseButtonState::Up), true, true),
            Some(true)
        );
        for state in [MouseButtonState::Down, MouseButtonState::Up] {
            assert_eq!(
                tray_visibility(&click(MouseButton::Right, state), true, false),
                None
            );
        }
        let double = TrayIconEvent::DoubleClick {
            id: "test".into(),
            position: Default::default(),
            rect: Default::default(),
            button: MouseButton::Left,
        };
        assert_eq!(tray_visibility(&double, true, false), None);
    }
}
