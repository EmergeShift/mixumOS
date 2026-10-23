# PoC：CRIU + Wayland 会话快照/恢复

> **状态：PoC 记录，不是产品路径。** 仅同机同内核；跨设备迁移未验证。
> **Status: PoC, not a product path.** Same machine and kernel only; cross-device migration is not verified.
> 结论：在 Apple Container 1.5.0 + `linux-mixum-micro`（6.18.35）上，
> 用 CRIU 3.17.1 对 **Sway（headless）+ foot** 整个会话树做 checkpoint，
> kill 后 restore，Sway IPC 正常，`swaymsg -t get_tree` 里仍能看到 `app_id: foot`。
> 这是“同机同内核”的 PoC，不是跨设备迁移。

## 环境

| 项 | 值 |
|---|---|
| 宿主 | macOS 27.0.1 / arm64 / Apple Container 1.5.0 |
| 内核 | `6.18.35-mixum-micro`（kata 6.18 基底，48-bit VA/PA，CRIU+Landlock+diag） |
| 镜像 | Debian bookworm-slim |
| CRIU | 3.17.1（Debian 包） |
| 合成器/客户端 | Sway 1.7 headless + foot 1.13.1 |
| 容器 | `--cap-add ALL`，PID 1 = tini |

## 关键步骤

```sh
# 1) 容器 PID 1 用 tini，保证 kill 后僵尸被回收（否则 restore 会 “Can't fork: File exists”）
container run -d --name criu-poc --cap-add ALL criu-poc:1

# 2) 会话脚本（demo 用户）：sway + foot 同属一个 shell 树
#    两端都在 dump 树里，才能避免 “Can't dump half of stream unix connection”
container exec criu-poc su -s /bin/sh demo -c \
  'nohup /usr/local/bin/session.sh >/tmp/session.log 2>&1 &'

# 3) dump 整个会话树（不是单个 task）
criu dump --tree <session-shell-pid> --images-dir /tmp/d \
  --shell-job --ext-unix-sk --ghost-limit 2147483648 --file-locks -v3

# 4) kill 会话，等 tini 回收 pid
kill -9 <shell> <sway> <foot>

# 5) restore
criu restore --images-dir /tmp/d --shell-job --ext-unix-sk -v3

# 6) 验证
SWAYSOCK=/tmp/xdg/sway-ipc.1000.<sway-pid>.sock swaymsg -t get_tree
# => "app_id": "foot"
```

## 结果

- `criu dump`：`Dumping finished successfully`；
- `criu restore`：`Restore finished successfully. Tasks resumed.`；
- 恢复后 Sway 进程、foot 进程 PID 与快照一致，Sway IPC 可查询，窗口树里 `app_id: foot`。

## 踩过的坑

1. **只 dump 任务子树会失败**：`unix: Can't dump half of stream unix connection`。
   Wayland 连接的另一端（合成器）必须在 dump 树里；要么 dump 整个容器树，要么把任务与合成器一起 `--tree`。
2. **外部 unix socket**：`--ext-unix-sk` 放行；没有 `CONFIG_UNIX_DIAG/INET_DIAG` 时 CRIU 连 socket 都收集不到。
3. **wlroots 共享内存**：需要 `--ghost-limit` 放大（示例 2 GiB）。
4. **文件锁**：`--file-locks`。
5. **PID 复用**：dump 后必须让旧进程被真正回收；PID 1 不回收僵尸时 restore 会 `Can't fork: File exists`。
6. **CRIU 版本**：Arch 的 CRIU 4.2.1 在本内核上 parasite 注入 SIGSEGV；Debian 3.17.1 正常。
7. `criu check` 仍报 `ns_last_pid` 只读（Apple Container 的 /proc/sys 只读），但本次 dump/restore 未受影响。

## 对 mixumOS 的要求

- 引擎的 memory/hybrid 快照应 dump **整个容器进程树**（或任务 + mixcomp 两端），而不是单个 task leader；
- dump 参数带上 `--ext-unix-sk --file-locks --ghost-limit`，restore 带 `--ext-unix-sk`；
- agentd 需要在快照前拿到合成器 pid，或直接以 entrypoint/PID 1 为根；
- 跨设备仍然禁止 memory/hybrid（CRIU 镜像依赖同内核与同设备）。
