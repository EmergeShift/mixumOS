# 目标:发行版工具链(PKGBUILD / pacman 仓库 / mkosi)

> 你负责 mixumOS 的发行版形态落地(roadmap 阶段 3):
> 让 mixumOS 以「发行版」而非「一堆二进制」的方式构建与升级。
> 这是一份自包含任务书:先读本文档,再动手。

## 必读(动手前按序读完)

1. `/Users/ashreo/agent-project/mixumos/docs/distro.md` —— 发行版设计(你的主规格)
2. `/Users/ashreo/agent-project/mixumos/docs/architecture.md` —— 两条铁律(发行版不是一堆二进制)
3. `/Users/ashreo/agent-project/mixumos/tests/integration/Dockerfile.e2e` —— 现有容器构建方式(将被发行版流程取代)

## 项目背景(30 秒版)

mixumOS 是基于 Arch Linux 的 Agent OS。用户拍板:完整发行版形态——
组件进 PKGBUILD,包进自有 pacman 仓库,镜像(mkosi 组装的 rootfs /
OCI)只是发行版产物。升级体验 = `pacman -Syu`;依赖库出 CVE 时
升一个 so 而不是重编整个世界。目标架构 arm64 + x86_64。

## 方向铁律(不得违背)

- **动态链接优先**:组件链系统库,不做 musl 静态宇宙(用户明确反对);
- 镜像是产物不是本体:任何镜像必须可追溯「由哪些包组装而来」;
- `reference/`(Go 参考实现)不进发行版,但**在其 Rust 替代就绪前**,
  它的二进制允许作为 PKGBUILD 的占位源(结构先行,后续只换 source);
- 中文注释与文档全角标点,平实文风;
- 不扩范围:签名仓库的密钥轮换、公网仓库分发、裸机 ISO 后置。

## 你的边界(只准动这些)

- 允许:`dist/pkgs/`(新建,三个包目录)、`dist/repo/`(仓库工具)、
  `dist/os/` 与 `dist/mkosi/`(镜像组装)、`scripts/`(构建入口脚本)、
  `Makefile`(根级,若不存在则新建)、`docs/distro.md`(只允许追加
  「工具链落地记录」节)、`goals/notes/dist.md`
- 只读/禁改:`reference/`、`mixcomp/`、`rust/`、`console/`、`cloud/`

## 现状(2026-10-04)

- 可用的构建产物:
  - Go 参考守护进程:`cd reference && GOOS=linux GOARCH=arm64
    CGO_ENABLED=0 go build -o build/mixumd-arm64 ./cmd/mixumd`
    (amd64 同理);
  - mixcomp:C 代码,`mixcomp/` 目录,Makefile 会自动探测 wlroots pc 名
    (Alpine `wlroots-0.18` / Arch `wlroots0.18`);
- Rust 组件(agentd/ctl)由其他 agent 并行开发,**PKGBUILD 以占位源
  先行**,接口按未来形态设计(`source=()` + 本地构建函数,清晰标注
  「占位,待 mixum-agentd crate 就绪替换」);
- 专用构建 VM:`colima list` 应有 `mixumos`(aarch64,Ubuntu 24.04 +
  Docker 29,代理已配好);所有 Arch 侧构建在该 VM 的 **Arch 容器**里做;
- 现有 e2e:`tests/integration/Dockerfile.e2e` + `reference/cmd/e2eclient`
  (验收你的镜像时用)。

## 任务

1. **PKGBUILD ×4**(`dist/pkgs/`):
   - `mixcomp`:C 构建依赖 wlroots0.18、libjpeg-turbo、json-c、
     libxkbcommon;build() 需现场生成 xdg-shell-protocol.h
     (wayland-scanner,参考 Dockerfile.e2e);check() 冒烟(可执行文件存在
     + `--help` 不炸);
   - `mixum-agentd`:占位源 + systemd 单元(mixumd.service 参考
     `dist/os/systemd/` 若有,否则按 protocol/装配顺序自写:先于
     compositor、工作目录 /var/lib/mixum);
   - `mixum-ctl`:占位源;
   - `mixum-base`:元包,依赖上面三个,并带 sysusers.d(mixum 系统用户)、
     tmpfiles.d(/var/lib/mixum 布局)与默认配置 `/etc/mixum/mixumd.json`;
   - 全部 `check()` 可跑、`arch=(aarch64 x86_64)`、`license` 补齐。
2. **仓库工具**(`dist/repo/build-repo.sh`):在 Arch 容器里 makechrootpkg
   (或 devtools 等价物)双架构出包,repo-add 生成
   `mixum-{aarch64,x86_64}.db.tar.zst`;脚本幂等、产物进 `dist/out/`
   (gitignore)。
3. **镜像组装**(`dist/mkosi/` 或等价脚本,自选并在文档记录理由):
   - 从仓库组装 Arch rootfs(pacman.conf 指向 mixum 仓库 + Arch 镜像);
   - 产物 A:OCI 容器 tar(替换 `tests/integration/Dockerfile.e2e` 的
     Alpine 镜像路径,标注「镜像由 dist 工具链产出」);
   - 产物 B:同源的 rootfs tar(供 k8s/其他导入);
   - 至少 aarch64 通路完整跑通;x86_64 通路:真机或 binfmt 方案二选一
     实现,跑不通则在文档写清卡点与所需资源(不要硬凑)。
4. **构建入口**(`scripts/` + 根 `Makefile`):
   `make dist` = 出包 → 仓库 → 镜像一条龙;`make dist-repo` / `make
   dist-image` 分步;全部幂等。
5. 文档:`docs/distro.md` 追加「工具链落地记录」:实际用的工具版本、
   与设计的偏差、x86_64 通路现状。

## 验收标准

- [ ] `make dist` 在 mixumos VM 内一键产出:双架构包、双仓库 db、
      aarch64 OCI 镜像;
- [ ] 产出的 OCI 镜像跑 e2e:`docker run -d -p 17601:7601 <镜像>` →
      `reference/cmd/e2eclient` 任务/快照链路 PASS(窗口流视 mixcomp
      运行时状态,注明即可);
- [ ] mixcomp 包在 Arch 容器内 makepkg 成功且依赖全部来自 pacman
      (无 vendored 静态库);
- [ ] `pacman -Syu` 场景验证:第二台容器以镜像为模板、指向产物仓库,
      升级单个包(重打一版 mixcomp)后重启服务生效;
- [ ] `goals/notes/dist.md`:交付清单、工具版本、x86_64 通路状态。

## 环境备忘

- Arch 容器获取:VM 内 `docker pull --platform linux/arm64 archlinux:base`
  (若拉不到用 `docker pull ghcr.io/archlinux/archlinux:base` 或从
  Arch Linux ARM rootfs tar 自起,方案二选一并写进文档);
- VM 代理已配好(192.168.5.2:7897);pacman 走代理失败时改用
  `curl -x` 手动 fetch 或检查 resolv.conf(223.5.5.5);
- makechrootpkg 需要 devtools 包(community)与 subvolumes;嫌重可
  用「pacman --sysroot 到纯目录 + tar 打包」的简化通路,但必须文档化
  权衡,并保持产物为标准 pacman 包。

## 协作规程

- 开工先:`cd /Users/ashreo/agent-project/mixumos && git worktree add
  ../mixumos-dist -b module/dist`,在 `../mixumos-dist` 里工作;
- 只提交你边界内的路径(`dist/out/` 产物永不入库,gitignore 已有 build/
  规则,新增 dist/out/ 规则放进你目录内的 .gitignore);提交信息中文一句话;
- 完成后:分支提交齐全 + notes 交付说明;不要合并 main;
- 遇阻:记录 notes 标注 BLOCKER(尤其 x86_64 通路与 Arch 容器获取)。
