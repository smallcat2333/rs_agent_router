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

/// 可见悬浮窗每秒恢复置顶层级；不依赖 egui 重绘，不激活窗口或重新显示已隐藏窗口。
unsafe extern "system" fn overlay_topmost_timer(hwnd: windows_sys::Win32::Foundation::HWND, _: u32, _: usize, _: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        if !is_overlay(hwnd) || IsWindowVisible(hwnd) == 0 {
            return;
        }
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0
            && GetWindow(hwnd, GW_HWNDPREV).is_null()
        {
            return;
        }
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
    }
}

thread_local! {
    /// 仅 GUI 线程维护当前悬浮窗按钮的客户区像素坐标。
    static OVERLAY_HITS: std::cell::RefCell<Option<(usize, [egui::Rect; 2])>> = const { std::cell::RefCell::new(None) };
}

/// 即使窗口已穿透也能检测鼠标进入按钮；只切换命中样式，不重绘或改变透明度。
unsafe extern "system" fn overlay_hit_timer(hwnd: windows_sys::Win32::Foundation::HWND, _: u32, _: usize, _: u32) {
    use windows_sys::Win32::{Foundation::POINT, Graphics::Gdi::ScreenToClient, UI::WindowsAndMessaging::*};
    OVERLAY_HITS.with(|state| {
        if let Some((owner, areas)) = *state.borrow() {
            if owner != hwnd as usize { return; }
            unsafe {
                let mut point = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut point) == 0 || ScreenToClient(hwnd, &mut point) == 0 { return; }
                let over_button = areas.iter().any(|area| area.contains(egui::pos2(point.x as f32, point.y as f32)));
                let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                // 按下后直到释放都保留输入，不能在一次点击中途撤销鼠标捕获。
                if old & WS_EX_TRANSPARENT == 0
                    && windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(1) < 0
                { return; }
                let style = if over_button { old & !WS_EX_TRANSPARENT } else { old | WS_EX_TRANSPARENT };
                if style != old { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize); }
            }
        }
    });
}

/// 模型菜单在滚动区外设定十行高度，避免弹层缓存的初始高度限制可见选项数。
pub fn model_picker(ui: &mut egui::Ui, selected: &mut Option<String>, choices: &[String]) {
    let popup_id = ui.make_persistent_id("model_picker_popup");
    let button = ui.add_sized([160., 24.], egui::Button::new(""));
    let color = ui.style().interact(&button).text_color();
    let text_rect = egui::Rect::from_min_max(
        button.rect.min + egui::vec2(6., 0.),
        button.rect.max - egui::vec2(24., 0.),
    );
    ui.painter().with_clip_rect(text_rect.intersect(ui.clip_rect())).text(
        text_rect.left_center(), egui::Align2::LEFT_CENTER,
        selected.as_deref().unwrap_or("CLI 默认"),
        egui::TextStyle::Button.resolve(ui.style()), color,
    );
    // 使用几何图形绘制箭头，不依赖字体是否包含下拉符号。
    let center = egui::pos2(button.rect.right() - 12., button.rect.center().y);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![center + egui::vec2(-4., -2.), center + egui::vec2(4., -2.),
             center + egui::vec2(0., 3.)],
        color, egui::Stroke::NONE,
    ));
    if button.clicked() {
        ui.memory_mut(|memory| memory.toggle_popup(popup_id));
    }
    egui::popup::popup_below_widget(ui, popup_id, &button,
        egui::PopupCloseBehavior::CloseOnClick, |ui| {
            let row_height = 26.;
            let spacing = ui.spacing().item_spacing.y;
            let rows = (choices.len() + 1).min(10);
            let height = rows as f32 * (row_height + spacing);
            ui.set_height(height);
            egui::ScrollArea::vertical().max_height(height).show(ui, |ui| {
                ui.spacing_mut().interact_size.y = row_height;
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                ui.selectable_value(selected, None, "CLI 默认");
                for model in choices {
                    ui.selectable_value(selected, Some(model.clone()), model);
                }
            });
        });
}

/// Windows 合成级透明、工具窗标记和圆角；缓存尺寸避免每次重绘都重设窗口区域。
#[derive(Default)]
pub struct OverlayWindow {
    hwnd: usize,
    shape: (i32, i32, i32),
    topmost_timer_started: bool,
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
    /// 穿透时只保留清空与关闭两个命中区；16ms 原生计时器不触发 UI 重绘。
    pub fn button_hit_regions(&mut self, passthrough: bool, areas: [egui::Rect; 2], scale: f32) {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        unsafe {
            if !is_overlay(self.hwnd as _) { return; }
            if passthrough {
                OVERLAY_HITS.with(|state| *state.borrow_mut() = Some((self.hwnd, areas.map(|rect| {
                    egui::Rect::from_min_max(rect.min * scale, rect.max * scale)
                }))));
                SetTimer(self.hwnd as _, 0x4152, 16, Some(overlay_hit_timer));
                overlay_hit_timer(self.hwnd as _, 0, 0, 0);
            } else {
                KillTimer(self.hwnd as _, 0x4152);
                OVERLAY_HITS.with(|state| *state.borrow_mut() = None);
            }
        }
    }
    /// 先撤下原生表面并刷新原覆盖区域；随后由 egui 销毁视口，避免关闭末帧留下黑框。
    pub fn hide(&mut self) {
        use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
        unsafe {
            let hwnd = self.hwnd as _;
            KillTimer(hwnd, 0x4152);
            KillTimer(hwnd, 0x4153);
            self.topmost_timer_started = false;
            OVERLAY_HITS.with(|state| *state.borrow_mut() = None);
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
    /// 应用透明度、圆角与工具窗样式；穿透模式让鼠标命中下层窗口，退出该模式恢复交互。
    pub fn apply(&mut self, pixels_per_point: f32, passthrough: bool) -> anyhow::Result<bool> {
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
                self.topmost_timer_started = false;
            }
            let hwnd = self.hwnd as HWND;
            let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let style = (old | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE)
                & !(WS_EX_APPWINDOW
                    | WS_EX_TRANSPARENT
                    | WS_EX_WINDOWEDGE
                    | WS_EX_CLIENTEDGE
                    | WS_EX_STATICEDGE
                    | WS_EX_DLGMODALFRAME);
            // 已启用局部穿透时由命中计时器维护该位，重绘不能先穿透再恢复而打断点击。
            let hit_tracking = OVERLAY_HITS.with(|state| state.borrow().as_ref().is_some_and(|(owner, _)| *owner == self.hwnd));
            let style = if passthrough {
                style | if hit_tracking { old & WS_EX_TRANSPARENT } else { WS_EX_TRANSPARENT }
            } else { style };
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
            // Builder 的 AlwaysOnTop 只负责初始状态；真实层级改变后需原生恢复。
            if old & WS_EX_TOPMOST == 0
                && SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // 仅注册一次，避免持续重绘反复重置计时器、使回调永远无法到期。
            if !self.topmost_timer_started {
                if SetTimer(hwnd, 0x4153, 1000, Some(overlay_topmost_timer)) == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                self.topmost_timer_started = true;
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

/// 最近有效活动按整分钟显示，避免秒级跳动；不改变活动计时或归档。
pub fn activity_age(last_activity: i64, now: i64) -> String {
    let minutes = now.saturating_sub(last_activity).max(0) / 60_000;
    if minutes >= 60 {
        format!("{}h{}m前", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m前")
    }
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

/// 两个窗口共用输出 TPS；主界面解释测量口径，小窗保持无悬浮提示。
fn output_throughput(ui: &mut egui::Ui, record: &Record, tooltip: bool) {
    let metrics = &record.metrics;
    let value = metrics.output_tps().map(|tps| {
        let estimate = if metrics.tps_estimated { "≈" } else { "" };
        format!("{estimate}{tps:.1}")
    }).unwrap_or_else(|| "—".into());
    let response = ui.label(egui::RichText::new(format!("TPS {value}")).small().color(ACCENT));
    if tooltip {
        let source = if record.task.backend == crate::protocol::Backend::Codex {
            "Codex：输出 Token ÷ 排除工具执行后的模型阶段秒数；包含请求等待，≈ 表示近似值。"
        } else if record.task.backend == crate::protocol::Backend::Opencode {
            "OpenCode：已接入原生 Token 用量；未提供可靠的流式耗时，TPS 保持未知。"
        } else if record.task.backend == crate::protocol::Backend::Agy {
            "Antigravity：完成回复步骤的输出 Token 加思考 Token，除以该步骤 duration_seconds；不含启动等待和工具步骤。"
        } else {
            "Claude：完整响应流的输出 Token ÷ 响应流秒数；不含首字等待、工具执行及子代理。"
        };
        let tokens = metrics.model_output_tokens.map(|tokens| tokens.to_string()).unwrap_or_else(|| "—".into());
        response.on_hover_text(format!(
            "{source}\n本次任务最近 10 轮（已有 {} 轮）配对输出：{tokens} Token；模型阶段：{:.3}s。每轮更新；输入与缓存不计入 TPS；计时或用量缺失时显示 —。",
            metrics.throughput_samples.len(),
            metrics.model_duration_ms as f64 / 1000.,
        ));
    }
}

/// 健康只描述操作系统进程证据；终态直接显示已结束，不冒充模型响应健康。
fn health_label(record: &Record) -> (&str, egui::Color32) {
    if record.state == "queued" {
        return ("等待名额", state_color("queued"));
    }
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
        "queued" => egui::Color32::from_rgb(185, 145, 240),
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
fn reply_latency(ui: &mut egui::Ui, record: &Record, tooltip: bool) {
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
            "agy_response" => "回复步骤耗时，不含启动等待",
            _ => "CLI 回复周期，已排除工具阶段，非精确 API 耗时",
        };
        detail.push_str(&format!(
            "\n近第 {} 条：{:.2}s（{source}）",
            index + 1,
            reply.duration_ms as f64 / 1000.
        ));
    }
    let response = ui.label(
        egui::RichText::new(format!("均耗时 {average}"))
            .small()
            .color(MUTED),
    );
    if tooltip { response.on_hover_text(detail); }
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
        "queued" => "排队中",
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

/// 将输出到达时间转为本机 H:MM:SS；耗时按下一条正文或结束时刻计算，时钟回拨不显示负数。
fn output_time_label(at_ms: i64, end_ms: i64) -> String {
    let seconds = end_ms.saturating_sub(at_ms).max(0) / 1000;
    format!("{}({}m{}s)", local_time_label(at_ms), seconds / 60, seconds % 60)
}

/// 将日志时间统一转换为本机 H:MM:SS，供 CLI 与 DCR 的输出行复用。
pub(crate) fn local_time_label(at_ms: i64) -> String {
    use windows_sys::Win32::{Foundation::{FILETIME, SYSTEMTIME}, System::Time::*};
    let ticks = ((at_ms as u64) + 11_644_473_600_000) * 10_000;
    let filetime = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        assert_ne!(FileTimeToSystemTime(&filetime, &mut utc), 0, "invalid output timestamp");
        assert_ne!(SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local), 0,
            "cannot convert output timestamp to local time");
    }
    format!("{}:{:02}:{:02}", local.wHour, local.wMinute, local.wSecond)
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
              let model_width = (ui.available_width() - 455.).max(80.);
              ui.allocate_ui_with_layout(egui::vec2(model_width, 20.), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_width(model_width);
                ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} · 模型 {} · 强度 {}",
                        record.task.profile_key(),
                        record.display_model(),
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
                  output_throughput(ui, record, true);
                  reply_latency(ui, record, true);
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
                    let mut main_lines = Vec::new();
                    for line in &record.lines {
                        let kind = text_kind(&line.text);
                        if matches!(kind, TextKind::Diagnostic | TextKind::Tool) {
                            notes.push((kind, &line.text));
                            continue;
                        }
                        if answer.is_some_and(|a| a.trim() == line.text.trim())
                            || (kind == TextKind::Result && answer.is_some())
                        {
                            continue;
                        }
                        main_lines.push((kind, line));
                    }
                    let has_main = !main_lines.is_empty();
                    if has_main {
                        ui.add_space(4.);
                        ui.label(egui::RichText::new("执行输出").size(12.).color(MUTED));
                    }
                    for (index, (kind, line)) in main_lines.iter().enumerate() {
                        let end_ms = main_lines.get(index + 1).map(|(_, next)| next.at_ms)
                            .or(record.finished_at).unwrap_or(now);
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{} ", output_time_label(line.at_ms, end_ms)), 0.,
                            egui::TextFormat { font_id: font.clone(), color: MUTED, ..Default::default() });
                        job.append(&line.text, 0.,
                            egui::TextFormat { font_id: font, color: kind.color(), ..Default::default() });
                        ui.add(
                            egui::Label::new(job)
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

/// 图标和标题共用拖动区，与两个按钮独立命中；返回关闭、拖动、清空终态任务的意图。
pub fn floating_header(ui: &mut egui::Ui, running: usize, model: &str, logo: egui::TextureId) -> (bool, bool, bool) {
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 80.).max(80.);
        let drag = ui
            .allocate_ui_with_layout(
                egui::vec2(width, 22.),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(width);
                    ui.image((logo, egui::vec2(18., 18.)));
                    ui.label(egui::RichText::new("Agent Router").size(13.).strong());
                    ui.label(
                        egui::RichText::new(format!("{running} 运行"))
                            .size(11.)
                            .color(ACCENT),
                    );
                    let (position, galley, _) =
                        egui::Label::new(egui::RichText::new(model).size(11.).color(MUTED))
                            .truncate().layout_in_ui(ui);
                    ui.painter().galley(position, galley, MUTED);
                },
            )
            .response
            .interact(egui::Sense::drag());
        let clear = ui
            .add_sized([46., 22.], egui::Button::new("清空").corner_radius(6));
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
            })
            .inner;
        ui.ctx().data_mut(|data| data.insert_temp(egui::Id::new("floating_button_rects"), [clear.rect, close.rect]));
        (close.clicked(), drag.drag_started(), clear.clicked())
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
                ui
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
                    );
                badge(ui, state_label(&record.state), state_color(&record.state));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(age).size(11.).color(MUTED));
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
                );
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
                reply_latency(ui, record, false);
                output_throughput(ui, record, false);
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
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.), egui::Sense::hover());
            ui.painter().galley(rect.min, galley, color);
        });
}

#[cfg(test)]
mod tests {
    /// 实际渲染验证灰色前缀、正文保色、工具不截断计时，以及下一条和终态固定耗时。
    #[test]
    fn output_timestamps_accumulate_then_freeze() {
        let ctx = egui::Context::default();
        let mut record = crate::store::tests::sample("output-time");
        record.state = "running".into();
        record.finished_at = None;
        let start = 1_789_430_400_000;
        record.append("First output".into(), start);
        record.append("工具调用 test".into(), start + 10_000);
        /// 抽取实际执行卡片的排版任务，供断言文字和分段颜色。
        fn jobs(ctx: &egui::Context, record: &Record, now: i64) -> Vec<egui::text::LayoutJob> {
            let output = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 1800.))),
                ..Default::default()
            }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| { execution(ui, record, now); });
            });
            output.shapes.into_iter().filter_map(|shape| {
                if let egui::Shape::Text(text) = shape.shape { Some(text.galley.job.as_ref().clone()) } else { None }
            }).collect()
        }
        for seconds in [0, 61, 121] {
            let rendered = jobs(&ctx, &record, start + seconds * 1000);
            let line = rendered.iter().find(|job| job.text.ends_with(" First output")).unwrap();
            assert_eq!(line.text, format!("{} First output", output_time_label(start, start + seconds * 1000)));
            assert_eq!(line.sections[0].format.color, MUTED);
            assert_eq!(line.sections[1].format.color, text_kind("First output").color());
        }
        record.append("Second output".into(), start + 121_000);
        record.state = "succeeded".into();
        record.finished_at = Some(start + 183_000);
        let rendered = jobs(&ctx, &record, start + 900_000);
        assert!(rendered.iter().any(|job| job.text.ends_with("(2m1s) First output")));
        assert!(rendered.iter().any(|job| job.text.ends_with("(1m2s) Second output")));
        assert!(output_time_label(start, start - 1000).ends_with("(0m0s)"));
        assert!(output_time_label(start, start + 3_661_000).ends_with("(61m1s)"));
    }
    use super::*;
    /// 在实际顶部水平滚动区中打开弹层，前十个选项必须完整落在裁剪区域内。
    #[test]
    fn model_popup_shows_ten_rows() {
        let ctx = egui::Context::default();
        load_fonts(&ctx);
        apply_theme(&ctx);
        let choices: Vec<String> = (1..=15).map(|i| format!("model-{i:02}")).collect();
        let mut selected = None;
        for _ in 0..3 {
            let output = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 800.))),
                ..Default::default()
            }, |ctx| {
                egui::TopBottomPanel::top("top").show(ctx, |ui| {
                    ui.add_space(40.);
                    egui::ScrollArea::horizontal().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_height(26.);
                            ui.memory_mut(|memory| memory.open_popup(ui.make_persistent_id("model_picker_popup")));
                            model_picker(ui, &mut selected, &choices);
                        });
                    });
                });
            });
            if output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.text() == "model-09")) {
                let visible = output.shapes.iter().filter(|shape| {
                    if let egui::epaint::Shape::Text(text) = &shape.shape {
                        let label = text.galley.text();
                        (label == "CLI 默认" || label.starts_with("model-"))
                            && shape.clip_rect.contains_rect(egui::Rect::from_min_size(text.pos, text.galley.size()))
                    } else { false }
                }).count();
                assert!(visible >= 10, "only {visible} rows visible");
                return;
            }
        }
        panic!("model popup did not render");
    }
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
        record.metrics.model_output_tokens = Some(400);
        record.metrics.model_duration_ms = 4000;
        record.metrics.tps_estimated = true;
        record.metrics.reply_durations.push_back(crate::metrics::ReplyDuration {
            duration_ms: 23500,
            source: "claude_stream".into(),
        });
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
                        floating_header(ui, 0, "cc-glm-5.3-max", egui::TextureId::User(1));
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
        let mean = text_position(&output, "均耗时 23.5s");
        assert!(first.x < mean.x && (first.y - mean.y).abs() < 2.);
        let tps = text_position(&output, "TPS ≈100.0");
        assert!(mean.x < tps.x && (mean.y - tps.y).abs() < 2.);
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
        let mut metrics_checked = 0;
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                // text_position 返回用于点击的内缩坐标，边界检查须还原为文字左上角。
                if (text.pos.y - (first.y - 3.)).abs() < 2. {
                    metrics_checked += 1;
                    assert!(
                        text.pos.x + text.galley.size().x <= size.x - 16.,
                        "{}: {}",
                        text.galley.text(),
                        text.pos.x + text.galley.size().x
                    );
                }
            }
        }
        assert_eq!(metrics_checked, 6);
        assert!(
            size.y - bottom >= 5. && size.y - bottom <= 14.,
            "height={} bottom={bottom}",
            size.y
        );
    }
    /// 主面板第二行按视觉顺序把 TPS 放在均耗时之后，较窄面板也不能裁掉指标。
    #[test]
    fn execution_tps_is_last_metric_and_fits() {
        let ctx = egui::Context::default();
        load_fonts(&ctx);
        apply_theme(&ctx);
        let mut record = crate::store::tests::sample("throughput");
        record.task.model = Some("gpt-5.6-luna".into());
        record.metrics.context_tokens = Some(130500);
        record.metrics.input_tokens = Some(648100);
        record.metrics.output_tokens = Some(1000);
        record.metrics.first_text_ms = Some(6800);
        record.metrics.reply_durations.push_back(crate::metrics::ReplyDuration {
            duration_ms: 23500,
            source: "codex_reply_cycle".into(),
        });
        record.metrics.model_output_tokens = Some(400);
        record.metrics.model_duration_ms = 4000;
        record.metrics.tps_estimated = true;
        for width in [650., 1000.] {
            let output = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 400.))),
                ..Default::default()
            }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    execution(ui, &record, 1000);
                });
            });
            let mean = text_position(&output, "均耗时 23.5s");
            let tps = text_position(&output, "TPS ≈100.0");
            assert!(mean.x < tps.x && (mean.y - tps.y).abs() < 2.);
            let mut metrics_checked = 0;
            for shape in &output.shapes {
                if let egui::epaint::Shape::Text(text) = &shape.shape
                    && (text.pos.y - (tps.y - 3.)).abs() < 2.
                {
                    metrics_checked += 1;
                    // 右对齐文字的 pos 是右侧锚点，galley.rect 才包含真实的左右边界。
                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                    assert!(shape.clip_rect.contains_rect(bounds), "{} clipped at width {width}: text {bounds:?}, clip {:?}", text.galley.text(), shape.clip_rect);
                }
            }
            assert!(metrics_checked >= 5);
        }
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
            assert!(style.apply(1., false).unwrap());
            let flags = GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32;
            assert_eq!(flags & WS_EX_APPWINDOW, 0);
            assert_ne!(flags & WS_EX_TOOLWINDOW, 0);
            assert_ne!(flags & WS_EX_LAYERED, 0);
            assert_ne!(flags & WS_EX_NOACTIVATE, 0);
            assert_ne!(flags & WS_EX_TOPMOST, 0);
            assert_eq!(flags & WS_EX_TRANSPARENT, 0);
            assert!(style.apply(1., true).unwrap());
            assert_ne!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT, 0);
            assert!(style.apply(1., false).unwrap());
            assert_eq!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT, 0);
            let mut cursor = POINT { x: 0, y: 0 };
            assert_ne!(GetCursorPos(&mut cursor), 0);
            assert_ne!(windows_sys::Win32::Graphics::Gdi::ScreenToClient(window.0, &mut cursor), 0);
            let point = egui::pos2(cursor.x as f32, cursor.y as f32);
            let button_area = egui::Rect::from_center_size(point, egui::vec2(100., 100.));
            style.button_hit_regions(true, [button_area; 2], 1.);
            assert_eq!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT, 0);
            // 重绘不能在鼠标已进入按钮后重新设成穿透，否则点击会丢失。
            assert!(style.apply(1., true).unwrap());
            assert_eq!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT, 0);
            style.button_hit_regions(true, [button_area.translate(egui::vec2(1000., 1000.)); 2], 1.);
            assert_ne!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT, 0);
            style.button_hit_regions(false, [button_area; 2], 1.);
            assert!(style.apply(1., false).unwrap());
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
            assert!(style.apply(1., false).unwrap());
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
            // 模拟置顶丢失；无需打开主界面或重绘，原生回调即可恢复且不抢焦点。
            let foreground = GetForegroundWindow();
            assert_ne!(SetWindowPos(window.0, HWND_NOTOPMOST, 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE), 0);
            assert_eq!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST, 0);
            overlay_topmost_timer(window.0, 0, 0, 0);
            assert_ne!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST, 0);
            assert_eq!(GetForegroundWindow(), foreground);
            // 另一置顶窗覆盖时，本窗置顶标志仍在；也必须恢复层级，不能只检查标志。
            let cover = Window(CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(), class.as_ptr(), WS_POPUP,
                -32000, -32000, 360, 388,
                std::ptr::null_mut(), std::ptr::null_mut(),
                std::ptr::null_mut(), std::ptr::null(),
            ));
            assert!(!cover.0.is_null());
            ShowWindow(cover.0, SW_SHOWNOACTIVATE);
            assert_ne!(SetWindowPos(cover.0, HWND_TOPMOST, 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE), 0);
            assert_ne!(GetWindowLongPtrW(window.0, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST, 0);
            overlay_topmost_timer(window.0, 0, 0, 0);
            assert_eq!(GetWindow(cover.0, GW_HWNDPREV), window.0);
            assert_eq!(GetForegroundWindow(), foreground);
            drop(cover);
            style.hide();
            assert_eq!(IsWindowVisible(window.0), 0);
            assert!(!style.topmost_timer_started);
            overlay_topmost_timer(window.0, 0, 0, 0);
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
                        floating_size(4, None),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = floating_header(ui, 3, "cx-gpt-5.6-luna-max", egui::TextureId::User(1));
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
        assert_eq!(activity_age(10, 0), "0m前");
        assert_eq!(activity_age(0, 59_999), "0m前");
        assert_eq!(activity_age(0, 60_000), "1m前");
        assert_eq!(activity_age(0, 3_600_000), "1h0m前");
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
        let group = text_position(&first, "source (1) · 0m前");
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
