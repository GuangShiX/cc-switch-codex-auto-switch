# Codex 自动换号：验证范围

此文档说明源码预览 `v3.20.4-codex-auto.1` 的证据范围。基线为 CC Switch 3.20.4；不发布测试所用账号、聊天标识、个人路径、运行日志或凭据。

## 源码回归

2026-10-06 发布前完成的相关检查：

| 检查                                   | 结果      | 覆盖范围                                                         |
| -------------------------------------- | --------- | ---------------------------------------------------------------- |
| 原生后台及桌面切换回归                 | 75 项通过 | 额度阈值、顺序查询、失效计划、退出确认、身份和原任务恢复状态核对 |
| 登录令牌处理回归                       | 44 项通过 | 当前登录令牌复用、账号绑定、缓存与刷新条件；测试使用合成数据     |
| 手动原生启用回归                       | 2 项通过  | 自动开关开/关均保留原供应商启用；账号绑定变化使旧计划失效        |
| 相关界面及调用回归                     | 65 项通过 | 开关、状态、取消、手动启用、排序、单个候选额度查询及禁用上游更新 |
| TypeScript、前端构建、原生嵌入前端构建 | 通过      | 类型和构建产物检查                                               |

这些检查不能证明远端账号仍有效、真实耗尽触发已执行，或任何机器都兼容桌面恢复协议。

## 安装验证

在 Windows 本机替换程序后，确认修改版 CC Switch 独立运行，原有托管账号、供应商绑定及列表顺序保留，数据库检查正常。安装前保留了本地回滚备份，未清空数据库或恢复旧登录令牌。

此发布仅包含源码，不附带上述本机 Debug 二进制或备份。读者自行构建的程序需要另行进行安装验证，不能把本机结果当成其安装结果。

真实桌面测试使用的是同一自动换号实现的本机功能构建。发布副本另外关闭上游更新通道，将 Gemini 客户端配置改为可选构建变量，并加入源码隐私检查；这些发布调整以源码检查和回归验证，不再次安装程序或操作当前账号。

## 真实完整流程

2026-10-06 测试从一个托管账号手动启用另一个可用账号，实际完成：

1. 暂停两条仍在运行的原聊天并保存恢复所需设置。
2. 正常关闭旧 Codex，确认旧桌面退出。
3. 重开 Codex，并核实新桌面使用目标账号。
4. 恢复同两条原聊天，确认产生新的正式轮次。
5. 比较项目、模型、聊天设置及权限，结果一致。

切号期间 CC Switch 连续运行，没有另起独立 OAuth manager，没有使用终端版 `codex resume`，没有启用旧自动换号插件。

候选查询遵循原列表顺序：首个候选额度不足，第二个可用后立即停止查询后续候选。另已确认最小化并隐藏到托盘时，原生五分钟定时器仍查询当前账号额度；本次当前账号可用，因此没有执行额外切号。

## 尚未真实验证

以下场景目前只有源码回归，未在本轮逐项进行真实桌面测试：

- 真实账号额度降至阈值后，由五分钟后台监测自动触发完整切号。
- 锁屏及 Windows 会话断开、恢复交互后的重新判断。
- 切换各阶段取消、用户同时手动改号、外部修改账号绑定。
- 所有候选均不可用、远端登录授权过期及长时间夜间运行。
- 其他电脑、其他 Codex 桌面版本、macOS 或 Linux 的完整恢复。

此预览不宣称长期稳定或全部边界已实测。账号额度缺少有效窗口时，候选会被视为无法判定，不会当作可用账号盲目切换。

## 自行运行相关回归

先安装依赖并构建前端；Rust 测试使用独立临时目录和合成账号，不能指向个人账号目录。

```sh
pnpm install --frozen-lockfile
pnpm typecheck
pnpm exec vitest run tests/components/CodexAutoSwitchApi.test.ts tests/components/CodexAutoSwitchPanel.test.tsx tests/components/CodexOauthQuotaFooter.test.tsx tests/components/ProviderActions.test.tsx tests/hooks/useProviderActions.test.tsx tests/hooks/useDragSort.test.tsx tests/lib/forkUpdater.test.ts
pnpm build:renderer
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib services::codex_
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib codex_oauth_auth
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib manual_codex_switch_tests
cargo check --locked --manifest-path src-tauri/Cargo.toml --bin cc-switch --features tauri/custom-protocol
```

CI 仅校验源码、合成回归及构建，不使用真实账号 secrets，不发布或安装程序，也不会执行完整桌面切号。
