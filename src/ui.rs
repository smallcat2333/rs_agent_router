//! 中文字体、活动排序树和真实执行面板。
use crate::store::Record;
use eframe::egui;
use std::{collections::BTreeMap, path::PathBuf};

pub const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(12, 19, 31);
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(19, 29, 44);
pub const CARD: egui::Color32 = egui::Color32::from_rgb(24, 36, 53);
pub const BORDER: egui::Color32 = egui::Color32::from_rgb(44, 62, 82);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(66, 198, 180);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(228, 235, 244);
pub const MUTED: egui::Color32 = egui::Color32::from_rgb(144, 163, 186);

/// Windows 合成级透明、工具窗标记和圆角；缓存尺寸避免每次重绘都重设窗口区域。
#[derive(Default)]
pub struct OverlayWindow {
    hwnd: usize,
    shape: (i32, i32, i32),
}

/// 仅匹配当前进程的悬浮窗，绝不修改其它应用或主管理窗口。
fn is_overlay(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != std::process::id() {
            return false;
        }
        let mut text = [0u16; 80];
        let count = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
        String::from_utf16_lossy(&text[..count as usize]) == "Agent Router · 悬浮监控"
    }
}

/// EnumWindows 回调只写调用方在栈上持有的句柄槽，命中后停止枚举。
unsafe extern "system" fn find_overlay(
    hwnd: windows_sys::Win32::Foundation::HWND,
    data: isize,
) -> i32 {
    if is_overlay(hwnd) {
        unsafe {
            *(data as *mut usize) = hwnd as usize;
        }
        0
    } else {
        1
    }
}

impl OverlayWindow {
    /// 先撤下原生表面并刷新原覆盖区域；随后由 egui 销毁视口，避免关闭末帧留下黑框。
    pub fn hide(&mut self) {
        use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
        unsafe {
            let hwnd = self.hwnd as _;
            if is_overlay(hwnd) && IsWindowVisible(hwnd) != 0 {
                let mut rect: RECT = std::mem::zeroed();
                let captured = GetWindowRect(hwnd, &mut rect) != 0;
                ShowWindow(hwnd, SW_HIDE);
                if captured {
                    RedrawWindow(
                        std::ptr::null_mut(),
                        &rect,
                        std::ptr::null_mut(),
                        RDW_INVALIDATE | RDW_ALLCHILDREN,
                    );
                }
            }
        }
    }
    /// 在原生窗口出现后应用 75% 不透明度、24px 圆角和非任务栏工具窗；失败明确返回。
    pub fn apply(&mut self, pixels_per_point: f32) -> anyhow::Result<bool> {
        use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
        unsafe {
            if !is_overlay(self.hwnd as HWND) {
                let mut hwnd = 0usize;
                EnumWindows(Some(find_overlay), &mut hwnd as *mut usize as isize);
                if hwnd == 0 {
                    return Ok(false);
                }
                self.hwnd = hwnd;
                self.shape = (0, 0, 0);
            }
            let hwnd = self.hwnd as HWND;
            let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let style = (old | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE)
                & !(WS_EX_APPWINDOW
                    | WS_EX_WINDOWEDGE
                    | WS_EX_CLIENTEDGE
                    | WS_EX_STATICEDGE
                    | WS_EX_DLGMODALFRAME);
            // winit 无边框窗口仍保留 WS_CAPTION；移除原生装饰，阻止激活时重绘标题栏。
            let old_frame = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let frame = (old_frame
                & !(WS_CAPTION
                    | WS_BORDER
                    | WS_THICKFRAME
                    | WS_SYSMENU
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX))
                | WS_POPUP;
            let reshow = ((old ^ style) & (WS_EX_APPWINDOW | WS_EX_TOOLWINDOW)) != 0
                && IsWindowVisible(hwnd) != 0;
            if old != style || old_frame != frame {
                if reshow {
                    ShowWindow(hwnd, SW_HIDE);
                }
                SetLastError(0);
                if old != style
                    && SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize) == 0
                    && GetLastError() != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                SetLastError(0);
                if old_frame != frame
                    && SetWindowLongPtrW(hwnd, GWL_STYLE, frame as isize) == 0
                    && GetLastError() != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            let (mut color, mut alpha, mut flags) = (0, 0, 0);
            let opacity_current =
                GetLayeredWindowAttributes(hwnd, &mut color, &mut alpha, &mut flags) != 0
                    && alpha == 190
                    && flags == LWA_ALPHA;
            if (!opacity_current || old != style)
                && SetLayeredWindowAttributes(hwnd, 0, 190, LWA_ALPHA) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut rect) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let shape = (
                rect.right - rect.left,
                rect.bottom - rect.top,
                (24. * pixels_per_point).round() as i32,
            );
            let region =
                CreateRoundRectRgn(0, 0, shape.0 + 1, shape.1 + 1, shape.2 * 2, shape.2 * 2);
            if region.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let actual = CreateRectRgn(0, 0, 0, 0);
            if actual.is_null() {
                DeleteObject(region);
                return Err(std::io::Error::last_os_error().into());
            }
            // 尺寸未变也可能被窗口系统清掉裁剪区域，必须核对真实状态。
            let matches = GetWindowRgn(hwnd, actual) != 0 && EqualRgn(region, actual) != 0;
            DeleteObject(actual);
            if self.shape != shape || !matches {
                if SetWindowRgn(hwnd, region, 0) == 0 {
                    DeleteObject(region);
                    return Err(std::io::Error::last_os_error().into());
                }
                // SetWindowRgn 成功后由系统拥有 region，不能再 DeleteObject。
                self.shape = shape;
            } else {
                DeleteObject(region);
            }
            if reshow {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            Ok(true)
        }
    }
}

/// 页面使用同一套深蓝灰色板，强调色取自应用图标。
pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = BACKGROUND;
    visuals.faint_bg_color = CARD;
    visuals.override_text_color = Some(TEXT);
    visuals.window_stroke = egui::Stroke::new(1., BORDER);
    visuals.selection.bg_fill = egui::Color32::from_rgb(25, 76, 84);
    visuals.selection.stroke = egui::Stroke::new(1., ACCENT);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1., BORDER);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1., MUTED);
    visuals.widgets.inactive.bg_fill = CARD;
    visuals.widgets.inactive.weak_bg_fill = CARD;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1., BORDER);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(33, 58, 77);
    visuals.widgets.hovered.weak_bg_fill = visuals.widgets.hovered.bg_fill;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1., ACCENT);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(28, 80, 86);
    visuals.widgets.active.weak_bg_fill = visuals.widgets.active.bg_fill;
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6., 3.);
        style.spacing.button_padding = egui::vec2(7., 3.);
        style.spacing.indent = 12.;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(21.));
    });
}

/// 秒级保留秒、分钟级保留分秒、小时级保留时分；省略零尾项。
pub fn elapsed_text(milliseconds: i64) -> String {
    let seconds = milliseconds / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        let m = seconds / 60;
        let s = seconds % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s}s")
        }
    } else {
        let h = seconds / 3600;
        let m = seconds % 3600 / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    }
}

/// 最近有效活动的相对时间；仅展示，不写回活动时间或延长归档。
pub fn activity_age(last_activity: i64, now: i64) -> String {
    format!("{}前", elapsed_text((now - last_activity).max(0)))
}

/// 小于千的 Token 显示整数，大数用 k/m；悬停可读原始精确值。
fn token_text(tokens: Option<u64>) -> String {
    match tokens {
        None => "—".into(),
        Some(n) if n >= 1_000_000 => format!("{:.1}m", n as f64 / 1_000_000.),
        Some(n) if n >= 1000 => format!("{:.1}k", n as f64 / 1000.),
        Some(n) => n.to_string(),
    }
}

/// 健康只描述操作系统进程证据；终态直接显示已结束，不冒充模型响应健康。
fn health_label(record: &Record) -> (&str, egui::Color32) {
    if !record.running() {
        return ("已结束", MUTED);
    }
    match record.health.phase.as_str() {
        "running" => ("进程存活", ACCENT),
        "starting" | "" => ("启动中", TextKind::Tool.color()),
        "stopping" => ("停止中", TextKind::Tool.color()),
        "reaping" | "settling" => ("回收中", TextKind::Tool.color()),
        _ => ("健康未知", TextKind::Error.color()),
    }
}

/// 只影响显示层的文本语义，原始日志和结果保持不变。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    Instruction,
    Tool,
    Diagnostic,
    Main,
    Result,
    Error,
}

impl TextKind {
    /// 每类文本固定颜色，避免不同任务中相同含义使用不同配色。
    pub fn color(self) -> egui::Color32 {
        match self {
            Self::Instruction => egui::Color32::from_rgb(119, 180, 251),
            Self::Tool => egui::Color32::from_rgb(225, 183, 112),
            Self::Diagnostic => MUTED,
            Self::Main => TEXT,
            Self::Result => egui::Color32::from_rgb(100, 216, 170),
            Self::Error => egui::Color32::from_rgb(245, 130, 143),
        }
    }
}

/// 已有规范化输出使用明确前缀；其余文本保留为主要输出。
pub fn text_kind(line: &str) -> TextKind {
    if line.starts_with("执行失败")
        || line.starts_with("进程错误")
        || line.starts_with("结果写入失败")
    {
        TextKind::Error
    } else if line.starts_with("最终结果 ·") {
        TextKind::Result
    } else if line.starts_with("工具调用") || line.starts_with("工具结果") {
        TextKind::Tool
    } else if [
        "CLI 日志",
        "CLI 输出",
        "正在启动 CLI",
        "运行中 · PID",
        "会话已建立",
        "任务结束 ·",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
    {
        TextKind::Diagnostic
    } else {
        TextKind::Main
    }
}

/// 状态按执行结果统一着色：运行绿、完成蓝、超时黄、失败或中断红；主动取消保持灰色。
pub fn state_color(state: &str) -> egui::Color32 {
    match state {
        "running" => egui::Color32::from_rgb(100, 216, 170),
        "succeeded" => egui::Color32::from_rgb(119, 180, 251),
        "timed_out" => egui::Color32::from_rgb(245, 207, 92),
        "failed" | "interrupted" => TextKind::Error.color(),
        _ => MUTED,
    }
}

/// 小标签统一边距和描边，用于状态、CLI 与图例。
pub fn badge(ui: &mut egui::Ui, label: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1., color.gamma_multiply(0.35)))
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).size(12.).color(color));
        });
}

/// 页面图例说明颜色含义，不要求用户记忆色彩对应关系。
pub fn legend(ui: &mut egui::Ui) {
    for (label, kind) in [
        ("指令", TextKind::Instruction),
        ("工具", TextKind::Tool),
        ("输出", TextKind::Main),
        ("结果", TextKind::Result),
    ] {
        ui.label(
            egui::RichText::new(format!("● {label}"))
                .color(kind.color())
                .size(12.),
        );
    }
}

/// 主面板与悬浮窗共用最近五条平均耗时，悬停可读每条样本和实际计时口径。
fn reply_latency(ui: &mut egui::Ui, record: &Record) {
    let replies = record.recent_replies();
    let average = if replies.is_empty() {
        "—".into()
    } else {
        format!(
            "{:.1}s",
            replies
                .iter()
                .map(|reply| reply.duration_ms as f64)
                .sum::<f64>()
                / replies.len() as f64
                / 1000.
        )
    };
    let mut detail = format!(
        "最近 {} 次完整模型回复的平均耗时（最多 5 次）；未完成回复不计入。",
        replies.len()
    );
    for (index, reply) in replies.iter().enumerate() {
        let source = match reply.source.as_str() {
            "claude_request" => "等待 + 响应",
            "claude_stream" => "响应流，缺少等待时间",
            _ => "CLI 回复周期，已排除工具阶段，非精确 API 耗时",
        };
        detail.push_str(&format!(
            "\n近第 {} 条：{:.2}s（{source}）",
            index + 1,
            reply.duration_ms as f64 / 1000.
        ));
    }
    ui.label(
        egui::RichText::new(format!("均耗时 {average}"))
            .small()
            .color(MUTED),
    )
    .on_hover_text(detail);
}

/// 结果及错误用独立卡片强调，全文仍可选择复制。
fn result_block(ui: &mut egui::Ui, label: &str, text: &str, kind: TextKind) {
    let color = kind.color();
    let fill = if kind == TextKind::Error {
        egui::Color32::from_rgb(48, 26, 38)
    } else {
        egui::Color32::from_rgb(17, 46, 40)
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1., color.gamma_multiply(0.4)))
        .corner_radius(7)
        .inner_margin(egui::Margin::same(5))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(egui::RichText::new(label).strong().size(12.).color(color));
            ui.add(
                egui::Label::new(egui::RichText::new(text).color(color))
                    .wrap()
                    .selectable(true),
            );
        });
}

/// 使用本机中文字体，避免默认 egui 字体缺字。
pub fn load_fonts(ctx: &egui::Context) {
    let path =
        PathBuf::from(std::env::var_os("WINDIR").expect("WINDIR missing")).join("Fonts/msyh.ttc");
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "chinese".into(),
        egui::FontData::from_owned(std::fs::read(path).expect("Microsoft YaHei unavailable"))
            .into(),
    );
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .insert(0, "chinese".into());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .push("chinese".into());
    ctx.set_fonts(fonts);
}
/// 树行和面板共用短状态名称，不显示内部日志。
pub fn state_label(state: &str) -> &str {
    match state {
        "running" => "运行中",
        "succeeded" => "已完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        "timed_out" => "超时",
        "interrupted" => "中断",
        other => other,
    }
}

/// UI 只报告操作意图，管理器统一校验并落盘。
pub enum Action {
    ClearFinished,
    Cancel(String),
    Archive(String, bool),
    Delete(String),
    DeleteGroup(Vec<String>),
}

/// 同层任务与分组按最新活动排序，分组活动取所有后代最大值。
pub fn tree(
    ui: &mut egui::Ui,
    records: &[&Record],
    depth: usize,
    prefix: &str,
    selected: &mut Option<String>,
    now: i64,
    actions: &mut Vec<Action>,
) {
    let mut groups: BTreeMap<(bool, String), Vec<&Record>> = BTreeMap::new();
    let mut nodes = Vec::new();
    for record in records {
        if depth == 0 && record.task.group_path.is_empty() {
            groups
                .entry((true, "未分组".to_owned()))
                .or_default()
                .push(record);
        } else if record.task.group_path.len() > depth {
            groups
                .entry((false, record.task.group_path[depth].clone()))
                .or_default()
                .push(record);
        } else {
            nodes.push((
                record.last_activity,
                (false, record.task.task_id.clone()),
                Some(record),
            ));
        }
    }
    for (name, children) in &groups {
        nodes.push((
            children.iter().map(|r| r.last_activity).max().unwrap(),
            name.clone(),
            None,
        ));
    }
    nodes.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (activity, key, record) in nodes {
        if let Some(record) = record {
            let response = ui
                .horizontal(|ui| {
                    let color = state_color(&record.state);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(58., 22.), egui::Sense::hover());
                    ui.painter().rect(
                        rect,
                        5.,
                        color.gamma_multiply(0.12),
                        egui::Stroke::new(1., color.gamma_multiply(0.35)),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        state_label(&record.state),
                        egui::FontId::proportional(12.),
                        color,
                    );
                    let width = (ui.available_width() - 122.).max(40.);
                    let name = ui
                        .allocate_ui_with_layout(
                            egui::vec2(width, 20.),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.set_min_width(width);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(record.task.label()).color(
                                            if selected.as_deref() == Some(&record.task.task_id) {
                                                ACCENT
                                            } else {
                                                TEXT
                                            },
                                        ),
                                    )
                                    .truncate()
                                    .sense(egui::Sense::click()),
                                )
                            },
                        )
                        .inner;
                    if name.clicked() {
                        *selected = Some(record.task.task_id.clone());
                    }
                    ui.label(
                        egui::RichText::new(elapsed_text(record.elapsed(now)))
                            .size(11.)
                            .color(MUTED),
                    )
                    .on_hover_text("累计执行耗时");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(activity_age(record.last_activity, now))
                                .size(11.)
                                .color(MUTED),
                        )
                        .on_hover_text("距最近有效活动；查看和探活不刷新时间");
                    });
                    name
                })
                .inner;
            response.context_menu(|ui| {
                if ui
                    .add_enabled(
                        !record.running(),
                        egui::Button::new(if record.archived {
                            "移回任务列表"
                        } else {
                            "归档任务"
                        }),
                    )
                    .clicked()
                {
                    actions.push(Action::Archive(
                        record.task.task_id.clone(),
                        !record.archived,
                    ));
                    ui.close_menu();
                }
                if ui
                    .add_enabled(!record.running(), egui::Button::new("删除任务（保留日志）"))
                    .clicked()
                {
                    actions.push(Action::Delete(record.task.task_id.clone()));
                    ui.close_menu();
                }
                if record.running() {
                    ui.small("运行任务请先取消或等待完成");
                }
            });
        } else {
            let path = format!("{prefix}/{key:?}");
            let (tint, background) = match depth {
                0 => (
                    TextKind::Instruction.color(),
                    egui::Color32::from_rgb(18, 32, 51),
                ),
                1 => (
                    egui::Color32::from_rgb(176, 157, 234),
                    egui::Color32::from_rgb(27, 28, 47),
                ),
                _ => (ACCENT, egui::Color32::from_rgb(17, 36, 42)),
            };
            egui::Frame::new()
                .fill(background)
                .stroke(egui::Stroke::new(1., tint.gamma_multiply(0.25)))
                .corner_radius(7)
                .inner_margin(egui::Margin::same(2))
                .show(ui, |ui| {
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!(
                            "{} ({}) · {}",
                            key.1,
                            groups[&key].len(),
                            activity_age(activity, now)
                        ))
                        .color(tint)
                        .strong(),
                    )
                    .id_salt(&path)
                    .default_open(true)
                    .show(ui, |ui| {
                        tree(ui, &groups[&key], depth + 1, &path, selected, now, actions)
                    })
                    .header_response
                    .context_menu(|ui| {
                        let children = &groups[&key];
                        if ui
                            .add_enabled(
                                children.iter().any(|record| !record.running()),
                                egui::Button::new("删除组内已结束任务（保留日志）"),
                            )
                            .clicked()
                        {
                            actions.push(Action::DeleteGroup(
                                children
                                    .iter()
                                    .map(|record| record.task.task_id.clone())
                                    .collect(),
                            ));
                            ui.close_menu();
                        }
                        ui.small("包含当前列表中该组的所有子组，保留运行任务");
                    });
                });
        }
    }
}

/// 执行监控只显示 CLI 输出、结果和取消操作，不维护聊天输入或消息气泡。
pub fn execution(ui: &mut egui::Ui, record: &Record, now: i64) -> Option<Action> {
    let mut action = None;
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1., BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(7))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.horizontal(|ui| {
                let title_width = (ui.available_width() - 325.).max(80.);
                ui.allocate_ui_with_layout(egui::vec2(title_width, 23.), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_min_width(title_width);
                    ui.add(egui::Label::new(egui::RichText::new(record.task.label()).size(15.).strong().color(TEXT)).truncate());
                });
                badge(ui, state_label(&record.state), state_color(&record.state));
                if record.running() && ui.button("取消").clicked() {
                    action = Some(Action::Cancel(record.task.task_id.clone()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (health, color) = health_label(record);
                    ui.label(egui::RichText::new(format!("● {health}")).color(color).size(12.))
                        .on_hover_text(format!("PID {:?} · 工作线程 {}\n距有效活动 {}\n静默输出不能证明模型故障。", record.health.pid, record.health.worker_alive, activity_age(record.last_activity, now)));
                    ui.label(egui::RichText::new(format!("会话 {}", elapsed_text(record.elapsed(now)))).size(12.).color(MUTED))
                        .on_hover_text(format!("会话累计执行耗时，不含轮次间等待；本轮 {}", elapsed_text(record.turn_elapsed(now))));
                });
            });
            ui.horizontal(|ui| {
              let model_width = (ui.available_width() - 385.).max(80.);
              ui.allocate_ui_with_layout(egui::vec2(model_width, 20.), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_width(model_width);
                ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} · 模型 {} · 强度 {}",
                        record.task.profile_key(),
                        record.task.model.as_deref().unwrap_or("CLI 默认"),
                        record.task.effort.as_deref().unwrap_or("默认")
                    ))
                    .color(MUTED)
                    .size(12.),
                )
                .halign(egui::Align::Min).truncate(),
                );
              });
              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                  let metrics = &record.metrics;
                  reply_latency(ui, record);
                  let latency = metrics.first_text_ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.)).unwrap_or_else(|| "—".into());
                  let label = if metrics.first_text_source.as_deref() == Some("completed_message") { "首段" } else { "首字" };
                  ui.label(egui::RichText::new(format!("{label} {latency}")).color(MUTED).size(12.))
                      .on_hover_text("从 CLI 启动至首次可见文本；非流式 CLI 只能测完整首段抵达时间，不是 API TTFT。");
                  ui.label(egui::RichText::new(format!("Token {}", token_text(metrics.total_tokens()))).color(TextKind::Tool.color()).size(12.))
                      .on_hover_text(format!("本轮累计：输入 {:?}，输出 {:?}，缓存命中 {:?}；— 表示 CLI 未提供。", metrics.input_tokens, metrics.output_tokens, metrics.cached_tokens));
                  ui.label(egui::RichText::new(format!("上下文 {}", token_text(metrics.context_tokens))).color(TextKind::Instruction.color()).size(12.))
                      .on_hover_text(format!("最近一次模型请求输入：{:?}；模型窗口：{:?}。不等于本轮累计用量，也不是字符数。", metrics.context_tokens, metrics.context_window_tokens));
              });
            });
            ui.add_space(1.);
            let height = ui.available_height();
            egui::ScrollArea::vertical()
                .id_salt((&record.task.task_id, "output"))
                .auto_shrink([false, false])
                .max_height(height)
                .stick_to_bottom(record.running())
                .show(ui, |ui| {
                    let preview: String = record
                        .task
                        .prompt
                        .chars()
                        .take(48)
                        .map(|c| if c.is_whitespace() { ' ' } else { c })
                        .collect();
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(20, 37, 58))
                        .corner_radius(6)
                        .inner_margin(egui::Margin::same(2))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            egui::CollapsingHeader::new(
                                egui::RichText::new(format!("任务指令 · {preview}"))
                                    .color(TextKind::Instruction.color())
                                    .size(12.),
                            )
                            .id_salt((&record.task.task_id, "instruction"))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&record.task.prompt)
                                            .color(TextKind::Instruction.color())
                                            .size(12.),
                                    )
                                    .wrap()
                                    .selectable(true),
                                );
                            });
                        });
                    let answer = if record.running() {
                        None
                    } else {
                        record.result["outcome"]["answer"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                    };
                    let mut notes = Vec::new();
                    let mut has_main = false;
                    for line in &record.lines {
                        let kind = text_kind(line);
                        if matches!(kind, TextKind::Diagnostic | TextKind::Tool) {
                            notes.push((kind, line));
                            continue;
                        }
                        if answer.is_some_and(|a| a.trim() == line.trim())
                            || (kind == TextKind::Result && answer.is_some())
                        {
                            continue;
                        }
                        if !has_main {
                            ui.add_space(4.);
                            ui.label(egui::RichText::new("执行输出").size(12.).color(MUTED));
                            has_main = true;
                        }
                        ui.add(
                            egui::Label::new(egui::RichText::new(line).color(kind.color()))
                                .wrap()
                                .selectable(true),
                        );
                    }
                    if !record.running() {
                        ui.add_space(5.);
                        let success = record.state == "succeeded";
                        let text = answer
                            .or_else(|| record.result["error"].as_str())
                            .unwrap_or(if success {
                                "执行完成，未返回文本。"
                            } else {
                                "本次执行未正常完成，可展开日志查看详情。"
                            });
                        result_block(
                            ui,
                            if success {
                                "返回结果"
                            } else {
                                "执行结果"
                            },
                            text,
                            if success {
                                TextKind::Result
                            } else {
                                TextKind::Error
                            },
                        );
                    } else if !has_main {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(if record.current_tool.is_empty() {
                                    "等待 CLI 输出…"
                                } else {
                                    &record.current_tool
                                })
                                .color(TextKind::Tool.color())
                                .size(12.),
                            );
                        });
                    }
                    if !notes.is_empty() {
                        ui.add_space(4.);
                        egui::CollapsingHeader::new(
                            egui::RichText::new(format!("工具与运行日志 ({})", notes.len()))
                                .color(TextKind::Tool.color())
                                .size(12.),
                        )
                        .id_salt((&record.task.task_id, "diagnostics"))
                        .show(ui, |ui| {
                            for (kind, line) in notes {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(line).color(kind.color()).size(12.),
                                    )
                                    .wrap()
                                    .selectable(true),
                                );
                            }
                        });
                    }
                });
        });
    action
}

/// 按实际显示数量分配高度，预留屏幕边缘；超过显示器高度的卡片通过滚动查看。
pub fn floating_size(count: usize, monitor_height: Option<f32>) -> egui::Vec2 {
    egui::vec2(
        400.,
        if count == 0 {
            86.
        } else {
            let height = 42. + 98. * count as f32;
            monitor_height.map_or(height, |screen| height.min((screen - 80.).max(86.)))
        },
    )
}

/// 拖动区与两个按钮独立命中；返回关闭、开始拖动、清空终态任务的意图。
pub fn floating_header(ui: &mut egui::Ui, running: usize, model: &str) -> (bool, bool, bool) {
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 80.).max(80.);
        let drag = ui
            .allocate_ui_with_layout(
                egui::vec2(width, 22.),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(width);
                    ui.label(egui::RichText::new("Agent Router").size(13.).strong());
                    ui.label(
                        egui::RichText::new(format!("{running} 运行"))
                            .size(11.)
                            .color(ACCENT),
                    );
                    ui.add(
                        egui::Label::new(egui::RichText::new(model).size(11.).color(MUTED))
                            .truncate(),
                    );
                },
            )
            .response
            .interact(egui::Sense::drag());
        let clear = ui
            .add_sized([46., 22.], egui::Button::new("清空").corner_radius(6))
            .on_hover_text("清理已结束任务，保留运行任务和日志")
            .clicked();
        let close = ui
            .scope(|ui| {
                // 固定正方形命中区和零内边距，圆角半径为直径的一半。
                ui.spacing_mut().button_padding = egui::Vec2::ZERO;
                ui.add_sized(
                    [22., 22.],
                    egui::Button::new(
                        egui::RichText::new("×")
                            .size(15.)
                            .color(egui::Color32::from_rgb(159, 65, 78)),
                    )
                    .fill(egui::Color32::from_rgb(248, 184, 193))
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(11),
                )
                .on_hover_text("关闭悬浮卡片")
                .clicked()
            })
            .inner;
        (close, drag.drag_started(), clear)
    })
    .inner
}

/// 四行卡片：标题/状态/健康一行、指标一行、内容两行；按固定高度避免底部留白。
pub fn floating_summary(ui: &mut egui::Ui, record: &Record, now: i64) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(24, 36, 53, 72))
        .stroke(egui::Stroke::new(
            1.,
            egui::Color32::from_rgba_unmultiplied(65, 92, 114, 110),
        ))
        .corner_radius(12)
        .inner_margin(egui::Margin::same(7))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.;
                let (health, color) = health_label(record);
                let age = activity_age(record.last_activity, now);
                let trailing_width = ui.fonts(|fonts| {
                    fonts.layout_no_wrap(
                        format!("{health} {age}"),
                        egui::FontId::proportional(11.),
                        MUTED,
                    ).size().x
                });
                let title_width = (ui.available_width() - trailing_width - 72.).max(30.);
                let title = ui
                    .allocate_ui_with_layout(
                        egui::vec2(title_width, 22.),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let (position, galley, response) = egui::Label::new(
                                egui::RichText::new(record.task.label()).size(12.).strong(),
                            )
                            .truncate()
                            .layout_in_ui(ui);
                            ui.painter().galley(position, galley, TEXT);
                            response
                        },
                    )
                    .inner;
                title.on_hover_text(format!(
                    "{}\n{} · {} · {}\n耗时 {} · 最近活动 {}",
                    record.task.label(),
                    record.task.profile_key(),
                    record.task.model.as_deref().unwrap_or("CLI 默认"),
                    record.task.effort.as_deref().unwrap_or("默认强度"),
                    elapsed_text(record.elapsed(now)),
                    activity_age(record.last_activity, now)
                ));
                badge(ui, state_label(&record.state), state_color(&record.state));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(age).size(11.).color(MUTED))
                        .on_hover_text("距离最近有效活动的时间");
                    ui.label(egui::RichText::new(health).size(11.).color(color));
                });
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.;
                ui.style_mut().text_styles.insert(
                    egui::TextStyle::Small,
                    egui::FontId::proportional(10.),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "本轮 {}",
                        elapsed_text(record.turn_elapsed(now))
                    ))
                    .small()
                    .color(MUTED),
                )
                .on_hover_text("最近一轮的执行耗时");
                ui.label(
                    egui::RichText::new(format!(
                        "上下文 {}",
                        token_text(record.metrics.context_tokens)
                    ))
                    .small()
                    .color(TextKind::Instruction.color()),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "Token {}",
                        token_text(record.metrics.total_tokens())
                    ))
                    .small()
                    .color(TextKind::Tool.color()),
                );
                if let Some(ms) = record.metrics.first_text_ms {
                    let name = if record.metrics.first_text_source.as_deref()
                        == Some("completed_message")
                    {
                        "首段"
                    } else {
                        "首字"
                    };
                    ui.label(
                        egui::RichText::new(format!("{name} {:.1}s", ms as f64 / 1000.))
                            .small()
                            .color(MUTED),
                    );
                }
                reply_latency(ui, record);
            });
            let text = if record.running() {
                if record.current_tool.is_empty() {
                    "等待 CLI 输出…"
                } else {
                    &record.current_tool
                }
            } else {
                record.result["outcome"]["answer"]
                    .as_str()
                    .or(record.result["error"].as_str())
                    .unwrap_or("执行已结束")
            };
            let mut summary: String = text.chars().take(120).collect();
            if text.chars().count() > 120 {
                summary.push('…');
            }
            let color = if record.running() {
                TextKind::Tool.color()
            } else {
                state_color(&record.state)
            };
            let mut job = egui::text::LayoutJob::simple(
                summary,
                egui::FontId::proportional(12.),
                color,
                ui.available_width(),
            );
            job.wrap.max_rows = 2;
            job.wrap.break_anywhere = true;
            let galley = ui.fonts(|fonts| fonts.layout_job(job));
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.), egui::Sense::hover());
            ui.painter().galley(rect.min, galley, color);
            response.on_hover_text(text);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 使用真实字体与主题检查四行卡片的行位置及窗口底部空白。
    #[test]
    fn floating_compact_rows_align_and_fit() {
        let ctx = egui::Context::default();
        load_fonts(&ctx);
        apply_theme(&ctx);
        let mut record = crate::store::tests::sample("compact");
        record.task.title = Some("T3 VM接口与表图小图联动".into());
        record.metrics.first_text_ms = Some(6800);
        record.metrics.context_tokens = Some(130500);
        record.metrics.input_tokens = Some(648100);
        record.metrics.output_tokens = Some(1000);
        let size = floating_size(4, Some(1080.));
        let mut bottom = 0.;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(8))
                    .show(ctx, |ui| {
                        floating_header(ui, 0, "cc-glm-5.3-max");
                        ui.separator();
                        for _ in 0..4 {
                            floating_summary(ui, &record, 1000);
                        }
                        bottom = ui.min_rect().bottom();
                    });
            },
        );
        let title = text_position(&output, record.task.label());
        let status = text_position(&output, "已完成");
        let health = text_position(&output, "已结束");
        assert!(title.x < status.x && status.x < health.x);
        assert!((title.y - health.y).abs() < 6.);
        let first = text_position(&output, "首字 6.8s");
        let mean = text_position(&output, "均耗时 —");
        assert!(first.x < mean.x && (first.y - mean.y).abs() < 2.);
        let timing = text_position(
            &output,
            &format!(
                "本轮 {}",
                elapsed_text(record.turn_elapsed(1000))
            ),
        );
        assert!(timing.x < first.x && (timing.y - first.y).abs() < 2.);
        let age = text_position(&output, &activity_age(record.last_activity, 1000));
        assert!(health.x < age.x && (health.y - age.y).abs() < 2.);
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                if (text.pos.y - first.y).abs() < 2. {
                    assert!(
                        text.pos.x + text.galley.size().x <= size.x - 16.,
                        "{}: {}",
                        text.galley.text(),
                        text.pos.x + text.galley.size().x
                    );
                }
            }
        }
        assert!(
            size.y - bottom >= 5. && size.y - bottom <= 14.,
            "height={} bottom={bottom}",
            size.y
        );
    }
    /// 隐藏的 Win32 测试窗口验证真实合成透明度、任务栏标志和圆角区域，不启动第二个管理器。
    #[test]
    fn native_overlay_is_layered_rounded_and_not_appwindow() {
        use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
        /// 测试退出时释放本线程创建的原生窗口。
        struct Window(HWND);
        impl Drop for Window {
            /// DestroyWindow 只用于该测试拥有的窗口。
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                }
            }
        }
        unsafe {
            let class: Vec<_> = "STATIC".encode_utf16().chain(Some(0)).collect();
            let title: Vec<_> = "Agent Router · 悬浮监控"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let window = Window(CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_CAPTION | WS_BORDER | WS_THICKFRAME | WS_SYSMENU,
                0,
                0,
                360,
                388,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ));
            assert!(!window.0.is_null());
            let mut style = OverlayWindow::default();
            assert!(style.apply(1.).unwrap());
            let flags = GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32;
            assert_eq!(flags & WS_EX_APPWINDOW, 0);
            assert_ne!(flags & WS_EX_TOOLWINDOW, 0);
            assert_ne!(flags & WS_EX_LAYERED, 0);
            assert_ne!(flags & WS_EX_NOACTIVATE, 0);
            assert_eq!(
                flags
                    & (WS_EX_WINDOWEDGE
                        | WS_EX_CLIENTEDGE
                        | WS_EX_STATICEDGE
                        | WS_EX_DLGMODALFRAME),
                0
            );
            for active in [1, 0, 1, 0] {
                SendMessageW(window.0, WM_NCACTIVATE, active, 0);
                assert_eq!(
                    GetWindowLongPtrW(window.0, GWL_STYLE) as u32
                        & (WS_CAPTION | WS_BORDER | WS_THICKFRAME),
                    0
                );
            }
            let (mut color, mut alpha, mut flags) = (0, 0, 0);
            assert_ne!(
                GetLayeredWindowAttributes(window.0, &mut color, &mut alpha, &mut flags),
                0
            );
            assert_eq!(alpha, 190);
            assert_eq!(flags, LWA_ALPHA);
            let region = CreateRectRgn(0, 0, 0, 0);
            assert_ne!(GetWindowRgn(window.0, region), 0);
            assert_eq!(PtInRegion(region, 0, 0), 0);
            assert_ne!(PtInRegion(region, 24, 24), 0);
            DeleteObject(region);
            // 模拟窗口重建样式后丢失 region；同样尺寸下也必须恢复。
            SetWindowRgn(window.0, std::ptr::null_mut(), 0);
            assert!(style.apply(1.).unwrap());
            let restored = CreateRectRgn(0, 0, 0, 0);
            assert_ne!(GetWindowRgn(window.0, restored), 0);
            assert_eq!(PtInRegion(restored, 0, 0), 0);
            DeleteObject(restored);
            assert_eq!(IsWindowVisible(window.0), 0);
            SetWindowPos(
                window.0,
                std::ptr::null_mut(),
                -32000,
                -32000,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            ShowWindow(window.0, SW_SHOWNOACTIVATE);
            style.hide();
            assert_eq!(IsWindowVisible(window.0), 0);
        }
    }
    /// 真实 egui 按下/释放命中右上按钮，不应被标题拖动区拦截。
    #[test]
    fn floating_close_does_not_start_drag() {
        let ctx = egui::Context::default();
        let draw = |events| {
            let mut action = (false, false, false);
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360., 388.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = floating_header(ui, 3, "cx-gpt-5.6-luna-max");
                    });
                },
            );
            (output, action)
        };
        let (output, _) = draw(vec![]);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.job.text == "cx-gpt-5.6-luna-max" && !text.galley.elided)));
        let pos = text_position(&output, "×");
        draw(click(pos, egui::PointerButton::Primary, true));
        let (_, action) = draw(click(pos, egui::PointerButton::Primary, false));
        assert_eq!(action, (true, false, false));
        let (output, _) = draw(vec![]);
        let clear = text_position(&output, "清空");
        draw(click(clear, egui::PointerButton::Primary, true));
        let (_, action) = draw(click(clear, egui::PointerButton::Primary, false));
        assert_eq!(action, (false, false, true));
    }
    /// 高度只跟零到三个可见任务数相关，不被长文本或额外后台任务撑大。
    #[test]
    fn floating_dimensions_are_bounded() {
        assert_eq!(floating_size(0, None), egui::vec2(400., 86.));
        assert_eq!(floating_size(1, None), egui::vec2(400., 140.));
        assert_eq!(floating_size(4, Some(1080.)), egui::vec2(400., 434.));
        assert_eq!(floating_size(20, Some(1080.)), egui::vec2(400., 1000.));
    }
    /// 耗时和活动时间使用同一紧凑单位，边界不会显示负数或错进位。
    #[test]
    fn compact_durations_and_activity_age() {
        for (ms, expected) in [
            (25000, "25s"),
            (122000, "2m2s"),
            (14580000, "4h3m"),
            (60000, "1m"),
            (3600000, "1h"),
        ] {
            assert_eq!(elapsed_text(ms), expected);
        }
        assert_eq!(activity_age(0, 18120000), "5h2m前");
        assert_eq!(activity_age(10, 0), "0s前");
        assert_eq!(token_text(None), "—");
    }
    /// 已有工具和诊断前缀收进辅助区，执行正文与最终结果保持独立。
    #[test]
    fn text_roles_preserve_main_output() {
        assert_eq!(text_kind("工具调用 · Read"), TextKind::Tool);
        assert_eq!(text_kind("CLI 日志 · trace"), TextKind::Diagnostic);
        assert_eq!(text_kind("最终结果 · 完成"), TextKind::Result);
        assert_eq!(text_kind("修改了计算函数"), TextKind::Main);
    }
    /// 以真实 egui 鼠标事件验证归档视图的右键删除，不依赖桌面是否锁定。
    #[test]
    fn archived_task_context_menu_dispatches_delete() {
        let ctx = egui::Context::default();
        ctx.style_mut(|s| s.animation_time = 0.);
        let mut record = crate::store::tests::sample("item");
        record.archived = true;
        let (first, _) = render(&ctx, &record, vec![]);
        let item = text_position(&first, "item");
        render(&ctx, &record, vec![egui::Event::PointerMoved(item)]);
        render(
            &ctx,
            &record,
            click(item, egui::PointerButton::Secondary, true),
        );
        render(
            &ctx,
            &record,
            click(item, egui::PointerButton::Secondary, false),
        );
        let (menu, _) = render(&ctx, &record, vec![]);
        let delete = text_position(&menu, "删除任务（保留日志）");
        render(
            &ctx,
            &record,
            click(delete, egui::PointerButton::Primary, true),
        );
        let (_, actions) = render(
            &ctx,
            &record,
            click(delete, egui::PointerButton::Primary, false),
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a,Action::Delete(id) if id=="item"))
        );
    }
    /// 顶层分组右键删除能收集嵌套叶子任务，不要求逐个展开和点击。
    #[test]
    fn group_context_menu_dispatches_descendant_ids() {
        let ctx = egui::Context::default();
        ctx.style_mut(|s| s.animation_time = 0.);
        let mut record = crate::store::tests::sample("nested-item");
        record.task.group_path = vec!["source".into(), "app".into(), "feat".into()];
        let (first, _) = render(&ctx, &record, vec![]);
        let group = text_position(&first, "source (1) · 0s前");
        render(
            &ctx,
            &record,
            click(group, egui::PointerButton::Secondary, true),
        );
        render(
            &ctx,
            &record,
            click(group, egui::PointerButton::Secondary, false),
        );
        let (menu, _) = render(&ctx, &record, vec![]);
        let delete = text_position(&menu, "删除组内已结束任务（保留日志）");
        render(
            &ctx,
            &record,
            click(delete, egui::PointerButton::Primary, true),
        );
        let (_, actions) = render(
            &ctx,
            &record,
            click(delete, egui::PointerButton::Primary, false),
        );
        assert!(
            actions.iter().any(
                |action| matches!(action, Action::DeleteGroup(ids) if ids == &["nested-item"])
            )
        );
    }
    /// 用固定尺寸画布运行实际任务树，收集绘制结果与操作意图。
    fn render(
        ctx: &egui::Context,
        record: &Record,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Vec<Action>) {
        let mut actions = vec![];
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800., 600.),
            )),
            events,
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                tree(ui, &[record], 0, "test", &mut None, 0, &mut actions)
            });
        });
        (output, actions)
    }
    /// 从真实绘制文本获取点击位置，避免手算控件坐标。
    fn text_position(output: &egui::FullOutput, text: &str) -> egui::Pos2 {
        output
            .shapes
            .iter()
            .find_map(|s| {
                if let egui::Shape::Text(t) = &s.shape {
                    if t.galley.job.text == text {
                        Some(t.pos + egui::vec2(3., 3.))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("missing rendered text: {text}"))
    }
    /// 每帧模拟一个按键阶段，使上下文菜单经历完整的按下/释放路径。
    fn click(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: Default::default(),
            },
        ]
    }
}
