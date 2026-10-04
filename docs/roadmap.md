# mixumOS 路线图

> 节奏原则:ddl 以月计。每个阶段以「设计文档先行、测试锁行为、
> 发行版产物可复现」为完成标准,不以代码行数为进度。

## 已完成(reference 阶段,Go 参考实现)

- [x] wire 协议 v1:envelope、ops、能力令牌(ed25519)、二进制帧头
- [x] 快照引擎语义:内容寻址块存储(zstd+AES-GCM)、快照树 DAG
      (无感前置快照 / 原地回退 / fork 分叉 / diff / GC)
- [x] 任务引擎:AgentDriver 接口、shell/external/browser 三驱动、
      进程组冻结(SIGSTOP)与快照联动
- [x] 显示 broker:窗口注册、帧分发、接管状态机(agent→user→free)
- [x] WSS 服务端(TLS 1.3)与跨节点块同步协议(去重推送)
- [x] mixcomp C 实现编译通过(wlroots 0.18,aarch64)
- [x] 容器 e2e:任务执行 → 前置快照 → 回退 → fork 全链路 PASS
- [x] 全栈集成测试(进程内双节点互推快照、去重生效)

## 阶段 1:Rust 协议与快照内核(当前)

目标:Rust workspace 立起来,先把与语言无关的内核做扎实。

- [ ] `mixum-protocol`:envelope/ops/caps/帧头/令牌,单测与 Go 参考实现同语义;
- [ ] `mixum-snapshot`:块存储(CAS+zstd+静态加密)、树 DAG(bolt→sled 或
      redb,选型时定)、COW 打包、CRIU 驱动、GC;
- [ ] 交叉验证:同一测试向量集,Go 与 Rust 实现互跑
      (令牌互验、清单互读、块互导)。

完成标准:`cargo test` 全绿 + 交叉向量测试通过。

## 阶段 2:mixum-agentd(容器内守护进程)

- [ ] WSS 服务端(tokio-tungstenite + rustls,TLS 1.3 强制);
- [ ] 任务引擎与 AgentDriver trait;external 桥协议实现;
- [ ] 显示 broker 与接管状态机;mixcomp 控制通道(协议不变);
- [ ] 与 mixcomp 的容器 e2e(复用 tests/integration,换 Rust 二进制)。

完成标准:现有 e2eclient 对 Rust agentd 全链路 PASS(协议兼容即证)。

## 阶段 3:发行版形态

- [ ] PKGBUILD ×3(mixum-agentd / mixcomp / mixum-ctl),动态链接走系统依赖;
- [ ] 自有 pacman 仓库工具(repo-add 流程,arm64 + x86_64 双仓库);
- [ ] mkosi:从仓库组装 Arch rootfs → OCI 镜像(两种产物同一来源);
- [ ] systemd 单元、sysusers、tmpfiles 进包;
- [ ] x86_64 构建通路(qemu-user 或 x86_64 构建机,binfmt)。

完成标准:`make dist` 一条命令产出双架构 OCI 镜像与 pacman 仓库。

## 阶段 4:agent 嵌入与 GUI 流体验

- [ ] AgentDriver trait 的 crate 化发布:你们 agent 以依赖方式接入;
- [ ] mixcomp 流体验打磨:自适应帧率、脏区、H.264(WebCodecs)可选;
- [ ] mixum-ctl:macOS 电力钩子(盒盖前快照上传)、本地窗口视图服务;
- [ ] 控制台 PWA:任务流、快照树(图状)、窗口画布(光标映射)、接管按钮。

完成标准:手机发指令 → 云端 agent 开 Chrome → mac 上看到独立原生风格窗口,
中途接管、回退、fork 全程可用。

## 阶段 5:云端与多设备

- [ ] k8s operator(kube-rs):MixumTask CRD、节点池亲和(arm64/x86_64)、
      快照卷挂载;
- [ ] gateway:多容器路由、设备配对、令牌分发;
- [ ] 内存快照跨内核迁移的可行性验证(CRIU 同内核约束的工程对策)。

## 持续事项

- 安全:威胁模型随阶段更新(security.md);
- 文档:每个阶段的实现与设计偏差回写设计文档;
- mixcomp 上游跟踪:wlroots 0.19 变更评估。
