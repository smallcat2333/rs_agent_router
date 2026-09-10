> 以下为独立发布前的历史验收记录。`%TEMP%` 下的证据仅存在于原开发环境，不随仓库分发；它们不代表本次发布或其他机器上的测试结果。当前可复现检查见 README 和 Windows CI。

# 验证记录

## 2026-09-10 — Web 统计收口（验证于 2026-09-09）

- 50 项 Rust 测试通过，2 项真实 CLI 查询/调用按约定默认忽略；Clippy 全目标严格检查、JavaScript 语法检查、格式检查和 Release 构建通过。
- 隔离浏览器预览通过 19 项检查：分页、平均评分、模型大小写合并、排序、审计筛选、评分/返工关联、最近五次回复样本、Esc、已删除筛选、文本注入隔离、空结果、日期校验、断线保留数据和恢复、评分口径、无 NaN、桌面与窄屏无横向溢出。
- 1440×1000 与 390×844 截图已检查，保存在 target/web-statistics-desktop.png、target/web-statistics-mobile.png；仅含明确标注的测试数据。
- 测试快照由 write_web_statistics_fixture 生成，浏览器验收代码保存于 tests/statistics.browser.js；模型正文及返工工作指令不进入统计接口。
- 构建时现有管理器仍有运行任务，未重启该实例。新版 EXE 已生成，重开后从主窗口“统计”进入新版页面。

## 2026-09-09 — 无输出超时与悬浮窗配置

- 1 秒阈值下，持续标准输出或错误日志约 1.8 秒的任务正常完成；输出停止后按最后一次输出计时，启动静默与子进程树回收验证通过。
- 悬浮窗默认上限 4，可配置为 1–20 并持久保存；主面板仍显示最近三项，悬浮窗独立取指定数量，超过屏幕高度时滚动查看。
- 标题栏完整显示 cx-gpt-5.6-luna-max，清空与关闭按钮鼠标命中测试通过。
- 相关集成、数量、尺寸及设置测试通过；Clippy 全目标严格检查、格式检查和 Release 构建通过。

## 2026-09-09 — 交互、模型配置与回复耗时收口

- Rust 测试 49 项通过，2 项真实 CLI 查询/调用测试默认忽略；本轮曾显式查询 Codex 模型目录，确认包含 gpt-5.6-luna。
- 本地强度表覆盖 Codex、Claude、GLM、DeepSeek、Kimi，已验证模型切换与 CLI 参数传递。
- 覆盖组删除、运行任务与日志保留、重启恢复、可配置归档、完整回复计时、最近五条跨轮次持久化和异常结束留样。
- Clippy 全目标严格检查通过；修复指标快照增大导致的枚举体积告警，使用 Box 传递快照。
- 主面板和悬浮窗显示最近五条平均耗时；Codex 是排除工具阶段的 CLI 观测值，不能视作中转站精确 API 计时。历史记录不回填估算值。

## 0.6.0 — 合并发布验收

- Rust 测试 36 项通过，1 项真实模型调用用例按原约定显式运行、默认忽略；Clippy 全目标与 Release 编译通过，统计页 JavaScript 语法检查通过。
- 修正测试隔离：1 秒仅用于超时用例，后续成功/续聊/审计用例使用 30 秒，生产超时逻辑未改。
- 正式实例版本 0.6.0，验收时 PID 142020，沿用原 router.local.json 与原 runs-dir，health 正常。
- 正式 UI 已存在“清空已结束”和“统计”按钮。未在用户数据上执行清空；隔离测试证明运行任务保留，终态/归档作隐藏标记，数据库记录和日志保留。
- 对正式悬浮卡片只读检查：Windows 原生 Alpha=190，窗口区域圆角有效；TOOLWINDOW 已设置、APPWINDOW 已移除，无独立任务栏项。检查不靠渲染图中的 Alpha 推断实际窗口透明。
- 重复 show 返回同一管理器；之前另做 4 次 Debug/Release 并发 show，全部返回同一 PID。
- 本机统计页面和 api/statistics 均返回 HTTP 200，schema_version=1，验收时包含 17 个历史会话。

正式验收脚本：`%TEMP%/router-v06-final-check.ps1`。本轮没有使用 computer-use，也没有启动额外的预览管理器。

## 0.5.0 — 紧凑布局、即时设置与置顶悬浮窗

2026-09-09，原生离线预览、UI Automation 与 Win32 验证；未使用 computer-use。

- 任务树名称、状态点、耗时和活动时间为单行左对齐，三级缩进和行距收紧；设置行水平居中对齐，字段组间留间距，执行面板内外纵向留白缩小。
- 主窗口没有“保存设置”按钮。通过原生数值控件将超时改为 321，preferences.json 自动更新；执行策略仍只影响后续任务。
- 主窗口开关能创建独立悬浮窗；Win32 GetClientRect 验证内容区 500×700，扩展窗口样式包含 TOPMOST。
- 主窗口最小化隐藏后，悬浮窗仍可见且原生绘制时间戳继续变化。悬浮窗没有暴露 UIA 子元素，所以不以空的无障碍文本误判停止刷新。
- 关闭悬浮窗会复位主窗口开关；再次打开和通过开关关闭均通过；关闭预览没有影响真实管理器。
- 合并托盘及模型归属修复后，26 项默认 Rust 测试通过，真实 CLI 用例保留为显式运行项；全目标 Clippy 与 Release 构建通过。

交互证据：`%TEMP%/router-v5-preview-20260909-145259/ui-verification.json`。
同目录 `preview.png` 为主页面原生渲染，`floating.png` 为悬浮窗原生渲染，内容均为离线样例。构建预览使用的临时插桩已恢复，不包含在正式程序中。

### 后续浮层优化

由初版 500×700 改为宽 360、高度按任务数量 86/164/276/388；半透明、无系统标题栏，不占独立任务栏按钮，首帧绘制后显示且不抢焦点。模型信息移至标题提示，摘要收为单行，顶部拖动区与 × 关闭按钮分开命中。

原生验证通过：实际非客户区无标题栏、TOPMOST、0/1/3 项动态高度、调整数量时保持同一窗口句柄、主窗隐藏后继续绘制、原生关闭及主窗开关联动。RGBA 渲染像素含透明和半透明值。证据：`%TEMP%/router-v5-overlay-20260909-150648/ui-verification.json`、同目录 `floating.png`。

右上角 × 用真实 egui 按下/释放事件验证点击返回关闭且不启动拖动；桌面鼠标注入未触发响应，不计作原生点击通过。浮层相关 UI 用例共 5 项通过，Clippy 全目标与 Release 编译通过。确认运行任务为零后正常重启，正式实例 PID 76680 沿用原配置和日志目录，health 正常；临时预览插桩全部恢复。

## 0.4.0 — 精简返回、健康与执行指标

2026-09-09，Windows 本机验证；开发任务运行期间未停止或重启原管理器。确认运行数为 0 后，正常退出旧版并按原配置和记录目录启动 0.4.0，health 返回 accepting_tasks=true。

| 验证 | 结果 |
| --- | --- |
| 默认等待只收摘要 | accepted/progress 不进入 stdout；摘要最多 800 Unicode 字符，完整结果和原始流留本地；--events 显式查看全文 |
| 精简查询 | 不回显 prompt，不把上一轮结果冒充运行中任务的新结果；--full 单独启用 |
| 并发和进程生命周期 | 独立测试命名管道同时运行四个真实替身进程，最近列表仅三项；断开客户端、取消、超时、退出错误及进程树回收通过 |
| 被动探活 | 服务不存在时不自动启动；返回工作线程与 OS 进程证据；探活不更新活动时间；探活期限和任务期限分离 |
| 超时与错误 | Rust 独立计时器回收静默子进程；启动失败、协议未完成、协议错误、结果写入失败仍明确结束 |
| 输出积压 | 分帧消费大量日志时，工作线程退出不会导致尚未读取的成功终态被误判失败 |
| Token 与首字 | GLM message_delta 的真实输入/缓存修正零占位；Codex 原生 last_token_usage 与累计用量分开；首字/完整首段标注不同来源 |
| 实际模型证据 | GLM-5.3/max 返回 ar-cc-glm-5.3-max；Codex 默认配置解析该任务原生模型 gpt-6-astra/medium，返回 ar-cx-gpt-6-astra-medium |
| UI 渲染 | 应用原生渲染输出 1440×900，检查配色、三级树、紧凑耗时、活动时间、右上指标和三块区域铺满；完成任务从内容顶部显示 |
| 归档/恢复与菜单 | 既有可控时钟、SQLite 恢复、日志保留、egui 右键删除测试通过 |
| Skill | agent-router 与 smallcat-feat-3-dev 校验通过；按来源区分提交归属，依赖审计后派发，共享文档与 Git 串行维护 |

Rust 默认测试 24 项通过；真实 CLI 测试另行显式运行通过（常规测试中标为 ignored，避免普通测试触发模型调用）。Clippy 全目标无警告，Debug 和 Release 编译通过。Release 先在 target/v04 独立构建，空闲后安装至标准 target/release；副本 SHA256 相同。两套 Skill 均通过 quick_validate；原有 argument-hint 参数说明移到正文，保留用法并满足校验规范。

执行服务记录：`%TEMP%/router-v4-service-1788935221118/verification.json`。最后一轮还暂停测试管理泵，确认 health 在 3 秒后返回 health_check_timeout，已接收任务继续运行。

真实 CLI 复验：`%TEMP%/router-v4-live-1788934949079/verification.json`。两个 CLI 都通过 Read 类工具读取输入，返回 42；GLM 上下文 4755、累计输入 9315，Codex 最新输入 18285、累计输入 35680，验证两种口径没有混用。以上数字仅为本次测试值。

原生渲染证据：`%TEMP%/router-v4-preview-20260909-142405/preview.png`。预览为临时数据、禁用生产 IPC 的离线构建，源码已恢复；不是实际开发任务截图。Windows 桌面截图工具两次返回 `IGraphicsCaptureItemInterop.CreateForMonitor failed / 0x800706BE`，未把桌面截图或重新点击托盘/退出确认算作本次通过；这些桌面交互沿用 0.3 的记录。

## 0.3.0 — CLI 执行服务

按最终需求收窄：保留 App 配置、执行服务与薄监控页；移除手动聊天流程、底部聊天输入框和自建消息历史。仅原 CLI 保存上下文，服务记录 session_id 及每次执行的日志。

| 验证 | 结果 |
| --- | --- |
| Harness 不可覆盖 CLI/模型/强度/权限/超时 | 工作 JSON 严格拒绝这些字段；配置快照来自 App |
| CLI 类型与模型分离 | Claude / Codex 两类；GLM-5.2 作为 Claude 模型，effort=low 实际传入 |
| 真实 CLI 执行 | Claude/GLM、Codex 都读取输入并返回 42 |
| 可选原生上下文复用 | 两个 CLI 的 session_id 保持相同，无工具调用返回上一轮测试标记；服务无消息历史层 |
| 写入权限由 App 决定 | App 勾选后 Write 写入 answer.txt=42 |
| 并发、取消、超时、退出、托盘 | 通过执行服务回归脚本 |
| 树行与详情 | 树中仅名称及状态/用时；详情 Esc 关闭；查看不更新活动 |
| 右键归档/删除 | 存储测试保证日志保留；egui 完整鼠标事件测试验证归档任务菜单派发 Delete |
| 旧 GLM 记录 | 数据库备份后转换为 Claude+原模型，日志路径不变；不伪造旧会话恢复 |
| 图标 | 用户 PNG 转换为多尺寸 ICO、窗口 PNG、托盘 PNG；EXE 图标资源提取检查通过 |
| 监控范围 | UI Automation 确认没有聊天发送或手工新建聊天控件 |

`cargo test --offline --bin rs_agent_router`：13 项通过。Clippy、Release 编译及 Skill 校验通过。

服务回归记录：`%TEMP%/router-v3-fixture-20260909-121603/`。

真实执行记录：`%TEMP%/router-v3-live-20260909-121727/verification.json`。

当前桌面会话的右键/截图自动化不稳定；删除菜单采用确定性的 egui 事件测试，日志保留采用 SQLite/文件测试，不把桌面自动化失败当作成功。图标原图保留在 assets/app.png。

Skill 已固定三级语义：来源为发起 Harness，第二层 APP，第三层 Feat；小任务是叶子。Harness 只传工作，不传路由参数。

## 0.2.0 — 统一管理页

2026-09-09，本机 Windows 实测。

`cargo test --offline --bin rs_agent_router`：9 项通过；Clippy 全目标无警告；Release 编译通过。

| 验证 | 结果 |
| --- | --- |
| 无参数启动、重复 show、首次 submit 自动启动 | 通过；同一 manager_pid，首次提交客户端正常退出 |
| 四个任务同时运行、右侧仅三个面板 | 通过；UI Automation 观察到三个取消按钮 |
| 最大三级分组、最近活动排序、完成参与排序 | 通过；分组边界与最近三个单元测试，真实任务三级分组界面可见 |
| 最小化隐藏、隐藏期间查询、show 恢复 | 通过；相同窗口恢复，任务继续运行 |
| 退出确认“继续运行” | 通过；运行任务未取消 |
| 退出确认“终止全部并退出” | 通过；等待客户端返回非零码和 manager_shutdown，测试子进程全部结束 |
| 等待客户端断开 | 通过；管理器继续持有任务，后续可查询/取消 |
| 单任务取消、超时、缺失 exe、协议错误、无完成事件 | 通过；没有把退出 0 误判为成功 |
| 重启恢复、四小时边界、分钟检查、归档路径不变 | 通过；使用可控时间测试 SQLite 持久化和异常中断恢复 |
| 查看与刷新详情 | 通过；UI Automation 打开并刷新，last_activity 未变化 |
| Claude / GLM / Codex 真实读取 | 全部 succeeded，返回 42，输入哈希不变 |
| GLM 真实 Write | succeeded，answer.txt 写入 42，原输入不变 |
| Skill | UTF-8 模式 quick_validate.py 通过 |

调试版集成记录：`%TEMP%/router-v2-fixture-20260909-104442/`。

Release 集成记录：`%TEMP%/router-v2-fixture-20260909-105313/`。

最终 Release 复验：`%TEMP%/router-v2-fixture-20260909-105822/`，配置与记录根目录包含中文和空格；首次提交、四任务、断开客户端、托盘、退出确认及所有失败用例通过。

真实调用记录：`%TEMP%/router-v2-live-20260909-104605/verification.json`。

首次自动启动测试发现管理进程继承 Harness 输出句柄，已改为 CreateProcessW 禁止句柄继承；退出状态重复拦截关闭事件也已修正，完整集成测试重跑通过。无障碍控件支持用于实际界面验证。

结果不可写的收口用例验证失败能释放运行位，不会无限等待。CLI 完成不代表真实业务验收或实际供应商模型身份已经验证；本次未覆盖长时高负载任务和跨登录会话桌面交互。

## 0.1.0 — 历史验证

日期：2026-09-09；平台：本机 Windows，Rust；真实调用使用已配置的本地 Claude Code / Codex CLI 认证。

| 用例 | 结果 |
| --- | --- |
| GLM 入口 Read 输入并求和 | succeeded；42；输入 SHA256 未变化 |
| Claude 入口 Read 输入并求和 | succeeded；42；输入 SHA256 未变化 |
| Codex 入口调用工具读取输入并求和 | succeeded；42；输入 SHA256 未变化 |
| GLM 入口 Read + Write answer.txt | succeeded；写入 42；原输入未变化 |
| 四个真实任务窗口 | 均被观察到；结果保存后关闭延迟 5166 / 5184 / 5226 / 5173 ms |
| CLI 退出 0 但无完成事件 | failed，路由器退出 1 |
| CLI 返回协议错误但退出 0 | failed，路由器退出 1 |
| 超时 | timed_out，路由器退出 1，测试子进程已结束 |
| 关闭已就绪的运行窗口 | cancelled，路由器退出 1，测试子进程已结束 |
| CLI 路径不存在 | failed，路由器退出 1，保留 result.json |

真实任务记录：`%TEMP%/rs-agent-router-20260909-093940/verification.json`。

异常记录：`%TEMP%/rs-agent-router-failures-20260909-094418/verification.json`。

Release 复验记录：`%TEMP%/rs-agent-router-failures-20260909-094608/verification.json`，五类异常均通过。`cargo test --offline --bin rs_agent_router` 两项通过；`cargo clippy --offline --all-targets -- -D warnings` 通过；Release 编译通过。

首次取消测试在窗口消息循环就绪前发出关闭消息，未触发取消，最终由超时回收；测试已加入 WaitForInputIdle 后重跑通过。该轮运行占用调试版 exe 导致一次 cargo test 无法覆盖文件，待进程退出后重新验证。

模型记录仅代表 CLI 报告：GLM 为 GLM-5.2，Claude 为 claude-sonnet-4-6；Codex 本轮事件未报告模型，保持 null。CC Switch 可能重映射上游，不能据此证明实际供应商模型身份。

覆盖边界：已验证单任务读取、单文件写入、可见状态与进程控制；未验证长时复杂代码开发、外部数据库或网络工具、队列和跨任务恢复。
