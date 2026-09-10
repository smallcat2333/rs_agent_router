//! 按明确模型标识读取本地强度表；不请求网络或随模型刷新重新发现强度。
use serde::Deserialize;
use std::sync::OnceLock;

/// 同一组明确模型共用已核对的档位，说明用于区分不支持与未收录。
#[derive(Deserialize)]
pub struct EffortRule {
    models: Vec<String>,
    pub levels: Vec<String>,
    pub note: String,
}

/// 内嵌 JSON 是唯一维护源，启动后首次使用时解析一次。
#[derive(Deserialize)]
struct EffortTable {
    entries: Vec<EffortRule>,
}

/// 查找已核对模型；允许大小写、[1m] 标记及八位日期快照，不推测未知版本或浮动别名。
pub fn lookup(model: Option<&str>) -> Option<&'static EffortRule> {
    static TABLE: OnceLock<EffortTable> = OnceLock::new();
    let model = model?.trim().to_ascii_lowercase();
    let model = model.strip_suffix("[1m]").unwrap_or(&model);
    let table = TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/model_efforts.json"))
            .expect("内嵌模型强度表格式错误")
    });
    table.entries.iter().find(|rule| {
        rule.models.iter().any(|known| {
            model == known
                || model
                    .strip_prefix(known.as_str())
                    .and_then(|suffix| suffix.strip_prefix('-'))
                    .is_some_and(|date| {
                        date.len() == 8 && date.bytes().all(|ch| ch.is_ascii_digit())
                    })
        })
    })
}

/// 模型切换后保留仍有效的档位，其余重置为默认，防止旧值继续传给新模型。
pub fn normalize(model: Option<&str>, effort: &mut Option<String>) {
    if effort
        .as_ref()
        .is_some_and(|value| !lookup(model).is_some_and(|rule| rule.levels.contains(value)))
    {
        *effort = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 校验维护表中的模型不重复，档位均能作为 CLI 强度参数，来源引用完整。
    #[test]
    fn catalog_has_unique_models_and_valid_sources() {
        let table: serde_json::Value =
            serde_json::from_str(include_str!("../assets/model_efforts.json")).unwrap();
        let mut models = std::collections::HashSet::new();
        for entry in table["entries"].as_array().unwrap() {
            for model in entry["models"].as_array().unwrap() {
                assert!(models.insert(model.as_str().unwrap()));
            }
            let mut levels = std::collections::HashSet::new();
            for level in entry["levels"].as_array().unwrap() {
                let level = level.as_str().unwrap();
                assert!(["low", "medium", "high", "xhigh", "max", "ultra"].contains(&level));
                assert!(levels.insert(level));
            }
            for source in entry["sources"].as_array().unwrap() {
                assert!(table["sources"].get(source.as_str().unwrap()).is_some());
            }
        }
    }

    /// 不同模型的上限、空档位及日期/上下文别名正确区分，不让未知版本继承旧能力。
    #[test]
    fn model_specific_levels_and_aliases() {
        assert_eq!(
            lookup(Some("gpt-5.6-luna")).unwrap().levels,
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(
            lookup(Some("gpt-6-astra"))
                .unwrap()
                .levels
                .iter()
                .any(|s| s == "ultra")
        );
        assert_eq!(
            lookup(Some("GLM-5.3")).unwrap().levels,
            ["low", "high", "max"]
        );
        assert_eq!(
            lookup(Some("deepseek-v4-flash")).unwrap().levels,
            ["low", "high", "max"]
        );
        assert_eq!(
            lookup(Some("kimi-k3[1m]")).unwrap().levels,
            ["low", "high", "max"]
        );
        assert_eq!(
            lookup(Some("claude-opus-4-5-20251101")).unwrap().levels,
            ["low", "medium", "high"]
        );
        assert!(lookup(Some("kimi-k2.6")).unwrap().levels.is_empty());
        assert!(lookup(Some("claude-opus-4-50")).is_none());
        assert!(lookup(Some("unknown-model")).is_none());
        assert!(lookup(None).is_none());
    }

    /// 切换模型不会沿用不支持的 max/ultra；有效值会保留，最终参数原样传到对应 CLI。
    #[test]
    fn selection_and_cli_arguments_follow_model() {
        let mut effort = Some("ultra".into());
        normalize(Some("gpt-5.6-luna"), &mut effort);
        assert_eq!(effort, None);
        effort = Some("max".into());
        normalize(Some("gpt-5.6-luna"), &mut effort);
        assert_eq!(effort.as_deref(), Some("max"));
        let mut record = crate::store::tests::sample("effort");
        record.task.model = Some("gpt-5.6-luna".into());
        record.task.effort = effort.clone();
        record.task.backend = crate::protocol::Backend::Codex;
        let profile = crate::protocol::Profile {
            program: Default::default(),
            model: record.task.model.clone(),
            effort,
        };
        let args = crate::runner::arguments(&record.task, &profile);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-c", "model_reasoning_effort=max"])
        );
        record.task.backend = crate::protocol::Backend::Claude;
        record.task.model = Some("glm-5.3".into());
        let args = crate::runner::arguments(&record.task, &profile);
        assert!(args.windows(2).any(|pair| pair == ["--effort", "max"]));
        normalize(Some("claude-opus-4-5"), &mut record.task.effort);
        assert_eq!(record.task.effort, None);
    }
}
