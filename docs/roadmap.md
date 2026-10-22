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

## 阶段 1:Rust 协议与快照内核 ✅

目标:Rust workspace 立起来,先把与语言无关的内核做扎实。

- [x] `mixum-protocol`:envelope/ops/caps/帧头/令牌,单测与原参考实现同语义;
- [x] `mixum-snapshot`:块存储(CAS+zstd+静态加密)、树 DAG(sled)、
      COW 打包、CRIU 驱动、GC;
- [x] 交叉验证:测试向量集锁定语义(令牌互验、清单互读、块互导;
      Go 参考实现移除后由 tests/cross-protocol 预生成向量承接)。

完成标准(已达成 2026-10-05):`cargo test` 全绿 + 交叉向量测试通过。

## 阶段 2:mixum-agentd(容器内守护进程)✅

- [x] WSS 服务端(tokio-tungstenite + rustls,TLS 1.3 强制);
- [x] 任务引擎与 AgentDriver trait;external 桥协议实现;
- [x] 显示 broker 与接管状态机;mixcomp 控制通道(协议不变);
- [x] 与 mixcomp 的容器 e2e(tests/integration,Rust 二进制)。

完成标准(已达成 2026-10-05):e2e 对 Rust agentd 全链路 PASS
(握手/系统信息/窗口流/任务/快照树/回退/fork)。

## 阶段 3:发行版形态(当前,收尾中)

- [x] PKGBUILD ×5(mixum-agentd / mixcomp / mixum-ctl / mixum-base /
      wlroots0.18 自建),动态链接走系统依赖;
- [x] 自有 pacman 仓库工具(repo-add 流程,arm64 + x86_64 双仓库);
- [x] rootfs/OCI 组装:pacstrap + 纯 shell OCI 打包(mkosi 以等价脚本
      替代;可启动磁盘镜像后置阶段 5);
- [x] systemd 单元、sysusers、tmpfiles 进包;
- [x] x86_64 构建通路(binfmt + qemu-user 模拟实跑,双架构 e2e PASS);
- [x] 双架构重打实测:mixum-agentd 切容器内 cargo 构建后全链重打
      (2026-10-17,Docker Desktop/WSL2 通路;x86_64 原生 + aarch64 qemu,
      双架构五包/仓库/OCI 齐产,导入启动+握手验收 PASS,多架构索引合并)。
- [x] 跨节点流转双节点 e2e(盒盖→云端续跑全链:snap.create → snap.push
      → 幂等重推 → 云端 restore 物化 → head 迁移)与引擎延迟基准
      (2026-10-17,数字与复现见 snapshots.md §9)。

完成标准:`make dist` 一条命令产出双架构 OCI 镜像与 pacman 仓库
(2026-10-17 达成;cargo 缓存卷提速已实施——重打增量 45s,跨编译
选项列为演进项,见 distro.md §7.3.1)。

## 阶段 4:agent 嵌入与 GUI 流体验

- [x] AgentDriver trait 的 crate 化发布(2026-10-18):SPI 独立为
      mixum-agent-driver crate(零内部依赖,Task/TaskSpec/TaskView/
      Sandbox/EventEmitter/Registry/ShellDriver),agentd 经再导出
      保持 API 不变;接入指南 docs/agent-driver.md,可运行示例
      hello_driver(in-process 与子进程桥接两条路);
- [x] mixcomp H.264 流(openh264 编码,attach 协商 + WebCodecs 解码,
      JPEG 路径保留;2026-10-17,静态内容 10s 流量 JPEG 的 1/84,
      动态内容预期 1/5~1/20);自适应帧率、脏区仍待做;
- [ ] mixum-ctl:macOS 电力钩子(盒盖前快照上传)、本地窗口视图服务;
- [ ] 控制台 PWA:任务流、快照树(图状)、窗口画布(光标映射)、接管按钮。

完成标准:手机发指令 → 云端 agent 开 Chrome → mac 上看到独立原生风格窗口,
中途接管、回退、fork 全程可用。

## 阶段 5:云端与多设备

- [ ] k8s operator(kube-rs):MixumTask CRD、节点池亲和(arm64/x86_64)、快照卷挂载;
- [x] gateway M1/M2(2026-10-18,gateway-e2e 全链):节点出站注册、
      envelope 路由(文本+二进制)、用户/节点令牌签发、设备配对/吊销、
      TLS、console 节点选择器;
- [x] gateway M3(2026-10-19,gateway-m3-e2e):`NodeRegistry` trait +
      k8s Lease 共享注册表(心跳续约)、副本间 WS mesh 双向跨副本路由、
      节点掉线跨副本清理;部署清单 cloud/k8s/60-gateway.yaml;
- [x] 内存快照跨内核迁移的可行性验证(2026-10-20 受控实验 PASS):源
      WSL 6.18+CRIU 4.2.1 → 目标 Debian 6.1.0-47+同版本 CRIU,目标
      control PASS 后跨内核 restore 两轮 PASS(计数 0→1→3 继续);此前
      「不可行」结论系环境混杂(版本混用/文件 fd/cwd/root mode/modules),
      已纠正;覆盖范围=最小静态进程,精确内核指纹门禁保留为保守默认,
      按场景扩展后再评估放宽(docs/criu-feasibility.md §4.4/§7)。

## 持续事项

- 安全:威胁模型随阶段更新(security.md);
- 兼容:mixumd↔mixcomp 私有通道未知记录类型忽略 + 告警计数(2026-10-19
  完成,EmergeShift#8 实现侧;规范侧 MAP display.md 同步补充);
- display.attach 插入订阅者与二次查表竞态(PR#6 遗留②,2026-10-19 以单锁
  区间消除,`attach` 不再二次查表);
- 文档:每个阶段的实现与设计偏差回写设计文档;
- mixcomp 上游跟踪:wlroots 0.19 变更评估。

## 2026-10-19:agentd 采纳 MAP v1

- `session.hello`(保留 `hello` 别名)、`map.v1`/`mixum.v1` 双子协议;
- task.event data 统一为对象(`state`/`log`/`step`/`progress`/`error`/`input`);
- 新实现:`task.reply` 人机输入通道、`session.resume`(≥256 事件重放环)、
  `mem.*` 与 `builtin` provider、`map1.` 令牌前缀(旧前缀仍接受);
- 标识符改 ULID(envelope/task/snapshot/memory);
- 验收:`scripts/map-sdk-e2e.sh`(TS SDK 零适配全环 + legacy 方言);
  规范见 `Mixum-Agent-Protocol/site/src/content/docs/spec/`。
