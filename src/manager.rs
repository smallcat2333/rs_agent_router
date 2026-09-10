//! 常驻任务管理：无并发上限、独立取消、订阅、持久化和可控时钟归档。
use crate::{
    ipc::{Envelope, Request},
    protocol::{Profiles, Routing, Work},
    review::{ReviewInput, ReviewRecord, ReworkInput, ReworkRecord, Verdict},
    runner::{self, Update},
    store::{Health, Record, Store},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
};

/// 每个后台任务独立持有控制权；订阅者断开不影响此对象。
struct Running {
    process: Option<cli_stream::ProcessHandle>,
    stop: Arc<AtomicU8>,
    events: Receiver<Update>,
    worker: JoinHandle<()>,
}

/// GUI 线程是元数据唯一写者，工作线程仅执行 CLI 并报告事件。
pub struct Manager {
    pub statistics_url: Option<String>,
    started: std::time::Instant,
    pub records: HashMap<String, Record>,
    pub profiles: Profiles,
    pub routing: Routing,
    pub root: PathBuf,
    pub shutting_down: bool,
    pub error: String,
    pub archive_hours: u64,
    store: Store,
    running: HashMap<String, Running>,
    subscribers: HashMap<String, Vec<SyncSender<Value>>>,
    last_archive: i64,
}

impl Manager {
    /// 恢复索引和终态；配置可在 GUI 中修复，数据库不可用直接报错。
    #[cfg(test)]
    pub fn new(root: PathBuf, profiles: Profiles, now: i64) -> Result<Self> {
        Self::new_with_archive(root, profiles, now, 4)
    }
    /// 从启动配置恢复任务，首次恢复和周期归档使用相同的保留时长。
    pub fn new_with_archive(
        root: PathBuf,
        profiles: Profiles,
        now: i64,
        archive_hours: u64,
    ) -> Result<Self> {
        ensure!(
            (1..=8760).contains(&archive_hours),
            "归档时间应为 1–8760 小时"
        );
        let store = Store::open(&root)?;
        let records = store
            .restore_with_retention(now, archive_hours as i64 * 3600000)?
            .into_iter()
            .map(|r| (r.task.task_id.clone(), r))
            .collect();
        Ok(Self {
            statistics_url: None,
            started: std::time::Instant::now(),
            records,
            profiles,
            routing: Routing::default(),
            root,
            shutting_down: false,
            error: String::new(),
            archive_hours,
            store,
            running: HashMap::new(),
            subscribers: HashMap::new(),
            last_archive: now,
        })
    }
    /// 活动数量由真实工作线程登记决定，与三个展示位无关。
    pub fn running_count(&self) -> usize {
        self.running.len()
    }
    /// 接收任务时固定模型配置快照，保存目录后立即启动，无队列。
    pub fn submit(&mut self, work: Work, now: i64) -> Result<String> {
        ensure!(!self.shutting_down, "manager_shutdown: 管理页正在退出");
        let profile = self
            .profiles
            .get(self.routing.backend.key())
            .context("CLI profile missing")?
            .clone();
        let task = work.resolve(&self.routing, &profile);
        task.validate()?;
        ensure!(
            !self.records.contains_key(&task.task_id),
            "duplicate_task_id: task already exists"
        );
        let directory = runner::prepare(&self.root, &task)?;
        let id = task.task_id.clone();
        let record = Record {
            reviews: Vec::new(),
            reworks: Vec::new(),
            turns: Vec::new(),
            metrics: Default::default(),
            health: Default::default(),
            task: task.clone(),
            directory: directory.clone(),
            state: "running".to_owned(),
            created_at: now,
            last_activity: now,
            finished_at: None,
            archived: false,
            result: Value::Null,
            current_tool: String::new(),
            lines: VecDeque::new(),
            turn: 1,
            session_id: None,
            profile: Some(profile),
            elapsed_ms: 0,
            turn_started_at: now,
            deleted: false,
        };
        self.launch_turn(&record)?;
        self.records.insert(id.clone(), record);
        Ok(id)
    }
    /// 启动一轮并保存独立日志目录；调用前已验证同任务没有其它运行轮次。
    fn launch_turn(&mut self, record: &Record) -> Result<()> {
        let directory = record.run_directory();
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("request.json"),
            serde_json::to_vec_pretty(&record.task)?,
        )?;
        self.store.save(record)?;
        let task = record.task.clone();
        let profile = record
            .profile
            .clone()
            .context("CLI profile snapshot missing")?;
        let id = task.task_id.clone();
        let stop = Arc::new(AtomicU8::new(0));
        let worker_stop = stop.clone();
        let (tx, events) = mpsc::channel();
        let worker =
            std::thread::spawn(move || runner::run(task, profile, directory, worker_stop, tx));
        self.running.insert(
            id.clone(),
            Running {
                process: None,
                stop,
                events,
                worker,
            },
        );
        Ok(())
    }
    /// 向原 CLI 会话追加工作指令，服务不维护自己的聊天历史。
    pub fn send(&mut self, id: &str, message: String, now: i64) -> Result<()> {
        ensure!(!self.shutting_down, "manager_shutdown");
        ensure!(!message.trim().is_empty(), "消息不能为空");
        let mut record = self.records.get(id).context("task_not_found")?.clone();
        ensure!(
            record.can_resume(),
            "会话正在运行或旧任务未保存会话，请等待完成或新建对话"
        );
        record.turn += 1;
        record.task.prompt = message.clone();
        record.task.resume_session = record.session_id.clone();
        record.state = "running".into();
        record.finished_at = None;
        record.archived = false;
        record.last_activity = now;
        record.turn_started_at = now;
        record.current_tool.clear();
        record.lines.clear();
        record.metrics = Default::default();
        record.health = Default::default();
        self.launch_turn(&record)?;
        self.records.insert(id.to_owned(), record);
        Ok(())
    }
    /// 一轮一次正式审计；相同 ID/内容幂等返回，历史、活动排序和执行状态不变。
    pub fn review(
        &mut self,
        id: &str,
        input: ReviewInput,
        now: i64,
    ) -> Result<(ReviewRecord, bool)> {
        ensure!(!self.shutting_down, "manager_shutdown");
        let score = input.validate()?;
        let mut record = self.records.get(id).context("task_not_found")?.clone();
        if let Some(existing) = record
            .reviews
            .iter()
            .find(|r| r.input.review_id == input.review_id)
        {
            ensure!(
                existing.input == input,
                "review_id_conflict: same ID has different content"
            );
            return Ok((existing.clone(), true));
        }
        ensure!(
            !record.running(),
            "task_running: wait for terminal state before review"
        );
        ensure!(
            input.turn == record.turn,
            "stale_review: turn does not match current execution"
        );
        ensure!(
            record.current_review().is_none(),
            "turn_already_reviewed: use the original review_id for retries"
        );
        let review = ReviewRecord {
            input,
            score,
            created_at_ms: now,
        };
        record.reviews.push(review.clone());
        self.store.save(&record)?;
        self.records.insert(id.to_owned(), record);
        Ok((review, false))
    }
    /// 显式返工绑定失败审计；只在派发被接收时计数，同 ID 重发不新建执行轮。
    pub fn rework(&mut self, id: &str, input: ReworkInput, now: i64) -> Result<(u32, bool)> {
        ensure!(!self.shutting_down, "manager_shutdown");
        input.validate()?;
        let mut record = self.records.get(id).context("task_not_found")?.clone();
        if let Some(existing) = record
            .reworks
            .iter()
            .find(|r| r.input.request_id == input.request_id)
        {
            ensure!(
                existing.input == input,
                "rework_id_conflict: same ID has different content"
            );
            return Ok((existing.turn, true));
        }
        ensure!(
            !record.running() && !record.deleted,
            "task_unavailable: running or deleted"
        );
        let review = record
            .current_review()
            .context("review_required: review current turn before rework")?;
        ensure!(
            review.input.review_id == input.review_id && review.input.verdict == Verdict::Rework,
            "rework_review_mismatch"
        );
        ensure!(
            record.turn > 0 && record.profile.is_some(),
            "profile_snapshot_missing: create a new task for legacy records"
        );
        let from_turn = record.turn;
        record.turn += 1;
        record.task.prompt = input.message.clone();
        // CLI 尚未建立原生会话时仍能返工，但提示词须包含完整上下文。
        record.task.resume_session = record.session_id.clone();
        record.state = "running".into();
        record.finished_at = None;
        record.archived = false;
        record.last_activity = now;
        record.turn_started_at = now;
        record.current_tool.clear();
        record.lines.clear();
        record.metrics = Default::default();
        record.health = Default::default();
        record.reworks.push(ReworkRecord {
            input,
            from_turn,
            turn: record.turn,
            created_at_ms: now,
        });
        self.launch_turn(&record)?;
        let turn = record.turn;
        self.records.insert(id.to_owned(), record);
        Ok((turn, false))
    }
    /// 幂等返工等待绑定原轮次；已完成的旧轮次直接重放终态，不订阅后续工作。
    fn wait_rework(&mut self, id: &str, turn: u32, reply: SyncSender<Value>) {
        let record = &self.records[id];
        if record.turn == turn && record.running() {
            self.subscribers
                .entry(id.to_owned())
                .or_default()
                .push(reply);
            return;
        }
        let directory = record.directory.join("turns").join(format!("{turn:04}"));
        let loaded = if record.turn == turn {
            Ok(record.result.clone())
        } else {
            std::fs::read(directory.join("result.json"))
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?))
        };
        let value = match loaded {
            Ok(report) => {
                json!({"type":"finished","task_id":id,"turn":turn,"state":record.turns.iter().find(|t|t.turn==turn).map(|t|t.state.as_str()).unwrap_or("failed"),
                "result_path":directory.join("result.json"),"turn_result_path":directory.join("result.json"),"result":report,"rework_count":record.reworks.len(),"idempotent":true})
            }
            Err(error) => {
                json!({"type":"error","task_id":id,"error_code":"result_unavailable","error":error.to_string()})
            }
        };
        let _ = reply.try_send(value);
    }
    /// 手动归档只改变视图，运行任务必须先结束。
    pub fn archive(&mut self, id: &str, archived: bool) -> Result<()> {
        let record = self.records.get_mut(id).context("task_not_found")?;
        ensure!(!record.running() && !record.deleted, "请先结束任务");
        record.archived = archived;
        self.store.save(record)
    }
    /// 删除作持久隐藏标记，保留日志和任务 ID，不复用旧目录。
    pub fn delete(&mut self, id: &str) -> Result<()> {
        let record = self.records.get_mut(id).context("task_not_found")?;
        ensure!(!record.running(), "请先结束任务");
        record.deleted = true;
        self.store.save(record)
    }
    /// 删除所选分组快照中的终态任务；执行时再核对状态，避免误删菜单打开后续跑的任务。
    pub fn delete_group(&mut self, ids: &[String]) -> Result<()> {
        for id in ids {
            let record = self.records.get(id).context("task_not_found")?;
            if !record.running() && !record.deleted {
                self.delete(id)?;
            }
        }
        Ok(())
    }
    /// 请求取消但不提前宣布完成，等待进程树退出事件。
    pub fn cancel(&self, id: &str) -> Result<()> {
        let task = self
            .running
            .get(id)
            .context("task_not_running: task missing or already finished")?;
        task.stop.store(1, Ordering::Relaxed);
        Ok(())
    }
    /// 确认退出后拒绝新任务，并给每个运行线程标记管理器退出原因。
    pub fn shutdown(&mut self) {
        self.shutting_down = true;
        for task in self.running.values() {
            task.stop.store(2, Ordering::Relaxed);
        }
    }
    /// 发送事件给等待客户端；慢客户端只能丢失自己的订阅，不拖慢执行。
    fn broadcast(&mut self, id: &str, value: Value) {
        if let Some(subscribers) = self.subscribers.get_mut(id) {
            subscribers.retain(|tx| tx.try_send(value.clone()).is_ok());
        }
    }
    /// 查询工作线程和其持有的进程句柄；静默输出不改变健康判断或活动排序。
    fn task_health(&self, id: &str) -> Health {
        let Some(task) = self.running.get(id) else {
            return Health {
                phase: "finished".into(),
                process_alive: Some(false),
                ..Default::default()
            };
        };
        let worker_alive = !task.worker.is_finished();
        let pid = task.process.as_ref().and_then(|p| p.pid());
        let process_alive = pid.and_then(crate::ipc::process_alive);
        let phase = if !worker_alive {
            "settling"
        } else if task.stop.load(Ordering::Relaxed) != 0 {
            "stopping"
        } else if task.process.is_none() {
            "starting"
        } else if pid.is_none() || process_alive == Some(false) {
            "reaping"
        } else if process_alive == Some(true) {
            "running"
        } else {
            "unknown"
        };
        Health {
            phase: phase.into(),
            pid,
            process_alive,
            worker_alive,
        }
    }

    /// 被动探活返回有界证据，不读取日志；探活响应本身证明管理泵仍在响应。
    pub fn health(&self, task_id: Option<&str>, now: i64) -> Value {
        let mut value = json!({"type":"health","manager_pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),
            "uptime_ms":self.started.elapsed().as_millis(),"accepting_tasks":!self.shutting_down,
            "running_tasks":self.running_count(),"checked_at_ms":now});
        if let Some(id) = task_id {
            let Some(record) = self.records.get(id).filter(|r| !r.deleted) else {
                return json!({"type":"error","error_code":"task_not_found","error":id});
            };
            value["task"] = json!({"task_id":id,"state":record.state,"health":self.task_health(id),
                "last_activity_at_ms":record.last_activity,"idle_ms":(now-record.last_activity).max(0),
                "elapsed_ms":record.elapsed(now),"turn_elapsed_ms":record.turn_elapsed(now),
                "timeout_seconds":record.task.timeout_seconds});
        }
        value
    }
    /// 处理单个 CLI 命令；返回 true 表示用户要求显示主窗口。
    pub fn request(&mut self, envelope: Envelope, now: i64) -> bool {
        let Envelope { request, reply } = envelope;
        if let Request::Health { task_id } = &request {
            let _ = reply.try_send(self.health(task_id.as_deref(), now));
            return false;
        }
        if self.shutting_down {
            let _ = reply.try_send(
                json!({"type":"error","error_code":"manager_shutdown","error":"管理页正在退出"}),
            );
            return false;
        }
        match request {
            Request::Review { task_id, review } => {
                let value = match self.review(&task_id, review, now) {
                    Ok((record, idempotent)) => {
                        json!({"type":"reviewed","task_id":task_id,"review_id":record.input.review_id,"turn":record.input.turn,"score":record.score,"verdict":record.input.verdict,"idempotent":idempotent})
                    }
                    Err(error) => {
                        json!({"type":"error","error_code":"review_rejected","error":error.to_string()})
                    }
                };
                let _ = reply.try_send(value);
            }
            Request::Rework {
                task_id,
                rework,
                wait,
            } => match self.rework(&task_id, rework, now) {
                Ok((turn, idempotent)) => {
                    let _=reply.try_send(json!({"type":"accepted","task_id":task_id,"turn":turn,"rework_count":self.records[&task_id].reworks.len(),"idempotent":idempotent,"manager_pid":std::process::id()}));
                    if wait {
                        self.wait_rework(&task_id, turn, reply);
                    }
                }
                Err(error) => {
                    let _=reply.try_send(json!({"type":"error","error_code":"rework_rejected","error":error.to_string()}));
                }
            },
            Request::Statistics { .. } => {
                let value = match &self.statistics_url {
                    Some(url) => json!({"type":"statistics_page","url":url}),
                    None => {
                        json!({"type":"error","error_code":"statistics_unavailable","error":"statistics listener has not started"})
                    }
                };
                let _ = reply.try_send(value);
            }
            Request::StatisticsData => {
                let _ = reply.try_send(crate::statistics::snapshot(self.records.values(), now));
            }
            Request::Health { .. } => unreachable!("health handled above"),
            Request::Show => {
                let _ = reply.try_send(json!({"type":"shown","manager_pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),"runs_dir":self.root}));
                return true;
            }
            Request::Submit { task, wait } => {
                match self.submit(task, now) {
                    Ok(id) => {
                        let _ = reply.try_send(json!({"type":"accepted","task_id":id,"manager_pid":std::process::id()}));
                        if wait {
                            self.subscribers.entry(id).or_default().push(reply);
                        }
                    }
                    Err(error) => {
                        let _ = reply.try_send(json!({"type":"error","error_code":"submit_rejected","error":format!("{error:#}")}));
                    }
                }
            }
            Request::Send {
                task_id,
                message,
                wait,
            } => match self.send(&task_id, message, now) {
                Ok(()) => {
                    let _=reply.try_send(json!({"type":"accepted","task_id":task_id,"turn":self.records[&task_id].turn,"manager_pid":std::process::id()}));
                    if wait {
                        self.subscribers.entry(task_id).or_default().push(reply);
                    }
                }
                Err(error) => {
                    let _=reply.try_send(json!({"type":"error","error_code":"send_rejected","error":error.to_string()}));
                }
            },
            Request::Status { task_id } => {
                let response = match self.records.get(&task_id) {
                    Some(record) if !record.deleted => {
                        json!({"type":"status","task":record,"health":self.task_health(&task_id),"elapsed_ms":record.elapsed(now),"result_path":record.directory.join("result.json"),"turn":record.turn,"turn_result_path":record.run_directory().join("result.json"),"manager_pid":std::process::id(),
                            "review":record.current_review().map(|r|json!({"review_id":r.input.review_id,"turn":r.input.turn,"score":r.score,"verdict":r.input.verdict})),"rework_count":record.reworks.len()})
                    }
                    _ => json!({"type":"error","error_code":"task_not_found","error":task_id}),
                };
                let _ = reply.try_send(response);
            }
            Request::Cancel { task_id } => {
                let response = match self.cancel(&task_id) {
                    Ok(()) => json!({"type":"cancel_requested","task_id":task_id}),
                    Err(error) => {
                        json!({"type":"error","error_code":"cancel_rejected","error":error.to_string()})
                    }
                };
                let _ = reply.try_send(response);
            }
        }
        false
    }
    /// 每次帧更新消费事件；同任务同帧只保存一次 SQLite，避免逐 token 写库。
    pub fn poll(&mut self, now: i64) -> Result<()> {
        let ids: Vec<_> = self.running.keys().cloned().collect();
        for id in ids {
            let mut terminal = None;
            let mut dirty = false;
            let worker_finished = self.running[&id].worker.is_finished();
            let messages: Vec<_> = self.running[&id].events.try_iter().take(512).collect();
            let messages_count = messages.len();
            for message in messages {
                match message {
                    Update::ProcessStarted(process) => {
                        self.running.get_mut(&id).unwrap().process = Some(process)
                    }
                    Update::Metrics(metrics) => {
                        self.records.get_mut(&id).unwrap().metrics = *metrics;
                        dirty = true;
                    }
                    Update::Activity(at_ms) => {
                        let record = self.records.get_mut(&id).unwrap();
                        record.last_activity = at_ms.max(record.last_activity);
                        dirty = true;
                    }
                    Update::Line {
                        text,
                        activity,
                        at_ms,
                    } => {
                        let record = self.records.get_mut(&id).unwrap();
                        record.append(text.clone());
                        if activity {
                            record.last_activity = at_ms.max(record.last_activity + 1);
                            if text.starts_with("工具调用") {
                                record.current_tool = text.chars().take(180).collect();
                            } else if text.starts_with("工具结果") {
                                record.current_tool.clear();
                            }
                            dirty = true;
                        }
                        self.broadcast(&id,json!({"type":"progress","task_id":id,"message":text,"activity":activity,"timestamp_ms":at_ms}));
                    }
                    Update::Finished(report) => terminal = Some(report),
                }
            }
            if terminal.is_none() && worker_finished && messages_count == 0 {
                terminal = Some(
                    json!({"task_id":id,"state":"failed","error_code":"worker_interrupted","error":"任务线程退出且未产生终态","finished_at_ms":now}),
                );
            }
            if let Some(mut report) = terminal {
                if self.shutting_down {
                    report["state"] = json!("cancelled");
                    report["error_code"] = json!("manager_shutdown");
                    report["error"] = json!("管理页退出，任务已终止");
                }
                let record = self.records.get_mut(&id).unwrap();
                record.state = report["state"].as_str().unwrap_or("failed").to_owned();
                record.finished_at = Some(report["finished_at_ms"].as_i64().unwrap_or(now));
                record.last_activity = record.finished_at.unwrap().max(record.last_activity + 1);
                record.current_tool.clear();
                record.result = report;
                record.elapsed_ms += record.result["duration_ms"]
                    .as_i64()
                    .unwrap_or_else(|| record.finished_at.unwrap() - record.turn_started_at)
                    .max(0);
                if let Some(session) = record.result["outcome"]["session_id"].as_str() {
                    record.session_id = Some(session.to_owned());
                }
                crate::statistics::finish_turn(record);
                let persistence = std::fs::write(
                    record.directory.join("result.json"),
                    serde_json::to_vec_pretty(&record.result)?,
                )
                .map_err(anyhow::Error::from)
                .and_then(|_| self.store.save(record));
                if let Err(error) = persistence {
                    // 不能因结果路径不可写而永远占用运行位；错误仍回传 Harness。
                    record.state = "failed".to_owned();
                    record.result["state"] = json!("failed");
                    record.result["error_code"] = json!("persistence_failed");
                    record.result["error"] = json!(format!("结果持久化失败：{error:#}"));
                    crate::statistics::finish_turn(record);
                    self.error = format!("任务 {id} 结果持久化失败：{error:#}");
                    let _ = self.store.save(record);
                }
                let response = json!({"type":"finished","task_id":id,"state":record.state,"elapsed_ms":record.elapsed_ms,"turn":record.turn,"rework_count":record.reworks.len(),"turn_result_path":record.run_directory().join("result.json"),"error_code":record.result["error_code"],"error":record.result["error"],"result_path":record.directory.join("result.json"),"result":record.result});
                // 先回收线程，后发布任务完成，退出确认不能早于真正回收。
                let task = self.running.remove(&id).unwrap();
                let _ = task.worker.join();
                self.broadcast(&id, response);
                self.subscribers.remove(&id);
            } else if dirty {
                self.store.save(&self.records[&id])?;
            }
            let health = self.task_health(&id);
            self.records.get_mut(&id).unwrap().health = health;
        }
        if now - self.last_archive >= 60000 {
            for record in self.records.values_mut() {
                if record.archive_due_after(now, self.archive_hours as i64 * 3600000) {
                    record.archived = true;
                    self.store.save(record)?;
                }
            }
            self.last_archive = now;
        }
        Ok(())
    }
}
impl Drop for Manager {
    /// 最后一道生命周期保证：释放管理器前取消并回收所有持有的工作线程。
    fn drop(&mut self) {
        self.shutdown();
        for (_, task) in self.running.drain() {
            let _ = task.worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 评分幂等、不可覆盖、不能写运行/过期轮次；软删除历史仍能审计，排序不变。
    #[test]
    fn reviews_are_durable_without_activity_changes() {
        let root = std::env::temp_dir().join(format!("router-review-{}", runner::now_ms()));
        let mut manager = Manager::new(root.clone(), Profiles::new(), 10).unwrap();
        let mut record = crate::store::tests::sample("graded");
        record.turn = 1;
        record.last_activity = 25;
        record.deleted = true;
        manager.records.insert("graded".into(), record);
        let input = crate::review::tests::sample("r1", 1, Verdict::Accepted);
        let (grade, duplicate) = manager.review("graded", input.clone(), 100).unwrap();
        assert_eq!(grade.score, 10);
        assert!(!duplicate);
        assert!(manager.review("graded", input.clone(), 200).unwrap().1);
        let mut conflict = input.clone();
        conflict.summary = "different content".into();
        assert!(manager.review("graded", conflict, 200).is_err());
        let mut another = input.clone();
        another.review_id = "r2".into();
        assert!(manager.review("graded", another.clone(), 200).is_err());
        manager.records.get_mut("graded").unwrap().state = "running".into();
        assert!(manager.review("graded", another, 200).is_err());
        manager.records.get_mut("graded").unwrap().state = "succeeded".into();
        assert_eq!(manager.records["graded"].last_activity, 25);
        assert_eq!(manager.records["graded"].reviews.len(), 1);
        drop(manager);
        let restored = Manager::new(root, Profiles::new(), 300).unwrap();
        assert_eq!(
            restored.records["graded"].current_review().unwrap().score,
            10
        );
        assert_eq!(restored.records["graded"].last_activity, 25);
        assert!(restored.records["graded"].deleted);
    }
    /// 输出积压分帧消费时，不把尚未读到 Finished 的已退出线程误判为异常。
    #[test]
    fn event_backlog_does_not_fake_worker_failure() {
        let root = std::env::temp_dir().join(format!("router-backlog-{}", runner::now_ms()));
        let mut manager = Manager::new(root.clone(), Profiles::new(), 0).unwrap();
        let mut record = crate::store::tests::sample("backlog");
        record.state = "running".into();
        record.finished_at = None;
        record.directory = root.join("backlog");
        std::fs::create_dir(&record.directory).unwrap();
        manager.records.insert("backlog".into(), record);
        let (tx, events) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for _ in 0..1100 {
                tx.send(Update::Line {
                    text: "CLI 日志 · noise".into(),
                    activity: false,
                    at_ms: 0,
                })
                .unwrap();
            }
            tx.send(Update::Finished(
                json!({"state":"succeeded","finished_at_ms":1}),
            ))
            .unwrap();
        });
        while !worker.is_finished() {
            std::thread::yield_now();
        }
        manager.running.insert(
            "backlog".into(),
            Running {
                process: None,
                stop: Arc::new(AtomicU8::new(0)),
                events,
                worker,
            },
        );
        manager.poll(1).unwrap();
        assert_eq!(manager.records["backlog"].state, "running");
        manager.poll(1).unwrap();
        manager.poll(1).unwrap();
        assert_eq!(manager.records["backlog"].state, "succeeded");
    }
    /// 归档和删除仅改元数据，运行中禁止操作，删除后也不删除日志。
    #[test]
    fn archive_delete_preserve_evidence() {
        let root = std::env::temp_dir().join(format!("router-visibility-{}", runner::now_ms()));
        let mut manager = Manager::new(root.clone(), Profiles::new(), 0).unwrap();
        let mut record = crate::store::tests::sample("item");
        record.directory = root.join("item");
        std::fs::create_dir(&record.directory).unwrap();
        std::fs::write(record.directory.join("console.log"), "evidence").unwrap();
        record.state = "running".into();
        manager.records.insert("item".into(), record);
        assert!(manager.archive("item", true).is_err());
        assert!(manager.delete("item").is_err());
        manager.records.get_mut("item").unwrap().state = "succeeded".into();
        manager.archive("item", true).unwrap();
        manager.delete("item").unwrap();
        drop(manager);
        let restored = Manager::new(root, Profiles::new(), 1).unwrap();
        assert!(restored.records["item"].deleted);
        assert!(crate::store::recent(restored.records.values().cloned()).is_empty());
        assert_eq!(
            std::fs::read_to_string(restored.records["item"].directory.join("console.log"))
                .unwrap(),
            "evidence"
        );
    }
    /// 组删除只隐藏所选终态任务，重启后仍保留运行任务、组外任务及全部日志。
    #[test]
    fn group_delete_preserves_running_siblings_and_logs() {
        let root =
            std::env::temp_dir().join(format!("router-group-delete-{}", uuid::Uuid::new_v4()));
        let mut manager = Manager::new(root.clone(), Profiles::new(), 0).unwrap();
        for id in ["done", "nested", "resumed", "outside"] {
            let mut record = crate::store::tests::sample(id);
            record.directory = root.join(id);
            std::fs::create_dir(&record.directory).unwrap();
            std::fs::write(record.directory.join("console.log"), "evidence").unwrap();
            manager.store.save(&record).unwrap();
            manager.records.insert(id.into(), record);
        }
        // 模拟打开分组菜单后，一个任务又开始执行；删除仍必须跳过它。
        manager.records.get_mut("resumed").unwrap().state = "running".into();
        manager
            .delete_group(&["done".into(), "nested".into(), "resumed".into()])
            .unwrap();
        assert!(manager.records["done"].deleted && manager.records["nested"].deleted);
        assert!(!manager.records["resumed"].deleted && !manager.records["outside"].deleted);
        drop(manager);
        let restored = Manager::new(root, Profiles::new(), 1).unwrap();
        assert!(restored.records["done"].deleted && restored.records["nested"].deleted);
        assert!(!restored.records["outside"].deleted);
        for record in restored.records.values() {
            assert_eq!(
                std::fs::read_to_string(record.directory.join("console.log")).unwrap(),
                "evidence"
            );
        }
    }
    /// 结果路径不可写时回传失败且回收运行位，不能无限等待。
    #[test]
    fn result_write_failure_still_settles_task() {
        let root = std::env::temp_dir().join(format!("router-write-error-{}", runner::now_ms()));
        let mut manager = Manager::new(root.clone(), Profiles::new(), 0).unwrap();
        let mut record = crate::store::tests::sample("broken");
        record.state = "running".into();
        record.finished_at = None;
        record.directory = root.join("broken");
        std::fs::create_dir_all(record.directory.join("result.json")).unwrap();
        manager.store.save(&record).unwrap();
        manager.records.insert("broken".into(), record);
        let (tx, events) = mpsc::channel();
        tx.send(Update::Finished(
            json!({"task_id":"broken","state":"succeeded","finished_at_ms":1}),
        ))
        .unwrap();
        drop(tx);
        manager.running.insert(
            "broken".into(),
            Running {
                process: None,
                stop: Arc::new(AtomicU8::new(0)),
                events,
                worker: std::thread::spawn(|| {}),
            },
        );
        manager.poll(2).unwrap();
        assert_eq!(manager.running_count(), 0);
        assert_eq!(
            manager.records["broken"].result["error_code"],
            "persistence_failed"
        );
    }
}
