# mixum-agentd(占位包)

mixumOS 容器内守护进程。当前处于占位期:

- 二进制 `/usr/bin/mixumd` 来自 `reference/`(Go 参考实现),
  由 `dist/repo/build-repo.sh` 构建,Rust 版(`rust/crates/mixum-agentd`)
  就绪后替换,包名与文件路径不变;
- `/usr/lib/systemd/system/mixumd.service`:装配顺序为 mixumd 先于
  compositor(mixcomp 连接它的 display.sock),工作目录 `/var/lib/mixum`;
- 默认配置由 `mixum-base` 提供(`/etc/mixum/mixumd.json`)。

升级方式:发行版内 `pacman -Syu` 或 `pacman -S mixum-agentd`。
