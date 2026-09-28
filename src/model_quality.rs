//! 所选供应商的 API 指纹检测；不创建 CLI 对话，凭据只经私有 stdin 传给 Node。
use crate::protocol::Backend;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{path::Path, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, time::{Duration, Instant}};

/// UI 更新只包含经过筛选的检测信息；finished 后允许再次启动。
pub struct Update {
    pub text: String,
    pub finished: bool,
}

/// 每次检测持有独立取消标记；关闭应用或释放状态会回收专属子进程。
pub struct Run {
    pub updates: mpsc::Receiver<Update>,
    pub stop: Arc<AtomicBool>,
}

impl Drop for Run {
    /// UI 不等待工作线程；工作线程在短轮询内取消进程树。
    fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); }
}

/// 固定所选 CLI、模型和基准文件快照，后台读取供应商配置并运行一次 quick 检测。
pub fn start(backend: Backend, model: String, reference: String) -> Run {
    let (tx, updates) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    std::thread::spawn(move || {
        if let Err(error) = execute(backend, &model, &reference, &worker_stop, &tx) {
            let _ = tx.send(Update { text: format!("无法检测：{error}"), finished: true });
        }
    });
    Run { updates, stop }
}

/// 读取外部 JSON，错误只描述文件用途，不把配置原文或凭据带入错误信息。
fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).context("无法读取供应商配置文件")?;
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| anyhow::anyhow!("供应商配置不是有效 JSON"))
}

/// 与模型列表同源读取 CC Switch 当前项；未配置时使用 CLI 的生效文件。
fn settings(home: &Path, backend: Backend) -> Result<Value> {
    let database = home.join(".cc-switch/cc-switch.db");
    if database.is_file() {
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("无法只读打开 CC Switch")?;
        let mut statement = connection.prepare(
            "SELECT settings_config FROM providers WHERE app_type = ?1 AND is_current = 1")?;
        let rows = statement.query_map([backend.key()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(rows.len() <= 1, "CC Switch 存在多个当前供应商");
        if let Some(text) = rows.first() {
            return serde_json::from_str(text).map_err(|_| anyhow::anyhow!("供应商配置不是有效 JSON"));
        }
    }
    match backend {
        Backend::Claude => read_json(&home.join(".claude/settings.json")),
        Backend::Codex => {
            let root = std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            let config = std::fs::read_to_string(root.join("config.toml"))
                .context("无法读取 Codex 配置")?;
            let auth = if root.join("auth.json").is_file() { read_json(&root.join("auth.json"))? }
                else { Value::Null };
            Ok(json!({"config":config,"auth":auth}))
        }
        _ => bail!("当前 CLI 没有可供检测器使用的 API 配置"),
    }
}

/// 必填外部字段仅接受非空字符串，避免把空配置作为官方地址或匿名认证。
fn required<'a>(value: &'a Value, key: &str, message: &str) -> Result<&'a str> {
    value[key].as_str().filter(|s| !s.trim().is_empty()).context(message.to_owned())
}

/// 组装唯一目的地址；不尝试其它供应商，不输出密钥，不把网页订阅令牌当 API Key。
fn endpoint_from_settings(backend: Backend, model: &str, settings: &Value) -> Result<Value> {
    match backend {
        Backend::Claude => {
            let env = &settings["env"];
            let base = required(env, "ANTHROPIC_BASE_URL", "Claude 未配置兼容 API 地址")?;
            let key = env["ANTHROPIC_AUTH_TOKEN"].as_str().filter(|s| !s.is_empty())
                .or_else(|| env["ANTHROPIC_API_KEY"].as_str().filter(|s| !s.is_empty()))
                .context("Claude 当前供应商没有 API Key")?;
            Ok(json!({"baseUrl":base,"apiKey":key,"model":model}))
        }
        Backend::Codex => {
            let text = required(settings, "config", "Codex 缺少供应商配置")?;
            let config: toml::Value = toml::from_str(text)
                .map_err(|_| anyhow::anyhow!("Codex 配置不是有效 TOML"))?;
            let provider_id = config.get("model_provider").and_then(toml::Value::as_str);
            let provider = provider_id.and_then(|id| config.get("model_providers")?.get(id));
            let base = match provider {
                Some(p) => p.get("base_url").and_then(toml::Value::as_str)
                    .context("当前 Codex 供应商缺少 base_url")?,
                None if provider_id.is_none() || provider_id == Some("openai") => "https://api.openai.com/v1",
                None => bail!("找不到 Codex 当前供应商定义"),
            };
            let key = if let Some(name) = provider.and_then(|p| p.get("env_key")).and_then(toml::Value::as_str) {
                std::env::var(name).map_err(|_| anyhow::anyhow!("Codex 供应商指定的密钥环境变量未设置"))?
            } else {
                required(&settings["auth"], "OPENAI_API_KEY", "Codex 当前配置没有 API Key；订阅登录不能用于此检测")?.to_owned()
            };
            ensure!(!key.trim().is_empty(), "Codex API Key 为空");
            Ok(json!({"baseUrl":base,"apiKey":key,"model":model}))
        }
        Backend::Opencode => {
            let (provider, api_model) = model.split_once('/').context("OpenCode 模型缺少供应商前缀")?;
            let definition = &settings["provider"][provider];
            let options = &definition["options"];
            let base = required(options, "baseURL", "所选 OpenCode 供应商没有 baseURL")?;
            let key = required(options, "apiKey", "所选 OpenCode 供应商没有 API Key；OAuth 登录不可用于此检测")?;
            let key = if let Some(name) = key.strip_prefix("{env:").and_then(|s| s.strip_suffix('}')) {
                std::env::var(name).map_err(|_| anyhow::anyhow!("OpenCode 密钥环境变量未设置"))?
            } else {
                ensure!(!key.starts_with('{'), "此 OpenCode 密钥引用方式不支持直接检测");
                key.to_owned()
            };
            let api_model = definition["models"][api_model]["id"].as_str().unwrap_or(api_model);
            Ok(json!({"baseUrl":base,"apiKey":key,"model":api_model}))
        }
        Backend::Agy => bail!("Antigravity 使用 CLI 登录，没有 OpenAI 兼容 API 配置"),
    }
}

/// 从明确对应的供应商读取地址和凭据；OpenCode 必须按模型前缀选 provider。
fn endpoint(backend: Backend, model: &str) -> Result<Value> {
    let home = std::env::var_os("USERPROFILE").context("缺少 USERPROFILE")?;
    let home = Path::new(&home);
    let settings = if backend == Backend::Opencode {
        let path = std::env::var_os("OPENCODE_CONFIG").map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".config/opencode/opencode.json"));
        read_json(&path)?
    } else { settings(home, backend)? };
    endpoint_from_settings(backend, model, &settings)
}

/// Node 通过 stdin 接收私有配置；取消和总超时均终止进程树，stderr 不回显。
fn execute(backend: Backend, model: &str, reference: &str, stop: &AtomicBool, tx: &mpsc::Sender<Update>) -> Result<()> {
    ensure!(!model.trim().is_empty(), "请先选择具体模型，CLI 默认无法确定检测对象");
    let endpoint = endpoint(backend, model);
    let input = match endpoint {
        Ok(endpoint) => json!({"model":model,"reference":reference,"endpoint":endpoint}),
        Err(error) => json!({"model":model,"reference":reference,"endpointError":error.to_string()}),
    };
    run_detector(&input, stop, tx)
}

/// 独立的进程传输入口，允许离线验证 Rust → Node 的参数、管道和结束事件。
fn run_detector(input: &Value, stop: &AtomicBool, tx: &mpsc::Sender<Update>) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "已取消");
    let (process, events) = cli_stream::Command::new("node")
        .args(["--input-type=module", "-e", include_str!("../assets/model-quality.mjs"), "--", "--ar-model-quality"])
        .stdin(cli_stream::Stdin::Piped).stderr(cli_stream::Stderr::Discarded)
        .timeout(Duration::from_secs(195)).start().context("无法启动 Node.js，请检查安装和 PATH")?;
    let result = (|| {
        process.write_line(&input.to_string())?;
        process.close_stdin()?;
        let deadline = Instant::now() + Duration::from_secs(190);
        let mut terminal = None;
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "已取消");
            ensure!(Instant::now() < deadline, "检测超时");
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(cli_stream::Event::Stdout { line, .. }) => {
                    let event: Value = serde_json::from_str(&line).context("检测器返回无效事件")?;
                    match event["type"].as_str() {
                        Some("progress") => { let _ = tx.send(Update {
                            text: if event["stage"] == "adapter" { "正在探测采样参数…".into() }
                                else { format!("采样中：{} / {}", event["done"], event["total"]) }, finished: false }); },
                        Some("error") => terminal = Some(format!("无法检测：{}", event["message"].as_str().context("错误事件缺少信息")?)),
                        Some("result") => terminal = Some(report(&event)?),
                        _ => bail!("检测器返回未知事件"),
                    }
                }
                Ok(cli_stream::Event::Exited { exit_code, .. }) => {
                    ensure!(matches!(exit_code, Some(0 | 1)), "检测进程异常退出");
                    let text = terminal.context("检测进程没有返回结果")?;
                    let _ = tx.send(Update { text, finished: true });
                    return Ok(());
                }
                Ok(cli_stream::Event::Error { .. }) => bail!("检测进程执行失败"),
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("检测进程连接中断"),
                _ => {}
            }
        }
    })();
    let _ = process.cancel();
    result
}

/// 展示检测证据，保留样本不足和协议差异；供应商自报身份本身不能判通过。
fn report(event: &Value) -> Result<String> {
    let verdict = required(event, "verdict", "检测结果缺少 verdict")?;
    let passed = event["passed"].as_bool().context("检测结果缺少通过状态")?;
    let conclusion = if passed { "通过：指纹匹配，API 回报模型一致" } else {
        match verdict { "match" => "未通过：指纹匹配，但 API 回报模型缺失或不一致",
            "mismatch" => "未通过：模型指纹不匹配", "uncertain" => "不确定：JSD 处于灰度区",
            "insufficient" => "无法判定：有效样本不足", _ => bail!("未知检测结论") }
    };
    let reported = event["reported"].as_array().context("检测结果缺少回报模型")?
        .iter().filter_map(Value::as_str).collect::<Vec<_>>().join("、");
    let jsd = event["meanJsd"].as_f64().map(|v| format!("{v:.3}")).unwrap_or_else(|| "—".into());
    let mut text = format!("{conclusion}\n所选模型：{}\nAPI 回报：{}\n基准模型：{}\nJSD：{jsd} · 可比维度：{} · 失败请求：{}",
        required(event, "model", "缺少所选模型")?, if reported.is_empty() { "未提供" } else { &reported },
        required(event, "reference", "缺少基准模型")?, event["comparableCells"], event["errors"]);
    if event["protocolMismatch"] == true { text.push_str("\n提示：内置基准与本次采样协议不同，结论仅供初筛。"); }
    if event["postReasoning"] == true { text.push_str("\n提示：无法关闭推理，指纹置信度降低。"); }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude 检测固定所选模型和当前供应商；不会输出或换用其它供应商凭据。
    #[test]
    fn claude_endpoint_uses_selected_model() {
        let endpoint = endpoint_from_settings(Backend::Claude, "claude-sonnet-4.5", &json!({"env": {
            "ANTHROPIC_BASE_URL":"https://example.invalid/v1", "ANTHROPIC_AUTH_TOKEN":"test-key"}})).unwrap();
        assert_eq!(endpoint["model"], "claude-sonnet-4.5");
        assert_eq!(endpoint["baseUrl"], "https://example.invalid/v1");
    }

    /// 不提取订阅登录 token；无 API Key 时明确失败。
    #[test]
    fn codex_subscription_is_not_an_api_key() {
        let error = endpoint_from_settings(Backend::Codex, "gpt-4o", &json!({
            "config":"model = 'gpt-4o'", "auth":{"tokens":{"access_token":"private-test"}}})).unwrap_err();
        assert!(error.to_string().contains("订阅登录"));
        assert!(!error.to_string().contains("private-test"));
    }

    /// TOML 解析供应商而非文本猜测，损坏配置不得泄露原文。
    #[test]
    fn codex_provider_selection_and_redaction() {
        let settings = json!({"config":"model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://example.invalid/v1'",
            "auth":{"OPENAI_API_KEY":"private-test"}});
        let endpoint = endpoint_from_settings(Backend::Codex, "gpt-4o", &settings).unwrap();
        assert_eq!(endpoint["baseUrl"], "https://example.invalid/v1");
        let error = endpoint_from_settings(Backend::Codex, "gpt-4o", &json!({"config":"private-test=["})).unwrap_err();
        assert!(!error.to_string().contains("private-test"));
    }

    /// OpenCode 按所选 provider 取认证并解析 API 模型 ID，不误用另一个 provider。
    #[test]
    fn opencode_selects_provider_and_model_mapping() {
        let endpoint = endpoint_from_settings(Backend::Opencode, "custom/alias", &json!({"provider":{
            "custom":{"options":{"baseURL":"https://example.invalid/v1","apiKey":"test-key"},
                "models":{"alias":{"id":"gpt-4o"}}}}})).unwrap();
        assert_eq!(endpoint["model"], "gpt-4o");
        assert!(endpoint_from_settings(Backend::Opencode, "other/alias", &json!({})).is_err());
    }

    /// 使用本机 Node 和检测器验证私有 stdin 及 JSONL，不读取用户配置或发送 API 请求。
    #[test]
    #[ignore = "requires Node.js and npm-linked llm-fingerprint-detector"]
    fn node_bridge_roundtrip_without_api() {
        let (tx, rx) = mpsc::channel();
        run_detector(&json!({"model":"gpt-4o","reference":"","endpointError":"fixture-no-api"}),
            &AtomicBool::new(false), &tx).unwrap();
        let update = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(update.finished);
        assert_eq!(update.text, "无法检测：fixture-no-api");
    }
}
