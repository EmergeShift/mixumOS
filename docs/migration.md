# 跨设备迁移（`.mart` migration）

> 状态：设计稿。目标是把“换设备继续干活”做成 MixumART 里的一个动作，而不是让用户手动拖文件。

## 1. 迁移的是什么

一个 mixumOS 节点由四层组成，迁移按层处理：

| 层 | 是否迁移 | 载体 |
|---|---|---|
| 工作区文件 | ✅ | `.mart`（MAP 快照格式） |
| 快照树 / 节点元数据 | ✅ | `migration.json`（树节点、origin、kernel/arch、guiSessions） |
| 容器定义（镜像、env、端口、cmd） | ✅ | `migration.json` 的 `container` 段 |
| 进程内存（CRIU） | ❌ 跨设备 | 同机同内核才考虑；跨设备不做 |
| GUI 窗口 | ✅ 重放 | `guiSessions` → 目标端重新拉起应用 |

**迁移不是“活容器搬家”**：目标是“用同一个镜像重建容器 + 恢复工作区 + 重放窗口”。

## 2. 迁移包

`MigrationBundle`（MixumART 内部管理，用户不需要手工解包）：

```
migration.json          # 元数据（见下）
workspace.mart          # 工作区文件快照（MAP .mart v1）
image.oci.tar           # 可选：目标无法直接 pull 时携带 OCI layout
```

`migration.json`：

```json
{
  "version": 1,
  "source": {
    "host": "mac-studio",
    "runtime": "apple_container",
    "node": "map-node",
    "exportedAt": "2026-10-22T03:00:00Z"
  },
  "container": {
    "image": "ghcr.io/emergeShift/mixumos:0.1.4",
    "imageDigest": "sha256:...",
    "env": { "MIXUM_JPEG_QUALITY": "90" },
    "ports": [{ "host": 17601, "container": 7601, "protocol": "tcp" }],
    "mounts": [],
    "command": []
  },
  "snapshot": {
    "node": "01J...",
    "branch": "main",
    "label": "迁移点",
    "kernel": "6.18.35-mixum-micro",
    "arch": "arm64",
    "createdAt": 1760000000000,
    "origin": "mac-studio",
    "guiSessions": [
      {
        "appId": "foot",
        "title": "shell",
        "width": 640,
        "height": 400,
        "command": "/usr/bin/foot",
        "args": ["--hold"],
        "cwd": "/var/lib/mixum/work",
        "env": { "WAYLAND_DISPLAY": "wayland-0" }
      }
    ]
  }
}
```

`.mart` 只装文件（MAP 规范明确不含压缩/加密/CRIU）；树与容器定义放在 `migration.json`。
需要单文件离线搬运时，MixumART 可以把两者打成一个 `tar.zst`，但**默认走 API 流式传输**。

## 3. 协议扩展

复用现有文件传输（`file.put` + `FILE` 二进制帧 + `file.put.commit`；`file.get`），
新增两个 op（未进 MAP v1 前作为 mixumOS 扩展，字段保持向后兼容）：

### `snap.export`

请求：

```json
{ "node": "01J...", "format": "mart" }
```

应答：

```json
{ "file": "mixum-01J....mart", "size": 12345678, "sha256": "..." }
```

文件落在 `work_root/transfers/`，客户端用 `file.get` 下载；导出时把 CAS 的 zstd 块
解压为 `.mart` 的原始块（块哈希都是明文 SHA-256，可直接复用）。

### `snap.import`

请求：

```json
{ "file": "mixum-01J....mart", "origin": "mac-studio" }
```

应答：

```json
{ "node": "01J...", "branch": "main", "chunks": 42, "bytes": 12345678 }
```

导入校验：magic/版本/头部长度/rkyv 清单/每个块哈希；缺失块写入本地 CAS，
节点挂到当前 head（或指定 parent），然后由调用方触发 `snap.restore`。

## 4. 端到端流程（MixumART 视角）

### 导出

1. 连接源环境（WSS），确认 agentd 版本支持 `snap.export`；
2. 选择快照节点 → `snap.export {format:"mart"}`；
3. `file.get` 流式下载 `.mart` 到 MixumART 缓存（带 SHA-256 校验）；
4. 收集容器定义（镜像 digest、env、ports、mounts、cmd）与 `guiSessions`；
5. 生成 `migration.json`，两者组成 bundle（默认只在本机缓存，不上传第三方）。

### 导入

1. 用户选择目标 runtime（Apple Container / Docker / WSL2）；
2. 确保镜像在目标可用：按 digest pull；失败则用 bundle 里的 `image.oci.tar` 导入；
3. 按 `container` 段创建容器（复用现有 `CreateSpec`）；
4. 启动容器，等待 agentd READY；
5. `file.put` 上传 `.mart` → `snap.import` → `snap.restore {node}`；
6. 目标端按 `guiSessions` 重放窗口（Task 5）；
7. MixumART 切换连接档案到新环境，窗口页重新 attach。

### 失败与回滚

- 镜像不可用：保留源环境，提示“目标缺少镜像”，不创建半成品容器；
- arch 不一致：`snap.restore` 会拒绝（当前 arch 强校验）；提示需要同架构或重建；
- `.mart` 校验失败：丢弃临时文件，源与目标都不改；
- restore 失败：目标容器保留在“已创建”状态，允许重试或删除；
- 导入是幂等的：同块不重复写；同节点 id 已存在时返回既有节点。

## 5. 安全

- `.mart` 只含工作区文件；不包含令牌、证书、`admin-token`；
- 传输走 WSS（TLS 1.3 + 能力令牌），文件 SHA-256 全链校验；
- 路径穿越：`.mart` 清单路径拒绝 `..`/绝对路径（MAP 规范）；
- 目标端导入前校验 magic/版本/长度上限，避免 zip-bomb 式头部；
- 可选：`.mart` 的加密层是 MAP 的后续工作，v1 不假装支持。

## 6. 兼容性

- 同架构 + 同镜像：完整支持（文件 + 树 + 会话）；
- 跨架构：只能迁移源码/数据，不能迁移二进制与进程；`arch` 检查会拒绝；
- 内核不同：disk 快照已放开（PR #39），memory/hybrid 仍拒绝；
- Docker Desktop：可作为源/目标，但没有 CRIU；
- `.mart` 版本：v1；未来压缩/加密通过 header version 演进，导入端拒绝未知版本。

## 7. 实现拆分

| 仓库 | 内容 |
|---|---|
| `mixumos` | `mixum-mart` crate（pack/open/verify/import）、`snap.export`/`snap.import`、CAS 桥接、测试 |
| `MixumART` | `migration.json` 生成、bundle 缓存、传输编排、目标端容器创建/restore、UI 动作 |
| `MAP` | `.mart` 规范与 SDK（`map-snapshot`）；mixumOS 以固定的 MAP 版本为准 |

## 8. 验收

1. 单测：`.mart` roundtrip、缺块导入、坏哈希拒绝、路径穿越拒绝；
2. 集成：同机两个容器之间 export→import→restore，文件一致、树一致；
3. 跨 runtime：WSL2（同架构）→ Apple Container，窗口按清单重开；
4. 失败注入：目标缺镜像、arch 不一致、`.mart` 截断；
5. 性能：1 GB 工作区导出/导入时间、峰值内存、传输去重率。
