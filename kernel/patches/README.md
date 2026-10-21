# 内核补丁策略

默认 **0 个补丁**。优先用 config 解决；config 解决不了的，才进这个目录。

## 准入条件

一个补丁要进 `patches/`，必须同时满足：

1. 有明确的、config 无法实现的能力（例如 virtio-gpu 的 checkpoint 状态）；
2. 有上游链接：已提交 upstream、或 issue/RFC 链接；
3. 有“去掉补丁会怎样”的回归测试或明确失败模式；
4. 补丁文件可 `git apply` 到锁定的内核版本，且在 arm64/x86_64 都编译通过。

## 命名

```
patches/<kernel-version>/<NNNN>-<short-name>.patch
```

## 当前

无。`linux-mixum-micro` 与 `linux-mixum` 目前只靠 config fragment 实现差异。
