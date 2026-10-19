//! op 名、错误码与能力(scope)常量。
//! 与已移除的 Go 参考实现逐项对齐,字符串形式由 tests/cross-protocol 锁定。

/// op 名常量。服务端回复在对应 op 后追加 ".ok" / ".err"。
pub mod op {
    pub const HELLO: &str = "hello";

    pub const TASK_SUBMIT: &str = "task.submit";
    pub const TASK_LIST: &str = "task.list";
    pub const TASK_GET: &str = "task.get";
    pub const TASK_CANCEL: &str = "task.cancel";
    pub const TASK_EVENTS: &str = "task.events";
    pub const TASK_EVENT: &str = "task.event";

    pub const SNAP_CREATE: &str = "snap.create";
    pub const SNAP_TREE: &str = "snap.tree";
    pub const SNAP_RESTORE: &str = "snap.restore";
    pub const SNAP_FORK: &str = "snap.fork";
    pub const SNAP_DELETE: &str = "snap.delete";
    pub const SNAP_DIFF: &str = "snap.diff";
    pub const SNAP_PUSH: &str = "snap.push";
    pub const SNAP_PULL: &str = "snap.pull";
    pub const SNAP_GC: &str = "snap.gc";
    pub const SNAP_CHUNK: &str = "snap.chunk";

    pub const DISPLAY_LIST: &str = "display.list";
    pub const DISPLAY_ATTACH: &str = "display.attach";
    pub const DISPLAY_DETACH: &str = "display.detach";
    pub const DISPLAY_WINDOWS: &str = "display.windows";
    pub const DISPLAY_INPUT: &str = "display.input";
    pub const DISPLAY_TAKEOVER: &str = "display.takeover";

    pub const SYS_INFO: &str = "sys.info";
    pub const SYS_PING: &str = "sys.ping";
}

/// 稳定错误码,客户端据此做程序化处理。字符串形式与 Go 实现一致(snake_case)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    ReplayDetected,
    Busy,
    Internal,
    Unsupported,
    RateLimited,
    SnapshotInvalid,
}

/// 能力(scope)。令牌只授最小集合;admin 覆盖全部。
/// 字符串形式必须与 Go 参考实现(已移除)完全一致(点号分隔)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Cap {
    #[serde(rename = "task.run")]
    TaskRun,
    #[serde(rename = "task.observe")]
    TaskObserve,
    #[serde(rename = "snap.manage")]
    SnapManage,
    #[serde(rename = "snap.sync")]
    SnapSync,
    #[serde(rename = "display.view")]
    DisplayView,
    #[serde(rename = "display.input")]
    DisplayInput,
    #[serde(rename = "admin")]
    Admin,
}

impl Cap {
    /// 拥有 have 是否满足需要 need(admin 覆盖一切)。
    pub fn covers(have: &[Cap], need: Cap) -> bool {
        have.iter().any(|c| *c == need || *c == Cap::Admin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_wire_names_match_reference() {
        // Go 参考实现的字符串形式(点号分隔)
        assert_eq!(
            serde_json::to_string(&Cap::TaskRun).unwrap(),
            "\"task.run\""
        );
        assert_eq!(
            serde_json::to_string(&Cap::DisplayInput).unwrap(),
            "\"display.input\""
        );
        assert_eq!(serde_json::to_string(&Cap::Admin).unwrap(), "\"admin\"");
    }

    #[test]
    fn admin_covers_all() {
        assert!(Cap::covers(&[Cap::Admin], Cap::SnapSync));
        assert!(Cap::covers(&[Cap::TaskRun], Cap::TaskRun));
        assert!(!Cap::covers(&[Cap::TaskRun], Cap::SnapManage));
    }
}
