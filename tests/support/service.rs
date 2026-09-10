//! 通过隔离命名管道和真实 Windows 进程验证执行服务，不占用用户正在运行的管理器。
use crate::{
    ipc::{self, Request},
    manager::Manager,
    protocol::{Backend, Profile, Profiles, Work},
    runner, store,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// 请求测试实例并读一条回复；仅等待指定任务时持续消费至终态。
fn exchange(sid: &str, request: Request) -> Value {
    let mut pipe = ipc::connect(sid).unwrap();
    writeln!(pipe, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let waiting = matches!(
        request,
        Request::Submit { wait: true, .. }
            | Request::Send { wait: true, .. }
            | Request::Rework { wait: true, .. }
    );
    let mut reader = BufReader::new(pipe);
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let value: Value = serde_json::from_str(&line).unwrap();
        if !waiting || matches!(value["type"].as_str(), Some("finished" | "error")) {
            return value;
        }
    }
}

/// 测试条件必须有界，失败保留临时目录用于诊断；不终止用户进程。
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(Instant::now() < deadline, "test condition timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 生成工作目录与唯一任务；业务路由仍完全由 Manager 配置。
fn work(root: &std::path::Path, id: &str, prompt: &str) -> Work {
    let dir = root.join(id);
    fs::create_dir_all(&dir).unwrap();
    Work {
        task_id: id.into(),
        title: Some(id.into()),
        group_path: vec![
            "测试Harness".into(),
            "rs_agent_router".into(),
            "Feat_健康".into(),
        ],
        workdir: dir,
        prompt: prompt.into(),
    }
}

/// RAII 保证测试失败后也停止测试工作线程，不影响生产服务。
struct TestPump {
    manager: Arc<Mutex<Manager>>,
    quit: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for TestPump {
    /// 请求退出并等待测试专属进程树回收。
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

/// 覆盖并发、静默健康、客户端断开、取消、独立超时、故障及退出错误。
#[test]
fn local_service_health_lifecycle_and_evidence() {
    use std::os::windows::process::CommandExt;
    let root = std::env::temp_dir().join(format!("router-v4-service-{}", runner::now_ms()));
    fs::create_dir_all(&root).unwrap();
    let fixture = root.join("fixture.exe");
    let output = std::process::Command::new("rustc")
        .args(["--edition", "2024"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_cli.rs"))
        .arg("-o")
        .arg(&fixture)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = Profile {
        program: fixture,
        model: Some("fixture".into()),
        effort: Some("low".into()),
    };
    let profiles: Profiles = [("claude".into(), profile.clone())].into();
    let manager = Arc::new(Mutex::new(
        Manager::new(root.join("runs"), profiles, runner::now_ms() as i64).unwrap(),
    ));
    manager.lock().unwrap().routing.timeout_seconds = 30;
    let sid = ipc::user_sid().unwrap();
    // 测试命名空间尚未创建；被动探活不得启动任何管理器。
    assert_eq!(ipc::probe(sid.clone(), None).unwrap(), 1);
    let (tx, rx) = mpsc::channel();
    ipc::serve(sid.clone(), tx).unwrap();
    let quit = Arc::new(AtomicBool::new(false));
    let pump_manager = manager.clone();
    let pump_quit = quit.clone();
    let pump = TestPump {
        manager: manager.clone(),
        quit,
        thread: Some(std::thread::spawn(move || {
            loop {
                {
                    let mut manager = pump_manager.lock().unwrap();
                    for req in rx.try_iter() {
                        manager.request(req, runner::now_ms() as i64);
                    }
                    if pump_quit.load(Ordering::Relaxed) {
                        manager.shutdown();
                    }
                    manager.poll(runner::now_ms() as i64).unwrap();
                    if pump_quit.load(Ordering::Relaxed) && manager.running_count() == 0 {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })),
    };
    for n in 1..=4 {
        assert_eq!(
            exchange(
                &sid,
                Request::Submit {
                    task: work(&root, &format!("parallel-{n}"), "hang"),
                    wait: false
                }
            )["type"],
            "accepted"
        );
    }
    until(|| (1..=4).all(|n| root.join(format!("parallel-{n}/child.pid")).exists()));
    until(|| {
        manager
            .lock()
            .unwrap()
            .records
            .values()
            .all(|r| r.health.phase == "running" && r.lines.len() >= 3)
    });
    assert_eq!(manager.lock().unwrap().running_count(), 4);
    assert_eq!(
        store::recent(manager.lock().unwrap().records.values().cloned()).len(),
        3
    );
    let before = manager.lock().unwrap().records["parallel-1"].last_activity;
    let health = exchange(
        &sid,
        Request::Health {
            task_id: Some("parallel-1".into()),
        },
    );
    assert_eq!(health["task"]["health"]["process_alive"], true);
    assert_eq!(health["task"]["health"]["worker_alive"], true);
    assert_eq!(
        manager.lock().unwrap().records["parallel-1"].last_activity,
        before
    );
    {
        // 暂停测试管理泵，探活三秒超时不能取消它已接收的任务。
        let paused = manager.lock().unwrap();
        assert_eq!(ipc::probe(sid.clone(), None).unwrap(), 1);
        assert!(paused.records["parallel-1"].running());
    }
    assert_eq!(
        exchange(
            &sid,
            Request::Cancel {
                task_id: "parallel-1".into()
            }
        )["type"],
        "cancel_requested"
    );
    until(|| manager.lock().unwrap().records["parallel-1"].state == "cancelled");
    let child: u32 = fs::read_to_string(root.join("parallel-1/child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(ipc::process_alive(child), Some(false));

    // 等待连接断开后执行继续；随后通过原 ID 取消。
    let mut detached = ipc::connect(&sid).unwrap();
    writeln!(
        detached,
        "{}",
        serde_json::to_string(&Request::Submit {
            task: work(&root, "detached", "hang"),
            wait: true
        })
        .unwrap()
    )
    .unwrap();
    let mut accepted = String::new();
    BufReader::new(detached).read_line(&mut accepted).unwrap();
    until(|| root.join("detached/child.pid").exists());
    assert_eq!(
        exchange(
            &sid,
            Request::Status {
                task_id: "detached".into()
            }
        )["task"]["state"],
        "running"
    );
    exchange(
        &sid,
        Request::Cancel {
            task_id: "detached".into(),
        },
    );

    manager.lock().unwrap().routing.timeout_seconds = 1;
    let timeout = exchange(
        &sid,
        Request::Submit {
            task: work(&root, "timeout", "hang"),
            wait: true,
        },
    );
    assert_eq!(timeout["state"], "timed_out");
    let child: u32 = fs::read_to_string(root.join("timeout/child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(ipc::process_alive(child), Some(false));
    // 总运行超过 1 秒但持续输出的任务应成功，停止输出后才开始静默超时。
    for (id, prompt, expected) in [
        ("active", "active", "succeeded"),
        ("stderr_active", "stderr_active", "succeeded"),
        ("active_then_silent", "active_then_silent", "timed_out"),
        ("silent", "silent", "timed_out"),
    ] {
        let reply = exchange(
            &sid,
            Request::Submit {
                task: work(&root, id, prompt),
                wait: true,
            },
        );
        assert_eq!(reply["state"], expected, "{id}: {reply}");
        if id != "silent" {
            assert!(reply["result"]["duration_ms"].as_u64().unwrap() >= 1500);
        }
        if expected == "timed_out" {
            assert_eq!(reply["result"]["error_code"], "idle_timeout");
        }
        if id == "active_then_silent" {
            assert!(reply["result"]["duration_ms"].as_u64().unwrap() >= 2300);
        }
    }
    // 后续非计时验证留出 Windows 启动/调度余量。
    manager.lock().unwrap().routing.timeout_seconds = 30;
    for (id, prompt, expected) in [
        ("success", "success", "succeeded"),
        ("incomplete", "incomplete", "failed"),
        ("error", "error", "failed"),
    ] {
        let reply = exchange(
            &sid,
            Request::Submit {
                task: work(&root, id, prompt),
                wait: true,
            },
        );
        assert_eq!(reply["state"], expected);
        assert!(PathBuf::from(reply["result_path"].as_str().unwrap()).is_file());
    }
    // 真实 IPC 审计与返工：不从 CLI 成功推断验收，重发/普通 send 不重复计数。
    let review =
        crate::review::tests::sample("review-success-1", 1, crate::review::Verdict::Rework);
    let activity = manager.lock().unwrap().records["success"].last_activity;
    assert_eq!(
        exchange(
            &sid,
            Request::Review {
                task_id: "success".into(),
                review: review.clone()
            }
        )["score"],
        8
    );
    assert_eq!(
        exchange(
            &sid,
            Request::Review {
                task_id: "success".into(),
                review
            }
        )["idempotent"],
        true
    );
    assert_eq!(
        manager.lock().unwrap().records["success"].last_activity,
        activity
    );
    let rework = crate::review::ReworkInput {
        request_id: "retry-success-1".into(),
        review_id: "review-success-1".into(),
        message: "success".into(),
    };
    let accepted = exchange(
        &sid,
        Request::Rework {
            task_id: "success".into(),
            rework: rework.clone(),
            wait: false,
        },
    );
    assert_eq!(accepted["turn"], 2);
    assert_eq!(accepted["rework_count"], 1);
    let repeated = exchange(
        &sid,
        Request::Rework {
            task_id: "success".into(),
            rework: rework.clone(),
            wait: true,
        },
    );
    assert_eq!(repeated["state"], "succeeded");
    assert_eq!(repeated["turn"], 2);
    let record = exchange(
        &sid,
        Request::Status {
            task_id: "success".into(),
        },
    );
    assert_eq!(record["rework_count"], 1);
    assert!(record["review"].is_null());
    let review =
        crate::review::tests::sample("review-success-2", 2, crate::review::Verdict::Accepted);
    assert_eq!(
        exchange(
            &sid,
            Request::Review {
                task_id: "success".into(),
                review
            }
        )["score"],
        10
    );
    assert_eq!(
        exchange(
            &sid,
            Request::Send {
                task_id: "success".into(),
                message: "success".into(),
                wait: true
            }
        )["state"],
        "succeeded"
    );
    let old = exchange(
        &sid,
        Request::Rework {
            task_id: "success".into(),
            rework,
            wait: true,
        },
    );
    assert_eq!(old["turn"], 2);
    assert_eq!(manager.lock().unwrap().records["success"].turn, 3);
    let data = exchange(&sid, Request::StatisticsData);
    let session = data["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["task_id"] == "success")
        .unwrap();
    assert_eq!(session["rework_count"], 1);
    assert_eq!(session["first_score"], 8);
    assert!(session["score"].is_null());
    assert_eq!(session["turns"].as_array().unwrap().len(), 3);
    manager.lock().unwrap().profiles.insert(
        "claude".into(),
        Profile {
            program: root.join("missing.exe"),
            ..profile
        },
    );
    assert_eq!(
        exchange(
            &sid,
            Request::Submit {
                task: work(&root, "missing", "success"),
                wait: true
            }
        )["state"],
        "failed"
    );
    assert_eq!(
        exchange(
            &sid,
            Request::Submit {
                task: work(&root, "success", "success"),
                wait: false
            }
        )["type"],
        "error"
    );
    let sid_wait = sid.clone();
    manager
        .lock()
        .unwrap()
        .profiles
        .get_mut("claude")
        .unwrap()
        .program = root.join("fixture.exe");
    let exit_work = work(&root, "shutdown", "hang");
    let waiting = std::thread::spawn(move || {
        exchange(
            &sid_wait,
            Request::Submit {
                task: exit_work,
                wait: true,
            },
        )
    });
    until(|| root.join("shutdown/child.pid").exists());
    manager.lock().unwrap().shutdown();
    let final_event = waiting.join().unwrap();
    assert_eq!(final_event["error_code"], "manager_shutdown");
    let compact = ipc::client_output(&final_event, true, false).unwrap();
    assert_eq!(compact["error_code"], "manager_shutdown");
    until(|| manager.lock().unwrap().running_count() == 0);
    assert_eq!(
        exchange(&sid, Request::Health { task_id: None })["accepting_tasks"],
        false
    );
    for n in 2..=4 {
        let child: u32 = fs::read_to_string(root.join(format!("parallel-{n}/child.pid")))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(ipc::process_alive(child), Some(false));
    }
    let report = json!({"root":root,"health":health,"timeout":ipc::client_output(&timeout,true,false),"shutdown":compact,"verified":["four_parallel","three_recent","passive_health","no_activity_on_health","cancel_tree","disconnect","timeout_tree","protocol_failure","spawn_failure","shutdown_error"]});
    fs::write(
        root.join("verification.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("service verification: {}", root.display());
    assert_eq!(
        pump.manager.lock().unwrap().routing.backend,
        Backend::Claude
    );
    drop(pump);
    let restored =
        Manager::new(root.join("runs"), Profiles::new(), runner::now_ms() as i64).unwrap();
    assert_eq!(restored.records["success"].reworks.len(), 1);
    assert_eq!(restored.records["success"].reviews.len(), 2);
}

/// 显式运行的真实 CLI 验证：读取用户配置快照，不改生产实例、认证或路由设置。
#[test]
#[ignore = "uses configured local Claude/Codex CLIs"]
fn configured_cli_roundtrip() {
    let root = std::env::temp_dir().join(format!("router-v4-live-{}", runner::now_ms()));
    let profiles: Profiles = serde_json::from_slice(
        &fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("router.local.json")).unwrap(),
    )
    .unwrap();
    let mut manager = Manager::new(root.join("runs"), profiles, runner::now_ms() as i64).unwrap();
    let mut evidence = vec![];
    for backend in [Backend::Claude, Backend::Codex] {
        manager.routing.backend = backend;
        manager.routing.timeout_seconds = 180;
        let task = work(
            &root,
            backend.key(),
            "只读取当前目录 input.json，计算 numbers 之和。最终只回答数字，不修改文件、不提交代码、不委派任务。",
        );
        fs::write(task.workdir.join("input.json"), r#"{"numbers":[7,11,24]}"#).unwrap();
        let id = manager.submit(task, runner::now_ms() as i64).unwrap();
        while manager.running_count() > 0 {
            manager.poll(runner::now_ms() as i64).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        let record = &manager.records[&id];
        assert_eq!(
            record.state,
            "succeeded",
            "result: {}",
            record.directory.display()
        );
        assert!(
            record.result["outcome"]["answer"]
                .as_str()
                .unwrap()
                .contains("42")
        );
        assert!(record.metrics.first_text_ms.is_some());
        assert!(record.metrics.total_tokens().is_some());
        assert!(record.metrics.context_tokens.is_some_and(|n| n > 0));
        assert!(record.result["executor"]["model"].is_string());
        assert!(record.result["executor"]["commit_prefix"].is_string());
        evidence.push(json!({"cli":backend,"state":record.state,"executor":record.result["executor"],"metrics":record.metrics,"result_path":record.directory.join("result.json")}));
    }
    fs::write(
        root.join("verification.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("configured CLI verification: {}", root.display());
}
