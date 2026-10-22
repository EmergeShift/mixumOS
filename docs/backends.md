# mixumOS 宿主后端形态

> mixumd(agentd)对运行环境没有任何容器/虚拟机假设:它是一个 Linux
> 守护进程,需要「一个 Linux 用户态 + 工作区目录(快照引擎打包
> work root)+ 可选的 Wayland 合成器(mixcomp)」。因此任何能提供
> 这三样的宿主技术都可以做后端。本文定义官方支持的四种形态。

## 1. Docker / OCI(默认形态)

- 产物:`make dist` 的 OCI 镜像(`mixumos-<ver>-<arch>.oci.tar`),
  `docker load`/`import` 后 `docker run -p 7601:7601` 使用;
- 适用:云端、k8s(mixum-operator,MixumTask CRD)、本机开发 e2e;
- 限制:容器内 CRIU 需特权变体(阶段 5);工作区建议挂独立卷。

## 2. WSL2(Windows 本机,零 Docker)

v0.1.0 起,发行版 rootfs 可直接导入为 WSL 发行版——rootfs 里有完整
systemd 与包管理,一键导入:

```sh
bash dist/os/wsl-import.sh            # 或按上文手工四步
```

**历史验证(2026-10-17,本机实测;本次 PR 未重新跑完整导入流程)**:rootfs 直导 → systemd 激活 →
mixumd 服务化(User=mixum,StateDirectory)→ Windows 直连
wss://127.0.0.1:7601(WSL2 localhost 转发)→ 真实快照引擎建快照 →
改动 → 原地回退 → 文件物化逐字一致;kernel 字段如实记录为
WSL 内核(跨内核恢复闸生效)。WSL 形态里 pacman/Arch 官方源可用,
单包升级路径照常。验证探针:[tests/integration/wsl-probe.mjs](../tests/integration/wsl-probe.mjs)。

- 工作区在 WSL ext4 上,快照引擎受益于真实 ext4;
- 窗口流:mixcomp 服务化已随包安装(mixcomp.service,0.1.0-3 起),
  GUI 应用以 mixum 用户连 `WAYLAND_DISPLAY=wayland-0`
  (XDG_RUNTIME_DIR=/run/mixum);**注意 GUI 应用需要字体**
  (pacman -S ttf-dejavu),缺字体会导致客户端应用启动即退
  (foot 实测)。**已知限制**:foot 在无 WSLg 的 headless 环境
  偶发 abort(230),窗口流端到端验证受阻——mixumd/mixcomp 侧
  注册与协商链路正常,待 foot-on-WSL 稳定性跟进或换最小
  Wayland 客户端复验;
- 已知限制:CRIU 可用性取决于 WSL 内核配置(memory/hybrid 快照
  在此形态下按不可用降级);跨内核快照恢复由 kernel 校验闸拒绝;
- 运维注意:服务以 User=mixum 运行,工作区文件勿用 root 手工种
  (回退清理需要文件属主一致,root 种的文件会导致 restore 报
  Permission denied——行为正确,报错可读)。

## 3. microsoft/nvx(microVM,观察中)

[nvx](https://github.com/microsoft/nvx) 是微软面向 agentic workloads
的跨平台 microVM 沙箱(OpenVMM 之上,Linux KVM/MSHV + Windows WHP),
与 mixumOS 分层互补:nvx 管 hypervisor 层的 VM 启动,mixumOS 是
guest 内的 OS/发行版。接入形态:mixumOS rootfs(OCI)作为 nvx guest
镜像,nvx 替代 Docker 成为「起环境」的后端。

状态:早期(README 仅 scripts/nvx.py 启动 Alpine/Ubuntu/Azure Linux,
快照/共享目录/GPU 未露出),**列为演进项**,待其 guest image 与 API
稳定后接入;届时 microVM 强隔离 + mixumOS 快照/窗口流是理想组合。

## 4. k8s(mixum-operator)

[MixumTask CRD](https://github.com/yubing0721/mixumOS-yubing-dev-fork)
调度 OCI 镜像到节点池(arm64/x86_64),快照卷挂载;云端/多容器路由
gateway 为阶段 5 内容。

## 选型速查

| 形态 | 适合 | 前置 | 隔离强度 |
|------|------|------|----------|
| Docker/OCI | 云端/服务器/k8s | docker | 容器 |
| WSL2 | Windows 本机开发/个人 agent | WSL2 | 容器(VM 内) |
| nvx | 强隔离 agentic 沙箱(未来) | OpenVMM/WHP | microVM |
| k8s | 多容器/团队/云端编排 | 集群 + operator | 容器 + 策略 |
