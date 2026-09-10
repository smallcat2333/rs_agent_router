//! CLI 原生指标与执行来源；缺失数据保留为空，不估算上下文或推理健康。
use crate::protocol::Backend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// 一次完整模型回复的耗时与测量口径；不保存正文。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplyDuration {
    pub duration_ms: u64,
    pub source: String,
}

/// 当前轮事件配对状态不落盘，重启不使用旧的计时起点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReplyTimer {
    claude: BTreeMap<String, (u64, Option<u64>)>,
    codex_start: Option<u64>,
    tools: BTreeSet<String>,
    completed_ids: BTreeSet<String>,
}

/// 每轮执行的指标快照。context 是最近一次请求输入，usage 是本轮累计用量。
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Metrics {
    pub context_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub first_text_ms: Option<u64>,
    pub first_text_source: Option<String>,
    pub reply_durations: VecDeque<ReplyDuration>,
    #[serde(skip)]
    reply_timer: ReplyTimer,
}

impl Metrics {
    /// 消费已验证的原生事件；不把累计 input_tokens 当作最新上下文长度。
    pub fn observe(&mut self, backend: Backend, value: &Value, elapsed_ms: u64) {
        self.observe_reply(backend, value, elapsed_ms);
        if backend == Backend::Claude {
            let event = &value["event"];
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
                    self.reply_timer
                        .claude
                        .insert(channel, (elapsed_ms, value["ttft_ms"].as_u64()));
                }
                Some("message_stop") => {
                    if let Some((start, wait)) = self.reply_timer.claude.remove(&channel) {
                        if let Some(duration) = elapsed_ms
                            .checked_sub(start)
                            .and_then(|ms| ms.checked_add(wait.unwrap_or(0)))
                        {
                            self.push_reply(
                                duration,
                                if wait.is_some() {
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

/// 返回配置与 CLI 报告两份证据；模型冲突、别名或缺少强度时不生成提交前缀。
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
    let (model, source) = if conflict {
        (None, None)
    } else if observed.is_some() {
        (observed, Some("cli_reported"))
    } else {
        (configured, configured.map(|_| "app_config"))
    };
    let prefix = model
        .zip(effort)
        .filter(|(m, e)| observed.is_some() && component(m) && component(e))
        .map(|(m, e)| {
            format!(
                "[ar-{}-{}-{}]",
                if backend == Backend::Claude {
                    "cc"
                } else {
                    "cx"
                },
                m.to_lowercase(),
                e.to_lowercase()
            )
        });
    json!({"cli":backend,"requested_model":requested,"reported_model":reported,
        "model":model,"model_source":source,"model_conflict":conflict,
        "effort":effort,"effort_source":effort.map(|_| "app_config"),"commit_prefix":prefix})
}

/// 通用路由别名不能证明实际工作模型，保留原始字段供审计。
fn explicit_model(model: &str) -> bool {
    !model.trim().is_empty()
        && !["auto", "default", "sonnet", "opus", "haiku"]
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
    read_native_metadata(path)
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
        assert!(conflict["commit_prefix"].is_null());
    }
}
