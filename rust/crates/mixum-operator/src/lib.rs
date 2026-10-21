//! mixum-operator —— 云端编排器:监听 MixumTask,为每条指令建一次性
//! mixumOS 任务容器(类 Manus 的云端 runner 形态)。
//!
//! 调谐逻辑刻意保持最小:
//! - MixumTask 无 pod → 建任务 Pod(镜像/架构/快照卷来自 spec);
//! - Pod 运行中 → status: running;
//! - Pod 结束 → status: completed/failed,Pod 由 ttlSecondsAfterFinished 回收;
//! - MixumTask 删除 → Pod 经所有者引用自动回收,无需显式处理。
//!
//! 快照接力:多个 MixumTask 指向同一 snapshotPvc 时,任务间可经
//! snap.push/pull 传递快照(docs/protocol.md §6),实现「云端继续跑」。

pub mod reconciler;

pub use reconciler::{reconcile, Context, Error};
