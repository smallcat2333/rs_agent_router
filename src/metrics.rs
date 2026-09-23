//! CLI 原生指标与执行来源；缺失数据保留为空，不估算上下文或推理健康。
use crate::protocol::Backend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[path = "metrics_codex.rs"]
mod codex;
pub use codex::CodexLiveMetrics;

/// 本次任务中一轮完整模型响应的配对用量与耗时。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ThroughputSample {
    pub output_tokens: u64,
    pub duration_ms: u64,
}

/// 一次完整模型回复的耗时与测量口径；不保存正文。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplyDuration {
    pub duration_ms: u64,
    pub source: String,
}

/// Claude 单次响应的开始、等待和最终输出用量；增量 usage 是累计值，不能相加。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeReplyTimer {
    start_ms: u64,
    wait_ms: Option<u64>,
    output_tokens: Option<u64>,
}

/// 当前轮事件配对状态不落盘，重启不使用旧的计时起点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReplyTimer {
    claude: BTreeMap<String, ClaudeReplyTimer>,
    codex_start: Option<u64>,
    tools: BTreeSet<String>,
    completed_ids: BTreeSet<String>,
}

/// 每轮执行的指标快照。context 是最近一次请求输入，usage 是本轮累计用量。
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Metrics {
    /// 最近一次回复报告的具体模型，忽略 CLI 配置别名与合成错误消息。
    pub reported_model: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub first_text_ms: Option<u64>,
    pub first_text_source: Option<String>,
    pub reply_durations: VecDeque<ReplyDuration>,
    /// 与 model_duration_ms 配对的输出用量，不含输入或缓存 Token。
    pub model_output_tokens: Option<u64>,
    /// 当前任务最近十轮配对模型时长；Codex 排除可配对的工具执行区间。
    pub model_duration_ms: u64,
    /// Codex 模型阶段包含请求等待，CLI 无逐 Token 边界，必须标为近似值。
    pub tps_estimated: bool,
    /// 仅保留当前任务最近十轮，不跨任务或续聊累计。
    pub throughput_samples: VecDeque<ThroughputSample>,
    #[serde(skip)]
    reply_timer: ReplyTimer,
}

impl Metrics {
    /// 加入完整响应，移除第十一条旧记录后重算配对总量；未知或零时长不入窗。
    fn push_throughput(&mut self, output_tokens: u64, duration_ms: u64) {
        if duration_ms == 0 {
            return;
        }
        self.throughput_samples.push_back(ThroughputSample { output_tokens, duration_ms });
        if self.throughput_samples.len() > 10 {
            self.throughput_samples.pop_front();
        }
        self.model_output_tokens = Some(self.throughput_samples.iter().map(|s| s.output_tokens).sum());
        self.model_duration_ms = self.throughput_samples.iter().map(|s| s.duration_ms).sum();
    }

    /// 消费已验证的原生事件；不把累计 input_tokens 当作最新上下文长度。
    pub fn observe(&mut self, backend: Backend, value: &Value, elapsed_ms: u64) {
        if backend == Backend::Agy {
            if value["event"] == "step_update"
                && value["step_update"]["step_type"] == "agent_response"
                && value["step_update"]["text_delta"].as_str().is_some_and(|text| !text.trim().is_empty())
                && self.first_text_ms.is_none()
            {
                self.first_text_ms = Some(elapsed_ms);
                self.first_text_source = Some("text_delta".into());
            }
            if value["event"] == "result" {
                let usage = &value["result"]["usage"];
                if let Some(input) = usage["input_tokens"].as_u64() {
                    self.input_tokens = Some(input);
                    self.context_tokens = Some(input);
                }
                if let Some(output) = usage["output_tokens"].as_u64() {
                    self.output_tokens = Some(output);
                    self.model_output_tokens = Some(output);
                }
                if let Some(cached) = usage["cache_read_tokens"].as_u64() {
                    self.cached_tokens = Some(cached);
                }
            }
            return;
        }
        if backend == Backend::Opencode {
            if value["type"] == "step_finish" && value["part"]["tokens"].is_object() {
                let tokens = &value["part"]["tokens"];
                let cached = tokens["cache"]["read"].as_u64().unwrap_or(0);
                let input = tokens["input"].as_u64().unwrap_or(0) + cached + tokens["cache"]["write"].as_u64().unwrap_or(0);
                let output = tokens["output"].as_u64().unwrap_or(0) + tokens["reasoning"].as_u64().unwrap_or(0);
                self.context_tokens = Some(input);
                self.input_tokens = Some(self.input_tokens.unwrap_or(0) + input);
                self.output_tokens = Some(self.output_tokens.unwrap_or(0) + output);
                self.cached_tokens = Some(self.cached_tokens.unwrap_or(0) + cached);
            }
            if self.first_text_ms.is_none() && visible_text(backend, value) {
                self.first_text_ms = Some(elapsed_ms);
                self.first_text_source = Some("completed_message".into());
            }
            return;
        }
        self.observe_reply(backend, value, elapsed_ms);
        if backend == Backend::Claude {
            let event = &value["event"];
            let model = if value["type"] == "assistant" {
                value["message"]["model"].as_str()
            } else if value["type"] == "stream_event" {
                event["message"]["model"].as_str()
            } else {
                None
            };
            if let Some(model) = model.filter(|model| explicit_model(model)) {
                self.reported_model = Some(model.to_owned());
            }
            if value["type"] == "assistant" {
                self.context_tokens = claude_input(&value["message"]["usage"]).filter(|n| *n > 0);
            } else if value["type"] == "stream_event" && event["type"] == "message_start" {
                self.context_tokens = claude_input(&event["message"]["usage"]).filter(|n| *n > 0);
            } else if value["type"] == "stream_event"
                && event["type"] == "message_delta"
                && let Some(input) = claude_input(&event["usage"]).filter(|n| *n > 0)
            {
                // GLM 的开始事件为零占位，真实输入和缓存用量在 message_delta 才提供。
                self.context_tokens = Some(input);
            }
            if value["type"] == "result" {
                let usage = &value["usage"];
                self.input_tokens = claude_input(usage);
                self.output_tokens = usage["output_tokens"].as_u64();
                self.cached_tokens = usage["cache_read_input_tokens"].as_u64();
            }
        } else if value["type"] == "turn.completed" {
            let usage = &value["usage"];
            // Codex input_tokens 已包含缓存，不能再次累加 cached_input_tokens。
            self.input_tokens = usage["input_tokens"].as_u64();
            self.output_tokens = usage["output_tokens"].as_u64();
            self.cached_tokens = usage["cached_input_tokens"].as_u64();
        }
        if self.first_text_ms.is_none() && visible_text(backend, value) {
            self.first_text_ms = Some(elapsed_ms);
            self.first_text_source = Some(
                if value["type"] == "stream_event" {
                    "stream_text"
                } else {
                    "completed_message"
                }
                .into(),
            );
        }
    }

    /// 只在输入与输出都已报告时计算总 Token 数。
    pub fn total_tokens(&self) -> Option<u64> {
        Some(self.input_tokens? + self.output_tokens?)
    }

    /// 按配对的输出 Token / 模型秒数计算 TPS；用量缺失或零时长时保持未知。
    pub fn output_tps(&self) -> Option<f64> {
        if self.model_duration_ms == 0 {
            return None;
        }
        Some(self.model_output_tokens? as f64 * 1000. / self.model_duration_ms as f64)
    }

    /// 配对完整回复边界；忽略 Claude 汇总副本，排除 Codex 工具阶段和重复完成事件。
    fn observe_reply(&mut self, backend: Backend, value: &Value, elapsed_ms: u64) {
        if backend == Backend::Claude {
            if value["type"] == "result" {
                self.reply_timer.claude.clear();
                return;
            }
            if value["type"] != "stream_event" {
                return;
            }
            let channel = value["parent_tool_use_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            match value["event"]["type"].as_str() {
                Some("message_start") => {
                    self.reply_timer.claude.insert(channel, ClaudeReplyTimer {
                        start_ms: elapsed_ms,
                        wait_ms: value["ttft_ms"].as_u64(),
                        output_tokens: None,
                    });
                }
                Some("message_delta") => {
                    if let Some(reply) = self.reply_timer.claude.get_mut(&channel)
                        && let Some(tokens) = value["event"]["usage"]["output_tokens"].as_u64()
                    {
                        reply.output_tokens = Some(tokens);
                    }
                }
                Some("message_stop") => {
                    if let Some(reply) = self.reply_timer.claude.remove(&channel) {
                        // 只配对主执行模型的完整响应；不把工具间隔、首字等待或子代理混入 TPS。
                        if channel.is_empty()
                            && let Some(tokens) = reply.output_tokens
                            && let Some(duration) = elapsed_ms.checked_sub(reply.start_ms).filter(|ms| *ms > 0)
                        {
                            self.push_throughput(tokens, duration);
                        }
                        if let Some(duration) = elapsed_ms
                            .checked_sub(reply.start_ms)
                            .and_then(|ms| ms.checked_add(reply.wait_ms.unwrap_or(0)))
                        {
                            self.push_reply(
                                duration,
                                if reply.wait_ms.is_some() {
                                    "claude_request"
                                } else {
                                    "claude_stream"
                                },
                            );
                        }
                    }
                }
                Some("error") => {
                    self.reply_timer.claude.remove(&channel);
                }
                _ => {}
            }
            return;
        }
        let item = &value["item"];
        let is_tool = matches!(
            item["type"].as_str(),
            Some("command_execution" | "mcp_tool_call" | "file_change" | "web_search")
        );
        match value["type"].as_str() {
            Some("turn.started") => {
                self.reply_timer.codex_start = Some(elapsed_ms);
                self.model_output_tokens = None;
                self.model_duration_ms = 0;
                self.throughput_samples.clear();
                self.tps_estimated = true;
                self.reply_timer.tools.clear();
                self.reply_timer.completed_ids.clear();
            }
            Some("item.started") if is_tool => {
                if let Some(id) = item["id"].as_str() {
                    self.reply_timer.tools.insert(id.into());
                }
                self.reply_timer.codex_start = None;
            }
            Some("item.completed") => {
                let Some(id) = item["id"].as_str() else {
                    return;
                };
                if !self.reply_timer.completed_ids.insert(id.into()) {
                    return;
                }
                if is_tool {
                    self.reply_timer.tools.remove(id);
                    if self.reply_timer.tools.is_empty() {
                        self.reply_timer.codex_start = Some(elapsed_ms);
                    }
                } else if item["type"] == "agent_message"
                    && self.reply_timer.tools.is_empty()
                    && item["text"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty())
                {
                    if let Some(start) = self.reply_timer.codex_start {
                        if let Some(duration) = elapsed_ms.checked_sub(start) {
                            self.push_reply(duration, "codex_reply_cycle");
                        }
                    }
                    self.reply_timer.codex_start = Some(elapsed_ms);
                }
            }
            Some("turn.completed" | "turn.failed" | "error") => {
                // 原生 token_count 维护 TPS 窗口，CLI 任务汇总仅更新总 Token。
                self.reply_timer.codex_start = None;
                self.reply_timer.tools.clear();
            }
            _ => {}
        }
    }

    /// 保留本轮最近五条完整回复；未完成回复不进入平均值。
    fn push_reply(&mut self, duration_ms: u64, source: &str) {
        self.reply_durations.push_back(ReplyDuration {
            duration_ms,
            source: source.into(),
        });
        if self.reply_durations.len() > 5 {
            self.reply_durations.pop_front();
        }
    }
}

/// Claude 输入用量分别计未缓存、缓存读取和缓存创建；缺少 input 时不推算。
fn claude_input(usage: &Value) -> Option<u64> {
    Some(
        usage["input_tokens"].as_u64()?
            + usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
            + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
    )
}

/// 检测用户可见文本，思考、心跳和工具参数不算首字。
pub fn visible_text(backend: Backend, value: &Value) -> bool {
    if backend == Backend::Agy {
        return value["event"] == "step_update"
            && value["step_update"]["step_type"] == "agent_response"
            && value["step_update"]["text_delta"].as_str().is_some_and(|text| !text.trim().is_empty());
    }
    if backend == Backend::Opencode {
        return value["type"] == "text" && value["part"]["text"].as_str().is_some_and(|text| !text.trim().is_empty());
    }
    if backend == Backend::Codex {
        value["type"] == "item.completed"
            && value["item"]["type"] == "agent_message"
            && value["item"]["text"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty())
    } else {
        let delta = &value["event"]["delta"];
        (value["type"] == "stream_event"
            && delta["type"] == "text_delta"
            && delta["text"].as_str().is_some_and(|s| !s.trim().is_empty()))
            || (value["type"] == "assistant"
                && value["message"]["content"]
                    .as_array()
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            b["type"] == "text"
                                && b["text"].as_str().is_some_and(|s| !s.trim().is_empty())
                        })
                    }))
    }
}

/// 保留请求与回复两份证据；切换后以最后实际回复模型归属提交，缺少身份或强度时不生成前缀。
pub fn executor(
    backend: Backend,
    requested: Option<&str>,
    effort: Option<&str>,
    reported: Option<&str>,
) -> Value {
    let configured = requested.filter(|m| explicit_model(m));
    let observed = reported.filter(|m| explicit_model(m));
    let conflict = configured
        .zip(observed)
        .is_some_and(|(a, b)| !a.eq_ignore_ascii_case(b));
    let (model, source) = if observed.is_some() {
        (observed, Some("cli_reported"))
    } else {
        (configured, configured.map(|_| "app_config"))
    };
    let prefix = model
        .zip(effort)
        .filter(|(m, e)| observed.is_some() && (component(m) || backend == Backend::Opencode && m.split('/').all(component)) && component(e))
        .map(|(m, e)| {
            format!(
                "[ar-{}-{}-{}]",
                backend.short(),
                m.to_lowercase().replace('/', "-"),
                e.to_lowercase()
            )
        });
    json!({"cli":backend,"requested_model":requested,"reported_model":reported,
        "model":model,"model_source":source,"model_conflict":conflict,
        "effort":effort,"effort_source":effort.map(|_| "app_config"),"commit_prefix":prefix})
}

/// 通用路由别名不能证明实际工作模型，保留原始字段供审计。
pub fn explicit_model(model: &str) -> bool {
    !model.trim().is_empty()
        && !["auto", "default", "sonnet", "opus", "haiku", "fable", "<synthetic>"]
            .contains(&model.to_ascii_lowercase().as_str())
}

/// 提交标识仅接受模型名常用 ASCII 字符，不把外部文本拼入 shell。
fn component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Codex 原生会话只提取身份和最后请求用量；不向主会话返回过程内容。
pub struct NativeMetadata {
    pub path: std::path::PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
}

/// 按本次 CLI 返回的 UUID 精确找最新 rollout，默认配置为空时仍可证明实际模型。
pub fn codex_metadata(session_id: &str) -> anyhow::Result<NativeMetadata> {
    read_native_metadata(codex_session_path(session_id)?)
}

/// 精确定位原生会话文件，供结束时元数据和运行中的增量读取共用。
fn codex_session_path(session_id: &str) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::{Context, ensure};
    ensure!(
        session_id.len() == 36
            && session_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "invalid native session ID"
    );
    let root = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE missing"))
                .join(".codex")
        })
        .join("sessions");
    let mut pending = vec![root];
    let mut matches = vec![];
    let suffix = format!("{session_id}.jsonl");
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() && entry.file_name().to_string_lossy().ends_with(&suffix)
            {
                matches.push((entry.metadata()?.modified()?, entry.path()));
            }
        }
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0));
    let path = matches
        .into_iter()
        .next()
        .context("native session metadata not found")?
        .1;
    Ok(path)
}

/// 流式读取指定会话，仅解析 turn_context 和 token_count，其他内容即时丢弃。
fn read_native_metadata(path: std::path::PathBuf) -> anyhow::Result<NativeMetadata> {
    use std::io::BufRead;
    let mut metadata = NativeMetadata {
        path: path.clone(),
        model: None,
        effort: None,
        context_tokens: None,
        context_window_tokens: None,
    };
    for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if !line.contains("\"turn_context\"") && !line.contains("\"token_count\"") {
            continue;
        }
        let value: Value = serde_json::from_str(&line)?;
        if value["type"] == "turn_context" {
            metadata.model = value["payload"]["model"].as_str().map(str::to_owned);
            metadata.effort = value["payload"]["effort"].as_str().map(str::to_owned);
            metadata.context_tokens = None;
            metadata.context_window_tokens = None;
        } else if value["type"] == "event_msg"
            && value["payload"]["type"] == "token_count"
            && value["payload"]["info"].is_object()
        {
            metadata.context_tokens =
                value["payload"]["info"]["last_token_usage"]["input_tokens"].as_u64();
            metadata.context_window_tokens =
                value["payload"]["info"]["model_context_window"].as_u64();
        }
    }
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 只累计配对的主模型响应，用量增量覆盖旧值，长工具间隔和首字等待均不影响 TPS。
    #[test]
    fn claude_tps_excludes_tool_gaps_wait_and_duplicate_usage() {
        let mut metrics = Metrics::default();
        for (start, duration, tokens) in [(100, 1000, 100), (60100, 3000, 300)] {
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","ttft_ms":5000,"event":{"type":"message_start"}}), start);
            for output in [20, tokens] {
                metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":output}}}), start + duration);
            }
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_stop"}}), start + duration);
            metrics.observe(Backend::Claude, &json!({"type":"assistant","message":{"usage":{"output_tokens":tokens}}}), start + duration);
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_stop"}}), start + duration);
        }
        for (kind, at) in [("message_start", 70000), ("message_delta", 71000), ("message_stop", 71000)] {
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","parent_tool_use_id":"child","event":{"type":kind,"usage":{"output_tokens":500}}}), at);
        }
        metrics.observe(Backend::Claude, &json!({"type":"result","usage":{"input_tokens":99999,"output_tokens":900}}), 100000);
        assert_eq!(metrics.model_output_tokens, Some(400));
        assert_eq!(metrics.model_duration_ms, 4000);
        assert_eq!(metrics.output_tps(), Some(100.));
        assert!(!metrics.tps_estimated);
        let restored: Metrics = serde_json::from_value(serde_json::to_value(&metrics).unwrap()).unwrap();
        assert_eq!(restored.output_tps(), Some(100.));
    }

    /// Claude 也按单任务最近十次完整响应取窗口，不把之前响应留在分子或分母中。
    #[test]
    fn claude_throughput_keeps_latest_ten_responses() {
        let mut metrics = Metrics::default();
        for n in 1..=12 {
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_start"}}), n * 10000);
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":n * 100}}}), n * 10000 + 1000);
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_stop"}}), n * 10000 + 1000);
        }
        assert_eq!(metrics.throughput_samples.len(), 10);
        assert_eq!(metrics.model_output_tokens, Some(7500));
        assert_eq!(metrics.model_duration_ms, 10000);
        assert_eq!(metrics.output_tps(), Some(750.));
    }

    /// CLI 任务汇总和重复完成事件不能覆盖原生逐响应窗口。
    #[test]
    fn codex_summary_preserves_native_window() {
        let mut metrics = Metrics::default();
        metrics.observe(Backend::Codex, &json!({"type":"turn.started"}), 100);
        for n in 1..=6 {
            metrics.observe(Backend::Codex, &json!({"type":"item.completed","item":{"id":format!("reply-{n}"),"type":"agent_message","text":"hi"}}), 100 + n * 100);
        }
        for (id, at) in [("tool1", 1100), ("tool2", 1500)] {
            metrics.observe(Backend::Codex, &json!({"type":"item.started","item":{"id":id,"type":"command_execution"}}), at);
        }
        for (id, at) in [("tool1", 101000), ("tool2", 201100), ("tool2", 201101)] {
            metrics.observe(Backend::Codex, &json!({"type":"item.completed","item":{"id":id,"type":"command_execution"}}), at);
        }
        metrics.push_throughput(400, 4000);
        for at in [204100, 205000] {
            metrics.observe(Backend::Codex, &json!({"type":"turn.completed","usage":{"input_tokens":99999,"output_tokens":400}}), at);
            assert_eq!(metrics.model_duration_ms, 4000);
            assert_eq!(metrics.output_tps(), Some(100.));
        }
        assert!(metrics.tps_estimated);
    }

    /// 不完整用量、零时长、倒序时间和缺少工具开始边界时都不伪造 TPS。
    #[test]
    fn tps_requires_complete_usage_and_timing() {
        for (start, stop, tokens) in [(100, 1000, None), (100, 100, Some(20)), (100, 99, Some(20))] {
            let mut metrics = Metrics::default();
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_start"}}), start);
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":tokens}}}), stop);
            metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"type":"message_stop"}}), stop);
            assert_eq!(metrics.output_tps(), None);
        }
        let mut codex = Metrics::default();
        codex.observe(Backend::Codex, &json!({"type":"turn.completed","usage":{"output_tokens":20}}), 1000);
        assert_eq!(codex.output_tps(), None);
        codex.observe(Backend::Codex, &json!({"type":"turn.started"}), 2000);
        codex.observe(Backend::Codex, &json!({"type":"item.completed","item":{"id":"missing-start","type":"file_change"}}), 8000);
        codex.observe(Backend::Codex, &json!({"type":"turn.completed","usage":{"output_tokens":20}}), 9000);
        assert_eq!(codex.output_tps(), None);
        let restored: Metrics = serde_json::from_value(json!({"output_tokens":20})).unwrap();
        assert_eq!(restored.output_tps(), None);
    }

    /// Claude 等待与响应时长相加，滚动保留五条；重复汇总、孤立结束和未完成回复不计数。
    #[test]
    fn reply_timing_claude_counts_only_complete_messages() {
        let mut metrics = Metrics::default();
        for n in 1..=6 {
            let start = n * 10000;
            metrics.observe(
                Backend::Claude,
                &json!({"type":"stream_event","ttft_ms":50,"event":{"type":"message_start"}}),
                start,
            );
            metrics.observe(
                Backend::Claude,
                &json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
                start + n * 100,
            );
            metrics.observe(
                Backend::Claude,
                &json!({"type":"stream_event","event":{"type":"message_stop"}}),
                start + n * 100,
            );
        }
        assert_eq!(
            metrics
                .reply_durations
                .iter()
                .map(|r| r.duration_ms)
                .collect::<Vec<_>>(),
            [250, 350, 450, 550, 650]
        );
        metrics.observe(
            Backend::Claude,
            &json!({"type":"stream_event","event":{"type":"message_stop"}}),
            70000,
        );
        metrics.observe(
            Backend::Claude,
            &json!({"type":"stream_event","event":{"type":"message_start"}}),
            71000,
        );
        assert_eq!(metrics.reply_durations.len(), 5);
        let mut restored: Metrics =
            serde_json::from_value(serde_json::to_value(&metrics).unwrap()).unwrap();
        restored.observe(
            Backend::Claude,
            &json!({"type":"stream_event","event":{"type":"message_stop"}}),
            72000,
        );
        assert_eq!(restored.reply_durations, metrics.reply_durations);
    }

    /// Codex 回复周期包括等待，但并行工具全部结束后才重新计时，重复完成不新增样本。
    #[test]
    fn reply_timing_codex_excludes_parallel_tool_time() {
        let mut metrics = Metrics::default();
        metrics.observe(Backend::Codex, &json!({"type":"turn.started"}), 100);
        metrics.observe(Backend::Codex, &json!({"type":"item.completed","item":{"id":"a","type":"agent_message","text":"hello"}}), 500);
        for (id, at) in [("tool1", 700), ("tool2", 800)] {
            metrics.observe(
                Backend::Codex,
                &json!({"type":"item.started","item":{"id":id,"type":"command_execution"}}),
                at,
            );
        }
        for (id, at) in [("tool1", 10000), ("tool2", 11000)] {
            metrics.observe(
                Backend::Codex,
                &json!({"type":"item.completed","item":{"id":id,"type":"command_execution"}}),
                at,
            );
        }
        let response =
            json!({"type":"item.completed","item":{"id":"b","type":"agent_message","text":"done"}});
        metrics.observe(Backend::Codex, &response, 11300);
        metrics.observe(Backend::Codex, &response, 11500);
        metrics.observe(Backend::Codex, &json!({"type":"turn.failed"}), 11600);
        assert_eq!(
            metrics
                .reply_durations
                .iter()
                .map(|r| r.duration_ms)
                .collect::<Vec<_>>(),
            [400, 300]
        );
        assert!(metrics.reply_timer.codex_start.is_none());
    }
    /// Claude 最近请求上下文和本轮累计用量分离，Codex 缓存不重复计数。
    #[test]
    fn usage_and_context_have_distinct_meanings() {
        let mut metrics = Metrics::default();
        metrics.observe(Backend::Claude, &json!({"type":"assistant","message":{"usage":{"input_tokens":220,"cache_read_input_tokens":3384},"content":[{"type":"text","text":"42"}]}}), 1250);
        metrics.observe(Backend::Claude, &json!({"type":"result","usage":{"input_tokens":3700,"cache_read_input_tokens":3384,"output_tokens":133}}), 2000);
        assert_eq!(metrics.context_tokens, Some(3604));
        assert_eq!(metrics.total_tokens(), Some(7217));
        assert_eq!(metrics.first_text_ms, Some(1250));
        let mut codex = Metrics::default();
        codex.observe(Backend::Codex, &json!({"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20}}), 2);
        assert_eq!(codex.total_tokens(), Some(120));
        assert_eq!(codex.context_tokens, None);
        assert_eq!(codex.first_text_ms, None);
    }
    /// 思考片段不触发首字，收到实际文本后不被后续完整消息覆盖。
    #[test]
    fn first_text_ignores_thinking() {
        let mut metrics = Metrics::default();
        metrics.observe(Backend::Claude, &json!({"type":"stream_event","event":{"delta":{"type":"thinking_delta","thinking":"hmm"}}}), 50);
        assert_eq!(metrics.first_text_ms, None);
        metrics.observe(
            Backend::Claude,
            &json!({"type":"stream_event","event":{"delta":{"type":"text_delta","text":"好"}}}),
            75,
        );
        assert_eq!(metrics.first_text_ms, Some(75));
        assert_eq!(metrics.first_text_source.as_deref(), Some("stream_text"));
    }
    /// GLM 的零占位不是上下文长度，最终增量用量包含缓存输入。
    #[test]
    fn glm_context_comes_from_message_delta() {
        let mut metrics = Metrics::default();
        metrics.observe(Backend::Claude,&json!({"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":0}}}}),0);
        assert_eq!(metrics.context_tokens, None);
        metrics.observe(Backend::Claude,&json!({"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":115,"cache_read_input_tokens":4617}}}),100);
        assert_eq!(metrics.context_tokens, Some(4732));
    }
    /// 原生会话选最新轮次身份和最后请求，累计 Token 不会用作上下文。
    #[test]
    fn native_metadata_uses_latest_turn_only() {
        let path = std::env::temp_dir().join(format!(
            "router-native-meta-{}.jsonl",
            crate::runner::now_ms()
        ));
        let lines = [
            json!({"type":"turn_context","payload":{"model":"old","effort":"low"}}),
            json!({"type":"turn_context","payload":{"model":"gpt-model","effort":"high"}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1234},"total_token_usage":{"input_tokens":9999},"model_context_window":10000}}}),
        ];
        std::fs::write(
            &path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let metadata = read_native_metadata(path).unwrap();
        assert_eq!(metadata.model.as_deref(), Some("gpt-model"));
        assert_eq!(metadata.effort.as_deref(), Some("high"));
        assert_eq!(metadata.context_tokens, Some(1234));
    }
    /// 代理返回 auto 时只能标注请求配置，不能据此生成实际执行模型的提交前缀。
    #[test]
    fn executor_provenance_is_explicit() {
        let value = executor(Backend::Claude, Some("GLM-5.2"), Some("high"), Some("auto"));
        assert!(value["commit_prefix"].is_null());
        assert_eq!(value["model_source"], "app_config");
        let verified = executor(
            Backend::Claude,
            Some("GLM-5.3"),
            Some("max"),
            Some("glm-5.3"),
        );
        assert_eq!(verified["commit_prefix"], "[ar-cc-glm-5.3-max]");
        assert!(executor(Backend::Codex, None, Some("high"), None)["commit_prefix"].is_null());
        let conflict = executor(
            Backend::Claude,
            Some("GLM-5.2"),
            Some("high"),
            Some("other-model"),
        );
        assert_eq!(conflict["model_conflict"], true);
        assert_eq!(conflict["commit_prefix"], "[ar-cc-other-model-high]");
    }
}
