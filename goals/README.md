# goals —— 多 agent 并行任务书

> 用法:每个文件是给一个实施 agent 的完整任务书,自包含,直接整份粘贴
> 给对方即可,不需要口头补背景。开工指令一句话:「读 goals/<模块>.md,
> 按协作规程开工」。

## 模块一览

| 任务书 | 模块 | 主要目录 | 语言 |
|--------|------|----------|------|
| [mixcomp-runtime.md](mixcomp-runtime.md) | 合成器运行时调通 + 窗口流/输入/接管验收 | mixcomp/, tests/ | C |
| [mixum-snapshot.md](mixum-snapshot.md) | 快照引擎 crate + Go 交叉向量 | rust/crates/mixum-snapshot | Rust |
| [mixum-agentd.md](mixum-agentd.md) | 容器内守护进程(WSS/任务/broker) | rust/crates/mixum-agentd | Rust |
| [mixum-ctl.md](mixum-ctl.md) | 主机 CLI + 电力钩子 + 本地视图 | rust/crates/mixum-ctl | Rust |
| [dist-toolchain.md](dist-toolchain.md) | PKGBUILD/仓库/mkosi 发行版工具链 | dist/, scripts/ | shell/make |
| [console-pwa.md](console-pwa.md) | Web 控制台(任务/窗口/快照树) | console/ | TS |

## 并行关系

- 六个模块**文件边界互不重叠**,可全并行(git worktree 隔离);
- 软依赖(不阻塞开工,验收时可能要注脚):
  - agentd 的窗口流验收 ← mixcomp-runtime 的修复;
  - dist 的 agentd 包验收 ← agentd crate(Rust 版),占位源先行不受影响;
  - console 的真窗口演示 ← mixcomp-runtime(有模拟帧兜底);
- 汇总合并顺序建议:mixcomp-runtime → snapshot/agentd/ctl/console → dist
  (dist 验收时换上最新二进制重打一版)。

## 给每个 agent 的通用约束(已写进各任务书,此处备份)

- 方向铁律:Rust 核心 + wlroots C 合成器 + 完整发行版;
  `reference/` 是可执行规格,只读不进发行版;
- 文档是唯一真相来源:改协议必须同步 docs/protocol.md 并递增版本;
- 边界外只读;遇阻记 `goals/notes/<模块>.md` 标注 BLOCKER,不越界硬修;
- 中文文档全角标点,平实文风;
- git:各 agent 在 `module/<名>` 分支(worktree 隔离)提交,由用户合并 main。

## notes 目录

各 agent 交付说明与阻塞记录写入 `goals/notes/<模块>.md`(自建)。
