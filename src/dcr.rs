//! DCR（Desktop Commander Remote）后端独立模块。
//!
//! 只负责本服务自身的进程生命周期、后台有界日志采集、每次启动前的外部实例
//! 检测，以及只读解析本机 DCR 工具历史。界面与 `mod` 接线由主 Harness 负责，
//! 本文件不触碰其它模块，也不把“进程存活/本地 MCP 连接”表述成“远程已在线”。

use crate::dcr_session::{
    DcrSession, EventTracker, PersistShared, SessionSnapshot, atomic_write, begin_session,
    mark_failed, mark_stopped, persist_loop, push_line,
};
use anyhow::{Context, Result, ensure};
use cli_stream::{Command as StreamCommand, Event, ProcessHandle, Stderr};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 固定且已检查过的 Desktop Commander 版本，避免 `@latest` 引入未验证行为。
const PACKAGE: &str = "@wonderwhy-er/desktop-commander@0.2.50";
/// 日志最多保留的行数与单行字符数，防止聊天式子进程撑爆内存/界面。
pub(crate) const LOG_CAP: usize = 200;
pub(crate) const LOG_LINE_MAX: usize = 500;
/// 工具历史只返回最近条数，`output` 单条最多字符数。
const HISTORY_CAP: usize = 100;
const HISTORY_OUTPUT_MAX: usize = 4000;
/// 历史文件只从尾部读取的字节上限，读取时再用 `take` 硬性封顶。
const HISTORY_TAIL_MAX: u64 = 4 * 1024 * 1024;

/// 官方 remote-device 明确表示“远程通道就绪”的日志片段。
const REMOTE_READY: &[&str] = &[
    "Device ready:",
    "Channel subscribed",
    "visible as online",
    "device is online again",
];
/// 官方日志里明确表示“远程已离线/通道断开”的片段（本地工具仍可用）。
const REMOTE_OFFLINE: &[&str] = &[
    "offline for remote calls",
    "could not be renewed",
    "Channel closed",
    "Channel error",
    "subscription timed out",
    "withdrawing broadcast capability",
    "Device marked as offline",
    "Presence track not",
];
/// 连接/鉴权过程中的日志；不包含本地 stdio transport 的调试输出。
const REMOTE_CONNECTING: &[&str] = &[
    "Starting MCP Device",
    "Connecting to Remote MCP",
    "Authenticating with Remote MCP",
    "Remote session restored",
    "Requesting device code",
    "Waiting for authorization",
];

/// 会话连接态；与 [`crate::dcr_session`] 的 `connection` 取值一一对应。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Failed,
    Offline,
    Online,
    Connecting,
}

impl Link {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Link::Failed => "failed",
            Link::Offline => "offline",
            Link::Online => "online",
            Link::Connecting => "connecting",
        }
    }
}

/// 当前 Unix 毫秒；系统时间异常时退回 0，绝不 panic。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

/// 只按官方 remote-device 日志判定连接态；本地 stdio 调试行不算。
pub(crate) fn classify_connection(line: &str) -> Option<Link> {
    if line.contains("Device startup failed") {
        return Some(Link::Failed);
    }
    if REMOTE_OFFLINE.iter().any(|marker| line.contains(marker)) {
        return Some(Link::Offline);
    }
    if REMOTE_READY.iter().any(|marker| line.contains(marker)) {
        return Some(Link::Online);
    }
    if REMOTE_CONNECTING.iter().any(|marker| line.contains(marker)) {
        return Some(Link::Connecting);
    }
    None
}

/// DCR 启动配置；默认不自动启动，走本机 HTTP 代理与用户主目录。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DcrConfig {
    pub auto_start: bool,
    pub proxy: String,
    pub workdir: PathBuf,
}

impl Default for DcrConfig {
    /// 默认仅描述“如何启动”，不会触发启动；工作目录取当前用户主目录。
    fn default() -> Self {
        Self {
            auto_start: false,
            proxy: "http://127.0.0.1:11809".into(),
            workdir: home_dir(),
        }
    }
}

/// 解析用户主目录，优先 Windows 的 USERPROFILE，其次 HOME，最后退回当前目录。
fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// 外部 DCR remote 的检测结果；Unknown 表示探测不可用，启动必须明确失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum External {
    Unknown,
    None,
    Present(u32),
}

/// 供界面读取的单条工具调用记录；不含 arguments，`output` 已截断。
pub struct DcrCall {
    pub timestamp: String,
    pub tool_name: String,
    pub duration_ms: Option<u64>,
    pub output: String,
    pub is_error: bool,
}

/// 单次运行的状态；每次 start 使用全新实例，旧采集线程无法覆盖新状态。
///
/// 日志与连接展示已统一收敛到 [`DcrSession`]，此处只保留进程级状态。
#[derive(Default)]
struct Shared {
    starting: bool,
    running: bool,
    pid: Option<u32>,
    last_error: Option<String>,
    status: String,
}

/// 当前运行的共享状态与进程槽；每次 start 都换成新的 Arc。
type SharedState = Arc<Mutex<Shared>>;
type ProcessSlot = Arc<Mutex<Option<ProcessHandle>>>;
/// 所有 DCR 网页调用共享的唯一会话；`new` 即创建，configure 只填充不替换。
pub type Monitor = Arc<Mutex<DcrSession>>;
/// 持久化线程使用的快照路径；configure_monitor 在启动前设置。
type PersistPaths = Arc<Mutex<Option<(PathBuf, PathBuf)>>>;

/// DCR remote 服务句柄：只管理本实例启动的进程树，不介入外部 DCR。
pub struct DcrService {
    shared: SharedState,
    slot: ProcessSlot,
    /// 每次 start/stop 递增；后台检测线程据此放弃已过期的启动。
    generation: Arc<AtomicU64>,
    /// 单会话状态；start 的新旧 drain 都用它，靠 generation 隔离写入。
    monitor: Monitor,
    /// 持久化共享状态；后台线程随本服务退出。
    persist: Arc<PersistShared>,
    persist_paths: PersistPaths,
    persist_thread: Option<std::thread::JoinHandle<()>>,
}

impl DcrService {
    /// 创建服务；不做任何外部检测或进程启动，但会话对象已存在且可读。
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                status: "未启动".into(),
                ..Shared::default()
            })),
            slot: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            monitor: Arc::new(Mutex::new(DcrSession::default())),
            persist: PersistShared::new(),
            persist_paths: Arc::new(Mutex::new(None)),
            persist_thread: None,
        }
    }

    /// 返回唯一会话句柄；`configure_monitor` 不会替换该 Arc。
    pub fn monitor(&self) -> Monitor {
        Arc::clone(&self.monitor)
    }

    /// 启动前配置独立 JSON 快照与旧本机历史来源。
    ///
    /// - 加载快照并填充既有会话（不恢复运行中状态）。
    /// - 旧本机历史只在首次导入，逐行标明 `[历史]`，不搬运单条耗时。
    /// - 保存失败通过会话日志与 `status()` 明确可见，绝不静默。
    pub fn configure_monitor(&mut self, path: PathBuf, history: PathBuf) -> Result<()> {
        let loaded = match std::fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<SessionSnapshot>(&bytes)
                    .with_context(|| format!("DCR 会话快照损坏：{}", path.display()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("读取 DCR 会话快照失败：{}", path.display()));
            }
        };
        let mut snapshot = loaded.unwrap_or_default();
        if !snapshot.history_imported {
            match read_history(&history) {
                Ok(calls) => {
                    // 历史早于现有日志：从后往前插到队首，保持时间顺序。
                    let lines = crate::dcr_session::history_lines(&calls);
                    for line in lines.into_iter().rev() {
                        snapshot.session.logs.push_front(line);
                    }
                    while snapshot.session.logs.len() > LOG_CAP {
                        snapshot.session.logs.pop_front();
                    }
                }
                Err(error) => {
                    let now = now_ms();
                    push_line(
                        &mut snapshot.session,
                        now,
                        &format!("[历史] 读取本机记录失败：{error:#}"),
                    );
                }
            }
            snapshot.history_imported = true;
        }
        snapshot.session.sanitize_after_load();
        {
            let mut session = self
                .monitor
                .lock()
                .map_err(|_| anyhow::anyhow!("DCR 会话锁不可用，无法加载快照"))?;
            *session = snapshot.session;
        }
        self.persist
            .history_imported
            .store(snapshot.history_imported, Ordering::SeqCst);
        if let Ok(mut paths) = self.persist_paths.lock() {
            *paths = Some((path, history));
        }
        if self.persist_thread.is_none() {
            let shared = Arc::clone(&self.persist);
            let monitor = Arc::clone(&self.monitor);
            let paths = Arc::clone(&self.persist_paths);
            self.persist_thread = Some(
                std::thread::Builder::new()
                    .name("dcr-session-persist".into())
                    .spawn(move || persist_loop(shared, monitor, paths))?,
            );
        }
        Ok(())
    }

    /// 受理一次启动：同步校验目录并置 starting 阻止重复，检测与派生都在后台。
    ///
    /// 检测结果为 Present 或 Unknown 时明确失败（不冒险启动）；用户关闭外部
    /// 实例后可直接再次调用 start，本方法不做任何跨次缓存。
    pub fn start(&mut self, config: &DcrConfig) -> Result<()> {
        ensure!(
            !self.running(),
            "DCR remote 已在运行或启动中，忽略重复启动"
        );
        validate_workdir(&config.workdir)?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let shared: SharedState = Arc::new(Mutex::new(Shared {
            starting: true,
            status: "正在后台检查外部 DCR remote…".into(),
            ..Shared::default()
        }));
        let slot: ProcessSlot = Arc::new(Mutex::new(None));
        self.shared = Arc::clone(&shared);
        self.slot = Arc::clone(&slot);
        let counter = Arc::clone(&self.generation);
        let monitor = Arc::clone(&self.monitor);
        if let Ok(mut session) = monitor.lock() {
            session.connection = "connecting".into();
            session.last_activity = now_ms();
        }
        let config = config.clone();
        std::thread::spawn(move || {
            run_background(config, shared, slot, monitor, generation, counter, detect_external);
        });
        Ok(())
    }

    /// 只终止本服务启动的进程树；失败时保留句柄以便重试，不影响外部 DCR。
    pub fn stop(&mut self) -> Result<()> {
        // 先递增代次：尚未派生的后台检测会据此放弃，避免 stop 之后仍 spawn。
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut guard = self
            .slot
            .lock()
            .map_err(|_| anyhow::anyhow!("DCR 进程槽锁不可用，无法停止"))?;
        match guard.as_ref().map(ProcessHandle::cancel) {
            None => {}
            Some(Ok(())) => *guard = None,
            Some(Err(error)) => {
                // 不能静默丢句柄：保留后返回错误，允许再次尝试停止。
                return Err(anyhow::anyhow!(
                    "停止 DCR remote 进程树失败（不影响外部 DCR）：{error}"
                ));
            }
        }
        drop(guard);
        if let Ok(mut state) = self.shared.lock() {
            state.starting = false;
            state.running = false;
            state.pid = None;
            state.status = "已停止（仅清理本服务启动的进程）".into();
        }
        if let Ok(mut session) = self.monitor.lock() {
            mark_stopped(&mut session, now_ms());
        }
        Ok(())
    }

    /// 界面定时器钩子：只做非阻塞状态校正，日志采集已在后台完成。
    pub fn poll(&mut self) {
        let exited = match self.slot.lock() {
            Ok(guard) => guard
                .as_ref()
                .is_some_and(|process| process.pid().is_none()),
            Err(_) => return,
        };
        if exited
            && let Ok(mut state) = self.shared.lock()
            && state.running
        {
            state.running = false;
            state.pid = None;
            if state.last_error.is_none() {
                state.status = "进程已退出（远程连接已结束）".into();
            }
        }
        if exited
            && let Ok(mut session) = self.monitor.lock()
        {
            // 幂等：已停止或无活动调用时不会改变 last_activity。
            mark_stopped(&mut session, now_ms());
        }
    }

    /// 进程是否存活或正在启动；仅表示进程/启动状态，不能推断已连接。
    pub fn running(&self) -> bool {
        self.shared
            .lock()
            .map(|state| state.running || state.starting)
            .unwrap_or(false)
    }

    /// 人类可读状态；按官方远端日志更新，明确区分“本地进程存活”与“远程在线”。
    ///
    /// 快照保存失败会附加在此处，保证持久化故障对用户明确可见。
    pub fn status(&self) -> String {
        let base = match self.shared.lock() {
            Ok(state) => match &state.last_error {
                Some(error) => format!("启动失败：{error}"),
                None => state.status.clone(),
            },
            Err(_) => "状态不可用（锁已中毒）".into(),
        };
        match self.persist.error.lock() {
            Ok(error) => match error.as_ref() {
                Some(error) => format!("{base} · {error}"),
                None => base,
            },
            Err(_) => format!("{base} · 持久化状态不可用（锁已中毒）"),
        }
    }
}

impl Default for DcrService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DcrService {
    /// 退出时只清理本服务启动的进程树，绝不波及其它 DCR 实例。
    ///
    /// 同时停止持久化线程并写最后一次快照；线程在本服务存续期内才存在。
    fn drop(&mut self) {
        let _ = self.stop();
        self.persist.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.persist_thread.take() {
            let _ = thread.join();
        }
        if let Some((path, _)) = self
            .persist_paths
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            let session = self
                .monitor
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            let snapshot = SessionSnapshot {
                session,
                history_imported: self.persist.history_imported.load(Ordering::SeqCst),
            };
            if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
                // 退出路径尽力保存；失败无处上报，但不会 panic。
                let _ = atomic_write(&path, &bytes);
            }
        }
    }
}

/// 后台阶段：先做一次新的外部检测（Present/Unknown 均明确失败），再派生进程。
fn run_background<F>(
    config: DcrConfig,
    shared: SharedState,
    slot: ProcessSlot,
    monitor: Monitor,
    generation: u64,
    counter: Arc<AtomicU64>,
    detect: F,
) where
    F: FnOnce() -> External + Send + 'static,
{
    let detected = detect();
    if !current(&counter, generation) {
        return;
    }
    match detected {
        External::Present(pid) => {
            fail_if_current(
                &shared,
                &monitor,
                &counter,
                generation,
                &format!("检测到已有外部 DCR remote 进程（PID {pid}），不能重复启动；请先停止外部实例"),
            );
            return;
        }
        External::Unknown => {
            fail_if_current(
                &shared,
                &monitor,
                &counter,
                generation,
                "无法确认外部 DCR remote 状态（检测不可用），为避免重复启动已中止",
            );
            return;
        }
        External::None => {}
    }
    let (program, args, mut env) = launch_plan(&config);
    match read_stream_guard_env() {
        Ok(value) => env.push(value),
        Err(error) => {
            fail_if_current(
                &shared,
                &monitor,
                &counter,
                generation,
                &format!("准备 DCR 文件读取修复失败：{error:#}"),
            );
            return;
        }
    }
    let command = StreamCommand::new(program.clone())
        .args(args)
        .cwd(config.workdir.clone())
        .env(env)
        .run_id("dcr-remote")
        .stderr(Stderr::Streamed);
    let (process, events) = match command.start() {
        Ok(pair) => pair,
        Err(error) => {
            fail_if_current(
                &shared,
                &monitor,
                &counter,
                generation,
                &format!("启动 DCR remote 失败：{error}"),
            );
            return;
        }
    };
    let pid = process.pid();
    let mut guard = match slot.lock() {
        Ok(guard) => guard,
        Err(_) => {
            let _ = process.cancel();
            fail_if_current(
                &shared,
                &monitor,
                &counter,
                generation,
                "状态锁不可用，已回收子进程",
            );
            return;
        }
    };
    // 持槽锁检查代次：stop() 先增代次再取句柄，确保不会在 stop 之后留下进程。
    if !current(&counter, generation) {
        drop(guard);
        let _ = process.cancel();
        return;
    }
    if let Ok(mut state) = shared.lock() {
        state.starting = false;
        state.running = true;
        state.pid = pid;
        state.last_error = None;
        state.status = connecting_status(pid);
    }
    if let Ok(mut session) = monitor.lock()
        && current(&counter, generation)
    {
        // 取得锁后再核对代次：stop 之后旧启动不得重置新会话。
        begin_session(&mut session, now_ms());
    }
    *guard = Some(process);
    drop(guard);
    std::thread::spawn(move || drain(events, shared, monitor, counter, generation));
}

/// 代次是否仍然有效；失效说明期间发生过 stop 或新的 start。
fn current(counter: &Arc<AtomicU64>, generation: u64) -> bool {
    counter.load(Ordering::SeqCst) == generation
}

/// 会话写入闸门：先取得 monitor 锁，再核对 generation。
///
/// 这样 stop 递增代次后，旧 drain 即使已经通过入口检查也无法再改同一 monitor。
struct MonitorGate<'a> {
    monitor: &'a Monitor,
    counter: &'a Arc<AtomicU64>,
    generation: u64,
}

impl MonitorGate<'_> {
    /// 代次仍有效时在锁内执行；失效返回 None，不写入任何状态。
    fn with<R>(&self, apply: impl FnOnce(&mut DcrSession) -> R) -> Option<R> {
        let mut session = self.monitor.lock().ok()?;
        if !current(self.counter, self.generation) {
            return None;
        }
        Some(apply(&mut session))
    }
}

/// 仅在本次启动仍然有效时写入失败状态。
fn fail_if_current(
    shared: &SharedState,
    monitor: &Monitor,
    counter: &Arc<AtomicU64>,
    generation: u64,
    message: &str,
) {
    if !current(counter, generation) {
        return;
    }
    if let Ok(mut state) = shared.lock() {
        state.starting = false;
        state.running = false;
        state.pid = None;
        state.last_error = Some(message.to_string());
        state.status = format!("启动失败：{message}");
    }
    if let Ok(mut session) = monitor.lock()
        && current(counter, generation)
    {
        mark_failed(&mut session, now_ms(), message);
    }
}

/// 构造启动计划：程序、固定参数、仅注入子进程的代理环境。
///
/// 参数全部为常量，工作目录只作为子进程 cwd 传入，绝不拼进命令行字符串，
/// 因此不存在 shell 注入面；程序直接指向 `npx.cmd`/`npx`，不经过 `cmd /C`。
fn launch_plan(config: &DcrConfig) -> (PathBuf, Vec<String>, Vec<(String, String)>) {
    let program = if cfg!(windows) {
        PathBuf::from("npx.cmd")
    } else {
        PathBuf::from("npx")
    };
    let args = vec!["-y".to_string(), PACKAGE.to_string(), "remote".to_string()];
    let mut env = Vec::new();
    let proxy = config.proxy.trim();
    if !proxy.is_empty() {
        // 只落在子进程环境里，不改动本进程环境，也不影响用户已有 DCR。
        env.push(("HTTP_PROXY".to_string(), proxy.to_string()));
        env.push(("HTTPS_PROXY".to_string(), proxy.to_string()));
    }
    (program, args, env)
}

/// 将固定版本的读取修复写入独立运行目录，仅通过子进程 NODE_OPTIONS 加载。
/// 内容哈希隔离不同构建，保留已有 Node 参数；不修改 npm 缓存或全局环境。
fn read_stream_guard_env() -> Result<(String, String)> {
    use std::hash::{Hash, Hasher};
    let source = include_str!("../assets/dcr-read-stream-guard.cjs");
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    let directory = std::env::temp_dir().join("rs-agent-router-runtime");
    std::fs::create_dir_all(&directory).context("创建 DCR 运行目录")?;
    let path = directory.join(format!("dcr-read-stream-{:016x}.cjs", hash.finish()));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(source) {
        std::fs::write(&path, source).context("写入 DCR 文件读取修复")?;
    }
    // Node 的选项解析支持双引号路径；用正斜杠避免 Windows 反斜杠转义。
    let path = path.to_string_lossy().replace('\\', "/");
    ensure!(!path.contains('"'), "DCR 运行目录不能包含双引号");
    let existing = std::env::var("NODE_OPTIONS").unwrap_or_default();
    Ok((
        "NODE_OPTIONS".into(),
        format!("{existing} --require \"{path}\""),
    ))
}

/// 工作目录必须真实存在且为目录，否则启动前明确报错。
fn validate_workdir(workdir: &Path) -> Result<()> {
    let metadata = std::fs::metadata(workdir)
        .with_context(|| format!("DCR 工作目录不可用：{}", workdir.display()))?;
    ensure!(
        metadata.is_dir(),
        "DCR 工作目录不是目录：{}",
        workdir.display()
    );
    Ok(())
}

/// 后台排空子进程事件，脱敏后写入有界日志，并按官方日志更新远程状态。
///
/// 会话写入全部经过 generation 校验：旧会话的 drain 不会覆盖新会话状态。
fn drain(
    events: std::sync::mpsc::Receiver<Event>,
    shared: SharedState,
    monitor: Monitor,
    counter: Arc<AtomicU64>,
    generation: u64,
) {
    let mut tracker = EventTracker::new();
    let gate = MonitorGate {
        monitor: &monitor,
        counter: &counter,
        generation,
    };
    while let Ok(event) = events.recv() {
        let now = now_ms();
        match event {
            Event::Stdout { line, .. } => handle_line(&shared, &gate, &mut tracker, &line, false, now),
            Event::Stderr { line, .. } => handle_line(&shared, &gate, &mut tracker, &line, true, now),
            Event::Error { message, .. } => {
                let safe = redact_secrets(&message);
                if let Ok(mut state) = shared.lock() {
                    state.running = false;
                    state.pid = None;
                    state.last_error = Some(safe.clone());
                    state.status = format!("启动失败：{safe}");
                }
                gate.with(|session| mark_failed(session, now, &safe));
            }
            Event::Exited {
                exit_code,
                cancelled,
                ..
            } => {
                if let Ok(mut state) = shared.lock() {
                    state.running = false;
                    state.pid = None;
                    state.status = if cancelled {
                        "已停止".into()
                    } else {
                        format!("进程已退出（退出码 {exit_code:?}），远程连接已结束")
                    };
                }
                gate.with(|session| mark_stopped(session, now));
            }
            _ => {}
        }
    }
}

/// 单行处理：原始行先交给事件识别器；被工具事件/结果消费的行不再参与连接判定。
fn handle_line(
    shared: &SharedState,
    gate: &MonitorGate,
    tracker: &mut EventTracker,
    line: &str,
    is_stderr: bool,
    now: i64,
) {
    // 1. 取锁后再核对代次；被消费的行（含结果 JSON）绝不用于连接状态。
    let consumed = gate
        .with(|session| tracker.observe(session, line, now))
        .unwrap_or(false);
    if consumed {
        return;
    }
    // 2. 识别失败的原始工具行也不得入库：宁可少记，也不重复记录 args/泄露秘密。
    if is_tool_payload(line) {
        return;
    }
    // 3. 进程级状态沿用官方连接标记。
    if let Ok(mut state) = shared.lock() {
        update_connection(&mut state, line);
    }
    // 4. 连接/授权/错误行进入唯一会话日志；仅真实连接变化才刷新活动时间。
    let link = classify_connection(line);
    let logged = is_session_log(line, is_stderr);
    if link.is_none() && !logged {
        return;
    }
    gate.with(|session| {
        if let Some(link) = link {
            let next = link.as_str();
            if session.connection != next {
                session.connection = next.to_string();
                session.last_activity = now;
            }
        }
        if logged {
            push_line(session, now, line);
        }
    });
}

/// 按官方远端日志把状态从“连接中”推进到在线/离线，不再永远等待连接确认。
fn update_connection(state: &mut Shared, line: &str) {
    match classify_connection(line) {
        Some(Link::Failed) => {
            state.last_error = Some(line.to_string());
            state.status = format!("启动失败：{line}");
        }
        Some(Link::Offline) => state.status = offline_status(state.pid),
        Some(Link::Online) => state.status = ready_status(state.pid),
        Some(Link::Connecting) if state.running => state.status = connecting_status(state.pid),
        _ => {}
    }
}

/// 远程连接中的状态文案；明确“尚未确认在线”。
fn connecting_status(pid: Option<u32>) -> String {
    format!("远程连接中（PID {}，尚未确认在线）", pid.unwrap_or(0))
}

/// 仅由官方远端就绪日志推进到的在线状态。
fn ready_status(pid: Option<u32>) -> String {
    format!("远程 DCR 在线（PID {}）", pid.unwrap_or(0))
}

/// 远端会话/通道断开时的状态；本地进程仍在，但不代表远程在线。
fn offline_status(pid: Option<u32>) -> String {
    format!("远程 DCR 已离线（PID {}，本地进程仍存活）", pid.unwrap_or(0))
}

/// 官方工具事件行的原始形态（含 args）；识别失败时也必须丢弃，不得入库。
pub(crate) fn is_tool_payload(line: &str) -> bool {
    line.contains("Received tool call ")
        || (line.contains("Tool call ")
            && (line.contains(" completed:") || line.contains(" failed:")))
}

/// 是否进入会话日志：连接状态、官方授权地址/短码，以及 stderr 错误行。
///
/// 工具调用原文（含 args）由事件识别器消费后不会到达这里，避免重复记录与泄露。
pub(crate) fn is_session_log(line: &str, is_stderr: bool) -> bool {
    if classify_connection(line).is_some() || looks_like_user_code(line) {
        return true;
    }
    let lowered = line.to_ascii_lowercase();
    const AUTH: &[&str] = &[
        "verification_uri",
        "verification url",
        "verification code",
        "user_code",
        "device code",
        "authorize",
        "login/device",
    ];
    if AUTH.iter().any(|marker| lowered.contains(marker)) {
        return true;
    }
    if !is_stderr {
        return false;
    }
    const ERRORS: &[&str] = &[
        "error", "fail", "denied", "unauthor", "timeout", "timed out", "econn", "enotfound",
        "crash", "refused",
    ];
    ERRORS.iter().any(|marker| lowered.contains(marker))
}

/// 形如 `XXXX-XXXX` 的短设备授权码（官方 user_code），需要原样展示给用户。
fn looks_like_user_code(line: &str) -> bool {
    let trimmed = line.trim();
    if !(6..=12).contains(&trimmed.len()) {
        return false;
    }
    let mut parts = trimmed.split('-');
    let (Some(left), Some(right), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let valid = |part: &str| {
        part.len() >= 3
            && part
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
    };
    valid(left) && valid(right)
}

/// 展示前去除敏感凭据与 URL query，保留 verification_uri 与短 user_code。
pub(crate) fn redact_secrets(line: &str) -> String {
    let without_query = strip_url_queries(line);
    let without_bearer = redact_bearer(&without_query);
    let without_pairs = redact_key_values(without_bearer);
    redact_long_tokens(&without_pairs)
}

/// 截掉 URL 的 `?query`/`#fragment`，保留 scheme/host/path 供用户识别授权地址。
fn strip_url_queries(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with("http://") || input[index..].starts_with("https://") {
            let mut end = index;
            while end < bytes.len()
                && !matches!(
                    bytes[end],
                    b' ' | b'\t' | b'"' | b'\'' | b'<' | b'>' | b'|' | b'\\' | b')' | b']'
                )
            {
                end += 1;
            }
            let url = &input[index..end];
            match url.find(['?', '#']) {
                Some(cut) => {
                    output.push_str(&url[..cut]);
                    output.push_str("?<已隐藏>");
                }
                None => output.push_str(url),
            }
            index = end;
        } else {
            let ch = input[index..].chars().next().unwrap();
            output.push(ch);
            index += ch.len_utf8();
        }
    }
    output
}

/// 把 `Bearer xxx` 形式的授权头替换为占位符。
fn redact_bearer(input: &str) -> String {
    let lowered = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while let Some(found) = lowered[index..].find("bearer") {
        let at = index + found;
        output.push_str(&input[index..at]);
        let mut end = at + "bearer".len();
        while end < input.len() && matches!(input.as_bytes()[end], b' ' | b'\t') {
            end += 1;
        }
        let value_start = end;
        while end < input.len()
            && !matches!(
                input.as_bytes()[end],
                b' ' | b'\t' | b'"' | b'\'' | b',' | b';' | b'}' | b')' | b'\r' | b'\n'
            )
        {
            end += 1;
        }
        if end > value_start {
            output.push_str("Bearer <已隐藏>");
            index = end;
        } else {
            output.push_str(&input[at..value_start]);
            index = value_start;
        }
    }
    output.push_str(&input[index..]);
    output
}

/// 把 `token=`/`api_key:` 等键值中的值替换为占位符（大小写不敏感）。
fn redact_key_values(mut text: String) -> String {
    const KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
    ];
    for key in KEYS {
        let mut search_from = 0;
        loop {
            let lowered = text.to_ascii_lowercase();
            let Some(found) = lowered[search_from..].find(key) else {
                break;
            };
            let at = search_from + found;
            let boundary_ok = at == 0 || {
                let before = text.as_bytes()[at - 1];
                !(before.is_ascii_alphanumeric() || before == b'_')
            };
            let bytes = text.as_bytes();
            let mut cursor = at + key.len();
            while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                cursor += 1;
            }
            if boundary_ok && cursor < bytes.len() && matches!(bytes[cursor], b'=' | b':') {
                cursor += 1;
                while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                    cursor += 1;
                }
                let value_start = cursor;
                while cursor < bytes.len()
                    && !matches!(
                        bytes[cursor],
                        b'&' | b' '
                            | b'\t'
                            | b'"'
                            | b'\''
                            | b','
                            | b';'
                            | b'}'
                            | b')'
                            | b']'
                            | b'\r'
                            | b'\n'
                    )
                {
                    cursor += 1;
                }
                if cursor > value_start {
                    text.replace_range(value_start..cursor, "<已隐藏>");
                }
                search_from = (value_start + "<已隐藏>".len()).min(text.len());
            } else {
                search_from = at + key.len();
            }
            if search_from >= text.len() {
                break;
            }
        }
    }
    text
}

/// 替换疑似 JWT/长凭据的 base64url 串，避免日志里出现可重放的令牌。
fn redact_long_tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let ch = input[index..].chars().next().unwrap();
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            let mut end = index;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'-' | b'.'))
            {
                end += 1;
            }
            let run = &input[index..end];
            if run.len() >= 40 && (run.contains('.') || run.len() >= 64) {
                output.push_str("<已隐藏>");
            } else {
                output.push_str(run);
            }
            index = end;
        } else {
            output.push(ch);
            index += ch.len_utf8();
        }
    }
    output
}

/// 按字符数截断，保留省略号；`max` 为结果的最大字符数。
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut output: String = text.chars().take(max - 1).collect();
    output.push('…');
    output
}

/// 从本机 `tool-history.jsonl` 只读解析最近工具调用记录。
///
/// - 缺失文件返回空；只从文件尾部有界读取，兼容历史轮转/截断。
/// - 允许末尾未写完行；只返回最近 [`HISTORY_CAP`] 条。
/// - `output` 最多 [`HISTORY_OUTPUT_MAX`] 字符；不返回 `arguments`。
/// - 完好的行中出现无法解析的格式错误时返回错误，不做全静默丢弃。
pub fn read_history(path: &Path) -> Result<Vec<DcrCall>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(Vec::new());
    }
    let start = length.saturating_sub(HISTORY_TAIL_MAX);
    // 多读一字节，确保尾部片段的首个换行仍可作为行边界使用。
    let from = if start > 0 { start - 1 } else { 0 };
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    // 即使文件在读取期间增长，take 也把实际读取字节数硬性封顶。
    file.by_ref()
        .take(HISTORY_TAIL_MAX + 1)
        .read_to_end(&mut bytes)?;
    let trailing_partial = bytes.last() != Some(&b'\n');
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        match text.find('\n') {
            Some(cut) => text = text[cut + 1..].to_string(),
            None => return Ok(Vec::new()),
        }
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if trailing_partial {
        // 末尾未写完的行按约定忽略，不视为格式错误。
        lines.pop();
    }
    let mut calls: VecDeque<DcrCall> = VecDeque::new();
    let mut malformed = 0usize;
    for line in lines.iter().rev() {
        if calls.len() >= HISTORY_CAP {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_call(trimmed) {
            Ok(call) => calls.push_front(call),
            Err(_) => malformed += 1,
        }
    }
    if malformed > 0 {
        anyhow::bail!(
            "DCR 工具历史存在无法解析的行（{malformed} 行，{}），不能静默忽略格式错误",
            path.display()
        );
    }
    Ok(calls.into_iter().collect())
}

/// 解析单行 JSON；只取时间、工具名、耗时、输出与错误标记，丢弃 arguments。
fn parse_call(line: &str) -> Result<DcrCall> {
    let value: serde_json::Value =
        serde_json::from_str(line).context("DCR 历史行不是合法 JSON")?;
    let object = value.as_object().context("DCR 历史行不是 JSON 对象")?;
    let timestamp = object
        .get("timestamp")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_name = object
        .get("toolName")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    ensure!(
        !tool_name.is_empty() || !timestamp.is_empty(),
        "DCR 历史行缺少 toolName 与 timestamp"
    );
    let duration_ms = object.get("duration").and_then(serde_json::Value::as_u64);
    let output_value = object.get("output");
    let is_error = output_value
        .and_then(|value| value.get("isError"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || object
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    let output = truncate_chars(&render_output(output_value), HISTORY_OUTPUT_MAX);
    Ok(DcrCall {
        timestamp,
        tool_name,
        duration_ms,
        output,
        is_error,
    })
}

/// 把 MCP 结果对象渲染成纯文本；优先 content 文本，其次 structuredContent。
pub(crate) fn render_output(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Object(object)) => {
            if let Some(items) = object.get("content").and_then(|value| value.as_array()) {
                let parts: Vec<String> = items
                    .iter()
                    .filter_map(|item| {
                        item.get("text")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                            .or_else(|| item.as_str().map(str::to_string))
                    })
                    .collect();
                if !parts.is_empty() {
                    return parts.join("\n");
                }
            }
            object
                .get("structuredContent")
                .map(serde_json::Value::to_string)
                .unwrap_or_default()
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// 后台探测外部 DCR remote；不可用时返回 Unknown，绝不阻塞调用方。
fn detect_external() -> External {
    #[cfg(windows)]
    {
        detect_external_windows()
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 平台没有内置的进程命令行查询手段，明确返回 Unknown。
        External::Unknown
    }
}

/// Windows 下用只读 CIM 查询匹配 `desktop-commander` 且带 `remote` 的进程。
#[cfg(windows)]
fn detect_external_windows() -> External {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // ErrorActionPreference=Stop：CIM 查询失败会变成终止错误并以非零退出，
    // 由下方 status.success() 判定映射为 Unknown，避免误报“未检测到”。
    const SCRIPT: &str = "$ErrorActionPreference='Stop'; Get-CimInstance Win32_Process -Filter \"Name='node.exe' or Name='cmd.exe'\" | Where-Object { $_.CommandLine -match 'desktop-commander' -and $_.CommandLine -match ' remote(\\s|$)' } | ForEach-Object { [Console]::Out.WriteLine($_.ProcessId) }";
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            parse_pids(&String::from_utf8_lossy(&output.stdout))
        }
        _ => External::Unknown,
    }
}

/// 从逐行 PID 输出里取第一个有效进程号。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_pids(text: &str) -> External {
    for line in text.lines() {
        if let Ok(pid) = line.trim().parse::<u32>() {
            return External::Present(pid);
        }
    }
    External::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每例独立临时文件，避免并发测试互相污染。
    fn temp_file(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "dcr-test-{}-{tag}-{index}.jsonl",
            std::process::id()
        ))
    }

    fn write_lines(path: &Path, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(path, body).unwrap();
    }

    /// 空状态：首次启动前不派生、不检测，也不得声称已连接。
    #[test]
    fn fresh_service_is_idle_and_never_claims_connection() {
        let service = DcrService::new();
        assert!(!service.running());
        let status = service.status();
        assert!(status.contains("未启动"), "got {status}");
        assert!(!status.contains("已连接"), "got {status}");
        assert!(service.status().contains("未启动"));
        assert!(service.monitor().lock().unwrap().logs.is_empty());
    }

    /// 缺失文件按空历史处理，而不是报错。
    #[test]
    fn missing_history_is_empty() {
        let path = temp_file("missing");
        assert!(read_history(&path).unwrap().is_empty());
    }

    /// 末尾未写完行被容忍，output 截断到 4000 字符，arguments 不外泄。
    #[test]
    fn history_truncates_and_ignores_arguments() {
        let path = temp_file("parse");
        let long_output = "x".repeat(5000);
        let line = serde_json::json!({
            "timestamp": "2026-09-16T01:22:57.146Z",
            "toolName": "read_file",
            "arguments": {"path": "ARG_SECRET_VALUE"},
            "output": {"content": [{"type": "text", "text": long_output}], "isError": false},
            "duration": 77
        })
        .to_string();
        let error_line = serde_json::json!({
            "timestamp": "2026-09-16T01:23:00.000Z",
            "toolName": "write_file",
            "arguments": {"path": "ARG_SECRET_VALUE"},
            "output": {"isError": true, "content": [{"type": "text", "text": "denied"}]},
            "duration": 3
        })
        .to_string();
        std::fs::write(&path, format!("{line}\n{error_line}\n{{\"timestamp\":\"partial")).unwrap();
        let calls = read_history(&path).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_name, "read_file");
        assert_eq!(calls[0].duration_ms, Some(77));
        assert!(!calls[0].is_error);
        assert_eq!(calls[0].output.chars().count(), HISTORY_OUTPUT_MAX);
        assert!(calls[1].is_error);
        for call in &calls {
            assert!(!call.output.contains("ARG_SECRET_VALUE"));
        }
    }

    /// 完整但无法解析的行必须报错，不能全部静默跳过。
    #[test]
    fn malformed_history_is_reported() {
        let path = temp_file("malformed");
        write_lines(&path, &["not-a-json-object".to_string()]);
        assert!(read_history(&path).is_err());
    }

    /// 只返回最近 100 条，且保持时间顺序。
    #[test]
    fn history_returns_latest_hundred() {
        let path = temp_file("hundred");
        let lines: Vec<String> = (0..150)
            .map(|index| {
                serde_json::json!({
                    "timestamp": format!("2026-09-16T00:00:{index:02}.000Z"),
                    "toolName": format!("tool_{index}"),
                    "output": {"content": [{"type": "text", "text": "ok"}]},
                    "duration": index
                })
                .to_string()
            })
            .collect();
        write_lines(&path, &lines);
        let calls = read_history(&path).unwrap();
        assert_eq!(calls.len(), HISTORY_CAP);
        assert_eq!(calls[0].tool_name, "tool_50");
        assert_eq!(calls[99].tool_name, "tool_149");
    }

    /// 超过尾部读取上限的文件仍只读有界一段，并正确返回最新 100 条。
    #[test]
    fn history_read_is_bounded_for_growing_files() {
        let path = temp_file("large");
        let total = 40_000;
        let lines: Vec<String> = (0..total)
            .map(|index| {
                serde_json::json!({
                    "timestamp": "2026-09-16T00:00:00.000Z",
                    "toolName": format!("tool_{index}"),
                    "output": {"content": [{"type": "text", "text": "ok"}]},
                    "duration": 1
                })
                .to_string()
            })
            .collect();
        let body = lines.join("\n");
        assert!(
            body.len() as u64 > HISTORY_TAIL_MAX,
            "测试文件需超过尾部上限"
        );
        write_lines(&path, &lines);
        let calls = read_history(&path).unwrap();
        assert_eq!(calls.len(), HISTORY_CAP);
        assert_eq!(calls[99].tool_name, format!("tool_{}", total - 1));
        assert_eq!(calls[0].tool_name, format!("tool_{}", total - 100));
    }

    /// 脱敏：URL query、Bearer、键值令牌被隐藏，验证地址与短授权码保留。
    #[test]
    fn redaction_keeps_verification_material_hides_tokens() {
        let auth_line = "请访问 https://mcp.example.com/authorize?access_token=SUPERSECRET 完成授权";
        let safe = redact_secrets(auth_line);
        assert!(!safe.contains("SUPERSECRET"));
        assert!(safe.contains("https://mcp.example.com/authorize"));
        assert!(safe.contains("已隐藏"));

        assert!(!redact_secrets("Authorization: Bearer abc.def.ghi").contains("abc.def.ghi"));
        let kv = redact_secrets("token=abcdef123&api_key:zzz999");
        assert!(!kv.contains("abcdef123") && !kv.contains("zzz999"));
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature";
        assert!(!redact_secrets(jwt).contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"));

        // 官方 device flow 的 verification_uri 与短 user_code 必须可见。
        let uri = "https://github.com/login/device";
        assert_eq!(redact_secrets(uri), uri);
        assert_eq!(redact_secrets("      WDJB-MJHT"), "      WDJB-MJHT");
    }

    /// 授权地址/短码、连接与错误行进入会话日志；普通安装噪音不进入。
    #[test]
    fn session_log_selection_covers_auth_and_connection() {
        assert!(is_session_log("      WDJB-MJHT", false));
        assert!(is_session_log("https://github.com/login/device", false));
        assert!(is_session_log("verification_uri: https://example.com/device", false));
        assert!(is_session_log("✅ Channel subscribed (recovered after 1 attempt)", false));
        assert!(is_session_log("❌ Channel error: socket closed", true));
        assert!(is_session_log("Starting MCP Device", false));
        assert!(!is_session_log("Added 5 packages in 2s", false));
        assert!(!is_session_log("npm notice ok", false));
        assert!(!looks_like_user_code("de4b36c0-2bd1-4075-8dfd-c0196f095858"));
        assert!(!looks_like_user_code("2026-09-16"));
        // 工具原始行一律识别为 payload，识别失败时也不会被当作普通日志记录。
        assert!(is_tool_payload(
            "🔧 Received tool call id1: read_file {\"path\":\"SECRET\"} metadata: {}"
        ));
        assert!(is_tool_payload("✅ Tool call read_file completed:"));
        assert!(is_tool_payload("❌ Tool call write_file failed: boom"));
        assert!(!is_tool_payload("✅ Channel subscribed"));
    }

    /// 结果体不得伪造连接，工具 args 不得进入会话日志，真实连接变化才刷新活动时间。
    #[test]
    fn session_result_body_never_forges_connection() {
        let shared: SharedState = Arc::new(Mutex::new(Shared::default()));
        let monitor: Monitor = Arc::new(Mutex::new(DcrSession {
            connection: "connecting".into(),
            ..DcrSession::default()
        }));
        let counter = Arc::new(AtomicU64::new(1));
        let gate = MonitorGate {
            monitor: &monitor,
            counter: &counter,
            generation: 1,
        };
        let mut tracker = EventTracker::new();
        handle_line(
            &shared,
            &gate,
            &mut tracker,
            "🔧 Received tool call id1: read_file {\"path\":\"/secret/SECRET\"} metadata: {}",
            false,
            100,
        );
        handle_line(&shared, &gate, &mut tracker, "✅ Tool call read_file completed:", false, 200);
        handle_line(
            &shared,
            &gate,
            &mut tracker,
            "{\"content\":[{\"type\":\"text\",\"text\":\"Device ready: forged\"}],\"isError\":false}",
            false,
            201,
        );
        {
            let session = monitor.lock().unwrap();
            assert_eq!(session.connection, "connecting", "结果体不得伪造在线");
            // 200 是真实的调用结束；若结果体（201）参与判定会变成 201。
            assert_eq!(session.last_activity, 200, "结果体不得刷新活动时间");
            assert!(
                session.logs.iter().any(|line| line.text.contains("开始调用 read_file（id1）")),
                "应记录开始调用"
            );
            assert!(
                !session.logs.iter().any(|line| line.text.contains("SECRET")),
                "不得重复记录工具 args"
            );
            assert!(
                session.logs.iter().any(|line| line.text.contains("结果 read_file（id1）：Device ready: forged")),
                "完成结果摘要应入库"
            );
        }
        // 真实连接日志推进状态并刷新活动时间。
        handle_line(&shared, &gate, &mut tracker, "✅ Channel subscribed", false, 300);
        handle_line(&shared, &gate, &mut tracker, "npm notice ordinary line", false, 400);
        let session = monitor.lock().unwrap();
        assert_eq!(session.connection, "online");
        assert_eq!(session.last_activity, 300, "普通日志不刷新活动时间");
    }

    /// 状态只按官方远端日志推进，本地 MCP 调试行不得被当成远程在线。
    #[test]
    fn connection_status_follows_official_remote_logs() {
        let mut state = Shared {
            running: true,
            pid: Some(7),
            status: connecting_status(Some(7)),
            ..Shared::default()
        };
        // 本地 stdio transport 调试行与 Supabase 配置连接都不算远程在线。
        update_connection(&mut state, "[DEBUG] Connecting MCP client to transport");
        update_connection(&mut state, "   - 🔌 Connected to Remote MCP");
        assert!(state.status.contains("远程连接中"), "got {}", state.status);
        // 官方就绪日志才切到在线。
        update_connection(&mut state, "✅ Channel subscribed (recovered after 1 attempt)");
        assert!(state.status.contains("远程 DCR 在线"), "got {}", state.status);
        // 官方断连日志切到离线，且不再宣称在线。
        update_connection(&mut state, "❌ Channel error: socket closed — disconnecting");
        assert!(state.status.contains("已离线"), "got {}", state.status);
        assert!(!state.status.contains("在线（"), "got {}", state.status);
        // 进程失败明确显示失败。
        update_connection(&mut state, " - ❌ Device startup failed: boom");
        assert!(state.status.contains("启动失败"), "got {}", state.status);
    }

    /// 外部检测 Present/Unknown 都必须明确失败，且不派生任何进程。
    #[test]
    fn detection_gate_fails_closed() {
        let shared: SharedState = Arc::new(Mutex::new(Shared::default()));
        let slot: ProcessSlot = Arc::new(Mutex::new(None));
        let monitor: Monitor = Arc::new(Mutex::new(DcrSession::default()));
        let counter = Arc::new(AtomicU64::new(1));
        run_background(
            DcrConfig::default(),
            Arc::clone(&shared),
            Arc::clone(&slot),
            Arc::clone(&monitor),
            1,
            Arc::clone(&counter),
            || External::Present(4321),
        );
        let state = shared.lock().unwrap();
        assert!(state.last_error.as_deref().unwrap().contains("4321"));
        assert!(!state.running && !state.starting);
        assert!(slot.lock().unwrap().is_none());
        assert_eq!(monitor.lock().unwrap().connection, "failed");

        let shared: SharedState = Arc::new(Mutex::new(Shared::default()));
        let counter = Arc::new(AtomicU64::new(1));
        let monitor: Monitor = Arc::new(Mutex::new(DcrSession::default()));
        run_background(
            DcrConfig::default(),
            Arc::clone(&shared),
            Arc::new(Mutex::new(None)),
            Arc::clone(&monitor),
            1,
            Arc::clone(&counter),
            || External::Unknown,
        );
        let state = shared.lock().unwrap();
        assert!(state.last_error.as_deref().unwrap().contains("无法确认"));
        assert!(!state.running && !state.starting);
        assert_eq!(monitor.lock().unwrap().connection, "failed");
    }

    /// stop 递增代次后，尚未派生的后台启动必须放弃，不得在 stop 之后 spawn。
    #[test]
    fn superseded_detection_does_not_spawn() {
        let shared: SharedState = Arc::new(Mutex::new(Shared::default()));
        let slot: ProcessSlot = Arc::new(Mutex::new(None));
        let monitor: Monitor = Arc::new(Mutex::new(DcrSession::default()));
        let generation = 1u64;
        let counter = Arc::new(AtomicU64::new(generation));
        let bump = Arc::clone(&counter);
        run_background(
            DcrConfig::default(),
            Arc::clone(&shared),
            Arc::clone(&slot),
            Arc::clone(&monitor),
            generation,
            Arc::clone(&counter),
            move || {
                // 模拟检测期间用户点了停止（generation 被 stop 递增）。
                bump.fetch_add(1, Ordering::SeqCst);
                External::None
            },
        );
        assert!(slot.lock().unwrap().is_none());
        assert_eq!(counter.load(Ordering::SeqCst), generation + 1);
        // 检测被取代：会话不得被写入“会话开始”等运行态。
        assert!(monitor.lock().unwrap().logs.is_empty());
    }

    /// stop 即使没有进程也会推进代次，用于取消有待检测的启动。
    #[test]
    fn stop_bumps_generation_without_process() {
        let mut service = DcrService::new();
        let before = service.generation.load(Ordering::SeqCst);
        service.stop().unwrap();
        assert_eq!(service.generation.load(Ordering::SeqCst), before + 1);
        assert!(!service.running());
    }

    /// 默认配置符合约定，启动参数固定且不把工作目录拼进命令行。
    #[test]
    fn defaults_and_launch_plan_are_safe() {
        let config = DcrConfig::default();
        assert!(!config.auto_start);
        assert_eq!(config.proxy, "http://127.0.0.1:11809");
        assert_eq!(config.workdir, home_dir());

        let config = DcrConfig {
            auto_start: true,
            proxy: "http://127.0.0.1:11809".into(),
            workdir: PathBuf::from("C:/work dir/with & metachar; dir"),
        };
        let (program, args, env) = launch_plan(&config);
        assert!(program.to_string_lossy().ends_with("npx.cmd") || program.to_string_lossy() == "npx");
        assert_eq!(args, vec!["-y", PACKAGE, "remote"]);
        assert!(!args.iter().any(|arg| arg.contains("metachar")));
        assert_eq!(env.len(), 2);
        assert_eq!(env[0], ("HTTP_PROXY".into(), config.proxy.clone()));
        assert_eq!(env[1], ("HTTPS_PROXY".into(), config.proxy.clone()));

        let blank = launch_plan(&DcrConfig {
            proxy: "   ".into(),
            ..config
        });
        assert!(blank.2.is_empty());
    }

    /// 工作目录必须存在且为目录。
    #[test]
    fn workdir_must_be_an_existing_directory() {
        assert!(validate_workdir(&std::env::temp_dir()).is_ok());
        let file = temp_file("workdir");
        std::fs::write(&file, "x").unwrap();
        assert!(validate_workdir(&file).is_err());
        std::fs::remove_file(&file).unwrap();
        assert!(validate_workdir(&file).is_err());
    }

    /// PID 解析用于外部检测结果，空输出按“未检测到”处理。
    #[test]
    fn external_pid_parsing() {
        assert_eq!(parse_pids("1234\r\n"), External::Present(1234));
        assert_eq!(parse_pids("abc\n"), External::None);
        assert_eq!(parse_pids(""), External::None);
    }

    /// 截断结果不超过上限且保留省略号。
    #[test]
    fn truncation_is_bounded() {
        assert_eq!(truncate_chars("abcd", 4), "abcd");
        let cut = truncate_chars("abcde", 4);
        assert_eq!(cut.chars().count(), 4);
        assert!(cut.ends_with('…'));
    }
}
