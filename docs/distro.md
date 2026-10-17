# mixumOS 发行版构建

> 状态:工具链已落地(§7,2026-10-04);mixum-agentd 切容器内 cargo 构建
> 后双架构重打实测收尾中(见 §6)。
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

## 6. mixum-agentd 的构建来源

Go 参考实现已移除(2026-10-16)。mixum-agentd 包由 PKGBUILD(cargo)在
构建容器内直接构建 Rust 实现:build-repo.sh 导出 rust/ 与
tests/cross-protocol 进容器,按 PKGBUILD 声明装依赖(rust/clang/cmake/perl)
后原生编译;二进制沿用历史名 mixumd(e2e 入口脚本按此名调用)。
原 Go 占位流程(宿主 go 交叉编译 + placeholder 包装)与
scripts/dist-e2e.sh 一并移除;双架构全链重打待 VM 实测。

## 7. 工具链落地记录(2026-10-04,module/dist 分支)

### 7.1 实际形态

```
dist/pkgs/          PKGBUILD ×5(wlroots0.18 自建包 + mixcomp + mixum-agentd
                    + mixum-ctl + mixum-base)
dist/repo/          build-repo.sh(宿主编排)+ container-build.sh(容器内两段式)
dist/os/            container-image.sh(pacstrap 组装)+ mkoci.sh(纯 shell
                    OCI 打包,带 blob 摘要自校验)+ build-image.sh(宿主编排)
scripts/            lib.sh(构建容器保障)+ dist-e2e.sh(e2e 验收)
Makefile            make dist / dist-repo / dist-image / dist-e2e / dist-clean
```

版本号唯一来源是 mixum-base 的 pkgver(当前 0.1.0);产物命名
`mixumos-<ver>-<arch>.oci.tar`、`mixumos-<ver>-<arch>-rootfs.tar.zst`、
仓库 `dist/out/repo-<arch>/mixum.db.tar.zst`。

### 7.2 与设计的偏差(及理由)

1. **wlroots0.18 由 mixum 仓库自建**。设计假设 0.18 可从官方源安装;
   落地时(2026-10)Arch 官方源与 Arch Linux ARM 源都只剩 0.20,而
   mixcomp 按 0.18 API 编写且其源码目录在工具链边界之外。故以上游
   0.18.2 源码自建包,最小化配置(仅 headless 后端 + pixman 渲染器,
   不拉 DRM/libinput/X11/EGL 依赖),ABI 钉在自有仓库——这正是
   「依赖库出安全更新时升一个 so」的落点:0.18.x 的修复单独重打本包。
   上游 meson 的版本化命名(wlroots-0.18)与 mixcomp 的探测列表天然吻合。
2. **mkosi 换成等价脚本(dist/os/)**。构建 VM 是 Ubuntu,引入 mkosi 还要
   解决宿主 pacman 依赖与版本锁定;包构建已有 Arch 容器体系,同一容器里
   pacstrap + 纯 shell OCI 打包通路更短、依赖更少。mkosi 的引入门槛只剩
   「可启动磁盘镜像(阶段 5)」,届时再评估。
3. **不用 makechrootpkg,直接在 Arch 容器里 makepkg(专用 builder 用户)**。
   devtools 的 chroot 副本与 btrfs subvolume 在容器体系里属于重复隔离;
   容器即干净构建环境,产物仍是标准 pacman 包(.PKGINFO/.BUILDINFO/MTREE
   齐全)。信任面 = 基础镜像 + pacman 镜像源,已记录。
4. **arm64 通路基于 Arch Linux ARM**。官方 Docker 镜像(Docker Hub 与
   ghcr)均无 arm64 manifest,Arch 官方仓库本身也没有 arm64 包;ALARM
   是 Arch 的 arm64 官方移植,包名/格式一致。rootfs tar 缓存于
   dist/out/ 根(不入库;文件名 ArchLinuxARM-aarch64-latest.tar.gz),
   下载源用 TUNA(官方 geo 镜像的 os/ 目录重定向后 404);ALARM 容器内
   pacman 镜像可用 MIXUM_ARM_MIRROR 覆盖(geo 重定向经部分代理不可达,
   TUNA 实测可达,2026-10-17)。
5. **x86_64 通路:binfmt + qemu-user**(构建 VM 内已注册 qemu-x86_64),
   无真机。mixumd 宿主交叉编译不经模拟;makepkg 全链(wlroots 编译、
   四包、pacstrap 组装)在模拟下实跑,OCI 镜像以 `docker import
   --platform linux/amd64` 导入后 e2eclient 全链 PASS——x86_64 与
   aarch64 同为完整通路,代价是构建时间约为原生的数倍。
6. **仓库 v1 不签名**(SigLevel = Never),GPG 签名后置(任务书范围外);
   产物保留标准签名位。
7. **占位源策略(已废弃)**:曾以宿主 go 交叉编译的 reference/mixumd
   作占位二进制;2026-10-16 随 Go 参考实现移除而废止,现行构建来源见 §6
   (容器内 cargo,二进制沿用历史名 mixumd)。
8. **mixcomp 的 check() 已默认严格**(历史上的启动段错误——虚拟 pointer
   时序等六处 0.18 迁移坑——已于 wayland-magic.md §8.1-8.6 修复,
   71 分钟稳定运行;2026-10-17 起默认收紧,`MIXCOMP_SMOKE_STRICT=0`
   可临时放宽)。另:源码树里混有历史误提交的 .o 文件,
   build() 先 make clean 兜底。

### 7.3 已验证(2026-10-04)

- aarch64 全链:`make dist-repo ARCH=aarch64` 产出 5 包 + 仓库 db;
  `make dist-image ARCH=aarch64` 产出 rootfs(787 MB)/zstd(317 MB)/
  OCI(331 MB,blob 摘要自校验通过);
- e2e:dist 产出镜像导入 docker 后,e2eclient 全链 PASS(握手 / 系统信息
  Arch Linux arm64 / 任务 / 快照树 / 回退 / fork;窗口流视 mixcomp 运行时
  状态,compositor=false 因上述已知段错误);
- pacman -Syu 单包升级:第二台容器以镜像为模板、挂 dist/out/repo-aarch64,
  mixcomp 0.1.0-1 → 0.1.0-2,docker restart 后 e2eclient PASS。同源重打
  的二进制 sha256 相同(确定性构建的旁证),版本元数据前进;
- x86_64 全链:`make dist-repo ARCH=x86_64`(qemu 模拟,binfmt 已注册)
  产出 5 包(含 Arch 侧 -debug 包)+ 仓库 db;`make dist-image` 产出
  rootfs(761 MB)/zstd(346 MB)/OCI(359 MB);e2eclient 对该镜像
  同样全链 PASS;
- 双架构:`make dist-image ARCHS="aarch64 x86_64"` 额外产出
  `mixumos-0.1.0-multi.oci.tar`(OCI image index,arm64+amd64 双 manifest);
- 幂等:重复 make dist-repo / dist-image 只刷新产物;wlroots0.18 已在
  仓库时跳过重建(删其包文件可强制)。

### 7.3.1 Docker Desktop/WSL2 通路重打(2026-10-17)

构建宿主从 colima VM 迁到 Windows Docker Desktop 后全链重打实测:
- x86_64(原生)与 aarch64(qemu-user)五包 + 双仓库 + rootfs/OCI 齐产;
  双架构镜像导入后启动、READY、TLS 握手、sys.info(真实快照后端 +
  compositor 在线)验收 PASS;`mixumos-0.1.0-multi.oci.tar` 双 manifest;
- mixum-agentd 的容器内 cargo 构建(PKGBUILD.cargo 转正后)首次实跑,
  暴露并修复 makepkg CFLAGS 覆盖 aws-lc-sys jitterentropy `-O0` 的问题
  (PKGBUILD 内 unset 注入 flags,见 7.4);
- 宿主工具链适配与踩坑见 7.4(git-bash 路径转换、pacman-key 无条件
  init、ALARM 镜像 MIXUM_ARM_MIRROR);提速演进项:cargo 缓存卷
  (现每次 makepkg 全量重建)、跨编译选项(通路验证保持 qemu 原生)。

### 7.4 环境要点(踩坑记录)

- 构建容器需要 `--privileged`:pacstrap 要对 target 挂 /proc、/sys;
- pacman 沙箱:6.x 关 `DisableSandbox`;7.1 起拆成
  `DisableSandboxFilesystem` + `DisableSandboxSyscalls` 两个细粒度开关,
  qemu 模拟下两个都要关(引导用 conf;烤进镜像的 conf 保持默认);
- ALARM rootfs 的 keyring 未初始化,首次需 `pacman-key --init` +
  `--populate archlinuxarm`;qemu 下 gpg 建钥偶发瞬时失败,脚本带重试;
- ALARM 的 makepkg 默认 PKGEXT=.pkg.tar.xz(Arch x86_64 是 .zst),
  工具链 glob 均按两种后缀处理;Arch 侧 makepkg 默认产出 -debug 包;
- 构建容器持久复用(mixum-build-<arch>),pacman 缓存热;
- docker context(原开发环境为 colima-mixumos),宿主只需 docker CLI。
