//! Router 与 DCR 仅在展示层合并；DCR 不构造 CLI 任务，不进入模型指标或调度。
use crate::{dcr_session::DcrSession, store::Record, ui};
use eframe::egui;
use std::collections::BTreeMap;

/// 冒号不属于合法 Router task_id，保证选中态不会与用户任务冲突。
pub const DCR_ID: &str = ":dcr";

/// 同一排序列表中的两类卡片，各自保留真实执行来源。
pub enum Card<'a> {
    Router(&'a Record),
    Dcr(&'a DcrSession),
}

impl Card<'_> {
    /// 最近有效活动用于两个视图的一致排序，不受刷新频率影响。
    fn activity(&self) -> i64 {
        match self {
            Self::Router(r) => r.last_activity,
            Self::Dcr(s) => s.last_activity,
        }
    }
    /// 同时刻以稳定标识排序，避免卡片位置抖动。
    fn key(&self) -> &str {
        match self {
            Self::Router(r) => &r.task.task_id,
            Self::Dcr(_) => DCR_ID,
        }
    }
}

/// 统一选择最近卡片；只有真正执行中的调用豁免悬浮无活动隐藏。
pub fn recent<'a>(
    records: &'a [Record],
    dcr: &'a DcrSession,
    limit: usize,
    now: i64,
    idle_ms: Option<i64>,
) -> Vec<Card<'a>> {
    let visible = |busy: bool, activity: i64| {
        busy || idle_ms.is_none_or(|idle| now.saturating_sub(activity) < idle)
    };
    let mut cards: Vec<_> = records
        .iter()
        .filter(|r| !r.archived && !r.deleted && visible(r.running(), r.last_activity))
        .map(Card::Router)
        .collect();
    if dcr.visible && !dcr.archived && visible(dcr.busy(), dcr.last_activity) {
        cards.push(Card::Dcr(dcr));
    }
    cards.sort_by(|a, b| {
        b.activity()
            .cmp(&a.activity())
            .then_with(|| a.key().cmp(b.key()))
    });
    cards.truncate(limit);
    cards
}

/// DCR 状态色与常规卡片色系一致，在线空闲不使用运行中计数。
fn color(session: &DcrSession) -> egui::Color32 {
    if session.busy() {
        ui::ACCENT
    } else {
        match session.connection.as_str() {
            "failed" => egui::Color32::LIGHT_RED,
            "connecting" => egui::Color32::YELLOW,
            "online" => ui::TextKind::Result.color(),
            _ => ui::MUTED,
        }
    }
}

/// 汇总当前工具；同名并发保持重复项以反映真实数量。
fn tools(session: &DcrSession, now: i64) -> String {
    if session.busy() {
        format!(
            "当前工具：{} · 并发 {} · 本次 {}",
            session.active_tools.join("、"),
            session.active_tools.len(),
            duration(session.current_elapsed(now) as f64)
        )
    } else {
        "等待网页端调用".into()
    }
}

/// 仅包含调用耗时，不展示模型、Token 或评分信息。
pub fn timing(session: &DcrSession, now: i64) -> String {
    let stats = session.window_stats(now);
    let mean = stats
        .mean_ms
        .map(|ms| format!("{}{}", if stats.estimated { "≈" } else { "" }, duration(ms)))
        .unwrap_or_else(|| "—".into());
    format!(
        "近 {}h · 累计耗时 {} · 累计调用 {} · 均耗时 {}",
        session.window_ms / 3_600_000,
        duration(stats.elapsed_ms as f64),
        stats.calls,
        mean
    )
}

/// 短调用保留毫秒，避免有效调用耗时被整秒格式压成 0s。
fn duration(ms: f64) -> String {
    if ms < 1000. {
        format!("{ms:.0}ms")
    } else if ms < 60_000. {
        format!("{:.1}s", ms / 1000.)
    } else {
        ui::elapsed_text(ms as i64)
    }
}

/// DCR 展示菜单只改变归档和可见性，停止入口留在顶部设置中。
fn menu(ui: &mut egui::Ui, session: &DcrSession, actions: &mut Vec<ui::Action>) {
    if ui
        .add_enabled(
            !session.busy(),
            egui::Button::new(if session.archived {
                "移回任务列表"
            } else {
                "归档会话"
            }),
        )
        .clicked()
    {
        actions.push(ui::Action::Archive(DCR_ID.into(), !session.archived));
        ui.close_menu();
    }
    if ui
        .add_enabled(!session.busy(), egui::Button::new("清除展示"))
        .clicked()
    {
        actions.push(ui::Action::Delete(DCR_ID.into()));
        ui.close_menu();
    }
}

/// 在来源层合并 DCR 单会话与 Router 分组，组内沿用现有任务树。
#[allow(clippy::too_many_arguments)]
pub fn tree(
    ui: &mut egui::Ui,
    records: &[&Record],
    dcr: &DcrSession,
    archived: bool,
    selected: &mut Option<String>,
    now: i64,
    actions: &mut Vec<ui::Action>,
) {
    let mut groups: BTreeMap<(bool, String), Vec<&Record>> = BTreeMap::new();
    for record in records {
        let key = match record.task.group_path.first() {
            Some(name) => (false, name.clone()),
            None => (true, "未分组".into()),
        };
        groups.entry(key).or_default().push(record);
    }
    let mut roots: Vec<_> = groups
        .iter()
        .map(|(key, rows)| {
            (
                rows.iter().map(|r| r.last_activity).max().unwrap(),
                Some(key),
            )
        })
        .collect();
    if dcr.visible && dcr.archived == archived {
        roots.push((dcr.last_activity, None));
    }
    roots.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, key) in roots {
        if let Some(key) = key {
            ui::tree(ui, &groups[key], 0, "groups", selected, now, actions);
        } else {
            let response = ui
                .horizontal(|ui| {
                    ui::badge(ui, "DCR", ui::ACCENT);
                    let response =
                        ui.selectable_label(selected.as_deref() == Some(DCR_ID), "DCR 调用");
                    if response.clicked() {
                        *selected = Some(DCR_ID.into());
                    }
                    ui.label(
                        egui::RichText::new(dcr.state_label())
                            .color(color(dcr))
                            .size(11.),
                    );
                    ui.label(
                        egui::RichText::new(ui::elapsed_text(dcr.total_elapsed(now)))
                            .color(ui::MUTED)
                            .size(11.),
                    );
                    response
                })
                .inner;
            response.context_menu(|ui| menu(ui, dcr, actions));
        }
    }
}

/// 主卡片使用既有边框和状态布局，点击详情查看同一份实时日志。
pub fn execution(
    ui: &mut egui::Ui,
    session: &DcrSession,
    now: i64,
    selected: &mut Option<String>,
    actions: &mut Vec<ui::Action>,
) {
    egui::Frame::new()
        .fill(ui::SURFACE)
        .stroke(egui::Stroke::new(1., ui::BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(7))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("DCR 调用").size(15.).strong().color(ui::TEXT));
                ui::badge(ui, session.state_label(), color(session));
                if ui.button("详情").clicked() {
                    *selected = Some(DCR_ID.into());
                }
                ui.menu_button("更多", |ui| menu(ui, session, actions));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(ui::activity_age(session.last_activity, now)).size(12.).color(ui::MUTED));
                });
            });
            ui.label(egui::RichText::new(timing(session, now)).size(12.).color(ui::MUTED))
                .on_hover_text("统计窗口跟随顶部归档时长。累计耗时仅算窗口内忙碌区间；累计调用按返回时间计数（含失败返回）。均耗时按这些返回样本计算，同名并发 FIFO 配对时标记 ≈。");
            ui.add(egui::Label::new(egui::RichText::new(tools(session, now)).size(12.)
                .color(if session.busy() { ui::TextKind::Tool.color() } else { ui::MUTED })).truncate());
            ui.separator();
            logs(ui, session, "dcr_card_logs", false);
        });
}

/// 悬浮卡片沿用普通卡片的四行尺寸与外观，共用展示数量上限。
pub fn floating(ui: &mut egui::Ui, session: &DcrSession, now: i64) {
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
            ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
            ui.spacing_mut().item_spacing.y = 2.;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("DCR 调用")
                        .size(12.)
                        .strong()
                        .color(ui::TEXT),
                );
                ui::badge(ui, session.state_label(), color(session));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(ui::activity_age(session.last_activity, now))
                            .size(11.)
                            .color(ui::MUTED),
                    );
                });
            });
            ui.label(
                egui::RichText::new(timing(session, now))
                    .size(10.)
                    .color(ui::MUTED),
            );
            ui.add(
                egui::Label::new(egui::RichText::new(tools(session, now)).size(10.).color(
                    if session.busy() {
                        ui::TextKind::Tool.color()
                    } else {
                        ui::MUTED
                    },
                ))
                .truncate(),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(summary(session))
                        .size(12.)
                        .color(ui::TEXT),
                )
                .truncate(),
            );
        });
}

/// 日志只读展示；读取不会修改活动时间或启动任何任务。
pub fn logs(ui: &mut egui::Ui, session: &DcrSession, id: &str, full: bool) {
    let width = ui.available_width().max(1.);
    let height = ui.available_height().max(1.);
    egui::ScrollArea::vertical()
        .id_salt(id)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .max_width(width)
        .max_height(height)
        .show(ui, |ui| {
            ui.set_max_width(width);
            if session.logs.is_empty() {
                ui.label(
                    egui::RichText::new("暂无调用日志")
                        .size(12.)
                        .color(ui::MUTED),
                );
            }
            let lines: Vec<_> = session
                .logs
                .iter()
                .filter(|line| full || !line.text.starts_with("[历史]"))
                .collect();
            let start = if full {
                0
            } else {
                lines.len().saturating_sub(20)
            };
            for line in &lines[start..] {
                let text = if full {
                    line.text.clone()
                } else {
                    display_line(&line.text)
                };
                let mut job = egui::text::LayoutJob::default();
                job.append(
                    &format!("{}  ", ui::local_time_label(line.at_ms)),
                    0.,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(12.),
                        color: ui::MUTED,
                        ..Default::default()
                    },
                );
                job.append(
                    &text,
                    0.,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(12.),
                        color: if text.contains("失败") || text.contains("中断") {
                            ui::TextKind::Error.color()
                        } else if text.starts_with("结果") {
                            ui::TextKind::Result.color()
                        } else {
                            ui::TEXT
                        },
                        ..Default::default()
                    },
                );
                ui.add(egui::Label::new(job).wrap().selectable(true));
            }
        });
}

/// 主卡片使用简短中文连接文案，完整原文仍在详情中保留。
fn display_line(text: &str) -> String {
    if text.contains("Device ready:") {
        "远程连接已就绪".into()
    } else if text.contains("Presence tracked") || text.contains("Device marked as online") {
        "远程连接已建立".into()
    } else if text.contains("Channel subscribed") {
        "远程通道已连接".into()
    } else if text.contains("Starting MCP Device") {
        "正在启动 DCR".into()
    } else if text.contains("Connecting to Remote MCP") {
        "正在连接远程服务".into()
    } else if text.contains("Connected to Remote MCP") {
        "已连接远程服务".into()
    } else if text.contains("Connecting to Local Desktop Commander") {
        "正在连接本机工具服务".into()
    } else if text.contains("Connected to Desktop Commander MCP") {
        "本机工具服务已连接".into()
    } else if text.contains("Session restored") {
        "已恢复授权会话".into()
    } else {
        text.to_owned()
    }
}

/// 悬浮窗优先显示最近调用摘要，避免将原始设备诊断行当作主要内容。
fn summary(session: &DcrSession) -> String {
    session
        .logs
        .iter()
        .rev()
        .find(|line| {
            ["结果 ", "完成调用 ", "调用失败 ", "调用中断 "]
                .iter()
                .any(|prefix| line.text.starts_with(prefix))
        })
        .map(|line| line.text.clone())
        .unwrap_or_else(|| match session.connection.as_str() {
            "online" => "远程连接正常".into(),
            "connecting" => "正在连接远程服务…".into(),
            "failed" => "连接失败，请打开详情查看日志".into(),
            _ => "远程连接已停止".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 长路径与多行输出必须裁剪在卡片内部，不得覆盖任务树或相邻卡片。
    #[test]
    fn long_logs_stay_inside_card_clip() {
        let ctx = egui::Context::default();
        let card = egui::Rect::from_min_size(egui::pos2(120., 70.), egui::vec2(420., 210.));
        let mut session = DcrSession::default();
        session.logs.push_back(crate::dcr_session::SessionLine {
            at_ms: 1000,
            text: format!("LONG_LOG_{}\n第二行", "path_without_spaces/".repeat(90)),
        });
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800., 500.),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
                        execution(ui, &session, 2000, &mut None, &mut Vec::new());
                    });
                });
            },
        );
        let logs: Vec<_> = output
            .shapes
            .iter()
            .filter(|shape| {
                matches!(&shape.shape,
            egui::epaint::Shape::Text(text) if text.galley.job.text.contains("LONG_LOG_"))
            })
            .collect();
        assert!(!logs.is_empty());
        for shape in logs {
            assert!(shape.clip_rect.min.x >= card.min.x);
            assert!(shape.clip_rect.max.x <= card.max.x);
            assert!(shape.clip_rect.min.y >= card.min.y);
            assert!(shape.clip_rect.max.y <= card.max.y);
        }
    }

    /// 主卡片与悬浮窗复用同一组指标，无样本显示破折号，短调用保留毫秒。
    #[test]
    fn timing_displays_mean_and_millisecond_precision() {
        let mut session = DcrSession::default();
        assert!(timing(&session, 0).contains("均耗时 —"));
        session.returns.push_back(crate::dcr_session::ReturnSample {
            at_ms: 0,
            elapsed_ms: 450,
            estimated: false,
        });
        assert!(timing(&session, 0).contains("均耗时 450ms"));
        assert!(timing(&session, 0).contains("累计调用 1"));
        session.returns.back_mut().unwrap().estimated = true;
        assert!(timing(&session, 0).contains("均耗时 ≈450ms"));
        assert_eq!(display_line("✅ Device ready:"), "远程连接已就绪");
    }

    /// DCR 与 Router 共用排序和上限，不保留额外固定位置。
    #[test]
    fn mixed_recent_respects_limit_and_activity() {
        let mut first = crate::store::tests::sample("first");
        first.last_activity = 100;
        let mut second = crate::store::tests::sample("second");
        second.last_activity = 300;
        let records = vec![first, second];
        let mut dcr = DcrSession {
            visible: true,
            last_activity: 200,
            ..Default::default()
        };
        let selected = recent(&records, &dcr, 2, 400, None);
        assert_eq!(
            selected.iter().map(Card::key).collect::<Vec<_>>(),
            ["second", DCR_ID]
        );
        dcr.last_activity = 50;
        assert!(
            recent(&records, &dcr, 2, 400, None)
                .iter()
                .all(|c| matches!(c, Card::Router(_)))
        );
    }

    /// 在线空闲会按时隐藏，真实调用中保持展示；清除与归档仍排除卡片。
    #[test]
    fn idle_visibility_distinguishes_online_from_busy() {
        let records = vec![];
        let mut dcr = DcrSession {
            visible: true,
            connection: "online".into(),
            last_activity: 10,
            ..Default::default()
        };
        assert!(recent(&records, &dcr, 4, 500, Some(100)).is_empty());
        dcr.active_tools.push("read_file".into());
        assert_eq!(recent(&records, &dcr, 4, 500, Some(100)).len(), 1);
        dcr.set_archived(true);
        assert!(recent(&records, &dcr, 4, 500, Some(100)).is_empty());
        dcr.set_archived(false);
        dcr.hide();
        assert!(recent(&records, &dcr, 4, 500, Some(100)).is_empty());
    }
}
