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

/// 固定所选 CLI 和模型快照，使用内置 ModelTrace 基准完成三次独立挑战。
pub fn start(backend: Backend, model: String) -> Run {
    let (tx, updates) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    std::thread::spawn(move || {
        if let Err(error) = execute(backend, &model, &worker_stop, &tx) {
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
            let base = env["ANTHROPIC_BASE_URL"].as_str().unwrap_or("https://api.anthropic.com/v1");
            let key = env["ANTHROPIC_AUTH_TOKEN"].as_str().filter(|s| !s.is_empty())
                .or_else(|| env["ANTHROPIC_API_KEY"].as_str().filter(|s| !s.is_empty()))
                .context("Claude 当前供应商没有 API Key")?;
            let auth_type = if env["ANTHROPIC_AUTH_TOKEN"].as_str().is_some_and(|s| !s.is_empty()) {
                "bearer"
            } else { "api_key" };
            Ok(json!({"baseUrl":base,"apiKey":key,"model":model,"protocol":"anthropic","authType":auth_type}))
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
            let protocol = provider.and_then(|p| p.get("wire_api")).and_then(toml::Value::as_str).unwrap_or("responses");
            ensure!(matches!(protocol, "responses" | "chat"), "不支持当前 Codex wire_api 协议");
            Ok(json!({"baseUrl":base,"apiKey":key,"model":model,"protocol":protocol}))
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
            let protocol = match definition["npm"].as_str() {
                Some("@ai-sdk/anthropic") => "anthropic",
                Some("@ai-sdk/openai") => "responses",
                Some("@ai-sdk/openai-compatible") => "chat",
                _ => bail!("所选 OpenCode 供应商未声明受支持的 API 协议"),
            };
            Ok(json!({"baseUrl":base,"apiKey":key,"model":api_model,"protocol":protocol,"authType":"api_key"}))
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
fn execute(backend: Backend, model: &str, stop: &AtomicBool, tx: &mpsc::Sender<Update>) -> Result<()> {
    ensure!(!model.trim().is_empty(), "请先选择具体模型，CLI 默认无法确定检测对象");
    let endpoint = endpoint(backend, model);
    let input = match endpoint {
        Ok(endpoint) => json!({"model":model,"endpoint":endpoint}),
        Err(error) => json!({"model":model,"endpointError":error.to_string()}),
    };
    run_detector(&input, stop, tx)
}

/// 独立的进程传输入口，允许离线验证 Rust → Node 的参数、管道和结束事件。
fn run_detector(input: &Value, stop: &AtomicBool, tx: &mpsc::Sender<Update>) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "已取消");
    let script = [include_str!("../vendor/modeltrace/fingerprint-core.mjs"),
        include_str!("../vendor/modeltrace/challenge-browser.mjs"),
        include_str!("../assets/model-quality.mjs")].join("\n");
    let mut input = input.clone();
    input["bank"] = serde_json::from_str(include_str!("../vendor/modeltrace/unified_bank.json"))
        .context("内置 ModelTrace 基准无效")?;
    let (process, events) = cli_stream::Command::new("node")
        .args(["--input-type=module", "-e", script.as_str(), "--", "--ar-model-quality"])
        .stdin(cli_stream::Stdin::Piped).stderr(cli_stream::Stderr::Discarded)
        .timeout(Duration::from_secs(375)).start().context("无法启动 Node.js，请检查安装和 PATH")?;
    let result = (|| {
        process.write_line(&input.to_string())?;
        process.close_stdin()?;
        let deadline = Instant::now() + Duration::from_secs(370);
        let mut terminal = None;
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "已取消");
            ensure!(Instant::now() < deadline, "检测超时");
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(cli_stream::Event::Stdout { line, .. }) => {
                    let event: Value = serde_json::from_str(&line).context("检测器返回无效事件")?;
                    match event["type"].as_str() {
                        Some("progress") => { let _ = tx.send(Update {
                            text: format!("数字挑战：已完成 {} / {}", event["done"], event["total"]), finished: false }); },
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

/// 展示唯一的 ModelTrace 结论；库内归因概率不等同于模型身份的真实性概率。
fn report(event: &Value) -> Result<String> {
    let verdict = required(event, "verdict", "检测结果缺少 verdict")?;
    let conclusion = match verdict {
        "match" => "通过：识别结果与所选模型、API 回报一致",
        "mismatch" => "未通过：识别模型与 API 回报不一致",
        "reported_mismatch" => "未通过：API 回报与所选模型不一致",
        "missing_model" => "无法判定：API 未完整回报模型名称",
        "insufficient" => "无法判定：未取得三份有效回答",
        _ => bail!("未知检测结论"),
    };
    let reported = event["reported"].as_array().context("检测结果缺少回报模型")?
        .iter().filter_map(Value::as_str).collect::<Vec<_>>().join("、");
    let probability = event["probability"].as_f64().map(|v| format!("{:.1}%", v * 100.)).unwrap_or_else(|| "—".into());
    let prediction = event["prediction"].as_str().filter(|s| !s.is_empty()).unwrap_or("—");
    let mut text = format!("{conclusion}\n所选模型：{}\nAPI 请求模型：{}\nAPI 回报：{}\n识别模型：{prediction}\n库内归因概率：{probability} · 有效回答：{} / 3\n基准版本：{}",
        required(event, "model", "缺少所选模型")?, required(event, "apiModel", "缺少 API 请求模型")?,
        if reported.is_empty() { "未提供" } else { &reported }, event["usedOutputs"],
        required(event, "bankDate", "缺少基准版本")?);
    for candidate in event["candidates"].as_array().context("缺少候选结果")? {
        text.push_str(&format!("\n  {}：{:.1}%", required(candidate, "model", "候选缺少模型")?,
            candidate["probability"].as_f64().context("候选缺少概率")? * 100.));
    }
    text.push_str("\n结果为现有基准库内的行为归因，仅供参考。");
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
        assert_eq!(endpoint["protocol"], "anthropic");
        assert_eq!(endpoint["authType"], "bearer");
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
        assert_eq!(endpoint["protocol"], "responses");
        let error = endpoint_from_settings(Backend::Codex, "gpt-4o", &json!({"config":"private-test=["})).unwrap_err();
        assert!(!error.to_string().contains("private-test"));
    }

    /// OpenCode 按所选 provider 取认证并解析 API 模型 ID，不误用另一个 provider。
    #[test]
    fn opencode_selects_provider_and_model_mapping() {
        let endpoint = endpoint_from_settings(Backend::Opencode, "custom/alias", &json!({"provider":{
            "custom":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"https://example.invalid/v1","apiKey":"test-key"},
                "models":{"alias":{"id":"gpt-4o"}}}}})).unwrap();
        assert_eq!(endpoint["model"], "gpt-4o");
        assert_eq!(endpoint["protocol"], "chat");
        assert!(endpoint_from_settings(Backend::Opencode, "other/alias", &json!({})).is_err());
    }

    /// 使用本机 Node 和检测器验证私有 stdin 及 JSONL，不读取用户配置或发送 API 请求。
    #[test]
    #[ignore = "requires Node.js"]
    fn node_bridge_roundtrip_without_api() {
        let (tx, rx) = mpsc::channel();
        run_detector(&json!({"model":"gpt-6-astra","endpointError":"fixture-no-api"}),
            &AtomicBool::new(false), &tx).unwrap();
        let update = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(update.finished);
        assert_eq!(update.text, "无法检测：fixture-no-api");
    }

    /// 本地 API 保持请求未返回时取消，验证 Rust 会及时回收正在等待网络的 Node 进程。
    #[test]
    #[ignore = "requires Node.js"]
    fn node_cancellation_reclaims_waiting_process() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let input = json!({"model":"gpt-6-astra","endpoint":{
            "model":"gpt-6-astra", "protocol":"chat", "apiKey":"fixture-key",
            "baseUrl":format!("http://{}/v1", server.server_addr())}});
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (tx, _rx) = mpsc::channel();
            let result = run_detector(&input, &worker_stop, &tx);
            let _ = done_tx.send(result);
        });
        let request = server.recv_timeout(Duration::from_secs(30)).unwrap().expect("Node 未发出挑战请求");
        stop.store(true, Ordering::Relaxed);
        let error = done_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap_err();
        assert_eq!(error.to_string(), "已取消");
        drop(request);
        worker.join().unwrap();
    }

    /// UI 使用同一结论枚举，样本不足时不会显示通过或保留旧 JSD 字段。
    #[test]
    fn result_text_distinguishes_incomplete_and_match() {
        let mut event = json!({"verdict":"insufficient","model":"gpt-6-astra","apiModel":"gpt-6-astra",
            "reported":["gpt-6-astra"],"prediction":"","probability":null,
            "usedOutputs":2,"bankDate":"2026-09-23","candidates":[]});
        let text = report(&event).unwrap();
        assert!(text.starts_with("无法判定"));
        assert!(!text.contains("JSD"));
        event["verdict"] = json!("match");
        event["prediction"] = json!("gpt-6-astra");
        event["probability"] = json!(0.9);
        event["usedOutputs"] = json!(3);
        assert!(report(&event).unwrap().starts_with("通过"));
    }
}
