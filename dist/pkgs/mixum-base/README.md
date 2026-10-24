# mixum-base

mixumOS 元包,一次装齐:

- `mixum-agentd`(容器内守护进程,占位期为 reference 的 mixumd)
- `mixcomp`(Wayland 合成器)
- `mixum-ctl`(主机侧 CLI,占位包)
- `nodejs` / `npm` / `curl`(Agent 一键安装:claude-code、codex、gemini-cli;
  opencode 使用 curl 安装脚本)

并布好系统布局:

- `usr/lib/sysusers.d/mixum.conf`:系统用户 `mixum`;
- `usr/lib/tmpfiles.d/mixum.conf`:`/var/lib/mixum`、`/var/lib/mixum/work`、
  `/run/mixum`(工作区根独立成卷,k8s 场景挂 PVC);
- `/etc/mixum/mixumd.json`:默认配置(改动经 pacman `.pacnew` 机制提示)。
