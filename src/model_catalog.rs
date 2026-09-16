//! Claude 读取当前供应商模型映射；Codex 通过 CLI 查询完整可选模型目录。
use crate::protocol::Backend;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::{fs, path::Path};

/// Codex 查询所选 CLI 的模型目录；Claude 读取当前供应商映射，无声明时读取生效配置。
pub fn discover(backend: Backend, program: &Path) -> Result<Vec<String>> {
    if backend == Backend::Opencode {
        return Ok(crate::opencode::catalog(program)?.models);
    }
    if backend == Backend::Codex {
        return codex_models(program);
    }
    let home = std::env::var_os("USERPROFILE").context("无法确定用户配置目录")?;
    let home = Path::new(&home);
    let database = home.join(".cc-switch/cc-switch.db");
    let mut models = Vec::new();
    if database.is_file() {
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("无法只读打开 CC Switch 数据库")?;
        let mut schema = connection.prepare("PRAGMA table_info(providers)")?;
        let columns = schema
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            ["app_type", "is_current", "settings_config"]
                .iter()
                .all(|name| columns.iter().any(|column| column == name)),
            "CC Switch providers 表结构不支持读取当前模型"
        );
        let mut statement = connection.prepare(
            "SELECT settings_config FROM providers WHERE app_type = ?1 AND is_current = 1",
        )?;
        let configs = statement
            .query_map([backend.key()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            configs.len() <= 1,
            "CC Switch 存在多个当前供应商，请重新选择供应商"
        );
        if let Some(config) = configs.first() {
            // 不把配置原文加入错误上下文，防止认证内容出现在日志或界面。
            let settings: Value = serde_json::from_str(config)
                .map_err(|_| anyhow::anyhow!("CC Switch 当前供应商配置不是有效 JSON"))?;
            models = configured_models(&settings);
        }
    }
    if models.is_empty() {
        let path = home.join(".claude/settings.json");
        if path.is_file() {
            let text = fs::read_to_string(path).context("无法读取 CLI 当前模型配置")?;
            let settings: Value = serde_json::from_str(&text)
                .map_err(|_| anyhow::anyhow!("Claude 当前配置不是有效 JSON"))?;
            models = configured_models(&settings);
        }
    }
    ensure!(!models.is_empty(), "当前 CC Switch / CLI 配置没有声明模型");
    Ok(models)
}

/// 通过所选 CLI 的 app-server 分页读取可选模型；不创建对话，所有退出路径回收专属进程。
fn codex_models(program: &Path) -> Result<Vec<String>> {
    let (process, events) = cli_stream::Command::new(program)
        .args(["app-server"])
        .stdin(cli_stream::Stdin::Piped)
        .start()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let result = (|| {
        rpc_model_request(
            &process,
            &events,
            0,
            "initialize",
            serde_json::json!({
                "clientInfo": {"name":"rs_agent_router", "version": env!("CARGO_PKG_VERSION")}
            }),
            deadline,
        )?;
        process.write_line(r#"{"method":"initialized","params":{}}"#)?;
        let mut models = Vec::new();
        let mut cursor = Value::Null;
        let mut id = 1;
        loop {
            let page = rpc_model_request(
                &process,
                &events,
                id,
                "model/list",
                serde_json::json!({"limit":100,"includeHidden":false,"cursor":cursor}),
                deadline,
            )?;
            let next = append_model_page(&mut models, &page)?;
            if next.is_null() {
                break;
            }
            ensure!(next != cursor, "Codex 模型分页游标未前进");
            cursor = next;
            id += 1;
        }
        ensure!(!models.is_empty(), "Codex CLI 未返回可选模型");
        Ok(models)
    })();
    process.cancel()?;
    result
}

/// 只等待当前 RPC 响应，忽略通知和诊断；总截止时间约束握手与全部分页。
fn rpc_model_request(
    process: &cli_stream::ProcessHandle,
    events: &std::sync::mpsc::Receiver<cli_stream::Event>,
    id: u64,
    method: &str,
    params: Value,
    deadline: std::time::Instant,
) -> Result<Value> {
    process
        .write_line(&serde_json::json!({"method":method,"id":id,"params":params}).to_string())?;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .context("Codex 模型刷新超时（20 秒）")?;
        match events
            .recv_timeout(remaining)
            .context("Codex 模型刷新超时或连接已关闭")?
        {
            cli_stream::Event::Stdout { line, .. } => {
                let response: Value = serde_json::from_str(&line)
                    .map_err(|_| anyhow::anyhow!("Codex app-server 返回非 JSON 数据"))?;
                if response["id"].as_u64() != Some(id) {
                    continue;
                }
                ensure!(
                    response.get("error").is_none(),
                    "Codex {method} 返回错误，代码 {}",
                    response["error"]["code"]
                );
                return response
                    .get("result")
                    .cloned()
                    .context("Codex RPC 响应缺少 result");
            }
            cli_stream::Event::Exited { .. } | cli_stream::Event::Error { .. } => {
                anyhow::bail!("Codex app-server 在返回模型列表前退出")
            }
            _ => {}
        }
    }
}

/// 只收集 model 标识并保持原顺序；验证分页字段，避免把展示名称或隐藏项当成可选模型。
fn append_model_page(models: &mut Vec<String>, page: &Value) -> Result<Value> {
    let entries = page["data"]
        .as_array()
        .context("Codex 模型列表缺少 data 数组")?;
    for entry in entries {
        if entry["hidden"].as_bool() == Some(true) {
            continue;
        }
        add_model(
            models,
            entry["model"]
                .as_str()
                .context("Codex 模型缺少 model 标识")?,
        );
    }
    let cursor = page
        .get("nextCursor")
        .context("Codex 模型列表缺少 nextCursor")?;
    ensure!(
        cursor.is_null() || cursor.is_string(),
        "Codex 模型分页游标类型错误"
    );
    Ok(cursor.clone())
}

/// 提取明确的模型字段，排除展示名称、认证信息及其他供应商元数据。
fn configured_models(settings: &Value) -> Vec<String> {
    let mut models = Vec::new();
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        if let Some(model) = settings
            .get("env")
            .and_then(|env| env.get(key))
            .and_then(Value::as_str)
        {
            add_model(&mut models, model);
        }
    }
    if let Some(model) = settings.get("model").and_then(Value::as_str) {
        add_model(&mut models, model);
    }
    models
}

/// 保持配置优先顺序并去重；外部空模型字段不作为可选项。
fn add_model(models: &mut Vec<String>, model: &str) {
    let model = model.trim();
    if !model.is_empty() && !models.iter().any(|existing| existing == model) {
        models.push(model.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 多页模型目录保留 Luna 等非默认模型，按模型 ID 去重且排除隐藏项。
    #[test]
    fn codex_catalog_includes_non_default_models_across_pages() {
        let mut models = Vec::new();
        let cursor = append_model_page(
            &mut models,
            &serde_json::json!({"data":[
            {"model":"gpt-6-astra","displayName":"Astra"},
            {"model":"gpt-5.6-luna","displayName":"Luna"}
        ],"nextCursor":"page2"}),
        )
        .unwrap();
        assert_eq!(cursor, "page2");
        append_model_page(
            &mut models,
            &serde_json::json!({"data":[
            {"model":"gpt-5.6-luna"},{"model":"gpt-5.6-sol"},
            {"model":"hidden-model","hidden":true}
        ],"nextCursor":null}),
        )
        .unwrap();
        assert_eq!(models, ["gpt-6-astra", "gpt-5.6-luna", "gpt-5.6-sol"]);
        assert!(
            append_model_page(
                &mut models,
                &serde_json::json!({"data":[],"nextCursor":123})
            )
            .is_err()
        );
    }

    /// 显式运行时只查询本机已配置 CLI 的模型目录，不创建会话或改变配置。
    #[test]
    #[ignore = "queries configured local Codex app-server"]
    fn configured_codex_catalog() {
        let profiles: crate::protocol::Profiles = serde_json::from_slice(
            &fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("router.local.json")).unwrap(),
        )
        .unwrap();
        let models = discover(Backend::Codex, &profiles["codex"].program).unwrap();
        println!("Codex models: {}", models.join(", "));
        assert!(models.iter().any(|model| model == "gpt-5.6-luna"));
    }

    /// 多个 Claude 别名映射同模型时只显示一次，展示名称和认证字段不能进入列表。
    #[test]
    #[ignore = "reads current local CC Switch model fields"]
    fn configured_claude_catalog() {
        let models = discover(Backend::Claude, Path::new("")).unwrap();
        println!("Claude configured models: {}", models.join(", "));
        assert!(models.iter().any(|model| model == "auto"));
        assert!(models.iter().any(|model| model == "deepseek-v4-flash"));
    }

    /// 多个 Claude 别名映射同模型时只显示一次，展示名称和认证字段不能进入列表。
    #[test]
    fn claude_models_exclude_names_and_credentials() {
        let settings = serde_json::json!({"env": {
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.3",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.3",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_FABLE_MODEL": "auto",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "展示名称",
            "ANTHROPIC_AUTH_TOKEN": "must-not-be-listed"
        }});
        assert_eq!(
            configured_models(&settings),
            vec!["glm-5.3", "deepseek-v4-flash", "auto"]
        );
    }
}
