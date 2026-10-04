# mixum-ctl(占位包)

mixumOS 主机侧 CLI。当前处于占位期:

- Rust 实现(`rust/crates/mixum-ctl`)就绪前,本包只占发行版包位,
  不提供二进制;
- 就绪后本包提供 `/usr/bin/mixum-ctl`(电力钩子、快照推送、本地视图)
  与命令补全,依赖 mixum-protocol crate 与 agentd 共享类型。

升级方式:发行版内 `pacman -Syu`。
