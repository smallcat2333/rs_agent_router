# Agent Router 0.6.0 — CLI 执行服务

[![Windows CI](https://github.com/smallcat2333/rs_agent_router/actions/workflows/ci.yml/badge.svg)](https://github.com/smallcat2333/rs_agent_router/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A Windows desktop task router for Claude and Codex CLIs, with task monitoring and execution records.

社区项目，与 OpenAI、Anthropic 及其他 CLI/模型供应商无官方隶属关系。

## 独立构建

需要 Windows 10/11 x64、Rust stable 和 MSVC C++ 构建工具。

```powershell
cargo test --locked
cargo build --release --locked
.\target\release\rs_agent_router.exe show
```

构建不需要相邻仓库；使用时需要自行安装并登录所选 Claude/Codex CLI，在管理页设置路径和执行参数。仓库不包含账号或本机配置。

Windows CI 只运行单元测试并构建 EXE。可从已通过的 Actions 运行下载构建产物；真实 CLI、桌面交互和硬件相关验证需要在本机进行。

## 贡献与许可

原创代码使用 [MIT License](LICENSE)。参与方式见 [CONTRIBUTING.md](CONTRIBUTING.md)，内置依赖来源与许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 发布记录

2026-09-10 | 0.6.0 | 建立独立发布副本，内置 cli-stream 并保留原许可证，补充公开仓库文档和 Windows CI。

Harness 只分配工作，Rust App 决定执行 CLI、模型、强度、权限和超时。服务负责启动、输出、状态、取消和留痕；界面仅用于配置与监控，不维护聊天输入框、消息气泡或自建聊天历史。

## 使用

在 RustRover 直接 Run 打开管理页，选择 **Claude / Codex CLI**，填写模型和强度。设置修改后自动保存并用于后续任务，无需额外点击保存；已运行任务保留原快照。GLM 是 Claude CLI 的模型，例如 `GLM-5.2`，不再是独立 CLI。

模型下拉框支持刷新：Claude 读取 CC Switch 当前供应商配置，Codex 在后台查询所选 CLI 的模型目录；“测试”按当前配置创建独立的“模型测试”分组任务。强度按 `assets/model_efforts.json` 的本地维护表显示，含资料来源和核对日期，不随刷新联网查询；切换模型时不支持的旧档位恢复默认。Claude 使用 `--effort`，Codex 使用 `model_reasoning_effort`；“默认”表示不覆盖原 CLI 默认值。超时默认 300 秒，归档默认 4h，顶部设置修改后自动记忆。

每个新任务固定当时的 App 配置快照。已有任务的原生会话复用原快照，避免中途更换 CLI。Harness 请求不能包含 backend、model、effort、allow_edits、timeout_seconds。

```text
rs_agent_router.exe show
rs_agent_router.exe submit --request task.json --wait
rs_agent_router.exe status --task-id T001
rs_agent_router.exe health --task-id T001
rs_agent_router.exe cancel --task-id T001
```

需要给同一小任务补充指令时，可选地复用原 CLI 保存的上下文：

```text
rs_agent_router.exe send --task-id T001 --message-file instruction.txt --wait
```

使用进程 API 逐项传参，并读取 stdout JSONL 和 stderr。`--wait` 默认只输出最终摘要（最多 800 字符）、结果路径、执行模型证据和指标，过程在本地消费、供 App 展示与日志留痕。需要排障时才加 `--events` 查看完整过程，或用 `status --full` 查询完整记录；主 Harness 通常只审查实际差异与验收结果，失败或有疑点才读必要日志片段。

首次提交自动启动唯一管理实例；取消请求不等于已回收，需等待终态。客户端断开不会取消任务；失败不自动重试。并行时同时启动多个提交客户端，分别收结果；不要等完第一个再提交第二个。

## 工作协议

复制 `task.example.json`，仅包含 task_id、title、group_path、workdir、prompt。工作目录必须是已存在的绝对路径；任务 ID 唯一，重复 ID 不覆盖记录。

Skill 的分组固定为 **来源 → APP → Feat → 小任务叶子**：例如 `['Codex桌面端','rs_agent_router','Feat_执行服务']`。来源指发起工作的 Harness，不是执行模型。没有 Feat 时用“未关联Feat”。协议允许零至三级分组，Skill 提交必须完整填写三级。

stdout 事件：accepted、progress（仅 --events）、finished、status、health、cancel_requested、shown、error。非等待提交退出 0 只表示接收成功；`--wait` 仅在本次执行 succeeded 时退出 0。GPT/Harness 必须另外核对文件差异和验收结果。

结果的 `executor` 返回 CLI、请求模型、报告模型、强度、取值来源及 `commit_prefix`。Router 任务审计通过后按 `[ar-cc-模型-强度]` / `[ar-cx-模型-强度]` 记录执行来源；主 Harness 可以串行代提交，直接实现仍用项目原前缀。App 配置值不证明代理实际路由；模型冲突、通用别名或缺少强度时前缀为空，不猜身份。程序不会自动提交代码。

## 监控与留痕

顶部“悬浮窗”开关打开半透明无边框置顶卡片，宽度 360，默认最多显示 4 个对话；顶部“悬浮上限”可设置为 1–20 并自动记忆。高度按对话数计算，超过屏幕高度时滚动查看。Windows 原生窗口不透明度约 75%，外层圆角 24；卡片属于同一管理器，不单独占任务栏。总标题栏显示当前新任务配置，如 cc-glm-5.3-max、cx-gpt-5.6-luna-max；已运行任务仍沿用启动时配置。顶部可拖动，“清空”只清理已结束任务，右上角 × 关闭。首次绘制后显示，不主动抢焦点；关闭卡片不影响主窗口和任务执行。主窗口隐藏到托盘后，卡片仍独立刷新。任务树行前显示固定宽度圆角状态卡：运行绿色、完成蓝色、超时黄色、失败或中断红色，主动取消灰色。

“清空已结束”移除所有非运行任务（含归档），保留运行任务及磁盘日志；仅作持久化的列表隐藏标记，不删除数据库记录、不释放旧任务 ID。

- 左侧显示任务名、状态、紧凑耗时（25s / 2m2s / 4h3m）及距最近活动时间（如 5h2m前）；分组时间取最新子任务。正文与日志不放进树行。
- 右侧最多展示最近活动三个任务，铺满分配区域；后台不设任务数上限。
- 面板顶部右侧显示上下文 Token、本轮总 Token、首字/首段耗时、累计会话耗时与进程健康；悬停查看精确用量和统计口径。CLI 没提供的指标显示 —，不以累计 Token 估算上下文。
- 首字指从 CLI 启动到首次可见文本抵达；非流式输出标为「首段」，不冒充 API TTFT。会话耗时不计轮次间等待。
- 深蓝灰主题；来源/APP/Feat 使用蓝/紫/青区分，指令蓝色、工具琥珀色、正文高对比、结果绿色、失败红色。辅助工具与诊断文本折叠展示。
- 详情查看请求、日志、结果，Esc 关闭；查看操作不更新活动时间。
- 任务右键可归档或删除；组标题右键可删除当前列表中该组及子组的已结束任务，保留运行任务。删除只移出列表，日志和 ID 占用保留。
- 完成满配置的归档时长后移入归档视图，默认四小时，不移动或删除日志；重启使用相同规则。
- 主面板和悬浮窗显示本任务最近 5 次完整回复的均耗时，跨续聊和返工保留；悬停查看各条耗时和计时来源。
- 最小化隐藏到托盘；有运行任务时退出需二次确认。确认后结束进程树，并向 Harness 返回 manager_shutdown。

SQLite 保存任务元数据。任务根目录保留稳定的最新 result.json；各次执行保存到 `turns/0001/`、`turns/0002/` 等，包含请求、启动参数、原始 stdout/stderr、事件和结果。CLI 自己保存原生上下文，服务只记录其 session_id。

旧 0.2 任务不移动日志；已有 GLM 元数据在数据库备份后显示为 Claude+原模型。旧版未保存 CLI 会话，不能追加指令，只能查看记录或新建小任务。异常重启将未完成执行标记 interrupted，不自动重跑。

## 健康与超时

`health` / `health --task-id <ID>` 被动探活，3 秒内返回管理器 PID、版本、运行数量及工作线程/子进程状态。探活不启动管理器、不调用模型、不改变任务活动时间；服务不可达为 manager_unavailable，无响应为 health_check_timeout，均非零退出。

顶部“无输出超时”按任务启动时的配置快照执行，默认 300 秒：首次从 CLI 启动开始计时，每次标准输出或错误日志出现非空内容时重新计时；持续输出不限制任务总时长。连续静默达到阈值后终止进程树，回收并保存结果后返回 timed_out / idle_timeout。Harness 不另设截止时间或根据探活结果杀任务。管理页确认退出后返回 manager_shutdown，取消该任务的等待客户端收到非零码。

## 构建与配置

```powershell
cargo build --release
```

复用仓库内 `vendor/cli-stream`（上游提交 cd42566aab9e21903e96b4d59ab7e674f7fb415d，MIT OR Apache-2.0），不自建 LLM 工具循环。无需相邻仓库；来源和本地适配说明见 `THIRD_PARTY_NOTICES.md`。

首次启动时，调试构建读取项目目录 router.local.json，Release 读取 exe 同目录配置，记录根目录默认为 exe 同目录 runs；可通过 --config / --runs-dir 指定位置。之后从 exe 同目录 startup.json 恢复上次使用的配置和任务目录，显式启动参数可覆盖；已有实例通过 App 保存设置，客户端不能临时替换它。

认证沿用 CLI/CC Switch，配置不保存密钥。Claude 的写权限仅开放 Read/Glob/Grep/Edit/Write，不开放 Bash；Codex 使用 read-only/workspace-write。任务需要的目录、文件范围和项目规则由 Harness 写入工作描述。

用户提供的原图在 assets/app.png，EXE、窗口和托盘图标均已接入。只有重新生成图标资产时需要 Python/Pillow：`python assets/prepare_icon.py assets/app.png`，正常 Cargo 构建不需要 Python。

## Web 统计

主窗口点击“统计”打开本机页面。按创建日期、CLI、配置模型、来源/APP/Feat、执行状态及审计状态筛选；图表、摘要和分页明细采用同一数据集。模型汇总忽略大小写，默认包含归档、不含已删除任务。

明细展示累计执行耗时、Token、当前评分、返工次数、本轮首字/首段与最近五次回复均耗时；点击会话可查看逐轮评分依据、返工与审计关联、回复计时样本及结果路径。评分由 Harness 经 review 提交，页面只读；未评分与未知用量保持“—”，不会作为零分或零消耗。

`cargo test --offline --bin rs_agent_router write_web_statistics_fixture` 生成 `target/web-statistics-fixture.json`。浏览器验收函数位于 `tests/statistics.browser.js`，在隔离 HTTP 预览页验证分页、筛选、评分历史、返工、空结果、断线恢复与窄屏布局，测试数据不写入实际任务库。

## 验证与 Skill

`cargo test --offline --bin rs_agent_router`、`cargo clippy --offline --all-targets -- -D warnings`。

Rust 测试中的 service 用例通过独立命名管道运行真实 Windows 替身进程，验证并发、探活、超时和进程回收，可以与用户的管理页同时运行。`verify-failures.ps1` 使用无网络替身验证桌面生命周期；`verify.ps1` 用真实 CLI 验证 App 路由、原生上下文与写入；随后 `verify-ui.ps1` 检查监控页、Esc 与图标。后三个脚本需要专用临时实例，先确认已有管理页没有工作并关闭它，不能中断正在执行的用户任务。

本仓库提供 CLI 服务；外部 Harness 可按上述 JSON 协议接入。开发环境的自定义 Skill 不包含在此发布副本中，也不是编译依赖。

## 变更记录

2026-09-09 | 0.6.0 | 主面板与悬浮窗新增最近 5 次完整模型回复的平均耗时，跨续聊/返工保留，悬停显示各条耗时及来源；Claude 有 ttft_ms 时统计等待加响应，否则仅记录响应流时长；Codex 使用排除工具阶段的 CLI 回复周期，非中转站精确 API 耗时。旧记录缺少边界时显示未知，不回填估算值。

2026-09-09 | 0.1.0 | 三入口单任务调用与日志。

2026-09-09 | 0.2.0 | 统一管理页、任务树、IPC、归档、托盘与 Skill。

2026-09-09 | 0.3.0 | 收窄为 CLI 执行服务：App 控制 CLI/模型/强度，Harness 仅分配小任务；精简监控页、右键归档/删除、修正铺满与 Esc，接入用户图标；只复用原 CLI 上下文，不维护聊天客户端。

2026-09-09 | 0.4.0 | 默认精简 Harness 返回，新增被动健康检查与独立超时计时器、执行模型/强度来源及 ar 提交标识；丰富任务树配色、活动时间和面板指标；Skill 按依赖并发派发，主 Harness 审计并串行维护共享档案及提交。

2026-09-09 | 0.5.0 | 紧凑布局：设置行水平对齐并按字段分隔，任务树单行左对齐，收紧执行面板纵向间距；设置即时自动保存；新增可开关、可拖动、按任务数量收缩的半透明无边框置顶卡片。

2026-09-09 | 0.6.0 | 合并审计评分、返工计数与本机统计页；悬浮卡片使用原生透明度、大圆角和工具窗标记；新增清空已结束任务，保留运行任务、元数据与日志。

2026-09-09 | 0.6.0 | 悬浮卡片关闭按钮改为淡红色圆形底、深红色 × 图标，保持原有关闭交互。

2026-09-09 | 0.6.0 | 完成模型目录刷新与测试、本地模型强度表、启动目录及顶部设置记忆、可配置归档、任务状态卡及组删除；修复任务标题重复悬浮提示，补齐最近五次回复平均耗时与逐条来源展示。

2026-09-09 | 0.6.0 | 悬浮窗默认上限改为 4 个对话，主窗口顶部可设置数量并自动保存；悬浮窗总标题栏显示当前配置的 cc/cx-模型-强度并随设置同步更新。

2026-09-09 | 0.6.0 | 超时由整个任务总时长改为连续无输出时长，CLI 标准输出和错误日志的非空内容均刷新计时；补充持续输出、停止输出、启动静默及进程树回收验证。

2026-09-09 | 0.6.0 | 完成 Web 统计交互验收，增加审计状态筛选、首字/首段与最近回复均耗时、逐条计时来源和返工审计关联；合并模型大小写分组，修正未知评分及空结果样式，新增可重复浏览器验收。

2026-09-10 | 0.6.0 | 主界面新增悬浮无活动隐藏分钟数并自动记忆，默认 30 分钟；已结束任务分别到期隐藏，运行任务不受影响，主界面与归档不变。悬浮窗以当前底边为基准向上扩展或向下收缩，拖动后以新位置为基准；隐藏滚动条，模型测试不再自动打开日志详情。
