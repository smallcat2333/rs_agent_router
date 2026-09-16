//! 增量读取本次 Codex 任务的原生用量；以原生时间戳扣除工具区间，逐响应刷新 TPS。
use super::{Metrics, codex_session_path};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{BufRead, Seek},
    path::PathBuf,
};

/// 每次 execute 独立创建；游标避免重复扫描正文，task_started 隔离续聊中的旧任务。
#[derive(Default)]
pub struct CodexLiveMetrics {
    path: Option<PathBuf>,
    offset: u64,
    active: bool,
    start_ms: Option<u64>,
    duration_ms: u64,
    tools: BTreeSet<String>,
    last_usage: Value,
    valid: bool,
}

impl CodexLiveMetrics {
    /// 精确找会话并读取新增完整行；尚未写完的 JSON 行留到下次，不丢数据。
    pub fn poll(&mut self, session: &str, launched_ms: u64, metrics: &mut Metrics) -> Result<()> {
        if self.path.is_none() {
            self.path = Some(codex_session_path(session)?);
        }
        let mut file = std::io::BufReader::new(std::fs::File::open(self.path.as_ref().unwrap())?);
        file.seek(std::io::SeekFrom::Start(self.offset))?;
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = file.read_line(&mut line)?;
            if bytes == 0 || !line.ends_with('\n') {
                break;
            }
            let event: Value = serde_json::from_str(&line)?;
            self.observe(&event, launched_ms, metrics)?;
            self.offset += bytes as u64;
        }
        Ok(())
    }

    /// 用量事件可能在工具前或工具后落盘；累计模型区间到该事件，工具并行取并集。
    fn observe(&mut self, event: &Value, launched_ms: u64, metrics: &mut Metrics) -> Result<()> {
        let payload = &event["payload"];
        let kind = payload["type"].as_str().unwrap_or("");
        if event["type"] != "event_msg" && event["type"] != "response_item" {
            return Ok(());
        }
        let at = timestamp_ms(
            event["timestamp"]
                .as_str()
                .context("missing native timestamp")?,
        )?;
        if at < launched_ms {
            if kind == "token_count" && payload["info"]["total_token_usage"].is_object() {
                self.last_usage = payload["info"]["total_token_usage"].clone();
            }
            return Ok(());
        }
        if event["type"] == "event_msg" && kind == "task_started" {
            self.active = true;
            self.start_ms = Some(at);
            self.duration_ms = 0;
            self.tools.clear();
            self.valid = true;
            metrics.throughput_samples.clear();
            metrics.model_output_tokens = None;
            metrics.model_duration_ms = 0;
            metrics.tps_estimated = true;
        } else if self.active && event["type"] == "response_item" {
            match kind {
                // 原生内建搜索没有配对起止时间，当前响应不能伪造模型耗时。
                "web_search_call" => self.valid = false,
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    self.accumulate(at);
                    if let Some(id) = payload["call_id"].as_str().or(payload["id"].as_str()) {
                        self.tools.insert(id.to_owned());
                    } else {
                        self.valid = false;
                    }
                }
                "function_call_output" | "custom_tool_call_output" => {
                    if !payload["call_id"]
                        .as_str()
                        .is_some_and(|id| self.tools.remove(id))
                    {
                        self.valid = false;
                    }
                    if self.tools.is_empty() {
                        self.start_ms = Some(at);
                    }
                }
                _ => {}
            }
        } else if self.active && event["type"] == "event_msg" && kind == "token_count" {
            let info = &payload["info"];
            let total = &info["total_token_usage"];
            if !total.is_object() || *total == self.last_usage {
                return Ok(());
            }
            self.last_usage = total.clone();
            self.accumulate(at);
            if self.valid
                && let Some(tokens) = info["last_token_usage"]["output_tokens"].as_u64()
            {
                metrics.push_throughput(tokens, self.duration_ms);
            }
            metrics.context_tokens = info["last_token_usage"]["input_tokens"].as_u64();
            metrics.context_window_tokens = info["model_context_window"].as_u64();
            self.duration_ms = 0;
            self.valid = true;
            if self.tools.is_empty() {
                self.start_ms = Some(at);
            }
        } else if event["type"] == "event_msg" && matches!(kind, "task_complete" | "task_aborted") {
            self.active = false;
        }
        Ok(())
    }

    /// 关闭当前模型区间；时间倒序时废弃当前样本，避免把异常时长计入窗口。
    fn accumulate(&mut self, at: u64) {
        if let Some(start) = self.start_ms.take() {
            if let Some(duration) = at.checked_sub(start) {
                self.duration_ms += duration;
            } else {
                self.valid = false;
            }
        }
    }
}

/// 解析原生日志固定 UTC ISO 时间，按公历天数转换毫秒，支持跨日及 1–3 位小数。
fn timestamp_ms(value: &str) -> Result<u64> {
    let (date, time) = value.split_once('T').context("invalid native timestamp")?;
    let date: Vec<u64> = date
        .split('-')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    let time = time
        .strip_suffix('Z')
        .context("native timestamp must be UTC")?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let clock: Vec<u64> = clock
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        date.len() == 3 && clock.len() == 3,
        "invalid native timestamp components"
    );
    let (year, month, day) = (date[0], date[1], date[2]);
    ensure!(
        year >= 1970
            && (1..=12).contains(&month)
            && (1..=31).contains(&day)
            && clock[0] < 24
            && clock[1] < 60
            && clock[2] < 60
            && fraction.len() <= 3
            && fraction.bytes().all(|b| b.is_ascii_digit()),
        "invalid native timestamp range"
    );
    let prior = year - 1;
    let leap_days = prior / 4 - prior / 100 + prior / 400 - (1969 / 4 - 1969 / 100 + 1969 / 400);
    let before_month = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][month as usize - 1];
    let leap = u64::from(
        month > 2
            && year.is_multiple_of(4)
            && (!year.is_multiple_of(100) || year.is_multiple_of(400)),
    );
    let days = (year - 1970) * 365 + leap_days + before_month + leap + day - 1;
    let millis = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>()? * 10_u64.pow(3 - fraction.len() as u32)
    };
    Ok(((days * 24 + clock[0]) * 3600 + clock[1] * 60 + clock[2]) * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 生成固定日期的原生事件，测试中仅按毫秒推进，不依赖真实等待。
    fn event(at: u64, kind: &str, payload: Value) -> Value {
        json!({"timestamp":format!("2026-09-13T00:{:02}:{:02}.{:03}Z", at / 60000, at / 1000 % 60, at % 1000), "type":kind,"payload":payload})
    }

    /// 模拟每轮累计及当轮输出；相同累计值代表重复通知而非新响应。
    fn usage(at: u64, total: u64, output: u64) -> Value {
        event(
            at,
            "event_msg",
            json!({"type":"token_count","info":{"total_token_usage":{"output_tokens":total},"last_token_usage":{"output_tokens":output,"input_tokens":100},"model_context_window":1000}}),
        )
    }

    /// 超过十轮只保留最近十轮；并行工具并集扣除，重复通知及任务汇总不加入窗口。
    #[test]
    fn rolling_ten_live_rounds_exclude_parallel_tools() {
        let mut live = CodexLiveMetrics::default();
        let mut metrics = Metrics::default();
        live.observe(
            &event(0, "event_msg", json!({"type":"task_started"})),
            0,
            &mut metrics,
        )
        .unwrap();
        let mut total = 0;
        for n in 0..12 {
            let base = n * 10000;
            for (id, at) in [("a", base + 1000), ("b", base + 2000)] {
                live.observe(
                    &event(
                        at,
                        "response_item",
                        json!({"type":"function_call","call_id":id}),
                    ),
                    0,
                    &mut metrics,
                )
                .unwrap();
            }
            for (id, at) in [("a", base + 6000), ("b", base + 9000)] {
                live.observe(
                    &event(
                        at,
                        "response_item",
                        json!({"type":"function_call_output","call_id":id}),
                    ),
                    0,
                    &mut metrics,
                )
                .unwrap();
            }
            let output = (n + 1) * 100;
            total += output;
            let completed = usage(base + 10000, total, output);
            live.observe(&completed, 0, &mut metrics).unwrap();
            live.observe(&completed, 0, &mut metrics).unwrap();
            assert_eq!(metrics.throughput_samples.len(), (n + 1).min(10) as usize);
            assert!(metrics.output_tps().is_some());
        }
        assert_eq!(metrics.model_duration_ms, 20000);
        assert_eq!(metrics.model_output_tokens, Some(7500));
        assert_eq!(metrics.output_tps(), Some(375.));
        metrics.observe(
            crate::protocol::Backend::Codex,
            &json!({"type":"turn.completed","usage":{"output_tokens":99999}}),
            999999,
        );
        assert_eq!(metrics.output_tps(), Some(375.));
    }

    /// 同一会话旧任务及续聊开始的旧用量通知不计数；零耗时和未知工具时间不伪造值。
    #[test]
    fn resumed_task_ignores_history_and_invalid_rounds() {
        let mut live = CodexLiveMetrics::default();
        let mut metrics = Metrics::default();
        let cutoff = timestamp_ms("2026-09-13T00:00:10Z").unwrap();
        for value in [
            event(0, "event_msg", json!({"type":"task_started"})),
            usage(1000, 1000, 1000),
            event(10000, "event_msg", json!({"type":"task_started"})),
            usage(10000, 1000, 1000),
            usage(10000, 1100, 100),
        ] {
            live.observe(&value, cutoff, &mut metrics).unwrap();
        }
        assert!(metrics.throughput_samples.is_empty());
        live.observe(
            &event(
                11000,
                "response_item",
                json!({"type":"function_call_output","call_id":"unknown"}),
            ),
            cutoff,
            &mut metrics,
        )
        .unwrap();
        live.observe(&usage(12000, 1200, 100), cutoff, &mut metrics)
            .unwrap();
        assert!(metrics.throughput_samples.is_empty());
        live.observe(&usage(13000, 1300, 100), cutoff, &mut metrics)
            .unwrap();
        assert_eq!(metrics.output_tps(), Some(100.));
        live.observe(
            &event(14000, "event_msg", json!({"type":"task_started"})),
            cutoff,
            &mut metrics,
        )
        .unwrap();
        assert!(metrics.throughput_samples.is_empty());
        assert_eq!(metrics.output_tps(), None);
    }

    /// 读取到半行时保留游标；补齐后仅处理一次，避免文件缓冲导致漏轮或重复轮。
    #[test]
    fn incremental_reader_preserves_partial_lines() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("router-tps-{}.jsonl", uuid::Uuid::new_v4()));
        let started = event(0, "event_msg", json!({"type":"task_started"})).to_string();
        let completed = usage(1000, 100, 100).to_string();
        let split = completed.len() / 2;
        std::fs::write(&path, format!("{started}\n{}", &completed[..split])).unwrap();
        let mut live = CodexLiveMetrics {
            path: Some(path.clone()),
            ..Default::default()
        };
        let mut metrics = Metrics::default();
        live.poll("unused", 0, &mut metrics).unwrap();
        assert_eq!(metrics.output_tps(), None);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", &completed[split..]).unwrap();
        drop(file);
        live.poll("unused", 0, &mut metrics).unwrap();
        live.poll("unused", 0, &mut metrics).unwrap();
        assert_eq!(metrics.output_tps(), Some(100.));
        assert_eq!(metrics.throughput_samples.len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    /// UTC 小数、跨日和闰年时间差都按实际毫秒计算。
    #[test]
    fn native_timestamps_cross_calendar_boundaries() {
        assert_eq!(timestamp_ms("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            timestamp_ms("2026-09-14T00:00:00.01Z").unwrap()
                - timestamp_ms("2026-09-13T23:59:59.9Z").unwrap(),
            110
        );
        assert_eq!(
            timestamp_ms("2024-03-01T00:00:00Z").unwrap()
                - timestamp_ms("2024-02-28T00:00:00Z").unwrap(),
            172800000
        );
    }

    /// 按显式提供的本机会话回放真实协议；仅输出指标，不泄露正文，默认不依赖本机文件。
    #[test]
    #[ignore = "requires ROUTER_TPS_SESSION pointing to a native rollout"]
    fn replay_native_session() {
        let path = PathBuf::from(
            std::env::var_os("ROUTER_TPS_SESSION").expect("ROUTER_TPS_SESSION missing"),
        );
        let mut live = CodexLiveMetrics {
            path: Some(path),
            ..Default::default()
        };
        let mut metrics = Metrics::default();
        live.poll("unused", 0, &mut metrics).unwrap();
        assert_eq!(metrics.throughput_samples.len(), 10);
        assert!(metrics.output_tps().is_some_and(|tps| tps > 0.));
        let before = metrics.clone();
        live.poll("unused", 0, &mut metrics).unwrap();
        assert_eq!(before, metrics);
        println!(
            "samples={} tokens={} duration_ms={} tps={:.2}",
            metrics.throughput_samples.len(),
            metrics.model_output_tokens.unwrap(),
            metrics.model_duration_ms,
            metrics.output_tps().unwrap()
        );
    }
}
