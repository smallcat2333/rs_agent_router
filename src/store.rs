//! SQLite 任务目录：稳定日志路径、最近活动和基于终态时间的四小时归档。
use crate::protocol::{Profile, Task};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

#[cfg(test)]
pub const ARCHIVE_AFTER_MS: i64 = 4 * 60 * 60 * 1000;

/// 活进程证据只驻留内存，重启后不得把旧 PID 当作新任务进程。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Health {
    pub phase: String,
    pub pid: Option<u32>,
    pub process_alive: Option<bool>,
    pub worker_alive: bool,
}

/// 一条完整执行输出及其到达时间；仅用于内存展示，不改写原始文本或磁盘日志。
#[derive(Clone, Debug)]
pub struct OutputLine {
    pub text: String,
    pub at_ms: i64,
}

/// 单个任务的持久状态；日志缓存不写入 SQLite，原始文件是真源。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub reviews: Vec<crate::review::ReviewRecord>,
    #[serde(default)]
    pub reworks: Vec<crate::review::ReworkRecord>,
    #[serde(default)]
    pub turns: Vec<crate::statistics::TurnSummary>,
    #[serde(default)]
    pub metrics: crate::metrics::Metrics,
    #[serde(skip)]
    pub health: Health,
    pub task: Task,
    pub directory: PathBuf,
    pub state: String,
    pub created_at: i64,
    pub last_activity: i64,
    pub finished_at: Option<i64>,
    pub archived: bool,
    pub result: Value,
    pub current_tool: String,
    #[serde(default)]
    pub turn: u32,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub profile: Option<Profile>,
    #[serde(default)]
    pub elapsed_ms: i64,
    #[serde(default)]
    pub turn_started_at: i64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(skip)]
    pub lines: VecDeque<OutputLine>,
}

impl Record {
    /// 跨续聊/返工取最新五条完整回复，排除当前轮摘要与实时指标的重复。
    pub fn recent_replies(&self) -> Vec<&crate::metrics::ReplyDuration> {
        self.metrics
            .reply_durations
            .iter()
            .rev()
            .chain(
                self.turns
                    .iter()
                    .rev()
                    .filter(|turn| turn.turn != self.turn)
                    .flat_map(|turn| turn.reply_durations.iter().rev()),
            )
            .take(5)
            .collect()
    }
    /// 优先展示最新回复模型；旧记录读取结果证据，没有实际证据才展示启动配置。
    pub fn display_model(&self) -> &str {
        self.metrics.reported_model.as_deref()
            .or_else(|| self.result["outcome"]["reported_model"].as_str()
                .filter(|model| crate::metrics::explicit_model(model)))
            .or(self.task.model.as_deref())
            .unwrap_or("CLI 默认")
    }
    /// 当前评分只属于当前轮；启动新轮不会把历史分数冒充新结果。
    pub fn current_review(&self) -> Option<&crate::review::ReviewRecord> {
        self.reviews
            .iter()
            .find(|review| review.input.turn == self.turn)
    }
    /// 新版每轮独立目录；旧记录继续读取原目录，绝不搬移日志。
    pub fn run_directory(&self) -> PathBuf {
        if self.turn == 0 {
            self.directory.clone()
        } else {
            self.directory
                .join("turns")
                .join(format!("{:04}", self.turn))
        }
    }
    /// 新会话保存了原生 session_id 后才允许续聊；旧版关闭保存，不能恢复。
    pub fn can_resume(&self) -> bool {
        !self.running()
            && !self.deleted
            && self.turn > 0
            && self.session_id.is_some()
            && self.profile.is_some()
    }
    /// 运行时间累计各轮执行，不把两轮之间的等待时间算作执行。
    pub fn elapsed(&self, now: i64) -> i64 {
        self.elapsed_ms
            + if self.state == "running" {
                (now - self.turn_started_at).max(0)
            } else {
                0
            }
    }
    /// 最近一轮耗时和累计会话耗时分开；正在执行时只计当前轮次。
    pub fn turn_elapsed(&self, now: i64) -> i64 {
        if self.state == "queued" {
            0
        } else if self.state == "running" {
            (now - self.turn_started_at).max(0)
        } else {
            self.result["duration_ms"].as_i64().unwrap_or(0)
        }
    }
    /// 未结束的活动任务包含排队，统一阻止重复续聊、归档及删除。
    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "running" | "queued")
    }
    /// 只用结束时间计算归档，阅读和活动排序不延长保留时间。
    #[cfg(test)]
    pub fn archive_due(&self, now: i64) -> bool {
        self.archive_due_after(now, ARCHIVE_AFTER_MS)
    }
    /// 按用户配置的保留时长判定终态归档；运行任务始终留在活动列表。
    pub fn archive_due_after(&self, now: i64, after_ms: i64) -> bool {
        !self.archived
            && !self.deleted
            && !self.running()
            && self.finished_at.is_some_and(|end| now - end >= after_ms)
    }
    /// 限制内存展示缓存，磁盘完整日志不裁剪。
    pub fn append(&mut self, text: String, at_ms: i64) {
        self.lines.push_back(OutputLine { text, at_ms });
        if self.lines.len() > 200 {
            self.lines.pop_front();
        }
    }
}

/// 单写者持久化；事务性 UPSERT 避免进程退出留下半个 JSON 索引。
pub struct Store {
    connection: Connection,
}
impl Store {
    /// 只管理新目录中的索引，不自动导入或迁移旧版日志。
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let connection = Connection::open(root.join("router.sqlite3"))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS tasks (task_id TEXT PRIMARY KEY, metadata TEXT NOT NULL);")?;
        Ok(Self { connection })
    }
    /// 保存一个任务的完整元数据；任务字段不拼接进 SQL。
    pub fn save(&self, record: &Record) -> Result<()> {
        self.connection.execute("INSERT INTO tasks VALUES (?1, ?2) ON CONFLICT(task_id) DO UPDATE SET metadata=excluded.metadata", params![record.task.task_id, serde_json::to_string(record)?])?;
        Ok(())
    }
    /// 启动恢复；异常中断的任务明确失败，不重试或猜测进程身份。
    #[cfg(test)]
    pub fn restore(&self, now: i64) -> Result<Vec<Record>> {
        self.restore_with_retention(now, ARCHIVE_AFTER_MS)
    }
    /// 启动恢复使用保存的归档时长，避免先按默认时长错误归档。
    pub fn restore_with_retention(&self, now: i64, after_ms: i64) -> Result<Vec<Record>> {
        let values = self
            .connection
            .prepare("SELECT metadata FROM tasks")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut records = Vec::new();
        if values
            .iter()
            .any(|s| serde_json::from_str::<Value>(s).is_ok_and(|r| r["task"]["backend"] == "glm"))
        {
            let database: String = self.connection.query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )?;
            let backup = PathBuf::from(database).with_file_name("router-before-v030.sqlite3");
            if !backup.exists() {
                self.connection
                    .execute("VACUUM INTO ?1", params![backup.display().to_string()])?;
            }
        }
        for value in values {
            let mut metadata: Value = serde_json::from_str(&value)?;
            // 仅迁移已存在的历史记录；新的 Harness 请求不再接受 glm 作为 CLI。
            if metadata["task"]["backend"] == "glm" {
                metadata["task"]["backend"] = json!("claude");
                if metadata["task"]["model"].is_null() {
                    metadata["task"]["model"] = metadata["result"]["requested_model"].clone();
                }
            }
            let mut record: Record = serde_json::from_value(metadata)?;
            if record.turn == 0 {
                record.elapsed_ms = record.result["duration_ms"]
                    .as_i64()
                    .unwrap_or_else(|| {
                        record.finished_at.unwrap_or(record.created_at) - record.created_at
                    })
                    .max(0);
                record.turn_started_at = record.created_at;
            }
            if record.running() {
                // CLI 已落盘但管理器尚未入库时，优先恢复真实终态。
                let completed = std::fs::read(record.run_directory().join("result.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .filter(|r| {
                        matches!(
                            r["state"].as_str(),
                            Some("succeeded" | "failed" | "cancelled" | "timed_out")
                        )
                    });
                record.result = completed.unwrap_or_else(|| json!({"task_id":record.task.task_id,"state":"interrupted","error_code":"manager_interrupted","error":"管理实例异常中断；未自动重跑","finished_at_ms":now}));
                if let Some(metrics) = record.result["outcome"].get("metrics") {
                    record.metrics = serde_json::from_value(metrics.clone())?;
                }
                record.state = record.result["state"].as_str().unwrap().to_owned();
                record.finished_at = Some(record.result["finished_at_ms"].as_i64().unwrap_or(now));
                record.last_activity = record.finished_at.unwrap();
                record.elapsed_ms += record.result["duration_ms"].as_i64().unwrap_or(0);
                if let Some(session) = record.result["outcome"]["session_id"].as_str() {
                    record.session_id = Some(session.to_owned());
                }
                std::fs::write(
                    record.directory.join("result.json"),
                    serde_json::to_vec_pretty(&record.result)?,
                )?;
            }
            if record.archive_due_after(now, after_ms) {
                record.archived = true;
            }
            crate::statistics::restore_turns(&mut record);
            self.save(&record)?;
            records.push(record);
        }
        Ok(records)
    }
}

/// 活跃视图最近三项；终态仍参与，查询不修改任何活动时间。
pub fn recent(records: impl Iterator<Item = Record>) -> Vec<Record> {
    recent_with_limit(records, 3)
}

/// 按指定上限选取最近活动任务，供悬浮窗独立配置；过滤与排序沿用主面板口径。
pub fn recent_with_limit(records: impl Iterator<Item = Record>, limit: usize) -> Vec<Record> {
    let mut values: Vec<_> = records.filter(|r| !r.archived && !r.deleted).collect();
    values.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.task.task_id.cmp(&b.task.task_id))
    });
    values.truncate(limit);
    values
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    /// 历史结果与实时模型优先于启动配置，错误占位模型不会冒充实际模型。
    #[test]
    fn model_display_uses_latest_reply_evidence() {
        let mut record = sample("model");
        record.task.model = Some("glm-5.3".into());
        assert_eq!(record.display_model(), "glm-5.3");
        record.result = serde_json::json!({"outcome":{"reported_model":"deepseek-v4-pro"}});
        assert_eq!(record.display_model(), "deepseek-v4-pro");
        record.metrics.reported_model = Some("deepseek-flash".into());
        assert_eq!(record.display_model(), "deepseek-flash");
        record.metrics.reported_model = None;
        record.result["outcome"]["reported_model"] = serde_json::json!("<synthetic>");
        assert_eq!(record.display_model(), "glm-5.3");
    }
    /// 历史 GLM 元数据迁移前备份数据库，日志不移动，旧会话不伪造恢复。
    #[test]
    fn legacy_glm_migrates_without_relocating_logs() {
        let root = std::env::temp_dir().join(format!("router-legacy-{}", crate::runner::now_ms()));
        let store = Store::open(&root).unwrap();
        let mut old = sample("old");
        old.directory = root.join("old");
        std::fs::create_dir(&old.directory).unwrap();
        std::fs::write(old.directory.join("console.log"), "kept").unwrap();
        let mut json = serde_json::to_value(&old).unwrap();
        json["task"]["backend"] = json!("glm");
        json["result"]["requested_model"] = json!("GLM-5.2");
        store
            .connection
            .execute(
                "INSERT INTO tasks VALUES (?1,?2)",
                params!["old", json.to_string()],
            )
            .unwrap();
        let restored = store.restore(100).unwrap();
        assert_eq!(restored[0].task.backend, crate::protocol::Backend::Claude);
        assert_eq!(restored[0].task.model.as_deref(), Some("GLM-5.2"));
        assert!(!restored[0].can_resume());
        assert!(root.join("router-before-v030.sqlite3").exists());
        assert_eq!(
            std::fs::read_to_string(old.directory.join("console.log")).unwrap(),
            "kept"
        );
    }
    /// 固定时间验证归档边界以及查看/活动时间不影响归档。
    #[test]
    fn archive_uses_finish_time() {
        let mut r = sample("one");
        r.finished_at = Some(100);
        r.last_activity = 99999999;
        assert!(!r.archive_due(100 + ARCHIVE_AFTER_MS - 1));
        assert!(r.archive_due(100 + ARCHIVE_AFTER_MS));
        r.finished_at = None;
        assert!(!r.archive_due(i64::MAX));
    }
    /// 自定义归档时长在首次恢复生效，重复重启不修改结束时间或提前归档。
    #[test]
    fn restart_retains_tasks_until_configured_deadline() {
        let root = std::env::temp_dir().join(format!("router-retention-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&root).unwrap();
        let mut record = sample("retained");
        record.directory = root.join("retained");
        record.finished_at = Some(100);
        store.save(&record).unwrap();
        drop(store);
        for now in [1000, 4 * 3600000 + 100, 6 * 3600000 + 99] {
            let manager =
                crate::manager::Manager::new_with_archive(root.clone(), Default::default(), now, 6)
                    .unwrap();
            assert!(!manager.records["retained"].archived);
            assert_eq!(manager.records["retained"].finished_at, Some(100));
        }
        let manager = crate::manager::Manager::new_with_archive(
            root,
            Default::default(),
            6 * 3600000 + 100,
            6,
        )
        .unwrap();
        assert!(manager.records["retained"].archived);
        record.state = "running".into();
        assert!(!record.archive_due_after(i64::MAX, 1));
    }
    /// 管理器每分钟检查归档，状态写回 SQLite 后重开仍可查询。
    #[test]
    fn periodic_archive_is_persisted() {
        let root =
            std::env::temp_dir().join(format!("router-periodic-{}", crate::runner::now_ms()));
        let mut manager =
            crate::manager::Manager::new(root.clone(), Default::default(), 0).unwrap();
        let mut record = sample("periodic");
        record.directory = root.join("periodic");
        record.finished_at = Some(100);
        manager.records.insert("periodic".into(), record);
        manager.poll(ARCHIVE_AFTER_MS + 99).unwrap();
        assert!(!manager.records["periodic"].archived);
        manager.poll(ARCHIVE_AFTER_MS + 60100).unwrap();
        assert!(manager.records["periodic"].archived);
        drop(manager);
        let store = Store::open(&root).unwrap();
        let records = store.restore(ARCHIVE_AFTER_MS + 60101).unwrap();
        assert!(records[0].archived);
    }
    /// 最近面板只限显示数量，且完成任务不被排除。
    #[test]
    fn newest_three_include_finished() {
        let records: Vec<_> = (0..4)
            .map(|n| {
                let mut r = sample(&n.to_string());
                r.last_activity = n;
                r
            })
            .collect();
        let selected = recent(records.clone().into_iter());
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].task.task_id, "3");
        assert_eq!(recent_with_limit(records.into_iter(), 4).len(), 4);
    }
    /// 验证 SQLite 重启恢复、启动归档及日志路径保持原样。
    #[test]
    fn restore_archives_and_interrupts_without_moving_logs() {
        let root = std::env::temp_dir().join(format!("router-store-{}", crate::runner::now_ms()));
        let store = Store::open(&root).unwrap();
        let mut done = sample("done");
        done.directory = root.join("done");
        std::fs::create_dir(&done.directory).unwrap();
        std::fs::write(done.directory.join("console.log"), "original").unwrap();
        store.save(&done).unwrap();
        let mut live = sample("live");
        live.state = "running".into();
        live.finished_at = None;
        live.directory = root.join("live");
        std::fs::create_dir(&live.directory).unwrap();
        store.save(&live).unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        let restored = store.restore(ARCHIVE_AFTER_MS + 10).unwrap();
        let done = restored.iter().find(|r| r.task.task_id == "done").unwrap();
        assert!(done.archived);
        assert_eq!(
            std::fs::read_to_string(done.directory.join("console.log")).unwrap(),
            "original"
        );
        let live = restored.iter().find(|r| r.task.task_id == "live").unwrap();
        assert_eq!(live.state, "interrupted");
        assert!(!live.archived);
        assert_eq!(live.result["error_code"], "manager_interrupted");
    }
    /// 跨轮次恢复最近五条回复，当前轮不重复计数，失败轮保留已完成的回复样本。
    #[test]
    fn recent_replies_span_turns_without_counting_current_twice() {
        let mut record = sample("replies");
        record.turn = 2;
        let result = serde_json::json!({"state":"succeeded","outcome":{"metrics":{"reply_durations":[
            {"duration_ms":100,"source":"claude_request"},
            {"duration_ms":200,"source":"claude_request"},
            {"duration_ms":300,"source":"claude_request"},
            {"duration_ms":400,"source":"claude_request"}
        ]}}});
        record
            .turns
            .push(crate::statistics::TurnSummary::from_result(
                1,
                record.task.backend,
                Default::default(),
                &result,
            ));
        let current = serde_json::json!({"state":"succeeded","outcome":{"metrics":{"reply_durations":[
            {"duration_ms":500,"source":"claude_request"}
        ]}}});
        record.metrics = serde_json::from_value(current["outcome"]["metrics"].clone()).unwrap();
        record
            .turns
            .push(crate::statistics::TurnSummary::from_result(
                2,
                record.task.backend,
                Default::default(),
                &current,
            ));
        assert_eq!(
            record
                .recent_replies()
                .iter()
                .map(|r| r.duration_ms)
                .collect::<Vec<_>>(),
            [500, 400, 300, 200, 100]
        );
        record.turn = 3;
        record.metrics = Default::default();
        let restored: Record =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        assert_eq!(
            restored
                .recent_replies()
                .iter()
                .map(|r| r.duration_ms)
                .collect::<Vec<_>>(),
            [500, 400, 300, 200, 100]
        );
        let mut failed = restored;
        failed.state = "failed".into();
        failed.result = serde_json::json!({"state":"failed"});
        failed
            .metrics
            .reply_durations
            .push_back(crate::metrics::ReplyDuration {
                duration_ms: 600,
                source: "claude_request".into(),
            });
        crate::statistics::finish_turn(&mut failed);
        failed.turn = 4;
        failed.metrics = Default::default();
        assert_eq!(
            failed
                .recent_replies()
                .iter()
                .map(|r| r.duration_ms)
                .collect::<Vec<_>>(),
            [600, 500, 400, 300, 200]
        );
    }
    /// 构造固定测试元数据，不启动 CLI。
    pub fn sample(id: &str) -> Record {
        Record {
            reviews: Vec::new(),
            reworks: Vec::new(),
            turns: Vec::new(),
            metrics: Default::default(),
            health: Default::default(),
            task: Task {
                task_id: id.to_owned(),
                title: None,
                group_path: vec![],
                backend: crate::protocol::Backend::Claude,
                workdir: std::env::temp_dir(),
                prompt: "test".to_owned(),
                model: None,
                effort: None,
                resume_session: None,
                allow_edits: false,
                clean_start: true,
                timeout_seconds: 1,
                retry_timeout_seconds: 60,
            },
            directory: std::env::temp_dir(),
            state: "succeeded".to_owned(),
            created_at: 0,
            last_activity: 0,
            finished_at: Some(0),
            archived: false,
            result: Value::Null,
            current_tool: String::new(),
            lines: VecDeque::new(),
            turn: 0,
            session_id: None,
            profile: None,
            elapsed_ms: 0,
            turn_started_at: 0,
            deleted: false,
        }
    }
}
