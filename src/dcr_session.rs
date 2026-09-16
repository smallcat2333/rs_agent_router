//! DCR 单会话状态：把同一 DCR 远端进程的所有网页调用归并为一条会话。
//!
//! 本模块只保存会话展示所需的派生态（连接、活动调用、耗时、日志），不接触
//! Manager、Token 或 CLI 调度；持久化快照也是独立 JSON，不进入任务索引。
//! 原始日志行的识别建立在官方 remote-device 的实际输出格式上，核对自：
//! `@wonderwhy-er/desktop-commander/dist/remote-device/{device,remote-channel}.js`。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::dcr::{DcrCall, LOG_CAP, LOG_LINE_MAX, redact_secrets, render_output, truncate_chars};

/// 记忆最近处理过的调用 ID，用于按 call_id 去重（与官方 SEEN_CALL_IDS_MAX 一致）。
const SEEN_CALL_IDS_MAX: usize = 100;
/// 单个完成结果最多消费的行数，避免畸形输出无限拖住会话。
const SWALLOW_MAX_LINES: usize = 200;
/// 单个完成结果保留的字符数上限；超出则截断但仍计数到 JSON 结束。
const RESULT_TEXT_MAX: usize = 4_000;

/// 连接状态的稳定取值；界面按此着色与显示。
pub const LINK_CONNECTING: &str = "connecting";
pub const LINK_ONLINE: &str = "online";
pub const LINK_OFFLINE: &str = "offline";
pub const LINK_STOPPED: &str = "stopped";
pub const LINK_FAILED: &str = "failed";

/// 单条会话日志；`at_ms` 为事件发生时间，`text` 已脱敏截断。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionLine {
    pub at_ms: i64,
    pub text: String,
}

/// 已结束的忙碌区间，用于裁剪统计窗口边界，并发只记录一次。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BusyPeriod {
    pub start_ms: i64,
    pub end_ms: i64,
}

/// 按返回时刻归属窗口的单次调用样本。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReturnSample {
    pub at_ms: i64,
    pub elapsed_ms: i64,
    pub estimated: bool,
}

/// 当前滚动窗口的指标；累计耗时是忙碌区间，均耗时是单次返回时长。
pub struct WindowStats {
    pub elapsed_ms: i64,
    pub calls: u64,
    pub mean_ms: Option<f64>,
    pub estimated: bool,
}

/// 与顶部默认归档时间一致，初始统计窗口为四小时。
fn default_window_ms() -> i64 { 4 * 3_600_000 }

/// 一条独立 DCR 会话的全部展示状态。
///
/// `active_tools` 中每一项对应一个正在执行的调用，同名并发会重复出现；
/// `elapsed_ms` 只累计实时忙碌区间，导入的历史耗时不计入其中。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DcrSession {
    pub visible: bool,
    pub archived: bool,
    pub last_activity: i64,
    pub connection: String,
    pub active_tools: Vec<String>,
    pub busy_since: Option<i64>,
    pub elapsed_ms: i64,
    /// 已实际返回的调用统计；独立于并发去重后的忙碌耗时。
    #[serde(default)]
    pub returned_count: u64,
    #[serde(default)]
    pub returned_elapsed_ms: i64,
    #[serde(default)]
    pub mean_estimated: bool,
    #[serde(default)]
    pub busy_periods: VecDeque<BusyPeriod>,
    #[serde(default)]
    pub returns: VecDeque<ReturnSample>,
    #[serde(default = "default_window_ms")]
    pub window_ms: i64,
    pub logs: VecDeque<SessionLine>,
}

impl Default for DcrSession {
    /// 未启动的空会话：停止态、无活动调用、无日志。
    fn default() -> Self {
        Self {
            visible: true,
            archived: false,
            last_activity: 0,
            connection: LINK_STOPPED.into(),
            active_tools: Vec::new(),
            busy_since: None,
            elapsed_ms: 0,
            returned_count: 0,
            returned_elapsed_ms: 0,
            mean_estimated: false,
            busy_periods: VecDeque::new(),
            returns: VecDeque::new(),
            window_ms: default_window_ms(),
            logs: VecDeque::new(),
        }
    }
}

impl DcrSession {
    /// 跟随归档设置更新窗口并剔除过期样本；不刷新会话活动时间。
    pub fn update_window(&mut self, now: i64, hours: u64) {
        self.window_ms = hours as i64 * 3_600_000;
        self.prune_samples(now);
    }

    /// 只保留窗口内返回及与窗口相交的忙碌区间，不保存全历史指标。
    fn prune_samples(&mut self, now: i64) {
        let cutoff = now.saturating_sub(self.window_ms);
        self.busy_periods.retain(|period| period.end_ms > cutoff);
        self.returns.retain(|sample| sample.at_ms >= cutoff);
    }

    /// 裁剪跨窗口的区间；进行中的耗时实时增加，返回数量按返回时间计入。
    pub fn window_stats(&self, now: i64) -> WindowStats {
        let cutoff = now.saturating_sub(self.window_ms);
        let mut elapsed_ms = self.busy_periods.iter().map(|period|
            period.end_ms.min(now).saturating_sub(period.start_ms.max(cutoff)).max(0)).sum::<i64>();
        if self.busy() && let Some(start) = self.busy_since {
            elapsed_ms += now.saturating_sub(start.max(cutoff)).max(0);
        }
        let mut calls = 0;
        let mut total = 0i64;
        let mut estimated = false;
        for sample in self.returns.iter().filter(|sample| sample.at_ms >= cutoff && sample.at_ms <= now) {
            calls += 1;
            total = total.saturating_add(sample.elapsed_ms);
            estimated |= sample.estimated;
        }
        WindowStats { elapsed_ms, calls, mean_ms: (calls > 0).then(|| total as f64 / calls as f64), estimated }
    }

    /// 结束一个非重叠忙碌区间，同时记录可按窗口重新裁剪的时间边界。
    fn finish_busy_period(&mut self, start: i64, now: i64) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(now.saturating_sub(start).max(0));
        self.busy_periods.push_back(BusyPeriod { start_ms: start, end_ms: now.max(start) });
        self.prune_samples(now);
    }

    /// 返回每次请求到返回的平均耗时；没有有效样本时不显示虚构的零值。
    #[cfg(test)]
    pub fn mean_elapsed(&self) -> Option<f64> {
        (self.returned_count > 0).then(|| self.returned_elapsed_ms as f64 / self.returned_count as f64)
    }
    /// 是否存在活动调用；进程存活不代表调用中。
    pub fn busy(&self) -> bool {
        !self.active_tools.is_empty()
    }

    /// 当前忙碌区间已持续的时间；空闲时为 0。
    pub fn current_elapsed(&self, now: i64) -> i64 {
        match self.busy_since {
            Some(start) if self.busy() => now.saturating_sub(start).max(0),
            _ => 0,
        }
    }

    /// 已完成忙碌区间总和加上当前区间；只按真实区间累计，不含空闲。
    pub fn total_elapsed(&self, now: i64) -> i64 {
        self.window_stats(now).elapsed_ms
    }

    /// 面向界面的中文状态；调用中优先于在线空闲。
    pub fn state_label(&self) -> &str {
        match self.connection.as_str() {
            LINK_FAILED => "启动失败",
            LINK_STOPPED => "已停止",
            LINK_OFFLINE => "离线",
            LINK_CONNECTING => "连接中",
            LINK_ONLINE => {
                if self.busy() {
                    "调用中"
                } else {
                    "在线空闲"
                }
            }
            _ => {
                if self.busy() {
                    "调用中"
                } else {
                    "已停止"
                }
            }
        }
    }

    /// 只隐藏展示，不停止进程也不改变会话状态。
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// 设置归档标记；归档不停止任何调用。
    pub fn set_archived(&mut self, archived: bool) {
        self.archived = archived;
    }

    /// 从磁盘快照恢复后调用：绝不恢复运行中状态，并强制不变量。
    pub(crate) fn sanitize_after_load(&mut self) {
        self.active_tools.clear();
        self.busy_since = None;
        self.connection = LINK_STOPPED.into();
        if self.elapsed_ms < 0 {
            self.elapsed_ms = 0;
        }
        while self.logs.len() > LOG_CAP {
            self.logs.pop_front();
        }
    }
}

/// 独立快照文件内容；`history_imported` 保证旧本机历史只导入一次。
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SessionSnapshot {
    #[serde(default)]
    pub session: DcrSession,
    #[serde(default)]
    pub history_imported: bool,
}

/// 追加一行日志：压平换行、脱敏、按字符截断，并按上限淘汰最旧行。
pub(crate) fn push_line(session: &mut DcrSession, at_ms: i64, text: &str) {
    let flat: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect();
    let safe = truncate_chars(&redact_secrets(&flat), LOG_LINE_MAX);
    session.logs.push_back(SessionLine { at_ms, text: safe });
    while session.logs.len() > LOG_CAP {
        session.logs.pop_front();
    }
}

/// 新会话开始：清空活动调用并进入连接中。耗时为累计值，跨重启保留。
pub(crate) fn begin_session(session: &mut DcrSession, now: i64) {
    session.active_tools.clear();
    session.busy_since = None;
    session.connection = LINK_CONNECTING.into();
    session.last_activity = now;
    push_line(session, now, "会话开始");
}

/// 会话停止/进程退出：未完成调用标记中断并清空计时状态。
///
/// 已发生的忙碌时间计入 `elapsed_ms`，随后清空 `busy_since`；重复调用无副作用。
pub(crate) fn mark_stopped(session: &mut DcrSession, now: i64) {
    let had_active = !session.active_tools.is_empty();
    if had_active {
        for tool in session.active_tools.clone() {
            push_line(session, now, &format!("调用中断 {tool}"));
        }
    }
    session.active_tools.clear();
    let was_busy = session.busy_since.is_some();
    if let Some(start) = session.busy_since.take() {
        session.finish_busy_period(start, now);
    }
    let changed = session.connection != LINK_STOPPED && session.connection != LINK_FAILED;
    if changed {
        session.connection = LINK_STOPPED.into();
    }
    if had_active || was_busy || changed {
        session.last_activity = now;
    }
    if had_active {
        push_line(session, now, "未完成调用已标记中断，计时状态已清除");
    }
}

/// 会话失败：中断未完成调用并明确进入失败态；重复失败不重复刷日志。
pub(crate) fn mark_failed(session: &mut DcrSession, now: i64, message: &str) {
    let had_active = !session.active_tools.is_empty();
    for tool in session.active_tools.clone() {
        push_line(session, now, &format!("调用中断 {tool}"));
    }
    session.active_tools.clear();
    if let Some(start) = session.busy_since.take() {
        session.finish_busy_period(start, now);
    }
    let changed = session.connection != LINK_FAILED;
    session.connection = LINK_FAILED.into();
    if changed || had_active {
        session.last_activity = now;
    }
    if changed {
        push_line(session, now, &format!("会话失败：{message}"));
    }
}

/// 把只读本机历史转成带标记的会话行：保留已有耗时与输出摘要，但不计入实时忙碌。
pub(crate) fn history_lines(calls: &[DcrCall]) -> Vec<SessionLine> {
    calls
        .iter()
        .map(|call| {
            let at_ms = parse_rfc3339_ms(&call.timestamp).unwrap_or(0);
            let mut text = format!("[历史] {}", call.tool_name);
            if call.is_error {
                text.push_str("（失败）");
            }
            if let Some(duration) = call.duration_ms {
                text.push_str(&format!(" · 耗时 {duration}ms"));
            }
            let summary = call.output.trim();
            if !summary.is_empty() {
                text.push_str(&format!("：{summary}"));
            }
            if !call.timestamp.is_empty() {
                text.push_str(&format!(" @ {}", call.timestamp));
            }
            SessionLine {
                at_ms,
                text: truncate_chars(&redact_secrets(&text), LOG_LINE_MAX),
            }
        })
        .collect()
}

/// 原子写入：同目录临时文件后重命名覆盖，避免半截快照。
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建 DCR 快照目录失败：{}", parent.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("dcr-session.json");
    let tmp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("写入 DCR 快照临时文件失败：{}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("替换 DCR 快照失败：{}", path.display()))?;
    Ok(())
}

/// 解析 `YYYY-MM-DDTHH:MM:SS[.fff][Z]` 为 Unix 毫秒；失败返回 None。
///
/// 只做无依赖的 UTC 计算，不猜测时区偏移；DCR 记录为 UTC（Z）。
fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let digit = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        if slice.is_empty() || !slice.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        slice.parse::<i64>().ok()
    };
    let year = digit(0..4)?;
    let month = digit(5..7)?;
    let day = digit(8..10)?;
    let hour = digit(11..13)?;
    let minute = digit(14..16)?;
    let second = digit(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let millis = if bytes.get(19) == Some(&b'.') {
        digit(20..23).unwrap_or(0)
    } else {
        0
    };
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000) + millis)
}

/// Howard Hinnant 的 civil→days 算法；返回 1970-01-01 起的天数。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shift = if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * (month + month_shift) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 一条活动调用；保存 ID 以便按 call_id 去重与按工具 FIFO 配对。
struct ActiveCall {
    id: String,
    tool: String,
    started_ms: i64,
    estimated: bool,
}

/// 有界结果累积：跨行拼出 JSON，提取摘要后结束；超限只截断不改计时。
struct ResultBuffer {
    tool: String,
    id: String,
    buffer: String,
    depth: i32,
    in_string: bool,
    escape: bool,
    started: bool,
    lines: usize,
    truncated: bool,
}

impl ResultBuffer {
    fn new(tool: String, id: String) -> Self {
        Self {
            tool,
            id,
            buffer: String::new(),
            depth: 0,
            in_string: false,
            escape: false,
            started: false,
            lines: 0,
            truncated: false,
        }
    }

    /// 喂入一行，返回 JSON 是否结束；结束时括号已配平或已达上限。
    fn feed(&mut self, line: &str) -> bool {
        self.lines += 1;
        if self.lines > SWALLOW_MAX_LINES {
            self.truncated = true;
            return true;
        }
        for ch in line.chars() {
            if self.escape {
                self.append(ch);
                self.escape = false;
                continue;
            }
            if self.in_string {
                self.append(ch);
                if ch == '\\' {
                    self.escape = true;
                } else if ch == '"' {
                    self.in_string = false;
                }
                continue;
            }
            match ch {
                '"' => {
                    self.in_string = true;
                    self.append(ch);
                }
                '{' | '[' => {
                    self.depth += 1;
                    self.started = true;
                    self.append(ch);
                }
                '}' | ']' => {
                    self.append(ch);
                    if self.started {
                        self.depth -= 1;
                        if self.depth <= 0 {
                            self.depth = 0;
                            return true;
                        }
                    }
                }
                _ => self.append(ch),
            }
        }
        if !self.started {
            self.truncated = true;
            return true;
        }
        false
    }

    /// 追加字符；超过上限后只标记截断，仍继续配平以找到结束。
    fn append(&mut self, ch: char) {
        if self.truncated {
            return;
        }
        if self.buffer.len() + ch.len_utf8() > RESULT_TEXT_MAX {
            self.truncated = true;
            return;
        }
        self.buffer.push(ch);
    }
}

/// 逐行识别 DCR 事件并更新会话；状态只属于当前 drain 线程。
pub(crate) struct EventTracker {
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    active: Vec<ActiveCall>,
    swallow: Option<ResultBuffer>,
    /// 完成行之后等待结果 JSON；只对紧邻的、确实以 JSON 起始的行生效。
    awaiting: Option<(String, String)>,
}

impl EventTracker {
    pub(crate) fn new() -> Self {
        Self {
            seen: VecDeque::new(),
            seen_set: HashSet::new(),
            active: Vec::new(),
            swallow: None,
            awaiting: None,
        }
    }

    /// 识别一行原始日志；返回 true 表示该行已被工具事件/结果消费。
    ///
    /// 被消费的行不得再参与连接判定，避免结果体里的文本伪造状态。
    pub(crate) fn observe(&mut self, session: &mut DcrSession, line: &str, now: i64) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        // 结果 JSON 跨行时优先消费，内部文本不触发任何事件。
        if self.swallow.is_some() {
            let done = self
                .swallow
                .as_mut()
                .map(|buffer| buffer.feed(trimmed))
                .unwrap();
            if done {
                let buffer = self.swallow.take().unwrap();
                self.finish_result(session, buffer, now);
            }
            return true;
        }
        if let Some((tool, id)) = self.awaiting.take() {
            if is_json_start(trimmed) {
                let mut buffer = ResultBuffer::new(tool, id);
                if buffer.feed(trimmed) {
                    self.finish_result(session, buffer, now);
                } else {
                    self.swallow = Some(buffer);
                }
                return true;
            }
            // 非 JSON：交回普通日志处理（连接/授权/错误仍可识别）。
        }
        if let Some(skip) = parse_skip_notice(trimmed) {
            match skip {
                // 本地已处理过的重复投递：不得结束原调用。
                SkipKind::Local(id) => {
                    if self.is_seen(&id) {
                        push_line(session, now, &format!("[重复] 已忽略本地重复投递 {id}"));
                    }
                }
                // remote-channel 的 DB claim 被拒：该调用不会执行，按 id 移除并结束忙碌。
                SkipKind::Claim(id) => self.reject_claim(session, &id, now),
            }
            return true;
        }
        if let Some((id, tool)) = parse_received(trimmed) {
            self.begin_call(session, id, tool, now);
            return true;
        }
        if let Some((tool, rest)) = parse_completed(trimmed) {
            let ended = self.end_call(session, &tool, now, false, "");
            let (tool, id) = ended.unwrap_or((tool, String::new()));
            self.start_result(session, tool, id, rest.trim(), now);
            return true;
        }
        if let Some((tool, detail)) = parse_failed(trimmed) {
            self.end_call(session, &tool, now, true, &detail);
            self.swallow = None;
            self.awaiting = None;
            return true;
        }
        false
    }

    /// 开始一个调用；同 ID 重复投递直接忽略。
    fn begin_call(&mut self, session: &mut DcrSession, id: String, tool: String, now: i64) {
        if self.is_seen(&id) {
            push_line(session, now, &format!("[重复] 已忽略重复调用 {id}（{tool}）"));
            return;
        }
        self.remember(&id);
        let estimated = self.active.iter().any(|call| call.tool == tool);
        if estimated {
            // 原生完成日志没有 ID，同名并发只能 FIFO 配对；明确标注均值为估算。
            session.mean_estimated = true;
            for call in self.active.iter_mut().filter(|call| call.tool == tool) { call.estimated = true; }
        }
        self.active.push(ActiveCall {
            id: id.clone(),
            tool: tool.clone(),
            started_ms: now,
            estimated,
        });
        sync_active(session, &self.active);
        if session.busy_since.is_none() {
            session.busy_since = Some(now);
        }
        session.visible = true;
        session.archived = false;
        session.last_activity = now;
        push_line(session, now, &format!("开始调用 {tool}（{id}）"));
    }

    /// 结束一个调用：同工具按 FIFO 配对，避免并发同名虚构精确单条耗时。
    ///
    /// 返回被结束调用的 (工具名, ID)，供结果摘要继续使用。
    fn end_call(
        &mut self,
        session: &mut DcrSession,
        tool: &str,
        now: i64,
        failed: bool,
        detail: &str,
    ) -> Option<(String, String)> {
        let Some(index) = self.active.iter().position(|call| call.tool == tool) else {
            push_line(session, now, &format!("完成调用 {tool}（无匹配活动调用）"));
            return None;
        };
        let call = self.active.remove(index);
        sync_active(session, &self.active);
        session.last_activity = now;
        session.returned_count += 1;
        session.returned_elapsed_ms = session.returned_elapsed_ms.saturating_add(now.saturating_sub(call.started_ms).max(0));
        session.returns.push_back(ReturnSample { at_ms: now, elapsed_ms: now.saturating_sub(call.started_ms).max(0), estimated: call.estimated });
        session.prune_samples(now);
        if failed {
            if detail.is_empty() {
                push_line(session, now, &format!("调用失败 {}（{}）", call.tool, call.id));
            } else {
                push_line(
                    session,
                    now,
                    &format!("调用失败 {}（{}）：{detail}", call.tool, call.id),
                );
            }
        } else {
            push_line(session, now, &format!("完成调用 {}（{}）", call.tool, call.id));
        }
        if self.active.is_empty()
            && let Some(start) = session.busy_since.take()
        {
            session.finish_busy_period(start, now);
        }
        Some((call.tool, call.id))
    }

    /// remote-channel 明确拒绝 DB claim：对应调用不会执行，按 id 移除。
    fn reject_claim(&mut self, session: &mut DcrSession, id: &str, now: i64) {
        let Some(index) = self.active.iter().position(|call| call.id == id) else {
            // 未在本地活动（例如重复 doorbell），忽略，不改动任何计时。
            return;
        };
        let call = self.active.remove(index);
        sync_active(session, &self.active);
        session.last_activity = now;
        push_line(
            session,
            now,
            &format!("调用被远端认领，已跳过 {}（{}）", call.tool, call.id),
        );
        if self.active.is_empty()
            && let Some(start) = session.busy_since.take()
        {
            session.finish_busy_period(start, now);
        }
    }

    /// 完成行的结果处理：行内已有 JSON 立即解析，否则等紧邻下一行。
    fn start_result(
        &mut self,
        session: &mut DcrSession,
        tool: String,
        id: String,
        rest: &str,
        now: i64,
    ) {
        self.swallow = None;
        if rest.is_empty() {
            self.awaiting = Some((tool, id));
            return;
        }
        if is_json_start(rest) {
            let mut buffer = ResultBuffer::new(tool, id);
            if buffer.feed(rest) {
                self.finish_result(session, buffer, now);
            } else {
                self.swallow = Some(buffer);
            }
        }
    }

    /// 结果结束时提取 content.text 摘要与 isError，写入有界脱敏日志。
    fn finish_result(&mut self, session: &mut DcrSession, buffer: ResultBuffer, now: i64) {
        let label = label(&buffer.tool, &buffer.id);
        if buffer.truncated {
            push_line(session, now, &format!("结果 {label}：结果过大已截断"));
        } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&buffer.buffer) {
            let (is_error, summary) = summarize(&value);
            let summary = summary.trim();
            let head = if is_error {
                format!("结果 {label}：失败")
            } else {
                format!("结果 {label}")
            };
            if summary.is_empty() {
                push_line(session, now, &head);
            } else {
                push_line(session, now, &format!("{head}：{summary}"));
            }
        } else {
            push_line(session, now, &format!("结果 {label}：（结果无法解析）"));
        }
    }

    fn is_seen(&self, id: &str) -> bool {
        self.seen_set.contains(id)
    }

    fn remember(&mut self, id: &str) {
        if !self.seen_set.insert(id.to_string()) {
            return;
        }
        self.seen.push_back(id.to_string());
        while self.seen.len() > SEEN_CALL_IDS_MAX {
            if let Some(oldest) = self.seen.pop_front() {
                self.seen_set.remove(&oldest);
            }
        }
    }
}

/// 保持公开字段与内部活动列表一致；同名并发保留重复项。
fn sync_active(session: &mut DcrSession, active: &[ActiveCall]) {
    session.active_tools = active.iter().map(|call| call.tool.clone()).collect();
}

/// 工具与 ID 的展示标签；缺少 ID 时只显示工具名。
fn label(tool: &str, id: &str) -> String {
    if id.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}（{id}）")
    }
}

/// 从结果对象提取 isError 与 content.text/structuredContent 摘要。
fn summarize(value: &serde_json::Value) -> (bool, String) {
    let is_error = value
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || value
            .get("output")
            .and_then(|output| output.get("isError"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    (is_error, render_output(Some(value)))
}

/// 是否以 JSON 值起始；用于判断完成行的下一行是否为真实结果。
fn is_json_start(line: &str) -> bool {
    matches!(line.trim_start().chars().next(), Some('{') | Some('['))
}

/// 官方去重提示的分类：本地重复投递 vs 远端 DB claim 被拒。
enum SkipKind {
    Local(String),
    Claim(String),
}

/// 识别官方去重/认领提示并取出 call_id。
fn parse_skip_notice(line: &str) -> Option<SkipKind> {
    const LOCAL: &str = "Duplicate delivery for call already handled here, skipping:";
    const CLAIM: &str = "Call already claimed (duplicate delivery), skipping:";
    const DOORBELL: &str = "Doorbell call already claimed:";
    if let Some(start) = line.find(LOCAL)
        && let Some(id) = take_id(&line[start + LOCAL.len()..])
    {
        return Some(SkipKind::Local(id));
    }
    if let Some(start) = line.find(CLAIM)
        && let Some(id) = take_id(&line[start + CLAIM.len()..])
    {
        return Some(SkipKind::Claim(id));
    }
    if let Some(start) = line.find(DOORBELL)
        && let Some(id) = take_id(&line[start + DOORBELL.len()..])
    {
        return Some(SkipKind::Claim(id));
    }
    None
}

/// 从提示尾部取一个不含空白的 call_id。
fn take_id(rest: &str) -> Option<String> {
    let id = rest.trim().trim_end_matches('.').trim();
    if id.is_empty() || id.contains(' ') {
        None
    } else {
        Some(id.to_string())
    }
}

/// 识别 `Received tool call <id>: <tool> <args> metadata:...`（device.js）。
fn parse_received(line: &str) -> Option<(String, String)> {
    const MARKER: &str = "Received tool call ";
    let start = line.find(MARKER)?;
    let rest = &line[start + MARKER.len()..];
    if !rest.contains(" metadata:") {
        return None;
    }
    let colon = rest.find(':')?;
    let id = rest[..colon].trim();
    if id.is_empty() {
        return None;
    }
    let after = rest[colon + 1..].trim_start();
    let end = after
        .find(|ch: char| ch.is_whitespace())
        .unwrap_or(after.len());
    let tool = after[..end].trim();
    if tool.is_empty() || tool.contains('{') {
        return None;
    }
    Some((id.to_string(), tool.to_string()))
}

/// 识别 `Tool call <tool> completed:`（device.js），返回工具名与行内剩余内容。
fn parse_completed(line: &str) -> Option<(String, String)> {
    const START: &str = "Tool call ";
    const MARK: &str = " completed:";
    let start = line.find(START)?;
    let rest = &line[start + START.len()..];
    let end = rest.find(MARK)?;
    let tool = rest[..end].trim();
    if tool.is_empty() || tool.contains("  ") {
        return None;
    }
    Some((tool.to_string(), rest[end + MARK.len()..].to_string()))
}

/// 识别 `Tool call <tool> failed: <message>`（device.js）。
fn parse_failed(line: &str) -> Option<(String, String)> {
    const START: &str = "Tool call ";
    const MARK: &str = " failed:";
    let start = line.find(START)?;
    let rest = &line[start + START.len()..];
    let end = rest.find(MARK)?;
    let tool = rest[..end].trim();
    if tool.is_empty() || tool.contains("  ") {
        return None;
    }
    Some((
        tool.to_string(),
        rest[end + MARK.len()..].trim().to_string(),
    ))
}

/// 持久化后台需要一个可共享、可停止、带最后保存错误的状态。
pub(crate) struct PersistShared {
    pub(crate) stop: AtomicBool,
    pub(crate) error: Mutex<Option<String>>,
    pub(crate) history_imported: AtomicBool,
}

impl PersistShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
            history_imported: AtomicBool::new(false),
        })
    }
}

/// 后台持久化循环：定时序列化，只有内容变化才原子落盘，停止后退出。
pub(crate) fn persist_loop(
    shared: Arc<PersistShared>,
    monitor: Arc<Mutex<DcrSession>>,
    paths: Arc<Mutex<Option<(std::path::PathBuf, std::path::PathBuf)>>>,
) {
    let mut last: Option<Vec<u8>> = None;
    while !shared.stop.load(Ordering::SeqCst) {
        for _ in 0..10 {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let Some(snapshot_path) = paths
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .map(|pair| pair.0)
        else {
            continue;
        };
        let session = match monitor.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => continue,
        };
        let snapshot = SessionSnapshot {
            session,
            history_imported: shared.history_imported.load(Ordering::SeqCst),
        };
        let bytes = match serde_json::to_vec_pretty(&snapshot) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if last.as_deref() == Some(bytes.as_slice()) {
            continue;
        }
        match atomic_write(&snapshot_path, &bytes) {
            Ok(()) => {
                last = Some(bytes);
                let had_error = shared
                    .error
                    .lock()
                    .map(|mut guard| guard.take().is_some())
                    .unwrap_or(false);
                if had_error {
                    push_persist_note(&monitor, "[持久化] 快照已恢复保存");
                }
            }
            Err(error) => {
                let text = format!("快照保存失败：{error:#}");
                if let Ok(mut guard) = shared.error.lock() {
                    *guard = Some(text.clone());
                }
                push_persist_note(&monitor, &format!("[持久化] {text}"));
            }
        }
    }
}

/// 仅在最近日志没有同类提示时追加，避免保存失败时刷屏。
fn push_persist_note(monitor: &Arc<Mutex<DcrSession>>, text: &str) {
    if let Ok(mut session) = monitor.lock() {
        let recent = session
            .logs
            .iter()
            .rev()
            .take(3)
            .any(|line| line.text.contains("[持久化]"));
        if !recent {
            let now = crate::dcr::now_ms();
            push_line(&mut session, now, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四小时窗口裁剪跨界忙碌区间，并按返回时刻计算次数和均值。
    #[test]
    fn rolling_window_clips_time_and_expires_returns() {
        const H: i64 = 3_600_000;
        let now = 10 * H;
        let mut session = DcrSession::default();
        session.elapsed_ms = 999 * H; // 旧全量聚合不能影响窗口统计。
        session.busy_periods.push_back(BusyPeriod { start_ms: now - 5 * H, end_ms: now - 3 * H });
        session.returns.extend([
            ReturnSample { at_ms: now - 5 * H, elapsed_ms: 9000, estimated: true },
            ReturnSample { at_ms: now - 4 * H, elapsed_ms: 300, estimated: false },
            ReturnSample { at_ms: now - H, elapsed_ms: 500, estimated: false },
        ]);
        session.active_tools.push("read_file".into());
        session.busy_since = Some(now - H / 2);
        let stats = session.window_stats(now);
        assert_eq!(stats.elapsed_ms, H + H / 2);
        assert_eq!(stats.calls, 2);
        assert_eq!(stats.mean_ms, Some(400.));
        assert!(!stats.estimated);
        session.active_tools.clear();
        session.busy_since = None;
        let later = session.window_stats(now + 4 * H + 1);
        assert_eq!(later.calls, 0);
        assert_eq!(later.elapsed_ms, 0);
        assert_eq!(later.mean_ms, None);
        session.update_window(now, 1);
        assert_eq!(session.window_stats(now).calls, 1);
        assert_eq!(session.window_stats(now).elapsed_ms, 0);
        assert_eq!(session.returns.len(), 1);
        let mut restored: DcrSession = serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
        restored.sanitize_after_load();
        assert_eq!(restored.window_stats(now).mean_ms, Some(500.));
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每例独立临时目录，避免并发测试互相污染。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dcr-session-{}-{tag}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 开始/重复投递：同一 call_id 不重复计数，新调用恢复可见并清除归档。
    #[test]
    fn received_call_dedupes_by_id() {
        let mut session = DcrSession {
            visible: false,
            archived: true,
            ..DcrSession::default()
        };
        let mut tracker = EventTracker::new();
        let line = "🔧 Received tool call id1: read_file {\"path\":\"x\"} metadata: {}";
        tracker.observe(&mut session, line, 100);
        assert_eq!(session.active_tools, vec!["read_file"]);
        assert!(session.busy());
        assert_eq!(session.busy_since, Some(100));
        assert_eq!(session.last_activity, 100);
        assert!(session.visible && !session.archived);

        tracker.observe(&mut session, line, 150);
        assert_eq!(session.active_tools.len(), 1, "重复投递不得重复计数");
        assert_eq!(session.last_activity, 100, "重复投递不改变活动时间");
    }

    /// 本地重复投递提示不能结束已有活动调用；DB claim 被拒才按 id 移除。
    #[test]
    fn local_duplicate_keeps_call_db_claim_rejects_it() {
        let mut session = DcrSession::default();
        let mut tracker = EventTracker::new();
        tracker.observe(
            &mut session,
            "🔧 Received tool call keep1: read_file {} metadata: {}",
            100,
        );
        tracker.observe(
            &mut session,
            "[DEBUG] Duplicate delivery for call already handled here, skipping: keep1",
            150,
        );
        assert_eq!(session.active_tools, vec!["read_file"], "本地重复不得结束原调用");
        assert_eq!(session.busy_since, Some(100));
        assert_eq!(session.last_activity, 100);
        assert_eq!(session.elapsed_ms, 0);

        tracker.observe(
            &mut session,
            "🔧 Received tool call drop1: write_file {} metadata: {}",
            200,
        );
        assert_eq!(session.active_tools.len(), 2);
        tracker.observe(
            &mut session,
            "[DEBUG] Call already claimed (duplicate delivery), skipping: drop1",
            260,
        );
        assert_eq!(session.active_tools, vec!["read_file"], "DB claim 被拒应移除对应活动");
        assert_eq!(session.busy_since, Some(100), "仍有其它活动，区间继续");
        assert_eq!(session.last_activity, 260);

        tracker.observe(
            &mut session,
            "[DEBUG] Call already claimed (duplicate delivery), skipping: keep1",
            300,
        );
        assert!(session.active_tools.is_empty());
        assert_eq!(session.busy_since, None);
        assert_eq!(session.elapsed_ms, 200, "100→300 一次性累计");
        // 未在本地活动的 doorbell 重复：不改动状态。
        tracker.observe(&mut session, "[DEBUG] Doorbell call already claimed: unknown", 400);
        assert_eq!(session.elapsed_ms, 200);
        assert_eq!(session.last_activity, 300);
    }

    /// 同名并发按工具 FIFO 配对，忙碌区间重叠只累计一次，空闲不计。
    #[test]
    fn concurrent_same_tool_fifo_and_timing() {
        let mut session = DcrSession::default();
        let mut tracker = EventTracker::new();
        tracker.observe(
            &mut session,
            "🔧 Received tool call a1: read_file {} metadata: {}",
            100,
        );
        tracker.observe(
            &mut session,
            "🔧 Received tool call a2: read_file {} metadata: {}",
            200,
        );
        assert_eq!(session.active_tools, vec!["read_file", "read_file"]);
        assert_eq!(session.busy_since, Some(100), "区间从首个调用开始");

        // 完成线之后的 JSON 结果被提取摘要，不再直接吞掉。
        tracker.observe(&mut session, "✅ Tool call read_file completed:\r", 300);
        tracker.observe(
            &mut session,
            " {\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false}",
            301,
        );
        assert!(
            session
                .logs
                .iter()
                .any(|line| line.text.contains("结果 read_file（a1）：ok")),
            "应保留完成结果摘要"
        );
        assert_eq!(session.active_tools.len(), 1);
        assert_eq!(session.busy_since, Some(100), "仍有并发调用，区间继续");

        tracker.observe(&mut session, "✅ Tool call read_file completed:", 500);
        assert!(session.active_tools.is_empty());
        assert!(!session.busy());
        assert_eq!(session.busy_since, None);
        // 100→500 一次性累计 400ms，重叠不重复。
        assert_eq!(session.elapsed_ms, 400);
        assert_eq!(session.current_elapsed(600), 0);
        assert_eq!(session.total_elapsed(600), 400);

        // 空闲 500ms 后再次调用，只累计真实区间。
        tracker.observe(
            &mut session,
            "🔧 Received tool call b1: write_file {} metadata: {}",
            1_000,
        );
        tracker.observe(&mut session, "✅ Tool call write_file completed:", 1_200);
        assert_eq!(session.elapsed_ms, 600, "空闲期不计入");
        assert_eq!(session.last_activity, 1_200);
    }

    /// 失败与停止：未完成调用中断，计时状态清空，连接进入对应状态。
    #[test]
    fn failure_and_stop_clear_timing() {
        let mut session = DcrSession::default();
        let mut tracker = EventTracker::new();
        tracker.observe(
            &mut session,
            "🔧 Received tool call f1: write_file {} metadata: {}",
            100,
        );
        tracker.observe(&mut session, "❌ Tool call write_file failed: boom token=SECRET", 200);
        assert!(session.active_tools.is_empty());
        assert_eq!(session.elapsed_ms, 100);
        assert_eq!(session.last_activity, 200);
        let failed = session.logs.back().unwrap();
        assert!(failed.text.contains("调用失败 write_file"));
        assert!(!failed.text.contains("SECRET"), "错误输出必须脱敏");

        tracker.observe(
            &mut session,
            "🔧 Received tool call s1: read_file {} metadata: {}",
            300,
        );
        mark_stopped(&mut session, 450);
        assert!(session.active_tools.is_empty());
        assert_eq!(session.busy_since, None);
        // 已有 100 + 中断区间 150 = 250ms。
        assert_eq!(session.elapsed_ms, 250);
        assert_eq!(session.connection, LINK_STOPPED);
        assert_eq!(session.last_activity, 450);
        // 重复停止无副作用，不改变活动时间。
        mark_stopped(&mut session, 999);
        assert_eq!(session.last_activity, 450);
    }

    /// 结果 isError=true 时保留失败标记与摘要。
    #[test]
    fn result_error_keeps_failure_marker() {
        let mut session = DcrSession::default();
        let mut tracker = EventTracker::new();
        tracker.observe(
            &mut session,
            "🔧 Received tool call e1: read_file {} metadata: {}",
            10,
        );
        tracker.observe(&mut session, "✅ Tool call read_file completed:", 20);
        tracker.observe(
            &mut session,
            "{\"isError\":true,\"content\":[{\"type\":\"text\",\"text\":\"permission denied\"}]}",
            21,
        );
        let last = session.logs.back().unwrap();
        assert!(
            last.text.contains("结果 read_file（e1）：失败：permission denied"),
            "got {}",
            last.text
        );
        assert!(session.active_tools.is_empty());
    }

    /// 状态文案覆盖全部固定取值，进程存活不算调用中。
    #[test]
    fn state_labels_match_connection() {
        let mut session = DcrSession::default();
        assert_eq!(session.state_label(), "已停止");
        session.connection = LINK_CONNECTING.into();
        assert_eq!(session.state_label(), "连接中");
        session.connection = LINK_ONLINE.into();
        assert_eq!(session.state_label(), "在线空闲");
        session.connection = LINK_OFFLINE.into();
        assert_eq!(session.state_label(), "离线");
        session.connection = LINK_FAILED.into();
        assert_eq!(session.state_label(), "启动失败");

        session.connection = LINK_ONLINE.into();
        session.active_tools = vec!["read_file".into()];
        assert!(session.busy());
        assert_eq!(session.state_label(), "调用中");

        session.hide();
        assert!(!session.visible);
        assert!(session.busy(), "隐藏不得停止调用");
        session.set_archived(true);
        assert!(session.archived);
        assert_eq!(session.active_tools.len(), 1);
    }

    /// 快照恢复不恢复运行中：活动调用、忙碌区间与连接态全部归零。
    #[test]
    fn snapshot_restore_never_resumes_running() {
        let dir = temp_dir("restore");
        let path = dir.join("session.json");
        let snapshot = SessionSnapshot {
            session: DcrSession {
                visible: false,
                archived: true,
                last_activity: 1_700_000_000_000,
                connection: LINK_ONLINE.into(),
                active_tools: vec!["read_file".into()],
                busy_since: Some(1_699_999_999_000),
                elapsed_ms: 42,
                returned_count: 0,
                returned_elapsed_ms: 0,
                mean_estimated: false,
                busy_periods: VecDeque::new(),
                returns: VecDeque::new(),
                window_ms: default_window_ms(),
                logs: VecDeque::from([SessionLine {
                    at_ms: 1,
                    text: "旧日志".into(),
                }]),
            },
            history_imported: true,
        };
        atomic_write(&path, &serde_json::to_vec_pretty(&snapshot).unwrap()).unwrap();

        let mut restored: SessionSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        restored.session.sanitize_after_load();
        assert!(restored.history_imported);
        assert!(!restored.session.visible);
        assert!(restored.session.archived, "归档状态应保留");
        assert_eq!(restored.session.elapsed_ms, 42, "累计耗时保留");
        assert_eq!(restored.session.last_activity, 1_700_000_000_000);
        assert!(restored.session.active_tools.is_empty());
        assert_eq!(restored.session.busy_since, None);
        assert_eq!(restored.session.connection, LINK_STOPPED);
        assert_eq!(restored.session.logs.len(), 1);
        assert!(!restored.session.busy());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 历史行标明来源，保留已有耗时与输出摘要，时间戳解析为事件时间。
    #[test]
    fn history_lines_keep_duration_and_output() {
        let calls = vec![
            DcrCall {
                timestamp: "2026-09-16T01:22:57.146Z".into(),
                tool_name: "read_file".into(),
                duration_ms: Some(77),
                output: "ok".into(),
                is_error: false,
            },
            DcrCall {
                timestamp: "not-a-time".into(),
                tool_name: "write_file".into(),
                duration_ms: None,
                output: String::new(),
                is_error: true,
            },
        ];
        let lines = history_lines(&calls);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.starts_with("[历史] read_file"));
        assert!(lines[0].text.contains("耗时 77ms"), "历史耗时保留");
        assert!(lines[0].text.contains("ok"), "历史输出摘要保留");
        assert!(lines[0].text.contains("2026-09-16T01:22:57.146Z"));
        assert!(lines[1].text.contains("（失败）"));
        assert_eq!(lines[1].at_ms, 0, "无法解析的时间不猜测");
        // 解析结果应为真实 UTC 毫秒，而不是 0。
        assert_eq!(lines[0].at_ms, 1_789_521_777_146);
    }

    /// 多行结果体被完整累积并提取摘要，不误判为调用事件。
    #[test]
    fn result_summary_from_multiline_body() {
        let mut session = DcrSession::default();
        let mut tracker = EventTracker::new();
        tracker.observe(
            &mut session,
            "🔧 Received tool call m1: read_file {} metadata: {}",
            10,
        );
        tracker.observe(&mut session, "✅ Tool call read_file completed:", 20);
        assert!(session.active_tools.is_empty());
        tracker.observe(&mut session, "{", 21);
        tracker.observe(
            &mut session,
            "  \"content\": [{\"type\": \"text\", \"text\": \"Received tool call x: y\"}]",
            22,
        );
        tracker.observe(&mut session, "}", 23);
        let joined: String = session.logs.iter().map(|line| line.text.clone()).collect();
        assert!(joined.contains("结果 read_file（m1）"), "应写入结果摘要");
        assert!(joined.contains("Received tool call x: y"), "结果文本保留在日志值中");
        assert!(session.active_tools.is_empty());
    }

    /// configure_monitor：不改 Arc，首次导入历史（含耗时/摘要）并标明，重启不重复导入。
    #[test]
    fn configure_monitor_imports_history_once_and_keeps_arc() {
        let dir = temp_dir("configure");
        let snapshot_path = dir.join("dcr-session.json");
        let history_path = dir.join("tool-history.jsonl");
        let line = serde_json::json!({
            "timestamp": "2026-09-16T01:22:57.146Z",
            "toolName": "read_file",
            "output": {"content": [{"type": "text", "text": "ok"}]},
            "duration": 77
        })
        .to_string();
        std::fs::write(&history_path, format!("{line}\n")).unwrap();

        let mut service = crate::dcr::DcrService::new();
        let arc = service.monitor();
        service
            .configure_monitor(snapshot_path.clone(), history_path.clone())
            .unwrap();
        assert!(
            Arc::ptr_eq(&arc, &service.monitor()),
            "configure 不得替换会话 Arc"
        );
        {
            let session = arc.lock().unwrap();
            assert!(
                session
                    .logs
                    .iter()
                    .any(|entry| entry.text.contains("[历史] read_file") && entry.text.contains("耗时 77ms")),
                "首次应导入并标明历史耗时"
            );
            assert_eq!(session.connection, LINK_STOPPED, "不恢复运行中");
            assert_eq!(session.elapsed_ms, 0, "历史耗时不进入实时忙碌累计");
            assert!(!session.busy());
        }
        // drop 会写最后快照（history_imported=true）。
        drop(service);

        let mut restarted = crate::dcr::DcrService::new();
        restarted
            .configure_monitor(snapshot_path.clone(), history_path.clone())
            .unwrap();
        let session = restarted.monitor();
        let session = session.lock().unwrap();
        let history = session
            .logs
            .iter()
            .filter(|entry| entry.text.contains("[历史]"))
            .count();
        assert_eq!(history, 1, "旧本机历史只在首次导入");
        assert_eq!(session.connection, LINK_STOPPED);
        assert_eq!(session.elapsed_ms, 0);
        drop(session);
        std::fs::remove_dir_all(&dir).ok();
    }
}
    /// 返回均耗时逐次计入，失败返回也计入，取消/中断不计入样本。
    #[test]
    fn mean_counts_returns_not_busy_intervals_or_interruptions() {
        let mut session = DcrSession::default();
        let mut tracker = EventTracker::new();
        assert_eq!(session.mean_elapsed(), None);
        tracker.observe(&mut session, "🔧 Received tool call avg1: read_file {} metadata: {}", 100);
        tracker.observe(&mut session, "🔧 Received tool call avg2: get_file_info {} metadata: {}", 200);
        tracker.observe(&mut session, "✅ Tool call read_file completed:", 400);
        tracker.observe(&mut session, "{}", 400);
        tracker.observe(&mut session, "❌ Tool call get_file_info failed: error", 700);
        assert_eq!(session.returned_count, 2);
        assert_eq!(session.elapsed_ms, 600);
        assert_eq!(session.mean_elapsed(), Some(400.));
        tracker.observe(&mut session, "🔧 Received tool call avg3: read_file {} metadata: {}", 800);
        mark_stopped(&mut session, 900);
        assert_eq!(session.returned_count, 2);
        assert_eq!(session.mean_elapsed(), Some(400.));
        let restored: DcrSession = serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
        assert_eq!(restored.mean_elapsed(), Some(400.));
    }
