//! 逐轮读取 Claude 调试日志，仅以 API 异常启动独立重试时限。
use std::{fs::File, io::{self, Read, Seek, SeekFrom}, path::Path, time::{Duration, Instant}};

/// 增量游标保留半行，连续错误不重置首次异常时间；不同任务各自持有实例。
#[derive(Default)]
pub struct RetryWatch {
    offset: u64,
    pending: Vec<u8>,
    started: Option<Instant>,
    pub error: Option<String>,
}
impl RetryWatch {
    /// 文件由 CLI 延迟创建；只消费完整新行，不把读取日志本身算作 CLI 活动。
    pub fn poll(&mut self, path: &Path, now: Instant) -> io::Result<bool> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let count = file.read_to_end(&mut self.pending)?;
        self.offset += count as u64;
        let Some(last) = self.pending.iter().rposition(|byte| *byte == b'\n') else { return Ok(false) };
        let complete: Vec<_> = self.pending.drain(..=last).collect();
        let mut began = false;
        for line in String::from_utf8_lossy(&complete).lines() {
            began |= self.observe(line, now);
        }
        Ok(began)
    }
    /// 只接受日志顶层 API 错误，不匹配工具结果、用户文本或一般调试错误。
    fn observe(&mut self, line: &str, now: Instant) -> bool {
        let Some((timestamp, message)) = line.split_once(" [ERROR] ") else { return false };
        if timestamp.len() > 35 || !timestamp.contains('T') {
            return false;
        }
        // 回传只保留重试次数，不暴露完整请求或错误响应内容。
        if message.starts_with("API error (attempt ") {
            let (attempt, detail) = message.split_once("): ").unwrap_or((message, ""));
            let status = detail.split_whitespace().next()
                .filter(|word| word.parse::<u16>().is_ok_and(|code| (100..600).contains(&code)))
                .unwrap_or("连接或流式错误");
            self.error = Some(format!("{attempt}): {status}"));
        } else if message.starts_with("Error streaming, falling back to non-streaming mode:") {
            self.error = Some("流式请求异常，CLI 转为非流式重试".to_owned());
        } else {
            return false;
        }
        if self.started.is_some() { return false }
        self.started = Some(now);
        true
    }
    /// 正常模型内容恢复后结束本次异常窗口；心跳和工具输出不调用此方法。
    pub fn recovered(&mut self) {
        self.started = None;
        self.error = None;
    }
    /// 异常期间以专用时限为准，避免普通静默时限提前结束本次重试窗口。
    pub fn active(&self) -> bool {
        self.started.is_some()
    }
    /// 从首次错误计时，重复失败不能无限延长。
    pub fn expired(&self, now: Instant, limit: Duration) -> bool {
        self.started.is_some_and(|started| now.duration_since(started) >= limit)
    }
}

/// 只将正常的模型内容作为恢复证据，忽略工具结果、init、错误及空心跳。
pub fn model_progress(value: &serde_json::Value) -> bool {
    if value["type"] == "stream_event" {
        let event = &value["event"];
        if event["type"] == "content_block_delta" {
            return ["text", "thinking", "partial_json"].iter()
                .any(|key| event["delta"][key].as_str().is_some_and(|text| !text.is_empty()));
        }
    }
    value["type"] == "assistant"
        && value.get("error").is_none_or(serde_json::Value::is_null)
        && value["message"]["model"] != "<synthetic>"
        && value["message"]["content"].as_array().is_some_and(|blocks| blocks.iter().any(|block|
            block["type"] == "tool_use" || block["text"].as_str().is_some_and(|text| !text.is_empty())))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 错误计时不因重试续期；正常模型内容恢复后可开始下一段独立异常。
    #[test]
    fn retry_deadline_and_recovery() {
        let now = Instant::now();
        let mut watch = RetryWatch::default();
        assert!(watch.observe("2026-09-15T08:34:29.092Z [ERROR] API error (attempt 1/11): 500 test", now));
        assert!(!watch.observe("2026-09-15T08:34:59.092Z [ERROR] API error (attempt 2/11): 500 test", now + Duration::from_secs(30)));
        assert!(watch.expired(now + Duration::from_secs(60), Duration::from_secs(60)));
        watch.recovered();
        assert!(!watch.expired(now + Duration::from_secs(90), Duration::from_secs(60)));
        assert!(watch.observe("2026-09-15T08:41:54.508Z [ERROR] Error streaming, falling back to non-streaming mode: fixture", now));
        assert!(watch.expired(now + Duration::from_secs(60), Duration::from_secs(60)));
        assert!(!watch.observe("2026-09-15T08:34:29.092Z [ERROR] Bash command failed: 500", now));
        assert!(!model_progress(&serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result"}]}})));
        assert!(!model_progress(&serde_json::json!({"type":"assistant","error":"api_error","message":{"model":"<synthetic>","content":[{"type":"text","text":"error"}]}})));
        assert!(model_progress(&serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}})));
    }
    /// 半行跨两次读取只处理一次，防止错误标题被写入边界截断而漏判。
    #[test]
    fn partial_log_lines_are_retained() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("router-retry-{}.log", uuid::Uuid::new_v4()));
        let mut file = File::create(&path).unwrap();
        file.write_all(b"2026-09-15T08:34:29.092Z [ERROR] API error (attempt ").unwrap();
        let mut watch = RetryWatch::default();
        let now = Instant::now();
        assert!(!watch.poll(&path, now).unwrap());
        file.write_all(b"1/11): 500 test\n").unwrap();
        assert!(watch.poll(&path, now).unwrap());
        assert!(!watch.poll(&path, now).unwrap());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
