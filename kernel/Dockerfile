# 内核构建环境（Debian bookworm）。
# 同时安装 arm64 / x86_64 交叉工具链，保证在 Apple Silicon 与 x86 宿主上都能构建。
FROM debian:bookworm

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential bc bison flex libssl-dev libelf-dev cpio kmod rsync \
    xz-utils zstd wget curl ca-certificates python3 perl dwarves \
    gcc-aarch64-linux-gnu gcc-x86-64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
