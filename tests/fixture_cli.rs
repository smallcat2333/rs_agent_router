//! 无网络 CLI 替身：用于验证协议失败、超时、取消与 Windows 子进程回收。
use std::{
    io::{self, Read, Write},
    time::Duration,
};

/// 读取测试指令；hang 创建持有 stdout 的子进程，验证整棵进程树都能取消。
fn main() {
    if std::env::args().any(|arg| arg == "--fixture-child") {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt).unwrap();
    if matches!(
        prompt.as_str(),
        "active" | "stderr_active" | "active_then_silent" | "silent"
    ) {
        if prompt != "silent" {
            for _ in 0..6 {
                if prompt == "stderr_active" {
                    eprintln!("fixture progress");
                    io::stderr().flush().unwrap();
                } else {
                    println!(
                        "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"working\"}}]}}}}"
                    );
                    io::stdout().flush().unwrap();
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        if matches!(prompt.as_str(), "active_then_silent" | "silent") {
            std::thread::sleep(Duration::from_secs(3));
        }
        println!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}"
        );
    } else if prompt == "hang" {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--fixture-child")
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        std::fs::write("child.pid", child.id().to_string()).unwrap();
        println!("{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fixture\"}}");
        child.wait().unwrap();
    } else if prompt == "success" {
        println!(
            "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fixture\",\"session_id\":\"fixture-session\"}}"
        );
        std::thread::sleep(Duration::from_millis(500));
        println!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"42\"}}]}}}}"
        );
        println!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"42\"}}"
        );
    } else if prompt == "error" {
        println!(
            "{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"fixture failure\"}}"
        );
    } else {
        println!("{{\"type\":\"system\",\"subtype\":\"init\"}}");
    }
}
