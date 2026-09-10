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
    backend: Backend,
    allow_edits: bool,
    timeout_seconds: u64,
    #[serde(default = "default_archive_hours")]
    archive_hours: u64,
    #[serde(default)]
    floating: bool,
    #[serde(default = "default_floating_limit")]
    floating_limit: usize,
    #[serde(default = "default_floating_idle_minutes")]
    floating_idle_minutes: u64,
    #[serde(default)]
    archived: bool,
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
fn floating_record_visible(record: &store::Record, now: i64, idle_minutes: u64) -> bool {
    record.running() || now.saturating_sub(record.last_activity) < (idle_minutes * 60_000) as i64
}

/// UI 编辑缓存与唯一管理器共享；无第二套执行状态机。
struct Dashboard {
    floating: Arc<AtomicBool>,
    floating_ready: Arc<AtomicBool>,
    floating_native: Arc<Mutex<ui::OverlayWindow>>,
    logo: egui::TextureHandle,
    manager: Arc<Mutex<Manager>>,
    config: PathBuf,
    preferences: Preferences,
    editing_profiles: Profiles,
    model_choices: Vec<String>,
    model_backend: Backend,
    model_refresh: Option<mpsc::Receiver<Result<Vec<String>, String>>>,
    selected: Option<String>,
    detail_tab: u8,
    detail_content: String,
    archived: bool,
    error: String,
    confirm_exit: bool,
    exit_request: Arc<AtomicBool>,
    _tray: TrayIcon,
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
    /// 独立置顶视口实时读取管理器；关闭它只关闭监控，不影响执行或主窗口。
    fn floating_window(&self, ctx: &egui::Context) {
        if !self.floating.load(Ordering::Relaxed) {
            self.floating_native.lock().unwrap().hide();
            return;
        }
        let enabled = self.floating.clone();
        let ready = self.floating_ready.clone();
        let native = self.floating_native.clone();
        let manager = self.manager.clone();
        let limit = self.preferences.floating_limit;
        let idle_minutes = self.preferences.floating_idle_minutes;
        let backend = self.preferences.backend;
        let cli = if backend == Backend::Claude {
            "cc"
        } else {
            "cx"
        };
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
                let styled = match native.lock().unwrap().apply(ctx.pixels_per_point()) {
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
                let (running, records) = {
                    let manager = manager.lock().unwrap();
                    (
                        manager.running_count(),
                        store::recent_with_limit(
                            manager.records.values()
                                .filter(|record| floating_record_visible(record, runner::now_ms() as i64, idle_minutes))
                                .cloned(),
                            limit,
                        ),
                    )
                };
                let size = ui::floating_size(
                    records.len(),
                    ctx.input(|input| input.viewport().monitor_size.map(|size| size.y)),
                );
                if let Some(rect) = ctx.input(|i| i.viewport().outer_rect)
                    .filter(|rect| (rect.size() - size).abs().max_elem() > 1.)
                {
                    ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::OuterPosition(
                        egui::pos2(rect.left(), rect.bottom() - size.y),
                    ));
                    ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::InnerSize(size));
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
                        let (close, drag, clear) = ui::floating_header(ui, running, &model_label);
                        if clear {
                            let mut manager = manager.lock().unwrap();
                            if let Err(error) = clear_finished(&mut manager) {
                                manager.error = error.to_string();
                            }
                            ctx.request_repaint_of(egui::ViewportId::ROOT);
                        }
                        if drag {
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
                                    ui::floating_summary(ui, record, runner::now_ms() as i64);
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
        manager.routing = Routing {
            backend: self.preferences.backend,
            allow_edits: self.preferences.allow_edits,
            timeout_seconds: self.preferences.timeout_seconds,
        };
        Ok(())
    }
    /// 手动刷新或切换 CLI 时只重读当前供应商的模型，不改变已保存的选择。
    fn refresh_models(&mut self) {
        self.model_backend = self.preferences.backend;
        self.model_choices.clear();
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
                crate::model_catalog::discover(backend, &program)
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
        let now = runner::now_ms() as i64;
        if let Some(result) = self
            .model_refresh
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.model_refresh = None;
            match result {
                Ok(models) => {
                    self.model_choices = models;
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
            if running > 0 {
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
                    ui::badge(ui, &format!("{running} 运行"), ui::ACCENT);
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
                                records.iter().any(|r| !r.running() && !r.deleted),
                                egui::Button::new("清空已结束"),
                            )
                            .on_hover_text("清空所有已结束任务（含归档），保留运行任务和磁盘日志")
                            .clicked()
                        {
                            actions.push(ui::Action::ClearFinished);
                        }
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
                            let key = match self.preferences.backend {
                                Backend::Claude => "claude",
                                Backend::Codex => "codex",
                            };
                            let profile =
                                self.editing_profiles.entry(key.into()).or_insert(Profile {
                                    program: PathBuf::new(),
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
                            egui::ComboBox::from_id_salt("model")
                                .width(160.)
                                .selected_text(profile.model.as_deref().unwrap_or("CLI 默认"))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut profile.model, None, "CLI 默认");
                                    if self.model_backend == self.preferences.backend {
                                        for model in &self.model_choices {
                                            ui.selectable_value(
                                                &mut profile.model,
                                                Some(model.clone()),
                                                model,
                                            );
                                        }
                                    }
                                });
                            refresh_models = ui
                                .add_enabled(
                                    self.model_refresh.is_none(),
                                    egui::Button::new(if self.model_refresh.is_some() {
                                        "刷新中…"
                                    } else {
                                        "刷新"
                                    }),
                                )
                                .on_hover_text("Claude 重读 CC Switch；Codex 查询 CLI 可选模型列表")
                                .clicked();
                            test_model = ui
                                .add_enabled(!shutting, egui::Button::new("测试"))
                                .on_hover_text("用所选配置提交一次模型测试任务")
                                .clicked();
                            ui.add_space(8.);
                            crate::model_efforts::normalize(
                                profile.model.as_deref(),
                                &mut profile.effort,
                            );
                            let effort_rule =
                                crate::model_efforts::lookup(profile.model.as_deref());
                            egui::ComboBox::from_id_salt("effort")
                                .selected_text(format!(
                                    "强度 {}",
                                    profile.effort.as_deref().unwrap_or("默认")
                                ))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut profile.effort, None, "默认");
                                    let levels = effort_rule
                                        .map(|rule| rule.levels.as_slice())
                                        .unwrap_or(&[]);
                                    for level in levels {
                                        ui.selectable_value(
                                            &mut profile.effort,
                                            Some(level.clone()),
                                            level,
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(
                                    effort_rule.map(|rule| rule.note.as_str()).unwrap_or(
                                        if profile.model.is_none() {
                                            "请先指定模型，再选择其支持的强度。"
                                        } else {
                                            "本地表尚未收录该模型，使用默认强度。"
                                        },
                                    ),
                                );
                            ui.add_space(10.);
                            ui.checkbox(&mut self.preferences.allow_edits, "允许修改文件");
                            ui.add_space(8.);
                            let timeout_label = ui.label("无输出超时");
                            ui.add(
                                egui::DragValue::new(&mut self.preferences.timeout_seconds)
                                    .range(1..=86400),
                            )
                            .labelled_by(timeout_label.id)
                            .on_hover_text("从启动或最近一次 CLI 非空输出开始计时；标准输出和错误日志均会重置，持续输出不限制任务总时长。");
                            ui.label("秒");
                            ui.label("归档");
                            ui.add(
                                egui::DragValue::new(&mut self.preferences.archive_hours)
                                    .range(1..=8760),
                            );
                            ui.label("h");
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
                    ui::tree(
                        ui,
                        &visible,
                        0,
                        "groups",
                        &mut self.selected,
                        now,
                        &mut actions,
                    );
                    if visible.is_empty() {
                        ui.label("暂无任务");
                    }
                });
            });
        if self.selected != previous {
            self.detail_tab = 2;
            self.load_detail();
        }
        let recent = store::recent(records.into_iter());
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
                    if let Some(action) = ui::execution(ui, record, now) {
                        actions.push(action);
                    }
                });
            }
            ui.allocate_rect(area, egui::Sense::hover());
        });
        if self.selected.is_some() {
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
                        "当前存在 {running} 个运行任务。退出将终止全部任务，并向 Harness 返回错误。"
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
            backend: Backend::Claude,
            allow_edits: false,
            timeout_seconds: 300,
            archive_hours: 4,
            floating: false,
            floating_limit: default_floating_limit(),
            floating_idle_minutes: default_floating_idle_minutes(),
            archived: false,
        },
        Err(e) => return Err(e.into()),
    };
    if legacy_glm {
        if let Some(profile) = profiles.get("glm").cloned() {
            profiles.insert("claude".into(), profile);
        }
    }
    profiles.retain(|key, _| key == "claude" || key == "codex");
    let manager = Arc::new(Mutex::new(Manager::new_with_archive(
        root,
        profiles.clone(),
        runner::now_ms() as i64,
        preferences.archive_hours,
    )?));
    manager.lock().unwrap().routing = Routing {
        backend: preferences.backend,
        allow_edits: preferences.allow_edits,
        timeout_seconds: preferences.timeout_seconds,
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
            .with_inner_size([1440., 900.])
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
                floating: Arc::new(AtomicBool::new(preferences.floating)),
                floating_ready: Arc::new(AtomicBool::new(false)),
                floating_native: Arc::new(Mutex::new(ui::OverlayWindow::default())),
                logo,
                manager: app_manager,
                config,
                model_backend: preferences.backend,
                model_choices: Vec::new(),
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
            dashboard.refresh_models();
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
        assert_eq!(old.floating_limit, 4);
        assert_eq!(old.floating_idle_minutes, 30);
        let settings = Preferences {
            backend: Backend::Codex,
            allow_edits: true,
            timeout_seconds: 600,
            archive_hours: 6,
            floating: true,
            floating_limit: 7,
            floating_idle_minutes: 45,
            archived: true,
        };
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(restored.backend, Backend::Codex);
        assert_eq!((restored.timeout_seconds, restored.archive_hours), (600, 6));
        assert!(restored.allow_edits && restored.floating && restored.archived);
        assert_eq!(restored.floating_limit, 7);
        assert_eq!(restored.floating_idle_minutes, 45);
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
