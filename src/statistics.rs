//! 会话统计从各轮已记录结果汇总；未知用量保留为空，评价和查看不改变任务活动排序。
use crate::{metrics::Metrics, protocol::Backend, review::ReviewRecord, store::Record};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

/// 每轮只保存统计需要的字段，不重复保存提示词、过程流和结果正文。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnSummary {
    #[serde(default)]
    pub reply_durations: Vec<crate::metrics::ReplyDuration>,
    pub turn: u32,
    pub state: String,
    pub duration_ms: Option<i64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub result_path: PathBuf,
}
impl TurnSummary {
    /// 新结果直接使用规范化指标；旧结果沿用同一 CLI 用量解析器，缺失则不估算。
    pub fn from_result(turn: u32, backend: Backend, path: PathBuf, result: &Value) -> Self {
        let metrics = if result["outcome"]["metrics"].is_object() {
            serde_json::from_value::<Metrics>(result["outcome"]["metrics"].clone())
                .unwrap_or_default()
        } else {
            let mut metrics = Metrics::default();
            metrics.observe(backend, &json!({"type":if backend == Backend::Claude {"result"} else {"turn.completed"},"usage":result["outcome"]["usage"]}), 0);
            metrics
        };
        let duration_ms = result["duration_ms"]
            .as_i64()
            .or_else(|| {
                Some(result["finished_at_ms"].as_i64()? - result["started_at_ms"].as_i64()?)
            })
            .filter(|ms| *ms >= 0);
        Self {
            reply_durations: metrics.reply_durations.into_iter().collect(),
            turn,
            state: result["state"].as_str().unwrap_or("unavailable").into(),
            duration_ms,
            input_tokens: metrics.input_tokens,
            output_tokens: metrics.output_tokens,
            cached_tokens: metrics.cached_tokens,
            result_path: path,
        }
    }
}

/// 初次启用统计时读取本任务已有各轮结果一次，保留原路径；缺失文件显式记作未知。
pub fn restore_turns(record: &mut Record) {
    if record.turns.is_empty() {
        if record.turn == 0 {
            record.turns.push(TurnSummary::from_result(
                0,
                record.task.backend,
                record.directory.join("result.json"),
                &record.result,
            ));
        } else {
            for turn in 1..=record.turn {
                if turn == record.turn && record.running() {
                    continue;
                }
                let path = record
                    .directory
                    .join("turns")
                    .join(format!("{turn:04}"))
                    .join("result.json");
                let result = if turn == record.turn {
                    record.result.clone()
                } else {
                    std::fs::read(&path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .filter(|v| v["task_id"] == record.task.task_id)
                        .unwrap_or(Value::Null)
                };
                record.turns.push(TurnSummary::from_result(
                    turn,
                    record.task.backend,
                    path,
                    &result,
                ));
            }
        }
    }
    if !record.running() {
        finish_turn(record);
    }
}

/// 终态后覆盖同一轮摘要，不重复累计；取消/超时和保存失败同样属于已执行轮次。
pub fn finish_turn(record: &mut Record) {
    let mut summary = TurnSummary::from_result(
        record.turn,
        record.task.backend,
        record.run_directory().join("result.json"),
        &record.result,
    );
    // 线程中断或结果落盘失败可能没有 outcome，已通过事件确认的完整回复仍须保留。
    if summary.reply_durations.is_empty() {
        summary.reply_durations = record.metrics.reply_durations.iter().cloned().collect();
    }
    if let Some(existing) = record.turns.iter_mut().find(|r| r.turn == record.turn) {
        *existing = summary;
    } else {
        record.turns.push(summary);
    }
    record.turns.sort_by_key(|r| r.turn);
}

/// 统计输出包含覆盖率，部分轮次未知时仍可展示已知用量，但不冒充完整总量。
#[derive(Debug, Serialize)]
pub struct TokenTotals {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached: Option<u64>,
    pub total: Option<u64>,
    pub known_turns: usize,
    pub completed_turns: u32,
    pub complete: bool,
}

/// 空集合/全未知保持 None；只加已报告值，缓存不再重复加到总 Token。
fn known_sum(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.flatten().reduce(|a, b| a + b)
}

/// Token 以完成轮次汇总；正在运行的一轮要等 CLI 报告完成用量后才入账。
pub fn totals(record: &Record) -> TokenTotals {
    let input = known_sum(record.turns.iter().map(|t| t.input_tokens));
    let output = known_sum(record.turns.iter().map(|t| t.output_tokens));
    let cached = known_sum(record.turns.iter().map(|t| t.cached_tokens));
    let total = known_sum([input, output].into_iter());
    let known_turns = record
        .turns
        .iter()
        .filter(|t| t.input_tokens.is_some() && t.output_tokens.is_some())
        .count();
    let completed_turns = if record.running() {
        record.turn.saturating_sub(1)
    } else {
        record.turn.max(1)
    };
    TokenTotals {
        input,
        output,
        cached,
        total,
        known_turns,
        completed_turns,
        complete: !record.running()
            && completed_turns > 0
            && known_turns == completed_turns as usize,
    }
}

/// Web 评价对象保持平铺契约；数据库内部仍保存经过验证的输入和服务端字段。
fn review_json(review: &ReviewRecord) -> Value {
    let mut value = serde_json::to_value(&review.input).expect("review serialization");
    value["score"] = json!(review.score);
    value["created_at_ms"] = json!(review.created_at_ms);
    value
}

/// 只向统计页提供指标和审计证据；原始任务 prompt、工作指令及运行日志不进入 Web API。
pub fn snapshot<'a>(records: impl Iterator<Item = &'a Record>, now: i64) -> Value {
    let mut sessions: Vec<_> = records.map(|record| {
        let current = record.reviews.iter().find(|r| r.input.turn == record.turn);
        let replies = record.recent_replies();
        let mean = (!replies.is_empty()).then(|| replies.iter().map(|reply| reply.duration_ms as f64).sum::<f64>() / replies.len() as f64);
        json!({"task_id":record.task.task_id,"title":record.task.label(),"group_path":record.task.group_path,
            "cli":record.task.backend,"model":record.task.model,"effort":record.task.effort,
            "state":record.state,"created_at_ms":record.created_at,"finished_at_ms":record.finished_at,"last_activity_ms":record.last_activity,
            "elapsed_ms":record.elapsed(now),"turn_count":record.turn.max(1),"tokens":totals(record),
            "latency":{"first_text_ms":record.metrics.first_text_ms,"first_text_source":record.metrics.first_text_source,
                "reply_mean_ms":mean,"reply_samples":replies},
            "score":current.map(|r| r.score),"first_score":record.reviews.first().map(|r|r.score),
            "review_status":current.map(|r| match r.input.verdict {crate::review::Verdict::Accepted=>"accepted",crate::review::Verdict::Rework=>"rework"}).unwrap_or("unreviewed"),
            "rework_count":record.reworks.len(),"reviews":record.reviews.iter().map(review_json).collect::<Vec<_>>(),"turns":record.turns,
            "reworks":record.reworks.iter().map(|rework| json!({"request_id":rework.input.request_id,
                "review_id":rework.input.review_id,"from_turn":rework.from_turn,"turn":rework.turn,"created_at_ms":rework.created_at_ms})).collect::<Vec<_>>(),
            "archived":record.archived,"deleted":record.deleted,"result_path":record.directory.join("result.json")})
    }).collect();
    sessions.sort_by(|a, b| {
        b["created_at_ms"]
            .as_i64()
            .cmp(&a["created_at_ms"].as_i64())
            .then_with(|| a["task_id"].as_str().cmp(&b["task_id"].as_str()))
    });
    json!({"schema_version":1,"rubric_version":crate::review::RUBRIC_VERSION,"generated_at_ms":now,"sessions":sessions})
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 多轮缓存不重复加计，全未知与部分已知不能都显示成零或完整数据。
    #[test]
    fn token_totals_track_coverage_and_unknowns() {
        let mut record = crate::store::tests::sample("totals");
        record.turn = 2;
        record.turns = vec![
            TurnSummary::from_result(
                1,
                Backend::Claude,
                PathBuf::new(),
                &json!({"state":"succeeded","outcome":{"usage":{"input_tokens":10,"cache_read_input_tokens":100,"output_tokens":5}}}),
            ),
            TurnSummary::from_result(
                2,
                Backend::Claude,
                PathBuf::new(),
                &json!({"state":"timed_out"}),
            ),
        ];
        let values = totals(&record);
        assert_eq!(values.total, Some(115));
        assert_eq!(values.cached, Some(100));
        assert_eq!(values.known_turns, 1);
        assert!(!values.complete);
        record.turns.clear();
        assert_eq!(totals(&record).total, None);
    }
    /// 只给当前轮次打当前分，上一轮评分保留历史，Web 返回不泄露 prompt 或完整 answer。
    #[test]
    fn snapshot_separates_current_review_from_history() {
        let mut record = crate::store::tests::sample("reviewed");
        record.task.prompt = "private prompt".into();
        record.result = json!({"outcome":{"answer":"private output"}});
        record.turn = 2;
        record.reviews.push(ReviewRecord {
            input: crate::review::tests::sample("r1", 1, crate::review::Verdict::Rework),
            score: 8,
            created_at_ms: 1,
        });
        record.metrics.first_text_ms = Some(1500);
        record.metrics.first_text_source = Some("stream_text".into());
        record.metrics.reply_durations = [1000, 3000]
            .into_iter()
            .map(|duration_ms| crate::metrics::ReplyDuration {
                duration_ms,
                source: "claude_request".into(),
            })
            .collect();
        record.reworks.push(crate::review::ReworkRecord {
            input: crate::review::ReworkInput {
                request_id: "rework1".into(),
                review_id: "r1".into(),
                message: "private rework instructions".into(),
            },
            from_turn: 1,
            turn: 2,
            created_at_ms: 2,
        });
        let data = snapshot([&record].into_iter(), 3);
        assert!(data["sessions"][0]["score"].is_null());
        assert_eq!(data["sessions"][0]["first_score"], 8);
        assert_eq!(data["sessions"][0]["latency"]["first_text_ms"], 1500);
        assert_eq!(data["sessions"][0]["latency"]["reply_mean_ms"], 2000.0);
        assert_eq!(data["sessions"][0]["reworks"][0]["review_id"], "r1");
        assert!(!data.to_string().contains("private"));
    }

    /// 生成仅用于本机浏览器验收的确定性快照，覆盖分页、评分、未知用量、归档与删除。
    #[test]
    fn write_web_statistics_fixture() {
        let mut records = Vec::new();
        for index in 0..30 {
            let mut record = crate::store::tests::sample(&format!("web-demo-{index:02}"));
            record.task.title = Some(if index == 0 {
                "测试 · 审计与返工会话".into()
            } else if index == 1 {
                "测试 · <img src=x onerror=alert(1)>".into()
            } else {
                format!("测试会话 {index:02}")
            });
            record.task.group_path =
                vec!["测试Harness".into(), "演示APP".into(), "统计验收".into()];
            record.task.backend = if index % 3 == 2 {
                Backend::Codex
            } else {
                Backend::Claude
            };
            record.task.model = Some(["GLM-5.3", "glm-5.3", "gpt-5.6-luna"][index % 3].into());
            record.task.effort = Some("max".into());
            record.created_at = 1788912000000 + index as i64 * 60000;
            record.finished_at = Some(record.created_at + 5000);
            record.last_activity = record.finished_at.unwrap();
            record.turn = 1;
            record.elapsed_ms = 5000;
            record.state =
                ["succeeded", "running", "timed_out", "cancelled", "failed"][index % 5].into();
            record.archived = index == 28;
            record.deleted = index == 29;
            if index % 4 != 3 {
                record.metrics.input_tokens = Some(1000);
                record.metrics.output_tokens = Some(200);
                record.metrics.cached_tokens = Some(600);
                record.metrics.first_text_ms = Some(1200);
                record.metrics.first_text_source = Some(
                    if record.task.backend == Backend::Codex {
                        "completed_message"
                    } else {
                        "stream_text"
                    }
                    .into(),
                );
                record.metrics.reply_durations = [1000, 2000, 3000]
                    .into_iter()
                    .map(|duration_ms| crate::metrics::ReplyDuration {
                        duration_ms,
                        source: if record.task.backend == Backend::Codex {
                            "codex_reply_cycle"
                        } else {
                            "claude_request"
                        }
                        .into(),
                    })
                    .collect();
            }
            record.result = json!({"state":record.state,"duration_ms":5000,"outcome":{"metrics":record.metrics}});
            if !record.running() {
                finish_turn(&mut record);
            }
            if index == 0 {
                let mut input = crate::review::tests::sample(
                    "review-before",
                    1,
                    crate::review::Verdict::Rework,
                );
                input.dimensions.correctness = 2;
                input.dimensions.completeness = 1;
                input.dimensions.compliance = 1;
                record.reviews.push(ReviewRecord {
                    score: input.validate().unwrap(),
                    input,
                    created_at_ms: record.last_activity,
                });
                record.turn = 2;
                record.reworks.push(crate::review::ReworkRecord {
                    input: crate::review::ReworkInput {
                        request_id: "rework-demo".into(),
                        review_id: "review-before".into(),
                        message: "fixture-only private instruction".into(),
                    },
                    from_turn: 1,
                    turn: 2,
                    created_at_ms: record.last_activity + 1,
                });
                finish_turn(&mut record);
            }
            if index == 0 || index == 3 {
                let mut input = crate::review::tests::sample(
                    &format!("review-{index}"),
                    record.turn,
                    crate::review::Verdict::Accepted,
                );
                input.dimensions.correctness = 4;
                input.dimensions.compliance = 1;
                if index == 3 {
                    input.dimensions.compliance = 2;
                }
                record.reviews.push(ReviewRecord {
                    score: input.validate().unwrap(),
                    input,
                    created_at_ms: record.last_activity + 2,
                });
            }
            if record.running() {
                record.finished_at = None;
                record.turn_started_at = record.created_at;
            }
            records.push(record);
        }
        let data = snapshot(records.iter(), 1788914000000);
        assert_eq!(data["sessions"].as_array().unwrap().len(), 30);
        let reviewed = data["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["task_id"] == "web-demo-00")
            .unwrap();
        assert_eq!(reviewed["score"], 9);
        assert_eq!(reviewed["first_score"], 5);
        assert!(!data.to_string().contains("fixture-only private"));
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/web-statistics-fixture.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
        println!("Web fixture: {}", path.display());
    }
}
