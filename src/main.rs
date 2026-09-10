//! 单实例管理页及 Harness CLI 客户端；命令行只负责派发，不拥有任务进程。
#![cfg_attr(windows, windows_subsystem = "windows")]
mod ipc;
mod launcher;
mod manager;
mod metrics;
mod model_catalog;
mod model_efforts;
mod protocol;
mod review;
mod runner;
#[cfg(test)]
#[path = "../tests/support/service.rs"]
mod service_tests;
mod statistics;
mod store;
mod ui;
mod webstats;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// 记住管理器实际使用的配置和索引目录；显式启动参数仍可覆盖。
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct StartupPaths {
    config: PathBuf,
    runs_dir: PathBuf,
}
impl StartupPaths {
    /// 只在文件不存在时使用首次启动默认值，损坏的路径记录明确报错。
    fn load(path: &std::path::Path, defaults: Self) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(defaults),
            Err(error) => Err(error.into()),
        }
    }
    /// 保存绝对路径，使工作目录变化不影响下一次恢复。
    fn save(&self, path: &std::path::Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// 启动配置仅在创建管理实例时生效。
#[derive(Parser)]
#[command(version, about = "Unified Claude / GLM / Codex task manager")]
struct Args {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    runs_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
}
/// 执行、审计与统计操作；无参数启动等同 show。
#[derive(Subcommand)]
enum Action {
    Show,
    /// 按统一量表记录 Harness 对当前轮次的审计。
    Review {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        request: PathBuf,
    },
    /// 绑定审计进行显式返工，重复 request_id 不重复执行。
    Rework {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        wait: bool,
        #[arg(long, requires = "wait")]
        events: bool,
    },
    /// 返回本机 Web 统计页地址；--open 同时打开浏览器。
    Stats {
        #[arg(long)]
        open: bool,
    },
    Submit {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        wait: bool,
        /// 显式查看完整过程事件；默认等待仅输出最终摘要。
        #[arg(long, requires = "wait")]
        events: bool,
    },
    Send {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        message_file: PathBuf,
        #[arg(long)]
        wait: bool,
        #[arg(long, requires = "wait")]
        events: bool,
    },
    Status {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        full: bool,
    },
    /// 被动探活，不启动管理器，三秒内报告管理器是否响应。
    Health {
        #[arg(long)]
        task_id: Option<String>,
    },
    Cancel {
        #[arg(long)]
        task_id: String,
    },
}
/// 输出结构化错误，不把连接失败静默转换为成功。
fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            let value = serde_json::json!({"type":"error","error_code":"router_error","error":format!("{error:#}")});
            eprintln!("{value}");
            1
        }
    };
    std::process::exit(code);
}
/// 管理页直接运行于主进程；提交客户端自动拉起管理器后连接管道。
fn run() -> Result<i32> {
    let args = Args::parse();
    let verbose = matches!(
        args.command,
        Some(
            Action::Submit { events: true, .. }
                | Action::Rework { events: true, .. }
                | Action::Send { events: true, .. }
                | Action::Status { full: true, .. }
        )
    );
    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().context("exe directory missing")?;
    let startup_file = exe_dir.join("startup.json");
    let startup = StartupPaths::load(
        &startup_file,
        StartupPaths {
            config: if cfg!(debug_assertions) {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("router.local.json")
            } else {
                exe_dir.join("router.local.json")
            },
            runs_dir: exe_dir.join("runs"),
        },
    )?;
    let config = std::path::absolute(args.config.unwrap_or(startup.config))?;
    let root = std::path::absolute(args.runs_dir.unwrap_or(startup.runs_dir))?;
    let sid = ipc::user_sid()?;
    let request = match args.command {
        None | Some(Action::Show) => {
            if let Some(_lock) = ipc::claim(&sid)? {
                StartupPaths {
                    config: config.clone(),
                    runs_dir: root.clone(),
                }
                .save(&startup_file)?;
                return launcher::show(config, root, sid);
            }
            return ipc::call(ipc::wait_connection(&sid)?, &ipc::Request::Show, false);
        }
        Some(Action::Health { task_id }) => return ipc::probe(sid, task_id),
        Some(Action::Review { task_id, request }) => {
            let review: review::ReviewInput = serde_json::from_slice(&std::fs::read(request)?)?;
            review.validate()?;
            ipc::Request::Review { task_id, review }
        }
        Some(Action::Rework {
            task_id,
            request,
            wait,
            ..
        }) => {
            let rework: review::ReworkInput = serde_json::from_slice(&std::fs::read(request)?)?;
            rework.validate()?;
            ipc::Request::Rework {
                task_id,
                rework,
                wait,
            }
        }
        Some(Action::Stats { open }) => ipc::Request::Statistics { open },
        Some(Action::Submit { request, wait, .. }) => {
            let task: protocol::Work = serde_json::from_slice(
                &std::fs::read(&request).with_context(|| format!("read {}", request.display()))?,
            )?;
            task.validate()?;
            ipc::Request::Submit { task, wait }
        }
        Some(Action::Status { task_id, .. }) => ipc::Request::Status { task_id },
        Some(Action::Send {
            task_id,
            message_file,
            wait,
            ..
        }) => ipc::Request::Send {
            task_id,
            message: std::fs::read_to_string(message_file)?,
            wait,
        },
        Some(Action::Cancel { task_id }) => ipc::Request::Cancel { task_id },
    };
    let pipe = match ipc::connect(&sid) {
        Ok(pipe) => pipe,
        Err(_) => {
            ipc::spawn_manager(&exe, &config, &root)?;
            ipc::wait_connection(&sid)?
        }
    };
    ipc::call(pipe, &request, verbose)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 已保存的路径优先于默认目录，重开不会回到空索引。
    #[test]
    fn startup_paths_survive_restart() {
        let file =
            std::env::temp_dir().join(format!("router-startup-{}.json", uuid::Uuid::new_v4()));
        let paths = StartupPaths {
            config: PathBuf::from("C:/router/custom.json"),
            runs_dir: PathBuf::from("C:/router/existing-runs"),
        };
        paths.save(&file).unwrap();
        let restored = StartupPaths::load(
            &file,
            StartupPaths {
                config: PathBuf::new(),
                runs_dir: PathBuf::new(),
            },
        )
        .unwrap();
        assert_eq!(restored, paths);
        std::fs::remove_file(file).unwrap();
    }
    /// IDE 无参数进入管理页；提交必须带任务文件。
    #[test]
    fn parse_modes() {
        assert!(Args::try_parse_from(["router"]).unwrap().command.is_none());
        assert!(Args::try_parse_from(["router", "submit"]).is_err());
        assert!(
            Args::try_parse_from(["router", "submit", "--request", "a.json", "--wait"]).is_ok()
        );
        assert!(
            Args::try_parse_from(["router", "submit", "--request", "a.json", "--events"]).is_err()
        );
        assert!(
            Args::try_parse_from([
                "router",
                "submit",
                "--request",
                "a.json",
                "--wait",
                "--events"
            ])
            .is_ok()
        );
        assert!(Args::try_parse_from(["router", "health", "--task-id", "a"]).is_ok());
        assert!(Args::try_parse_from(["router", "status", "--task-id", "a", "--full"]).is_ok());
        assert!(
            Args::try_parse_from(["router", "review", "--task-id", "a", "--request", "r.json"])
                .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "router",
                "rework",
                "--task-id",
                "a",
                "--request",
                "r.json",
                "--events"
            ])
            .is_err()
        );
        assert!(Args::try_parse_from(["router", "stats", "--open"]).is_ok());
    }
}
