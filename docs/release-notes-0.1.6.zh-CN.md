# mixumOS 0.1.6 — aarch64 + x86_64

状态：pre-release。目标日期：2026-10-27。构建 commit：`bd24377`
（发布 tag 另含本说明）。上一个版本：0.1.5。

> 英文原文：[release-notes-0.1.6.md](release-notes-0.1.6.md)。

## 亮点

- **内存/混合快照进入可用路径。** 在目标 rootfs 内编译打了补丁的 CRIU 4.1，
  关闭 LTO、保留 parasite 符号；引擎支持 memory-only dump/restore、磁盘+内存
  hybrid restore、cgroup 冻结顺序、restore 超时和 `MIXUM_CRIU_BIN`。
- **权限边界显式化。** 任务默认以非特权 `mixum` 用户运行，root 只做 supervisor；
  单条 sudo 命令通过 `mixum-sudo`/`privilege.*` 审批后执行，不提供通用 root shell；
  客户端晚连接时 `privilege.list` 会补发待审批请求。
- **沙箱与重放加固。** HOME 级 Landlock 写白名单、cgroup 生命周期修复、重放环境
  白名单、`pre_exec` 通过预开 fd 挂 cgroup，以及真实 Landlock 写拒绝测试。
- **网关契约对齐。** WebSocket 子协议同时支持 `map.v1` 和 `mixum.v1`；手写回复
  补 `ts`；RFC 0003 的节点路由/控制操作已实现并测试。
- **Agent 一键安装。** `mixum-ctl agent` 支持 Claude Code、Codex、Gemini CLI、
  OpenCode，安装后写入提示词模板；以任务用户运行，npm 前缀在用户目录。基础
  镜像新增 `nodejs`/`npm`/`curl`，开箱即可安装。
- **首个 x86_64 镜像。** 修复 x86_64 构建（CRIU `!lto`、mixum-ctl 清掉 makepkg
  的 CFLAGS/LDFLAGS 以链接 `ring`），本次同时发布 aarch64 和 x86_64 OCI/rootfs。
- **文档与发行整理。** 英文优先、中文翻译；README/品牌刷新；内核 GPL-2.0 声明、
  SECURITY/CONTRIBUTING；镜像内置 CJK 字体。

## 快照与 CRIU

- 新增 `criu-mixum 4.1` 包；在 aarch64 目标 rootfs 内编译 CRIU，避免构建机
  glibc 与镜像不一致导致 parasite 崩溃。
- memory-only restore 不再预冻结/解冻旧 cgroup；hybrid 先冻结磁盘视图，再做
  CRIU dump/restore。
- CRIU restore 使用 `-d` 脱离，避免长任务导致 RPC 超时。
- 单任务 dump 不再带 `--ext-unix-sk`；sandbox 任务的 parasite 处理仍在调查。
- 新增 `MIXUM_CRIU_BIN`、memory remount 和有界 dump timeout。
- 快照 tags 持久化 `base`/`baseDigest`，restore 校验同一 base。
- CRIU 控制标记带 `generated_at`；受控实验会拒绝陈旧标记和残留 counter 进程。

## 权限与沙箱

- 任务默认 `mixum` 用户；root 只做 supervisor 并执行批准的单个命令。
- `mixum-sudo` 请求进入 `privilege.request` 队列；`privilege.list` 补发未决请求；
  审批后只执行批准的命令。
- HOME 下的工具目录是显式 Landlock 写目标；HOME 本身保持只读。
- cgroup freezer 句柄引用计数；per-task 信号不再误用全局 freezer。
- GUI 重放仅限本机 disk 快照，过滤记录的环境变量，等待旧进程退出，并复用普通
  GUI 启动的隔离路径。
- `pre_exec` 只向预开的 `cgroup.procs` fd 写入；用真实 Landlock 拒绝测试替代
  旧的 `/proc/self/status` 跳过逻辑。

## 网关

- WebSocket 子协议优先 `map.v1`，兼容 `mixum.v1`。
- 手写网关回复补 `ts`，修复 Rust/`MapClient` 在 `gateway.node.list` 上超时。
- RFC 0003 的节点路由与控制操作落地：节点 token、配对、设备列表/吊销、类型化错误。

## Agent 安装与镜像

- `mixum-ctl agent list/install/template` 支持 Claude Code、Codex、Gemini CLI、
  OpenCode，支持 `--dry-run` 和 `--cmd`。
- 安装使用 `$HOME/.local` 和 `$HOME/.opencode/bin`，不再要求 root；缺少工具时
  给出明确的非 root 错误。
- `mixum-base` 增加 `nodejs`、`npm`、`curl` 依赖；镜像内置 CJK 字体和 Noto fallback。
- x86_64 构建修复：CRIU 包使用 `!lto`；`mixum-ctl` 在 `build()`/`check()` 清掉
  makepkg 的 CFLAGS/CXXFLAGS/LDFLAGS，保证 `ring` 链接。

## 文档与项目

- 主文档英文优先，补齐中文翻译。
- README、品牌资源、SECURITY/CONTRIBUTING、内核 GPL-2.0 声明按公开仓库整理。
- 0.1.4 / 0.1.5 发布说明翻译完成。

## 验证

- 本机 `make dist ARCH=aarch64` 构建 aarch64。
- GitHub Actions `make dist ARCH=x86_64` 构建 x86_64。
- 镜像内包版本：

```text
mixum-base 0.1.6-1
mixum-agentd 0.1.6-1
mixcomp 0.1.6-1
mixum-ctl 0.1.6-1
criu-mixum 4.1-1（aarch64 镜像）
```

- Rust workspace 测试在发布 commit 上通过（`cargo test --workspace`）。

## 已知限制

- x86_64 镜像暂未包含目标 rootfs 内的 CRIU 编译；本版内存/混合快照仍以 aarch64
  为主。
- 内存/混合快照仍是 pre-release，使用前确认内核与 CRIU 版本匹配。
- 跨内核 CRIU 仍是受控实验，不是通用迁移保证。
- x86_64 镜像已构建，但未在 Apple Container 上验证（Apple Container 仅 macOS/arm64）；
  请在 x86_64 Docker 上验证。
- `mixum-ctl agent install` 会在环境内联网下载上游包/脚本，需要网络。

## 升级提示

- 新环境使用 `0.1.6` 镜像；已有容器在重建前仍使用旧镜像。
- aarch64 镜像已内置 CRIU；0.1.5 旧镜像需要手动安装。
- 0.1.6 的 Agent 安装可使用镜像内置的 `nodejs`/`npm`/`curl`；自定义镜像仍需
  自行提供。
- 升级后重启环境，确保使用新 entrypoint 和包。

## 发布资产

| 文件 | SHA256 |
|---|---|
| `mixumos-0.1.6-aarch64.oci.tar` | `ba4fe9d800f5a89c1fef0ecfae7bcafd1dd46f0e73e32f8bbc7a4c465c619493` |
| `mixumos-0.1.6-aarch64-rootfs.tar.zst` | `92f2c6e2d5dfa665b23e1bc7e88f201f88db36f0810d28386f6a0caa8e03d09d` |
| `mixumos-0.1.6-aarch64-repo.tar.zst` | `3ba8c30d2cf5f1f2b7bd1f57b548dd0c3b7ddf4f176230632e8b65f7000c86d8` |
| `mixumos-0.1.6-x86_64.oci.tar` | `1968504645d43236f3e2b94992d160625079a81c852ac281f9c57027510fc0c0` |
| `mixumos-0.1.6-x86_64-rootfs.tar.zst` | `1604d2fb341e330972e669bf1b845027f1131a4c830477f63be62c00f2298193` |
| `BUILDINFO-0.1.6` | `27722fa8eb5c6a7297cb3ad646c268c83d3bcfe517b795623e1fdcabe20dbf4f` |

## 贡献者

感谢本次发布的三位贡献者：

- **AshReo**（[@ashreo](https://github.com/ashreo)）— 发布、内核/CRIU、快照、沙箱、网关、文档
- **yubing0721**（[@yubing0721](https://github.com/yubing0721)）— 网关、Agent 安装、构建修复、发行
- **MASOCHIST / LimDN**（[@LimDN](https://github.com/LimDN)）— display attach/路由、快照树锁、早期后端工作

也感谢所有测试早期镜像并报告问题的人。
