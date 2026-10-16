# mixum-agentd

mixumOS 容器内守护进程(Rust 实现):

- 二进制 `/usr/bin/mixumd` 由 PKGBUILD 在构建容器内从
  `rust/crates/mixum-agentd` cargo 构建,包名与文件路径沿用历史名;
- `/usr/lib/systemd/system/mixumd.service`:装配顺序为 mixumd 先于
  compositor(mixcomp 连接它的 display.sock),工作目录 `/var/lib/mixum`;
- 默认配置由 `mixum-base` 提供(`/etc/mixum/mixumd.json`)。

升级方式:发行版内 `pacman -Syu` 或 `pacman -S mixum-agentd`。
