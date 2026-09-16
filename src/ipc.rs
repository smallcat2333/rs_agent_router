//! 当前 Windows 用户专用命名管道；CLI 连接断开不改变已接收任务的生命周期。
use crate::protocol::Work;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::windows::io::FromRawHandle,
    ptr,
    sync::mpsc::{self, Sender, SyncSender},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*},
};

/// 一条连接处理一个命令，等待模式持续接收其任务事件直到 finished。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Show,
    Review {
        task_id: String,
        review: crate::review::ReviewInput,
    },
    Rework {
        task_id: String,
        rework: crate::review::ReworkInput,
        wait: bool,
    },
    Statistics {
        open: bool,
    },
    StatisticsData,
    Submit {
        task: Work,
        wait: bool,
    },
    Send {
        task_id: String,
        message: String,
        wait: bool,
    },
    Status {
        task_id: String,
    },
    Health {
        task_id: Option<String>,
    },
    Cancel {
        task_id: String,
    },
}

/// UI 线程接收的命令；满缓冲的慢客户端被取消订阅，不阻塞任务本身。
pub struct Envelope {
    pub request: Request,
    pub reply: SyncSender<Value>,
}

/// 拥有 Win32 句柄，确保所有错误路径释放。
pub struct Handle(pub HANDLE);
impl Drop for Handle {
    /// 只关闭本实例持有的内核句柄。
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// UTF-16 API 参数带结尾零，不经过 shell。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Windows 参数转义，不经 CMD，保留带空格和结尾反斜杠的目录。
fn quote_argument(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        result.push(ch);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

/// 禁止继承任何客户端句柄，避免自动启动的常驻 GUI 持有 Harness 输出管道。
pub fn spawn_manager(
    exe: &std::path::Path,
    config: &std::path::Path,
    root: &std::path::Path,
) -> Result<()> {
    let args = [
        exe.display().to_string(),
        "show".into(),
        "--config".into(),
        config.display().to_string(),
        "--runs-dir".into(),
        root.display().to_string(),
    ];
    let mut command = wide(
        &args
            .iter()
            .map(|s| quote_argument(s))
            .collect::<Vec<_>>()
            .join(" "),
    );
    unsafe {
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(
            wide(&exe.display().to_string()).as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_NO_WINDOW,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut info,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let _process = Handle(info.hProcess);
        let _thread = Handle(info.hThread);
    }
    Ok(())
}

/// 从当前进程令牌读取用户 SID，用于隔离命名空间和管道 ACL。
pub fn user_sid() -> Result<String> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let token = Handle(token);
        let mut size = 0;
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        Ok(value)
    }
}

/// 单实例锁；已存在实例返回 None，不能绕过并启动第二个任务库写者。
pub fn claim(sid: &str) -> Result<Option<Handle>> {
    unsafe {
        let name = wide(&format!("Local\\AgentRouter-v2-{sid}"));
        let handle = CreateMutexW(ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let existed = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = Handle(handle);
        Ok(if existed { None } else { Some(owned) })
    }
}

/// 固定用户专用 pipe 名称，不绑定端口、不允许远程客户端。
fn pipe_name(sid: &str) -> Vec<u16> {
    // 测试使用独立命名空间，绝不连接/接管当前用户正在工作的生产管理器。
    let namespace = if cfg!(test) {
        format!("test-{}", std::process::id())
    } else {
        "v2".into()
    };
    wide(&format!("\\\\.\\pipe\\AgentRouter-{namespace}-{sid}"))
}

/// 创建仅当前用户可访问的监听端，后续连接交给独立线程。
fn create_pipe(sid: &str) -> Result<File> {
    unsafe {
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(&format!("D:P(A;;GA;;;{sid})")).as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = CreateNamedPipeW(
            pipe_name(sid).as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            &security,
        );
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor);
        if handle == INVALID_HANDLE_VALUE {
            return Err(error.into());
        }
        Ok(File::from_raw_handle(handle.cast()))
    }
}

/// 在返回前建立首个监听实例，保证启动错误同步报告给管理页。
pub fn serve(sid: String, tx: Sender<Envelope>) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let first = create_pipe(&sid)?;
    std::thread::spawn(move || {
        let mut pipe = first;
        loop {
            let connected = unsafe {
                ConnectNamedPipe(pipe.as_raw_handle().cast(), ptr::null_mut()) != 0
                    || GetLastError() == ERROR_PIPE_CONNECTED
            };
            if connected {
                let channel = tx.clone();
                std::thread::spawn(move || {
                    let _ = connection(pipe, channel);
                });
            }
            match create_pipe(&sid) {
                Ok(next) => pipe = next,
                Err(_) => break,
            }
        }
    });
    Ok(())
}

/// 解码命令并转发回复；错误关闭当前连接，不中断其它任务。
fn connection(mut pipe: File, tx: Sender<Envelope>) -> Result<()> {
    let mut line = String::new();
    BufReader::new(pipe.try_clone()?).read_line(&mut line)?;
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            writeln!(
                pipe,
                "{}",
                json!({"type":"error","error_code":"invalid_request","error":error.to_string()})
            )?;
            return Ok(());
        }
    };
    let waiting = matches!(
        request,
        Request::Submit { wait: true, .. }
            | Request::Send { wait: true, .. }
            | Request::Rework { wait: true, .. }
    );
    let (reply, rx) = mpsc::sync_channel(256);
    tx.send(Envelope { request, reply })?;
    for value in rx {
        writeln!(pipe, "{value}")?;
        if !waiting || matches!(value["type"].as_str(), Some("finished" | "error")) {
            break;
        }
    }
    Ok(())
}

/// 连接用户专用 pipe；SQOS 禁止服务端模拟客户端身份。
pub fn connect(sid: &str) -> std::io::Result<File> {
    unsafe {
        let handle = CreateFileW(
            pipe_name(sid).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(File::from_raw_handle(handle.cast()))
        }
    }
}

/// 等待其它进程完成管理器初始化；不因并发启动重复创建管理实例。
pub fn wait_connection(sid: &str) -> Result<File> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match connect(sid) {
            Ok(pipe) => return Ok(pipe),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error).context("manager connection timed out");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// CLI 输出可机读 JSONL；即使服务端崩溃也不能把断流误判为成功。
pub fn call(mut pipe: File, request: &Request, verbose: bool) -> Result<i32> {
    writeln!(pipe, "{}", serde_json::to_string(request)?)?;
    let waiting = matches!(
        request,
        Request::Submit { wait: true, .. }
            | Request::Send { wait: true, .. }
            | Request::Rework { wait: true, .. }
    );
    let mut reader = BufReader::new(pipe);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("manager_disconnected: query status before deciding whether to retry");
        }
        let value: Value = serde_json::from_str(&line)?;
        if let Some(output) = client_output(&value, waiting, verbose) {
            writeln!(std::io::stdout(), "{output}")?;
        }
        let kind = value["type"].as_str().unwrap_or("");
        if kind == "statistics_page" && matches!(request, Request::Statistics { open: true }) {
            webbrowser::open(value["url"].as_str().context("statistics URL missing")?)?;
        }
        if kind == "error" {
            return Ok(1);
        }
        if kind == "finished" {
            return Ok(if value["state"] == "succeeded" { 0 } else { 1 });
        }
        if !waiting {
            return Ok(0);
        }
    }
}

/// Unicode 字符边界截断，不截断 UTF-8 字节；完整文本始终留在本地结果文件。
fn bounded(value: &str, limit: usize) -> (String, bool) {
    let mut chars = value.chars();
    let text = chars.by_ref().take(limit).collect();
    (text, chars.next().is_some())
}

/// 默认只放行紧凑终态或状态；过程在客户端消费，不进入 Harness 的主会话。
pub fn client_output(value: &Value, waiting: bool, verbose: bool) -> Option<Value> {
    if verbose {
        return Some(value.clone());
    }
    match value["type"].as_str() {
        Some("accepted" | "progress") if waiting => None,
        Some("finished" | "status") => {
            let status = value["type"] == "status";
            let record = &value["task"];
            let task = &record["task"];
            let state = if status {
                &record["state"]
            } else {
                &value["state"]
            };
            let report = if state == "running" || state == "queued" {
                &Value::Null
            } else if status {
                &record["result"]
            } else {
                &value["result"]
            };
            let (summary, truncated) =
                bounded(report["outcome"]["answer"].as_str().unwrap_or(""), 800);
            let (error, _) = bounded(
                report["error"]
                    .as_str()
                    .or(value["error"].as_str())
                    .unwrap_or(""),
                400,
            );
            let executor = if state == "running" || state == "queued" {
                serde_json::from_value::<crate::protocol::Backend>(task["backend"].clone())
                    .ok()
                    .map(|backend| {
                        crate::metrics::executor(
                            backend,
                            task["model"].as_str(),
                            task["effort"].as_str(),
                            None,
                        )
                    })
                    .unwrap_or(Value::Null)
            } else {
                report["executor"].clone()
            };
            Some(
                json!({"type":value["type"],"task_id":if status { &task["task_id"] } else { &value["task_id"] },
                "state":state,"summary":summary,"summary_truncated":truncated,
                "result_path":value["result_path"],"turn":value["turn"],"turn_result_path":value["turn_result_path"],"error_code":report["error_code"],"error":error,
                "executor":executor,"metrics":if status { &record["metrics"] } else { &report["outcome"]["metrics"] },
                "duration_ms":report["duration_ms"],"elapsed_ms":value["elapsed_ms"],"review":value["review"],"rework_count":value["rework_count"],
                "health":value["health"],"manager_pid":value["manager_pid"]}),
            )
        }
        Some("error") => {
            let (error, _) = bounded(value["error"].as_str().unwrap_or(""), 400);
            Some(json!({"type":"error","error_code":value["error_code"],"error":error}))
        }
        _ => Some(value.clone()),
    }
}

/// 三秒被动探活；连接/响应异常只报告，不自动启动、重试或取消服务中的任务。
pub fn probe(sid: String, task_id: Option<String>) -> Result<i32> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<Value> {
            let mut pipe = connect(&sid)?;
            writeln!(
                pipe,
                "{}",
                serde_json::to_string(&Request::Health { task_id })?
            )?;
            let mut line = String::new();
            BufReader::new(pipe).read_line(&mut line)?;
            Ok(serde_json::from_str(&line)?)
        })();
        let _ = tx.send(result);
    });
    let value = match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            json!({"type":"error","error_code":"manager_unavailable","error":error.to_string()})
        }
        Err(_) => {
            json!({"type":"error","error_code":"health_check_timeout","error":"管理器在 3 秒探活期限内未响应；任务是否结束请查询终态"})
        }
    };
    writeln!(std::io::stdout(), "{value}")?;
    Ok(
        if value["type"] == "health" && value["accepting_tasks"] == true {
            0
        } else {
            1
        },
    )
}

/// 查询已登记 PID 的内核状态；权限不足返回未知，绝不伪装为健康。
pub fn process_alive(pid: u32) -> Option<bool> {
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return if GetLastError() == ERROR_INVALID_PARAMETER {
                Some(false)
            } else {
                None
            };
        }
        let handle = Handle(handle);
        match WaitForSingleObject(handle.0, 0) {
            WAIT_TIMEOUT => Some(true),
            WAIT_OBJECT_0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 默认等待无过程内容，摘要有界，显式 events 才得到完整报告。
    #[test]
    fn compact_wait_does_not_echo_context() {
        for kind in ["accepted", "progress"] {
            assert!(
                client_output(
                    &json!({"type":kind,"message":"private prompt"}),
                    true,
                    false
                )
                .is_none()
            );
        }
        let value = json!({"type":"finished","task_id":"t","state":"succeeded","result_path":"evidence/result.json",
            "result":{"outcome":{"answer":"结果".repeat(700)},"executor":{"model":"GLM-5.2"},"prompt":"private prompt"}});
        let output = client_output(&value, true, false).unwrap();
        assert_eq!(output["summary"].as_str().unwrap().chars().count(), 800);
        assert_eq!(output["summary_truncated"], true);
        assert!(output.get("result").is_none());
        assert!(!output.to_string().contains("private prompt"));
        assert_eq!(client_output(&value, true, true).unwrap(), value);
    }
    /// 新轮运行时不暴露上一轮结果为本轮结果；故障仍有独立错误字段。
    #[test]
    fn compact_status_never_reports_old_turn_success() {
        let value = json!({"type":"status","task":{"task":{"task_id":"t","prompt":"private"},"state":"running","result":{"outcome":{"answer":"old"}}}});
        let output = client_output(&value, false, false).unwrap();
        assert_eq!(output["summary"], "");
        assert_eq!(output["state"], "running");
        assert!(!output.to_string().contains("private"));
    }
}
