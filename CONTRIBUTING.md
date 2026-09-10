# 参与贡献

欢迎通过 Issue 报告问题，或提交范围明确的 Pull Request。

## 本地验证

使用 Windows 10/11 x64、Rust stable 和 MSVC C++ 构建工具，在仓库根目录执行：

```powershell
cargo test --locked
cargo build --release --locked
```

涉及真实 CLI 账号、键盘、计划任务或桌面交互的检查，请手动执行并在 PR 中注明结果。自动测试通过不代表这些场景已经验收。

## 提交要求

- 说明问题、改动范围和验证结果；保持修改聚焦。
- 行为变更同步更新 README，非简单逻辑补充必要测试。
- 不提交登录缓存、API Key、本机配置、运行记录和编译产物；日志先脱敏。
- 本项目原创代码采用 MIT 许可证，第三方代码保留其原许可证。
