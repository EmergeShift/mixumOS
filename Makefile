# mixumOS 发行版构建入口(设计见 docs/distro.md,落地记录见同文档「工具链落地记录」)。
#
# 常用目标(全部幂等):
#   make dist          包 → 仓库 → 镜像 一条龙(默认 aarch64)
#   make dist-repo     只出 pacman 包 + 仓库 db
#   make dist-image    只组装 rootfs + OCI 镜像(需已有仓库)
#   make dist-e2e      导入镜像并跑 e2e 容器(任务/快照链路验收)
#   make dist-clean    清理 dist/out/
#
# 构建位置:mixumos VM(colima)的 Arch 容器内;宿主机只需 docker CLI。
# 变量:ARCH=aarch64|x86_64|"aarch64 x86_64",MIXUM_PROXY 覆盖容器代理。

DOCKER_CONTEXT ?= colima-mixumos
export DOCKER_CONTEXT

ARCH ?= aarch64

.PHONY: dist dist-repo dist-image dist-e2e dist-clean

dist: dist-repo dist-image

dist-repo:
	ARCHS="$(ARCH)" bash dist/repo/build-repo.sh

dist-image:
	ARCHS="$(ARCH)" bash dist/os/build-image.sh

dist-e2e:
	bash scripts/dist-e2e.sh $(firstword $(ARCH))

dist-clean:
	rm -rf dist/out
