# mixumOS 发行版构建

> 状态:设计定稿;工具落地在阶段 3。
> 原则:mixumOS 是发行版,镜像(OCI / rootfs)是它的产物,不是它的本体。

## 1. 形态

```
PKGBUILD(mixum-agentd / mixcomp / mixum-ctl)
   │ makepkg(arm64 / x86_64)
   ▼
pacman 包(*.pkg.tar.zst)
   │ repo-add → dist/repo/{arm64,x86_64}/
   ▼
mkosi(pacman 从仓库组装)
   ├──> mixumOS rootfs(可启动 / cloud-init 就绪)
   └──> OCI 容器镜像(cloud/k8s 用,同一来源 layer 化)
```

升级体验:发行版内 `pacman -Syu` 走标准 Arch 仓库 + mixum 仓库;
单个组件 `pacman -S mixum-agentd` 独立升级。依赖库(wlroots、openssl)
的安全更新由 Arch 仓库承接,mixum 包只重打包不重编世界。

## 2. PKGBUILD 约定

- **动态链接优先**:mixcomp 链系统 wlroots0.18/libjpeg-turbo/json-c;
  Rust 组件默认动态链接 glibc(Arch 的 rust 打包惯例),不用 musl 静态——
  静态宇宙二进制与发行版哲学相悖,这是本项目的明确决策;
- `check()`:跑 crate 单测 / mixcomp 的冒烟测试;
- systemd 单元、sysusers.d、tmpfiles.d 随包安装(不再散落镜像层);
- 包清单:

| 包 | 内容 | depends |
|----|------|---------|
| mixum-agentd | /usr/bin/mixum-agentd、mixumd.service、sysusers | rust 运行时无需、wlroots0.18(运行时链接 mixcomp 协议无依赖,仅文件系统交互)、criu(可选) |
| mixcomp | /usr/bin/mixcomp、README | wlroots0.18、libjpeg-turbo、json-c、libxkbcommon |
| mixum-ctl | /usr/bin/mixum-ctl、补全 | 无(rust 动态链接常规库) |
| mixum-base(元包) | 默认配置、/var/lib/mixum 布局 | 上述三者 |

- 构建矩阵:`dist/pkgs/<pkg>/` 一份 PKGBUILD,`makechrootpkg` 在
  arm64 与 x86_64 的 chroot 里各出一份;x86_64 无真机时用
  qemu-user-static + binfmt(colima VM 内可开)。

## 3. pacman 仓库

- 布局:`dist/repo/<arch>/` 各自独立,`repo-add mixum.db.tar.zst *.pkg.tar.zst`;
- 签名:仓库密钥与包签名(makepkg --sign),GPG 公钥随镜像内置,
  客户端/镜像的 pacman.conf 指向 mixum 仓库并信任该密钥;
- 分发:v1 随镜像内置仓库目录(离线可用);公网仓库后置。

## 4. mkosi 组装

- `dist/mkosi/mkosi.conf`:发行版 archlinux,`Packages=` 指向本地仓库,
  `CleanPackageDetect=yes` 保持最小;
- 容器产物:mkosi 输出 tar → `dist/os/import-oci.sh` 转OCI layout
  (或直接 `mkosi --format=oci`),双架构 manifest 由脚本合并;
- 关键镜像内配置:
  - `mixum-base.service` 先于 agentd(目录/密钥/身份初始化);
  - `WLR_BACKENDS` 环境由 mixcomp 自设(headless),无需镜像干预;
  - 工作区 `/var/lib/mixum/work` 独立卷声明(k8s 场景挂 PVC,
    快照引擎 skip 列表已排除自身存储);
  - 容器内需要 `CAP_SYS_ADMIN`?不需要:overlayfs/btrfs 检测是运行时
    降级逻辑,默认 directory 后端即可工作;CRIU 需要特权,镜像提供
    `mixum-privileged` 变体(阶段 5);
- 可启动镜像(云外裸机/VM 场景):同一 mkosi 配置 `--format=disk`,
  cloud-init 接管首启配置。

## 5. 版本与发布

- 语义化:主版本随协议主版本(envelope.v);
- 包版本 = crate/源码版本,pacman epoch 管理重打包;
- 发布产物:`make dist` 一键产出
  `dist/out/{repo-arm64, repo-x86_64, mixumos-<ver>-<arch>.oci.tar}`;
- 可复现:PKGBUILD 固定源(sdist + sha256),mkosi 版本锁定;
  构建机差异记录在 `dist/out/BUILDINFO`。

## 6. 与 reference 的关系

reference/(Go)不进发行版。它的 e2e(tests/integration)保留,
Rust agentd 就绪后同一套 e2e 换二进制即验协议兼容。
