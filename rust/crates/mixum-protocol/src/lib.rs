//! mixum-protocol —— mixumOS wire 协议 v1。
//!
//! 规格:[docs/protocol.md];跨实现行为锁:tests/cross-protocol 回归向量。
//! 本 crate 是 Rust 侧唯一协议事实来源:envelope、op 常量、能力令牌、
//! 二进制帧头。任何不兼容变更必须递增 [`PROTOCOL_VERSION`] 并更新文档。

pub mod envelope;
pub mod gui;
pub mod frames;
pub mod ops;
pub mod token;

pub use gui::GuiSession;

/// 协议主版本。不兼容变更必须递增(见 docs/protocol.md §7)。
pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_reference() {
        // Go 参考实现(已移除)的 ProtoVersion = 1
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
