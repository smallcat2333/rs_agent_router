//! 后台调用及可靠留痕；复用 cli-stream 管理管道和 Windows Job Object。
use crate::protocol::{Backend, Outcome, Profile, Task};
use anyhow::{Context, Result};
use cli_stream::{Command, Event, ProcessHandle, Stdin};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{RecvTimeoutError, Sender},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// 执行更新交给管理器关联任务 ID，再分发给 UI 和等待客户端。
#[derive(Clone)]
pub enum Update {
    ProcessStarted(ProcessHandle),
    Metrics(Box<crate::metrics::Metrics>),
    /// 部分文本仅更新活动时间，完整消息仍由既有协议处理以避免重复显示。
    Activity(i64),
    Line {
        text: String,
        activity: bool,
        at_ms: i64,
    },
    Finished(Value),
}

/// 离开运行作用域时终止仍存活的子进程，避免日志写入失败留下孤儿任务。
struct ChildGuard(ProcessHandle);
impl Drop for ChildGuard {
    /// 已退出进程无需处理，未退出进程通过上游取消整棵进程树。
    fn drop(&mut self) {
        if self.0.pid().is_some() {
            let _ = self.0.cancel();
        }
    }
}

/// 返回 Unix 毫秒，供日志跨进程关联。
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_millis()
}

/// 报告展示消息和真实活动时间，诊断消息不参与活动排序。
fn publish(tx: &Sender<Update>, _task_id: &str, line: &str) {
    let activity =
        !line.trim().is_empty() && !line.starts_with("CLI 日志") && !line.starts_with("CLI 输出");
    let _ = tx.send(Update::Line {
        text: line.to_owned(),
        activity,
        at_ms: now_ms() as i64,
    });
}

/// 构造逐项参数；提示词经 stdin 传输，避免 shell 转义和命令行长度限制。
pub fn arguments(task: &Task, profile: &Profile) -> Vec<String> {
    let mut args: Vec<String> = if task.backend == Backend::Codex {
        vec![
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--color",
            "never",
            "--sandbox",
            if task.allow_edits {
                "workspace-write"
            } else {
                "read-only"
            },
        ]
    } else {
        vec![
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--strict-mcp-config",
            "--permission-mode",
            "dontAsk",
            "--tools",
            if task.allow_edits {
                "Read,Glob,Grep,Edit,Write"
            } else {
                "Read,Glob,Grep"
            },
            "--allowedTools",
            if task.allow_edits {
                "Read,Glob,Grep,Edit,Write"
            } else {
                "Read,Glob,Grep"
            },
        ]
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(model) = task.model.as_ref().or(profile.model.as_ref()) {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(effort) = task.effort.as_ref().or(profile.effort.as_ref()) {
        if task.backend == Backend::Codex {
            args.extend(["-c".to_owned(), format!("model_reasoning_effort={effort}")]);
        } else {
            args.extend(["--effort".to_owned(), effort.clone()]);
        }
    }
    if let Some(session) = &task.resume_session {
        if task.backend == Backend::Codex {
            args.extend(["resume".to_owned(), session.clone()]);
        } else {
            args.extend(["--resume".to_owned(), session.clone()]);
        }
    }
    if task.backend == Backend::Codex {
        args.push("-".to_owned());
    }
    args
}

/// 预建唯一任务目录，重复 ID 直接失败，不覆盖先前审计记录。
pub fn prepare(root: &Path, task: &Task) -> Result<PathBuf> {
    task.validate()?;
    fs::create_dir_all(root)?;
    let directory = root.join(&task.task_id);
    fs::create_dir(&directory)
        .context("task ID already exists or run directory cannot be created")?;
    fs::write(
        directory.join("request.json"),
        serde_json::to_vec_pretty(task)?,
    )?;
    Ok(directory)
}

/// 工作线程入口；无论启动失败或协议失败，都写统一终态并通知 UI。
pub fn run(
    task: Task,
    profile: Profile,
    directory: PathBuf,
    cancel: Arc<AtomicU8>,
    tx: Sender<Update>,
) {
    let started = now_ms();
    let result = execute(&task, &profile, &directory, cancel.clone(), &tx);
    let report = match result {
        Ok(value) => value,
        Err(error) => json!({"state":"failed","error":format!("{error:#}"),"task_id":task.task_id}),
    };
    let mut report = report;
    report["started_at_ms"] = json!(started);
    report["finished_at_ms"] = json!(now_ms());
    report["requested_model"] = json!(task.model.as_ref().or(profile.model.as_ref()));
    report["backend"] = json!(task.backend);
    report["requested_effort"] = json!(task.effort);
    report["session_persisted"] = json!(true);
    report["router_version"] = json!(env!("CARGO_PKG_VERSION"));
    report["executor"] = crate::metrics::executor(
        task.backend,
        task.model.as_deref().or(profile.model.as_deref()),
        task.effort.as_deref(),
        report["outcome"]["reported_model"].as_str(),
    );
    if task.backend == Backend::Codex
        && let Some(session) = report["outcome"]["session_id"].as_str()
    {
        match crate::metrics::codex_metadata(session) {
            Ok(metadata) => {
                report["executor"] = crate::metrics::executor(
                    task.backend,
                    task.model.as_deref(),
                    metadata.effort.as_deref().or(task.effort.as_deref()),
                    metadata.model.as_deref(),
                );
                if metadata.model.is_some() && report["executor"]["model_source"] == "cli_reported"
                {
                    report["executor"]["model_source"] = json!("native_session");
                }
                if metadata.effort.is_some() {
                    report["executor"]["effort_source"] = json!("native_session");
                }
                report["executor"]["identity_path"] = json!(metadata.path);
                report["executor"]["requested_effort"] = json!(task.effort);
                report["outcome"]["reported_model"] = json!(metadata.model);
                report["outcome"]["metrics"]["context_tokens"] = json!(metadata.context_tokens);
                report["outcome"]["metrics"]["context_window_tokens"] =
                    json!(metadata.context_window_tokens);
            }
            Err(error) => {
                report["executor"]["identity_error"] = json!(error.to_string());
            }
        }
    }
    if let Some(metrics) = report["outcome"].get("metrics") {
        let _ = tx.send(Update::Metrics(Box::new(
            serde_json::from_value(metrics.clone()).expect("internal metrics schema"),
        )));
    }
    if cancel.load(Ordering::Relaxed) == 2 {
        report["state"] = json!("cancelled");
        report["error_code"] = json!("manager_shutdown");
        report["error"] = json!("管理页退出，任务已终止");
    }
    if let Err(error) = fs::write(
        directory.join("result.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    ) {
        report["state"] = json!("failed");
        report["error_code"] = json!("result_write_failed");
        publish(&tx, &task.task_id, &format!("结果写入失败：{error}"));
    }
    publish(
        &tx,
        &task.task_id,
        &format!(
            "任务结束 · {} · 记录：{}",
            report["state"],
            directory.display()
        ),
    );
    let _ = tx.send(Update::Finished(report));
}

/// 连续无 CLI 输出达到阈值才超时；持续输出不限制总时长，终止后等待进程树回收。
fn execute(
    task: &Task,
    profile: &Profile,
    directory: &Path,
    cancel: Arc<AtomicU8>,
    tx: &Sender<Update>,
) -> Result<Value> {
    let args = arguments(task, profile);
    fs::write(
        directory.join("launch.json"),
        serde_json::to_vec_pretty(
            &json!({"program":profile.program,"args":args,"workdir":task.workdir,"prompt_transport":"stdin"}),
        )?,
    )?;
    let mut raw = File::create(directory.join("stdout.jsonl"))?;
    let mut errors = File::create(directory.join("stderr.log"))?;
    let mut console = File::create(directory.join("console.log"))?;
    let mut events_file = File::create(directory.join("events.jsonl"))?;
    publish(tx, &task.task_id, "正在启动 CLI");
    let (handle, rx) = Command::new(&profile.program)
        .cwd(&task.workdir)
        .args(args)
        .run_id(&task.task_id)
        .stdin(Stdin::Piped)
        .start()?;
    let child = ChildGuard(handle);
    let _ = tx.send(Update::ProcessStarted(child.0.clone()));
    let pid = child.0.pid();
    // 单独发送长提示词，主事件循环持续排空 stdout，避免双向管道互相等待。
    let input_handle = child.0.clone();
    let prompt = task.prompt.clone();
    let writer = std::thread::spawn(move || -> Result<()> {
        input_handle.write(prompt.as_bytes())?;
        input_handle.close_stdin()?;
        Ok(())
    });
    let start = Instant::now();
    let mut last_output = start;
    let idle_timeout = Duration::from_secs(task.timeout_seconds);
    let mut stopped: Option<&str> = None;
    let mut outcome = Outcome::default();
    let mut process_error = None;
    let mut last_text_activity = Instant::now();
    let exit_code;
    loop {
        if stopped.is_none() {
            if cancel.load(Ordering::Relaxed) != 0 {
                stopped = Some("cancelled");
            }
            if let Some(reason) = stopped {
                child.0.cancel()?;
                publish(tx, &task.task_id, reason);
            }
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                if matches!(&event, Event::Stdout { line, .. } | Event::Stderr { line, .. } if !line.trim().is_empty())
                {
                    last_output = Instant::now();
                }
                writeln!(
                    events_file,
                    "{}",
                    json!({"timestamp_ms":now_ms(),"event":event})
                )?;
                let lines = match event {
                    Event::Started { .. } => vec![format!("运行中 · PID {}", pid.unwrap_or(0))],
                    Event::Stdout { line, .. } => {
                        writeln!(raw, "{line}")?;
                        match serde_json::from_str::<Value>(&line) {
                            Ok(value) => {
                                let old = outcome.metrics.clone();
                                outcome.metrics.observe(
                                    task.backend,
                                    &value,
                                    start.elapsed().as_millis() as u64,
                                );
                                if old != outcome.metrics {
                                    let _ =
                                        tx.send(Update::Metrics(Box::new(outcome.metrics.clone())));
                                }
                                if value["type"] == "stream_event"
                                    && crate::metrics::visible_text(task.backend, &value)
                                    && (old.first_text_ms.is_none()
                                        || last_text_activity.elapsed()
                                            >= Duration::from_millis(250))
                                {
                                    let _ = tx.send(Update::Activity(now_ms() as i64));
                                    last_text_activity = Instant::now();
                                }
                                outcome.observe(task.backend, &value)
                            }
                            Err(_) => vec![format!("CLI 输出 · {line}")],
                        }
                    }
                    Event::Stderr { line, .. } => {
                        writeln!(errors, "{line}")?;
                        vec![format!("CLI 日志 · {line}")]
                    }
                    Event::Error { message, .. } => {
                        process_error = Some(message.clone());
                        vec![format!("进程错误 · {message}")]
                    }
                    Event::Exited {
                        exit_code: code,
                        cancelled,
                        ..
                    } => {
                        // 静默超时在本地设置 stopped，其他取消不能冒充超时。
                        if cancelled && stopped.is_none() {
                            stopped = Some("cancelled");
                        }
                        exit_code = code;
                        break;
                    }
                    _ => Vec::new(),
                };
                for line in lines {
                    writeln!(console, "{line}")?;
                    publish(tx, &task.task_id, &line);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("process event stream closed without exit event")
            }
        }
        if stopped.is_none() && last_output.elapsed() >= idle_timeout {
            stopped = Some(if cancel.load(Ordering::Relaxed) != 0 {
                "cancelled"
            } else {
                "timed_out"
            });
            child.0.cancel()?;
            publish(tx, &task.task_id, stopped.unwrap());
        }
    }
    let input_result = writer
        .join()
        .map_err(|_| anyhow::anyhow!("prompt writer panicked"))?;
    if stopped.is_none() {
        input_result?;
    }
    let state = stopped.unwrap_or(if outcome.succeeded(exit_code) && process_error.is_none() {
        "succeeded"
    } else {
        "failed"
    });
    let mut report = json!({"task_id":task.task_id,"state":state,"exit_code":exit_code,"pid":pid,"duration_ms":start.elapsed().as_millis(),"outcome":outcome,"process_error":process_error});
    if state == "timed_out" {
        report["error_code"] = json!("idle_timeout");
        report["error"] = json!(format!(
            "连续 {} 秒没有 CLI 输出，已终止任务进程树",
            task.timeout_seconds
        ));
    }
    Ok(report)
}
