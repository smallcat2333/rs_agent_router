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

/// 每轮统一附加交付约束；保留原任务文本，不改变任务权限和明确禁止事项。
fn execution_prompt(prompt: &str) -> String {
    format!("Router 执行约束：\n\
代码实现或修复任务，在任务授权范围内完成必要的自测：优先运行项目已有的相关测试或最小可运行验证，失败时修复后重测。不得只写测试而不运行、不得把静态阅读称为测试通过。明确要求不运行工具的任务遵从原要求。\n\
命令执行仅用于本任务必要的检查、构建与测试，遵守指定目录、文件范围和禁止事项；不得借测试绕过只读权限或执行未授权的安装、网络、Git提交、部署、破坏性命令。\n\
如缺少命令工具、环境或依赖，明确报告未执行的命令与阻塞原因，不能伪称已通过。修复涉及范围外文件时停止该修复并报告。\n\
最终默认只回传：1. 修改文件清单及每项一句变化；2. 测试摘要（实际命令、退出码、通过/失败/未执行）；3. 失败证据与遗留阻塞（最小错误片段、日志或文件路径，无则写无）。完整代码和日志留本地，不复制回传，不重复任务全文，不自行评分。\n\n--- Router 任务正文 ---\n{prompt}")
}

/// 构造逐项参数；提示词经 stdin 传输，避免 shell 转义和命令行长度限制。
pub fn arguments(task: &Task, profile: &Profile) -> Vec<String> {
    let mut args: Vec<String> = if task.backend == Backend::Agy {
        // Antigravity CLI 使用与 Claude 兼容的 stream-json 输出。
        vec![
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ]
    } else if task.backend == Backend::Opencode {
        vec!["run", "--format", "json", "--thinking"]
    } else if task.backend == Backend::Codex {
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
                "Read,Glob,Grep,Edit,Write,Bash"
            } else {
                "Read,Glob,Grep"
            },
            "--allowedTools",
            if task.allow_edits {
                "Read,Glob,Grep,Edit,Write,Bash"
            } else {
                "Read,Glob,Grep"
            },
        ]
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    if task.clean_start && task.backend == Backend::Claude {
        // 只改变指令自动加载，不更换认证、模型或既有工具权限。
        args.push("--safe-mode".to_owned());
    }
    if let Some(model) = task.model.as_ref().or(profile.model.as_ref()) {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(effort) = task.effort.as_ref().or(profile.effort.as_ref()) {
        if task.backend == Backend::Codex {
            args.extend(["-c".to_owned(), format!("model_reasoning_effort={effort}")]);
        } else if task.backend == Backend::Opencode {
            args.extend(["--variant".to_owned(), effort.clone()]);
        } else {
            args.extend(["--effort".to_owned(), effort.clone()]);
        }
    }
    if let Some(session) = &task.resume_session {
        if task.backend == Backend::Codex {
            args.extend(["resume".to_owned(), session.clone()]);
        } else if task.backend == Backend::Opencode {
            args.extend(["--session".to_owned(), session.clone()]);
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
    if task.backend == Backend::Opencode && report["state"] == "failed" && report["error"].is_null() {
        report["error_code"] = json!("opencode_error");
        report["error"] = json!(report["outcome"]["error"].as_str().unwrap_or("OpenCode 未正常完成本轮请求"));
    }
    if task.backend == Backend::Opencode
        && let (Some(session), Some(message)) = (report["outcome"]["session_id"].as_str(), report["outcome"]["message_id"].as_str())
    {
        match crate::opencode::identity(&profile.program, &task.workdir, session, message) {
            Ok(model) => {
                report["outcome"]["reported_model"] = json!(model);
                report["outcome"]["metrics"]["reported_model"] = json!(model);
            }
            Err(error) => { report["identity_error"] = json!(error.to_string()); }
        }
    }
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
    let launched_ms = now_ms() as u64;
    let mut args = arguments(task, profile);
    let debug_path = directory.join("claude-debug.log");
    if task.backend == Backend::Claude {
        args.extend(["--debug-file".to_owned(), debug_path.to_string_lossy().into_owned()]);
    }
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
    let mut command = Command::new(&profile.program)
        .cwd(&task.workdir)
        .args(args)
        .run_id(&task.task_id)
        .stdin(Stdin::Piped);
    if task.backend == Backend::Opencode {
        command = command.env(crate::opencode::environment(task.allow_edits));
    }
    if task.backend == Backend::Agy && !task.agy_proxy.is_empty() {
        let socks_url = format!("socks5://{}", task.agy_proxy);
        command = command.env(vec![
            ("HTTP_PROXY".to_string(), socks_url.clone()),
            ("HTTPS_PROXY".to_string(), socks_url.clone()),
            ("ALL_PROXY".to_string(), socks_url),
        ]);
    }
    let (handle, rx) = command.start()?;
    let child = ChildGuard(handle);
    let _ = tx.send(Update::ProcessStarted(child.0.clone()));
    let pid = child.0.pid();
    // 单独发送长提示词，主事件循环持续排空 stdout，避免双向管道互相等待。
    let input_handle = child.0.clone();
    let prompt = execution_prompt(&task.prompt);
    let writer = std::thread::spawn(move || -> Result<()> {
        input_handle.write(prompt.as_bytes())?;
        input_handle.close_stdin()?;
        Ok(())
    });
    let start = Instant::now();
    let mut last_output = start;
    let idle_timeout = Duration::from_secs(task.timeout_seconds);
    let retry_timeout = Duration::from_secs(task.retry_timeout_seconds);
    let mut retry_watch = crate::retry_watch::RetryWatch::default();
    let mut retry_timed_out = false;
    let mut stopped: Option<&str> = None;
    let mut outcome = Outcome::default();
    let mut native = crate::metrics::CodexLiveMetrics::default();
    let mut native_error = None;
    // 每秒增量读取一次；终止后再排空，防止末轮用量落盘晚于 CLI 完成事件。
    let mut refresh_metrics = |outcome: &mut Outcome| {
        if task.backend == Backend::Codex && let Some(session) = outcome.session_id.as_deref() {
            let old = outcome.metrics.clone();
            match native.poll(session, launched_ms, &mut outcome.metrics) {
                Ok(()) => native_error = None,
                Err(error) => {
                    let message = format!("原生 TPS 读取失败：{error:#}");
                    if native_error.as_ref() != Some(&message) {
                        publish(tx, &task.task_id, &message);
                        native_error = Some(message);
                    }
                }
            }
            if old != outcome.metrics {
                let _ = tx.send(Update::Metrics(Box::new(outcome.metrics.clone())));
            }
        }
    };
    let mut last_native_poll = Instant::now();
    let mut process_error = None;
    let mut last_text_activity = Instant::now();
    let exit_code;
    loop {
        if last_native_poll.elapsed() >= Duration::from_secs(1) {
            refresh_metrics(&mut outcome);
            last_native_poll = Instant::now();
        }
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
                                if crate::retry_watch::model_progress(&value) {
                                    retry_watch.recovered();
                                }
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
        if task.backend == Backend::Claude && stopped.is_none() {
            if retry_watch.poll(&debug_path, Instant::now())? {
                let message = format!("上游请求异常，允许 CLI 重试 {} 秒；重复报错不延长等待", task.retry_timeout_seconds);
                writeln!(console, "{message}")?;
                publish(tx, &task.task_id, &message);
            }
            if retry_watch.expired(Instant::now(), retry_timeout) {
                retry_timed_out = cancel.load(Ordering::Relaxed) == 0;
                stopped = Some(if retry_timed_out { "timed_out" } else { "cancelled" });
                child.0.cancel()?;
                publish(tx, &task.task_id, stopped.unwrap());
            }
        }
        if stopped.is_none() && !retry_watch.active() && last_output.elapsed() >= idle_timeout {
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
    refresh_metrics(&mut outcome);
    let state = stopped.unwrap_or(if outcome.succeeded(exit_code) && process_error.is_none() {
        "succeeded"
    } else {
        "failed"
    });
    let mut report = json!({"task_id":task.task_id,"state":state,"exit_code":exit_code,"pid":pid,"duration_ms":start.elapsed().as_millis(),"outcome":outcome,"process_error":process_error});
    if retry_timed_out {
        report["error_code"] = json!("retry_timeout");
        report["error"] = json!(format!("上游异常后重试等待已达到 {} 秒，已终止任务进程树", task.retry_timeout_seconds));
        report["upstream_error"] = json!(retry_watch.error);
    } else if state == "timed_out" {
        report["error_code"] = json!("idle_timeout");
        report["error"] = json!(format!(
            "连续 {} 秒没有 CLI 输出，已终止任务进程树",
            task.timeout_seconds
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod execution_contract_tests {
    use super::*;
    /// Claude 按快照隔离指令；关闭可恢复默认加载，Codex 参数保持原行为。
    #[test]
    fn clean_start_arguments_follow_task_snapshot() {
        let mut task = crate::store::tests::sample("clean-args").task;
        let profile = Profile { program: "cli".into(), model: None, effort: None };
        for backend in [Backend::Claude, Backend::Codex, Backend::Opencode] {
            task.backend = backend;
            for resume in [None, Some("existing-session".to_owned())] {
                task.resume_session = resume;
                for enabled in [true, false] {
                    task.clean_start = enabled;
                    let args = arguments(&task, &profile);
                    assert_eq!(args.iter().any(|arg| arg == "--safe-mode"),
                        enabled && backend == Backend::Claude);
                    assert!(!args.iter().any(|arg| arg == "project_doc_max_bytes=0"));
                    assert_eq!(args.iter().any(|arg| arg == "existing-session"), task.resume_session.is_some());
                    assert!(!args.iter().any(|arg| arg.contains("bypass") || arg == "--bare"));
                }
            }
        }
    }
    /// 开放自测命令不改变只读任务白名单；stdin 完整保留原任务文本。
    #[test]
    fn self_test_tools_respect_edit_permission() {
        let mut task = crate::store::tests::sample("self-test").task;
        task.backend = Backend::Claude;
        let profile = Profile { program: "claude".into(), model: None, effort: None };
        for editable in [false, true] {
            task.allow_edits = editable;
            let args = arguments(&task, &profile);
            for flag in ["--tools", "--allowedTools"] {
                let index = args.iter().position(|arg| arg == flag).unwrap();
                assert_eq!(args[index + 1].split(',').any(|tool| tool == "Bash"), editable);
            }
        }
        let original = "不要运行工具\n只返回测试文本";
        assert!(execution_prompt(original).ends_with(original));
    }
}
