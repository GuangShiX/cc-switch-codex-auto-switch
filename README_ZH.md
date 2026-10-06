# CC Switch · Codex 桌面自动换号

基于 [CC Switch 3.20.4](https://github.com/farion1231/cc-switch) 的第三方 Windows 分支，由 CC Switch 原生后台检测托管 Codex 账号额度、切换账号、重开 Codex 桌面并恢复此次暂停的原聊天。

当前发布为 **源码预览**（`v3.20.4-codex-auto.1`）。不附带本机测试使用的 Debug 程序，也不代表上游官方发行版。原 CC Switch 作者、功能及 MIT 许可仍予以保留。

## 工作方式

- 每 **5 分钟**查询当前托管账号额度；**5 小时剩余严格低于 5%**，或**周剩余为 0%**时触发切换。恰好 5% 和周剩余 1% 可继续使用，不设 20% 恢复门槛。
- 按 CC Switch 原账号列表从上到下查询，跳过当前账号及相同账号绑定；找到第一个可用候选立即停止查询。不额外维护优先级列表，不预先刷新全部账号。
- 原生后台完成状态核对、暂停此次仍在运行的任务、保存原聊天设置、正常退出 Codex、确认退出、启用目标账号、重开桌面、核对目标身份、恢复原聊天。手动“启用”复用原供应商服务及同一桌面重开、恢复实现。
- 后台监测不依赖 CC Switch 窗口可见、页面或焦点；托盘仍可查询。锁屏或会话断开时跳过切换，使旧计划失效；恢复交互后重新判断，不积压重放。
- 原项目、模型及权限设置保留。不会自动继续用户手动停止、等待审批或已经完成的任务；不确定的切换或恢复结果先核对，不重复发送继续。
- Codex 页面和设置页提供自动换号开关、当前状态、取消及失败原因。候选均不可用时显示原因并等待下一次检查，不循环重启。

自动换号默认关闭，需用户主动开启。普通手动账号启用保留原有行为；后续桌面重开或恢复失败会给出具体说明，不把已成功的账号启用误报为失败。

## 已验证范围

2026-10-06 的真实 Windows 测试完成了：手动原生启用后切到另一托管账号、正常重开 Codex、确认新桌面身份、恢复两条原运行聊天，并核对项目、模型及权限保持一致；窗口隐藏到托盘后，原生五分钟定时查询也正常运行。

低额度自动触发、锁屏、取消、用户改号、所有候选不可用等边界目前通过源码回归，未逐项进行真实桌面测试。不据此承诺长期稳定或其他机器、Codex 版本的兼容性。详细分类见 [验证说明](docs/codex-auto-switch-validation.md)。

额度响应不完整时，不会擅自认定候选可用；当前可用性判定需要有效的 5 小时及周额度窗口。桌面恢复依赖 Codex 当前桌面协议，协议变化可能需要更新此分支。

## 构建

Windows 构建需要 Node.js 22、pnpm 10.12.3、Rust 1.95、MSVC C++ 构建工具及 WebView2。项目锁文件和 Rust 工具链配置已包含在源码中。

```sh
corepack enable
corepack prepare pnpm@10.12.3 --activate
pnpm install --frozen-lockfile
pnpm typecheck
pnpm build
```

仅检查前端和原生嵌入资源构建：

```sh
pnpm build:renderer
cargo check --locked --manifest-path src-tauri/Cargo.toml --bin cc-switch --features tauri/custom-protocol
```

Gemini 的可选令牌刷新需要在构建时提供 `CC_SWITCH_GEMINI_OAUTH_CLIENT_ID` 和 `CC_SWITCH_GEMINI_OAUTH_CLIENT_SECRET`；仓库不内置这些值。未提供时跳过 Gemini 刷新，这不影响 Codex 托管账号切换。请勿提交构建时的客户端配置。

Windows PowerShell 操作使用 PowerShell 7。此分支已关闭上游自动更新通道；更新源码或后续下载应使用本仓库，避免被上游发行包覆盖。

## 账号及隐私

账号仍由 CC Switch 现有本地托管账号与供应商服务管理；本改造不清空数据库、不要求逐个重新登录，也不依赖旧自动换号插件或模型调用切号工具。

此仓库只发布源码、合成测试样例和通用文档，不包含个人账号、登录令牌、凭据、数据库、聊天记录、恢复日志、安装备份或本机开发历史。请勿将这些运行数据提交到仓库或附在公开问题中；反馈时先脱敏。

## 来源和许可

上游：[farion1231/cc-switch](https://github.com/farion1231/cc-switch)，基线 `3.20.4`，提交 `43e1d99`。继承 [MIT License](LICENSE)，保留原作者版权声明。

原上游文档作为来源资料保留于 [README_UPSTREAM.md](README_UPSTREAM.md) 和 [README_ZH_UPSTREAM.md](README_ZH_UPSTREAM.md)；其中官方发行、更新及赞助链接属于上游项目。此分支的功能和验证范围以本 README 为准。

English: this is an unofficial Windows source preview based on CC Switch 3.20.4. Native background quota monitoring, desktop restart and original-chat restoration are implemented here. Only the validation scope described above is claimed; no personal credentials or runtime data are distributed.
