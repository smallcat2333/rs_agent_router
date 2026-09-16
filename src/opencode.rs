//! OpenCode 原生 CLI 适配：模型与 variant 目录、事件归一和指定消息的执行身份。
use anyhow::{Context, Result, ensure};
use cli_stream::{Command, Event, Stdin};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::{Duration, Instant}};

/// 模型目录携带 CLI 原生 variant，不能套用其他 CLI 的强度表。
#[derive(Default)]
pub struct Catalog {
    pub models: Vec<String>,
    pub variants: BTreeMap<String, Vec<String>>,
}

/// 只读辅助命令有固定短期限，始终回收进程，不把原始导出内容写入 UI。
fn output(program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program).args(args.iter().copied()).stdin(Stdin::Piped);
    if let Some(cwd) = cwd { command = command.cwd(cwd); }
    let (process, events) = command.start()?;
    let result = (|| {
        process.close_stdin()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stdout = String::new();
        loop {
            let remaining = deadline.checked_duration_since(Instant::now()).context("OpenCode 查询超时（20 秒）")?;
            match events.recv_timeout(remaining).context("OpenCode 查询超时或连接关闭")? {
                Event::Stdout { line, .. } => { stdout.push_str(&line); stdout.push('\n'); }
                Event::Exited { exit_code, .. } => { ensure!(exit_code == Some(0), "OpenCode 查询失败，退出码 {exit_code:?}"); return Ok(stdout); }
                Event::Error { message, .. } => anyhow::bail!("OpenCode 进程错误：{message}"),
                _ => {}
            }
        }
    })();
    let _ = process.cancel();
    result
}

/// 查询 CLI 生效模型目录，认证和供应商由用户的 OpenCode / CC Switch 配置决定。
pub fn catalog(program: &Path) -> Result<Catalog> {
    parse_catalog(&output(program, &["models", "--verbose"], None)?)
}

/// CLI 以 provider/model 单行加独立 JSON 对象输出，逐项解析以保留原生档位。
fn parse_catalog(text: &str) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    let mut remaining = text.trim();
    while !remaining.is_empty() {
        let (name, rest) = remaining.split_once('\n').context("OpenCode 模型目录缺少元数据")?;
        let name = name.trim();
        ensure!(name.contains('/'), "OpenCode 模型标识缺少 provider");
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        let info = stream.next().context("OpenCode 模型元数据为空")??;
        let variants = info["variants"].as_object().context("OpenCode 模型缺少 variants")?;
        catalog.models.push(name.to_owned());
        catalog.variants.insert(name.to_owned(), variants.keys().cloned().collect());
        remaining = rest[stream.byte_offset()..].trim();
    }
    ensure!(!catalog.models.is_empty(), "OpenCode 没有可用模型");
    Ok(catalog)
}

/// 以环境级权限覆盖实现只读/可编辑，不使用无条件自动批准，也不修改用户配置。
pub fn environment(allow_edits: bool) -> Vec<(String, String)> {
    let access = if allow_edits { "allow" } else { "deny" };
    vec![
        ("OPENCODE_PERMISSION".into(), json!({"*":"deny","read":"allow","glob":"allow","grep":"allow","edit":access,"bash":access}).to_string()),
        ("OPENCODE_AUTO_SHARE".into(), "false".into()),
    ]
}

/// 保留 sessionID 和当前消息 ID；工具步骤完成不等于整轮成功，错误事件优先。
pub fn observe(outcome: &mut crate::protocol::Outcome, value: &Value) -> Vec<String> {
    if let Some(session) = value["sessionID"].as_str() { outcome.session_id = Some(session.to_owned()); }
    let part = &value["part"];
    if let Some(message) = part["messageID"].as_str() { outcome.message_id = Some(message.to_owned()); }
    match value["type"].as_str() {
        Some("step_start") => { outcome.completed = false; outcome.answer.clear(); Vec::new() }
        Some("text") => {
            let text = part["text"].as_str().unwrap_or("");
            if !outcome.answer.is_empty() { outcome.answer.push('\n'); }
            outcome.answer.push_str(text);
            vec![text.to_owned()]
        }
        Some("tool_use") => {
            outcome.tool_calls += 1;
            vec![format!("工具调用 · {} {}", part["tool"].as_str().unwrap_or(""), part["state"]["input"]),
                 format!("工具结果 · {}", if part["state"]["status"] == "error" { &part["state"]["error"] } else { &part["state"]["output"] })]
        }
        Some("step_finish") => {
            outcome.completed = part["reason"] == "stop";
            outcome.usage = part["tokens"].clone();
            Vec::new()
        }
        Some("error") => {
            outcome.failed = true;
            let message = value["error"]["data"]["message"].as_str()
                .or_else(|| value["error"]["name"].as_str()).unwrap_or("OpenCode 请求失败");
            outcome.error = Some(message.to_owned());
            vec![format!("执行失败 · {message}")]
        }
        _ => Vec::new(),
    }
}

/// 只读取本轮已观测消息的原生身份，禁止误用续聊历史的最后一条旧消息。
pub fn identity(program: &Path, cwd: &Path, session: &str, message: &str) -> Result<String> {
    let exported: Value = serde_json::from_str(&output(program, &["export", session], Some(cwd))?)?;
    identity_from_export(&exported, message)
}

/// 按消息 ID 关联导出，旧轮次或其他并行消息不能替代本轮身份。
fn identity_from_export(exported: &Value, message: &str) -> Result<String> {
    let info = exported["messages"].as_array().context("OpenCode 导出缺少 messages")?
        .iter().map(|entry| &entry["info"])
        .find(|info| info["id"] == message && info["role"] == "assistant")
        .context("OpenCode 导出缺少本轮已观测消息")?;
    let provider = info["providerID"].as_str().context("OpenCode 消息缺少 providerID")?;
    let model = info["modelID"].as_str().context("OpenCode 消息缺少 modelID")?;
    Ok(format!("{provider}/{model}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// provider 保留且档位来自元数据；只读配置拒绝写入和命令执行。
    #[test]
    fn catalog_and_permissions() {
        let result = parse_catalog("go/a\n{\"variants\":{\"high\":{},\"low\":{}}}\ngo/b\n{\"variants\":{}}").unwrap();
        assert_eq!(result.models, ["go/a", "go/b"]);
        assert_eq!(result.variants["go/a"], ["high", "low"]);
        let permission: Value = serde_json::from_str(&environment(false)[0].1).unwrap();
        assert_eq!(permission["edit"], "deny");
        assert_eq!(permission["bash"], "deny");
        assert_eq!(permission["read"], "allow");
    }
    /// 工具步骤与空退出不能冒充完成，最终 stop 才成功，错误始终压过成功标记。
    #[test]
    fn native_event_completion() {
        let mut outcome = crate::protocol::Outcome::default();
        observe(&mut outcome, &json!({"type":"step_finish","sessionID":"ses_test","part":{"messageID":"msg_1","reason":"tool-calls"}}));
        assert!(!outcome.succeeded(Some(0)));
        observe(&mut outcome, &json!({"type":"text","part":{"messageID":"msg_1","text":"working"}}));
        observe(&mut outcome, &json!({"type":"step_start","part":{"messageID":"msg_2"}}));
        observe(&mut outcome, &json!({"type":"text","part":{"messageID":"msg_2","text":"ok"}}));
        observe(&mut outcome, &json!({"type":"step_finish","part":{"messageID":"msg_2","reason":"stop"}}));
        assert!(outcome.succeeded(Some(0)));
        assert_eq!(outcome.answer, "ok");
        assert_eq!(outcome.message_id.as_deref(), Some("msg_2"));
        observe(&mut outcome, &json!({"type":"error","error":{"name":"APIError"}}));
        assert!(!outcome.succeeded(Some(0)));
    }

    /// 验证续聊参数、当前消息身份与原生缓存用量，防止采用旧轮次模型或重复计 Token。
    #[test]
    fn resume_identity_and_usage() {
        use crate::protocol::{Backend, Profile};
        let mut task = crate::store::tests::sample("oc").task;
        task.backend = Backend::Opencode;
        task.model = Some("go/muse".into());
        task.effort = Some("high".into());
        task.resume_session = Some("ses_current".into());
        let args = crate::runner::arguments(&task, &Profile { program:"opencode".into(), model:None, effort:None });
        assert_eq!(&args[..3], ["run", "--format", "json"]);
        assert!(args.windows(2).any(|pair| pair == ["--variant", "high"]));
        assert!(args.windows(2).any(|pair| pair == ["--session", "ses_current"]));
        assert!(!args.iter().any(|arg| arg == "--auto" || arg == "--effort"));
        let exported = json!({"messages":[
            {"info":{"id":"old","role":"assistant","providerID":"go","modelID":"old"}},
            {"info":{"id":"current","role":"assistant","providerID":"go","modelID":"muse"}}
        ]});
        assert_eq!(identity_from_export(&exported,"current").unwrap(),"go/muse");
        assert!(identity_from_export(&exported,"missing").is_err());
        let mut metrics = crate::metrics::Metrics::default();
        metrics.observe(Backend::Opencode, &json!({"type":"step_finish","part":{"tokens":{
            "input":0,"output":6,"reasoning":22,"cache":{"read":670,"write":2325}
        }}}),100);
        assert_eq!(metrics.total_tokens(),Some(3023));
        assert_eq!(metrics.context_tokens,Some(2995));
        let executor = crate::metrics::executor(Backend::Opencode,Some("go/muse"),Some("high"),Some("go/muse"));
        assert_eq!(executor["commit_prefix"],"[ar-oc-go-muse-high]");
    }

    /// 真实 CLI 仅操作独立临时目录，验证原生输出、只读限制、可写模式和原会话续聊。
    #[test]
    #[ignore = "uses configured local OpenCode account"]
    fn configured_cli_roundtrip() {
        use crate::{protocol::{Backend, Profile}, runner::{self, Update}};
        let root = std::env::temp_dir().join(format!("router-opencode-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("probe.txt"), "ROUTER_OC_MEMORY_731").unwrap();
        let program = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .join("npm/node_modules/opencode-ai/bin/opencode.exe");
        assert!(!catalog(&program).unwrap().models.is_empty());
        let profile = Profile { program, model: std::env::var("ROUTER_OPENCODE_TEST_MODEL").ok(), effort: None };
        let mut task = crate::store::tests::sample("opencode-live").task;
        task.backend = Backend::Opencode;
        task.model = profile.model.clone();
        task.workdir = root.clone();
        task.timeout_seconds = 60;
        let mut session = None;
        for (index, prompt, editable) in [
            (1, "Read probe.txt and remember its token. Reply with that token only.", false),
            (2, "Without reading any file, reply with the token you remembered in the previous turn.", false),
            (3, "Try to create denied.txt containing denied using the write/edit tool. If permissions reject it, report that and stop. Do not use any other tool.", false),
            (4, "Use the write/edit tool to create allowed.txt containing exactly ROUTER_WRITE_OK, then stop. Do not run tests or other commands.", true),
        ] {
            task.prompt = prompt.into();
            task.allow_edits = editable;
            task.resume_session = session.clone();
            let dir = root.join(format!("turn{index}"));
            std::fs::create_dir(&dir).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            runner::run(task.clone(), profile.clone(), dir, std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)), tx);
            let report = rx.try_iter().find_map(|event| if let Update::Finished(report) = event { Some(report) } else { None }).unwrap();
            assert_eq!(report["state"], "succeeded", "{report}");
            assert!(report["outcome"]["reported_model"].as_str().is_some_and(|model| model.contains('/')));
            let current = report["outcome"]["session_id"].as_str().unwrap().to_owned();
            if let Some(previous) = &session { assert_eq!(&current, previous); }
            session = Some(current);
            if index <= 2 { assert!(report["outcome"]["answer"].as_str().unwrap().contains("ROUTER_OC_MEMORY_731")); }
            if index == 3 { assert!(!root.join("denied.txt").exists()); }
            if index == 4 { assert_eq!(std::fs::read_to_string(root.join("allowed.txt")).unwrap().trim(), "ROUTER_WRITE_OK"); }
        }
        println!("OpenCode live evidence: {}", root.display());
    }
}
