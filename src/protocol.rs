//! 任务契约及 Claude/Codex/OpenCode 事件归一；退出码和协议完成事件共同决定成功。
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

/// CLI 类型；模型名称（包括 GLM）不属于 CLI 枚举。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Claude,
    Codex,
    Opencode,
    Agy,
}
impl Backend {
    /// 配置表的 CLI 键，和模型名称无关。
    pub fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Agy => "agy",
        }
    }
    /// UI 与提交归属统一使用 CLI 简称。
    pub fn short(self) -> &'static str {
        match self { Self::Claude => "cc", Self::Codex => "cx", Self::Opencode => "oc", Self::Agy => "agy" }
    }
}

/// 本地配置只保存程序路径和模型，认证沿用 CLI 自身配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub program: PathBuf,
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

/// 配置按 claude/codex/opencode 名称索引。
pub type Profiles = BTreeMap<String, Profile>;

/// Harness 只描述工作；CLI、模型、强度及执行权限由管理页决定。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub task_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub group_path: Vec<String>,
    pub workdir: PathBuf,
    pub prompt: String,
}

/// 已保存的路由默认项；任务创建时固定快照，续聊不改变 CLI 身份。
#[derive(Clone)]
pub struct Routing {
    pub backend: Backend,
    pub allow_edits: bool,
    pub clean_start: bool,
    pub timeout_seconds: u64,
    pub retry_timeout_seconds: u64,
    /// Agy 后端启用代理时的 SOCKS5 地址 (ip:port)；空串表示不启用。
    pub agy_proxy: String,
}
/// 默认给异常请求重试六十秒，独立于普通无输出超时。
pub fn default_retry_timeout_seconds() -> u64 {
    60
}
impl Default for Routing {
    /// 默认只读 Claude CLI，模型和强度取应用配置。
    fn default() -> Self {
        Self {
            backend: Backend::Claude,
            allow_edits: false,
            clean_start: true,
            timeout_seconds: 300,
            retry_timeout_seconds: default_retry_timeout_seconds(),
            agy_proxy: String::new(),
        }
    }
}
impl Work {
    /// 工作字段沿用任务验证，不接收外部执行配置。
    pub fn validate(&self) -> Result<()> {
        self.clone()
            .resolve(
                &Routing::default(),
                &Profile {
                    program: PathBuf::new(),
                    model: None,
                    effort: None,
                },
            )
            .validate()
    }
    /// 解析工作输入后与配置快照组合；Harness 不能覆盖模型路由。
    pub fn resolve(self, routing: &Routing, profile: &Profile) -> Task {
        Task {
            task_id: self.task_id,
            title: self.title,
            group_path: self.group_path,
            workdir: self.workdir,
            prompt: self.prompt,
            backend: routing.backend,
            model: profile.model.clone(),
            effort: profile.effort.clone(),
            allow_edits: routing.allow_edits,
            clean_start: routing.backend == Backend::Claude && routing.clean_start,
            timeout_seconds: routing.timeout_seconds,
            retry_timeout_seconds: routing.retry_timeout_seconds,
            agy_proxy: if routing.backend == Backend::Agy { routing.agy_proxy.clone() } else { String::new() },
            resume_session: None,
        }
    }
}

/// Harness 单任务输入；工作目录必须为绝对路径，ID 不允许目录穿越。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub task_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub group_path: Vec<String>,
    pub backend: Backend,
    pub workdir: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(skip)]
    pub resume_session: Option<String>,
    pub allow_edits: bool,
    /// 新任务保存启动模式；旧记录缺少该字段时保留原来的非纯净会话行为。
    #[serde(default)]
    pub clean_start: bool,
    pub timeout_seconds: u64,
    #[serde(default = "default_retry_timeout_seconds")]
    pub retry_timeout_seconds: u64,
    /// Agy 后端代理地址快照，仅 Agy 任务时有值。
    #[serde(default)]
    pub agy_proxy: String,
}

impl Task {
    /// 校验外部任务输入，拒绝空任务、非法 ID 和无效工作目录。
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.task_id.is_empty()
                && self.task_id.len() <= 80
                && self
                    .task_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "task_id must contain 1..80 ASCII letters, digits, '-' or '_'"
        );
        ensure!(
            self.workdir.is_absolute() && self.workdir.is_dir(),
            "workdir must be an existing absolute directory"
        );
        ensure!(!self.prompt.trim().is_empty(), "prompt is empty");
        ensure!(self.timeout_seconds > 0, "timeout_seconds must be positive");
        ensure!(self.retry_timeout_seconds > 0, "retry_timeout_seconds must be positive");
        ensure!(
            self.group_path.len() <= 3 && self.group_path.iter().all(|s| !s.trim().is_empty()),
            "group_path must contain zero to three non-empty names"
        );
        Ok(())
    }
    /// 显示名称不参与身份匹配，任务 ID 始终为唯一键。
    pub fn label(&self) -> &str {
        self.title
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.task_id)
    }
    /// 返回配置索引名称，不把模型名字误认为 CLI 类型。
    pub fn profile_key(&self) -> &'static str {
        self.backend.key()
    }
}

/// 协议累计状态；原始事件另外逐行落盘，不依赖界面保留全部输出。
#[derive(Debug, Default, Serialize)]
pub struct Outcome {
    pub error: Option<String>,
    pub message_id: Option<String>,
    pub session_id: Option<String>,
    pub cli_model: Option<String>,
    pub reported_model: Option<String>,
    pub completed: bool,
    pub failed: bool,
    pub answer: String,
    pub tool_calls: usize,
    pub usage: Value,
    pub metrics: crate::metrics::Metrics,
}

impl Outcome {
    /// 解析已知事件；未知事件保留于原始日志，绝不伪造成功。
    pub fn observe(&mut self, backend: Backend, value: &Value) -> Vec<String> {
        if backend == Backend::Opencode {
            return crate::opencode::observe(self, value);
        }
        let mut lines = Vec::new();
        let kind = value["type"].as_str().unwrap_or("");
        if backend != Backend::Codex {
            if let Some(id) = value["session_id"].as_str() {
                self.session_id = Some(id.to_owned());
            }
            if kind == "system" && value["subtype"] == "init" {
                self.cli_model = value["model"].as_str().map(str::to_owned);
                lines.push(format!(
                    "会话已建立 · {}",
                    self.cli_model.as_deref().unwrap_or("CLI 未报告模型")
                ));
            }
            if matches!(kind, "assistant" | "stream_event") {
                let model = if kind == "assistant" {
                    &value["message"]["model"]
                } else {
                    &value["event"]["message"]["model"]
                };
                if let Some(model) = model.as_str().filter(|m| crate::metrics::explicit_model(m)) {
                    self.reported_model = Some(model.to_owned());
                }
            }
            if kind == "assistant" {
                if let Some(blocks) = value["message"]["content"].as_array() {
                    for block in blocks {
                        match block["type"].as_str() {
                            Some("text") => {
                                lines.push(block["text"].as_str().unwrap_or("").to_owned())
                            }
                            Some("tool_use") => {
                                self.tool_calls += 1;
                                lines.push(format!(
                                    "工具调用 · {} {}",
                                    block["name"].as_str().unwrap_or(""),
                                    block["input"]
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
            if kind == "user" {
                lines.push("工具结果已收到".to_owned());
            }
            if kind == "result" {
                self.completed = true;
                self.failed |= value["is_error"] == true
                    || value["subtype"] != "success"
                    || value["permission_denials"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty());
                self.answer = value["result"].as_str().unwrap_or("").to_owned();
                self.usage = value["usage"].clone();
                lines.push(format!("最终结果 · {}", self.answer));
            }
        } else {
            match kind {
                "thread.started" => {
                    self.session_id = value["thread_id"].as_str().map(str::to_owned);
                    lines.push("会话已建立".to_owned());
                }
                "item.started" | "item.completed" => {
                    let item = &value["item"];
                    match item["type"].as_str() {
                        Some("agent_message") if kind == "item.completed" => {
                            self.answer = item["text"].as_str().unwrap_or("").to_owned();
                            lines.push(self.answer.clone());
                        }
                        Some("command_execution" | "mcp_tool_call" | "file_change")
                            if kind == "item.started" =>
                        {
                            self.tool_calls += 1;
                            lines.push(format!("工具调用 · {item}"));
                        }
                        Some("command_execution" | "mcp_tool_call" | "file_change")
                            if kind == "item.completed" =>
                        {
                            lines.push(format!("工具结果 · {item}"))
                        }
                        _ => {}
                    }
                }
                "turn.completed" => {
                    self.completed = true;
                    self.usage = value["usage"].clone();
                }
                "turn.failed" => {
                    self.completed = true;
                    self.failed = true;
                    lines.push(format!("执行失败 · {}", value["error"]));
                }
                "error" => lines.push(format!("CLI 提示 · {}", value["message"])),
                _ => {}
            }
        }
        lines
    }

    /// 必须同时收到成功结果和退出码 0，防止静默退出误判完成。
    pub fn succeeded(&self, code: Option<i32>) -> bool {
        code == Some(0) && self.completed && !self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    /// CLI 的初始化声明不能覆盖供应商实际响应的模型标识。
    #[test]
    fn response_model_is_distinct_from_cli_configuration() {
        let mut outcome = Outcome::default();
        outcome.observe(
            Backend::Claude,
            &json!({"type":"system","subtype":"init","model":"GLM-5.3"}),
        );
        assert_eq!(outcome.cli_model.as_deref(), Some("GLM-5.3"));
        assert!(outcome.reported_model.is_none());
        for model in ["glm-5.3", "deepseek-v4-pro", "<synthetic>"] {
            outcome.metrics.observe(Backend::Claude,
                &json!({"type":"assistant","message":{"model":model,"content":[]}}), 0);
            outcome.observe(Backend::Claude,
                &json!({"type":"assistant","message":{"model":model,"content":[]}}));
        }
        assert_eq!(outcome.reported_model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(outcome.metrics.reported_model.as_deref(), Some("deepseek-v4-pro"));
        outcome.observe(
            Backend::Claude,
            &json!({"type":"assistant","message":{"model":"auto","content":[]}}),
        );
        assert_eq!(outcome.reported_model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(outcome.cli_model.as_deref(), Some("GLM-5.3"));
    }
    /// 分组最多三级，空分组允许，超深和空名称必须拒绝。
    #[test]
    fn group_contract() {
        let mut task: Work = serde_json::from_value(
            json!({"task_id":"test","workdir":std::env::temp_dir(),"prompt":"test"}),
        )
        .unwrap();
        assert!(task.validate().is_ok());
        task.group_path = vec!["项目".into(), "需求".into(), "执行".into()];
        assert!(task.validate().is_ok());
        task.group_path.push("第四级".into());
        assert!(task.validate().is_err());
        task.group_path = vec![" ".into()];
        assert!(task.validate().is_err());
    }
    /// Harness 不能指定执行参数，配置快照必须完全来自 App。
    #[test]
    fn work_rejects_routing_fields() {
        let value = json!({"task_id":"w","workdir":std::env::temp_dir(),"prompt":"do work"});
        let work: Work = serde_json::from_value(value.clone()).unwrap();
        for key in [
            "backend",
            "model",
            "effort",
            "allow_edits",
            "clean_start",
            "timeout_seconds",
            "retry_timeout_seconds",
        ] {
            let mut bad = value.clone();
            bad[key] = json!("override");
            assert!(serde_json::from_value::<Work>(bad).is_err());
        }
        let task = work.resolve(
            &Routing {
                backend: Backend::Claude,
                allow_edits: true,
                clean_start: true,
                timeout_seconds: 300,
                retry_timeout_seconds: 60,
            },
            &Profile {
                program: PathBuf::new(),
                model: Some("GLM-5.2".into()),
                effort: Some("high".into()),
            },
        );
        assert_eq!(task.backend, Backend::Claude);
        assert_eq!(task.model.as_deref(), Some("GLM-5.2"));
        assert_eq!(task.effort.as_deref(), Some("high"));
        assert!(task.allow_edits);
        assert!(task.clean_start);
    }
    /// 设置只影响新任务；持久化显式保留模式，旧会话不能被误标为纯净。
    #[test]
    fn clean_start_snapshot_and_legacy_task() {
        let work: Work = serde_json::from_value(json!({
            "task_id":"clean", "workdir":std::env::temp_dir(), "prompt":"test"
        })).unwrap();
        let profile = Profile { program: "claude".into(), model: None, effort: None };
        let mut routing = Routing::default();
        let task = work.clone().resolve(&routing, &profile);
        routing.clean_start = false;
        assert!(task.clean_start);
        assert!(!work.resolve(&routing, &profile).clean_start);
        let mut saved = serde_json::to_value(&task).unwrap();
        assert!(serde_json::from_value::<Task>(saved.clone()).unwrap().clean_start);
        saved.as_object_mut().unwrap().remove("clean_start");
        assert!(!serde_json::from_value::<Task>(saved).unwrap().clean_start);
    }
    /// 覆盖零退出码缺少协议结果、权限拒绝和正常 Claude 完成。
    #[test]
    fn claude_completion_is_not_exit_code_alone() {
        let mut state = Outcome::default();
        assert!(!state.succeeded(Some(0)));
        state.observe(
            Backend::Claude,
            &json!({"type":"result","subtype":"success","result":"42","is_error":false}),
        );
        assert!(state.succeeded(Some(0)));
        state.observe(
            Backend::Claude,
            &json!({"type":"result","subtype":"success","permission_denials":[{"tool":"Edit"}]}),
        );
        assert!(!state.succeeded(Some(0)));
    }
    /// Codex 消息本身不能完成任务，turn.completed 才能完成。
    #[test]
    fn codex_requires_completed_turn() {
        let mut state = Outcome::default();
        state.observe(
            Backend::Codex,
            &json!({"type":"item.completed","item":{"type":"agent_message","text":"42"}}),
        );
        assert!(!state.succeeded(Some(0)));
        state.observe(
            Backend::Codex,
            &json!({"type":"turn.completed","usage":{"input_tokens":3}}),
        );
        assert!(state.succeeded(Some(0)));
        assert!(!state.succeeded(Some(1)));
    }
}
