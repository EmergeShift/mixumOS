# mixumOS

基于 Arch Linux 的 Agent OS 发行版:容器即 agent 运行环境,操作可回退、
可分叉、可在设备间迁移;agent 操作的 GUI 以独立原生窗口流到手机或电脑。

- 设计文档入口:[docs/architecture.md](docs/architecture.md)
- 路线图:[docs/roadmap.md](docs/roadmap.md)(ddl 以月计,文档先行)
- 协议规范:[docs/protocol.md](docs/protocol.md)

## 当前状态(2026-10-04)

| 部分 | 状态 |
|------|------|
| reference/(Go 参考实现) | 全栈可用:任务/快照树/回退/fork/跨节点同步/接管状态机,容器 e2e PASS |
| mixcomp(C/wlroots 0.18) | 编译通过 aarch64;每窗口捕获 + 协议注入;运行时调通中 |
| docs/ | 架构/协议/快照/合成器/发行版/安全/agent 桥 已定稿 |
| rust/(核心重写) | 阶段 1:mixum-protocol 起步 |
| dist/(发行版) | 设计定稿,工具落地在阶段 3 |

## 快速体验(reference 参考实现)

```sh
# 双架构核心二进制
cd reference && GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o /tmp/mixumd-arm64 ./cmd/mixumd
cd reference && GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o /tmp/mixumd-amd64 ./cmd/mixumd

# 单元测试
cd reference && go test ./internal/...

# 容器 e2e(mixumd + mixcomp + foot 窗口流 + 任务 + 快照)
docker build -f tests/integration/Dockerfile.e2e -t mixumos-e2e .
docker run -d --name mixum-e2e -p 17601:7601 mixumos-e2e
# 令牌在容器日志 READY 行
```

## 目录

见 [docs/architecture.md](docs/architecture.md) 的目录结构一节。
简述:`rust/` 核心(workspace),`mixcomp/` C 合成器,
`reference/` Go 可执行规格(不进发行版),`dist/` 发行版构建,
`tests/` 集成 e2e,`docs/` 一切设计的唯一真相来源。

## 许可证

本仓库以 [Apache-2.0](LICENSE) 发布。mixcomp 的"魔改 Wayland 合成器"
指基于 wlroots 自研,仓库不包含任何上游组件的修改版源码:

- 仓库内源码(rust/mixcomp/reference/console/dist 脚本)为原创;
  构建期拉取的上游 wlroots 0.18.2 tarball 未打补丁(MIT,见其 PKGBUILD);
- 运行时动态链接的第三方库按各自许可:wayland/wayland-protocols(MIT)、
  libxkbcommon(MIT)、json-c(MIT)、libjpeg-turbo(IJG/BSD-3)、zstd(BSD);
- mixumOS 镜像内含 Arch/ALARM 基础系统(glibc/systemd/coreutils 等
  GPL/LGPL 组件),其源码合规由上游发行仓库满足。
