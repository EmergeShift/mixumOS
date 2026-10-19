//! mixum-agentd：容器内守护进程的 Rust 实现。
//!
//! 线上 JSON 和二进制格式全部来自 [`mixum_protocol`]；本 crate 只负责会话、
//! 任务、显示 broker 和快照宿主的生命周期编排。

pub mod auth;
pub mod display;
pub mod memory;
pub mod server;
pub mod snapshot;
pub mod task;
pub mod wire;

/// 跨节点快照同步(协议 §6),依赖真实快照引擎。
#[cfg(feature = "real-snapshot")]
pub mod sync;

pub use server::{Config, Server};
