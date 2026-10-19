//! op 名、错误码与能力(scope)常量。
//! 与已移除的 Go 参考实现逐项对齐,字符串形式由 tests/cross-protocol 锁定。

/// op 名常量。服务端回复在对应 op 后追加 ".ok" / ".err"。
pub mod op {
    /// MAP 规范名(session.md §Handshake)。
    pub const SESSION_HELLO: &str = "session.hello";
    /// 过渡期别名:旧客户端仍发 `hello`,应答按请求 op 回 `hello.ok`。
    pub const HELLO: &str = "hello";
    pub const SESSION_RESUME: &str = "session.resume";
    pub const SESSION_PING: &str = "session.ping";

    pub const TASK_SUBMIT: &str = "task.submit";
    pub const TASK_LIST: &str = "task.list";
    pub const TASK_GET: &str = "task.get";
    pub const TASK_CANCEL: &str = "task.cancel";
    pub const TASK_EVENTS: &str = "task.events";
    pub const TASK_EVENT: &str = "task.event";
    pub const TASK_REPLY: &str = "task.reply";

    pub const MEM_PROVIDERS: &str = "mem.providers";
    pub const MEM_SEARCH: &str = "mem.search";
    pub const MEM_GET: &str = "mem.get";
    pub const MEM_PUT: &str = "mem.put";
    pub const MEM_DELETE: &str = "mem.delete";
    pub const MEM_LIST: &str = "mem.list";
    pub const MEM_IMPORT: &str = "mem.import";
    pub const MEM_EXPORT: &str = "mem.export";

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
    AlreadyExists,
    ReplayDetected,
    Busy,
    Internal,
    Unsupported,
    RateLimited,
    Timeout,
    Unavailable,
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
    /// MAP-Memory 读(providers/search/get/list/export)。
    #[serde(rename = "mem.read")]
    MemRead,
    /// MAP-Memory 写(put/delete/import)。
    #[serde(rename = "mem.write")]
    MemWrite,
    /// MAP-VM 管理(保留;节点尚未实现 vm.* 时按 unsupported 回落)。
    #[serde(rename = "vm.manage")]
    VmManage,
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
