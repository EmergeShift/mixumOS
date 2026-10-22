# map-snapshot (vendored)

来源：`EmergeShift/Mixum-Agent-Protocol` `sdk/rust/crates/map-snapshot`，
commit `3d3ec030f997641102143378ca59abbf8e6ba786`（Apache-2.0）。

vendored 的原因：mixumOS 的 CI 没有该私有仓库的凭据；路径依赖保证开源构建可复现。
唯一改动：把 `map_core::types::now_ms()` 替换为本地 `now_ms()`，去掉对 map-core 的依赖。

同步方式：从 Mixum-Agent-Protocol 重新拷贝 crate，重放上述一处改动。
