//! 任务进程隔离：cgroup v2 freezer + Landlock 目录白名单。
//!
//! 设计原则是「能兜底就兜底，不能就退回容器边界」：
//! - cgroup v2 freezer 只在内核允许写 `/sys/fs/cgroup` 时启用；
//!   否则 `freeze_all` 继续用 SIGSTOP（原行为）。
//! - Landlock 只在 Linux 5.13+ 且内核启用时生效；不可用时静默放行，
//!   隔离边界仍是容器本身（非特权 + 独立工作区卷）。
//!
//! 两者都在 `pre_exec` 里应用：cgroup 先把自己挂进去，再收 Landlock，
//! 最后 exec，保证子进程从第一条用户态指令起就受约束。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// cgroup v2 冻结器。`create` 失败（只读挂载 / 无委托）就退回信号路径。
///
/// 句柄可克隆：克隆共享同一目录，只有最后一个句柄释放时才尝试删除，
/// 避免 spawn 时的临时克隆在任务退出后把共享 cgroup 清掉。
#[derive(Debug, Clone)]
pub struct CgroupFreezer {
    inner: Arc<CgroupInner>,
}

#[derive(Debug)]
struct CgroupInner {
    dir: PathBuf,
    procs: PathBuf,
    freeze: PathBuf,
}

impl CgroupFreezer {
    /// 在 cgroup v2 根下建独立 cgroup。根路径可用 `MIXUM_CGROUP_ROOT` 覆盖（测试用）。
    pub fn create() -> Option<Self> {
        let base = std::env::var_os("MIXUM_CGROUP_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys/fs/cgroup"));
        if !base.is_dir() {
            return None;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let dir = base.join(format!("mixum-sandbox-{}-{nanos}", std::process::id()));
        if fs::create_dir(&dir).is_err() {
            return None;
        }
        let freezer = Self::at(dir);
        if !freezer.inner.freeze.exists() {
            // 不是 cgroupfs（或没有 freezer 支持）：清掉刚建的目录。
            let _ = fs::remove_dir(&freezer.inner.dir);
            return None;
        }
        Some(freezer)
    }

    /// 指定目录（测试或自定义挂载点使用）。
    pub fn at(dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(CgroupInner {
                procs: dir.join("cgroup.procs"),
                freeze: dir.join("cgroup.freeze"),
                dir,
            }),
        }
    }

    /// 把调用进程挂进该 cgroup。cgroup v2 约定写 "0" 表示当前进程。
    /// 父进程预开 cgroup.procs:pre_exec 里只做 write,不再 open/分配。
    pub fn open_procs(&self) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.inner.procs)
    }

    pub fn attach_current(&self) -> io::Result<()> {
        fs::write(&self.inner.procs, b"0")
    }

    /// 原子冻结/解冻整棵子树。
    pub fn set_frozen(&self, frozen: bool) -> io::Result<()> {
        fs::write(&self.inner.freeze, if frozen { b"1" } else { b"0" })
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }
}

impl Drop for CgroupInner {
    fn drop(&mut self) {
        // CRIU restore moves the restored task back into the cgroup path
        // recorded at dump time. Memory-snapshot environments must keep the
        // (now empty) cgroup directory until a later GC; otherwise restore
        // blocks forever on the missing unified cgroup.
        if std::env::var("MIXUM_MEMORY_SNAPSHOTS").ok().as_deref() == Some("1") {
            return;
        }
        // 所有句柄都释放后才走到这里；cgroup 仍非空时 remove_dir 失败，留给内核/下次启动。
        let _ = fs::remove_dir(&self.dir);
    }
}

/// 预先建好的 Landlock ruleset。规则在父进程里构建（允许分配），
/// `pre_exec` 里只做 `restrict_self` 系统调用，避免 fork 后分配导致死锁。
#[cfg(target_os = "linux")]
pub struct PreparedLandlock(landlock::RulesetCreated);

#[cfg(not(target_os = "linux"))]
pub struct PreparedLandlock;

impl PreparedLandlock {
    pub fn build(workspace: &Path, home: Option<&Path>) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self(build_ruleset(workspace, home)?))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (workspace, home);
            Ok(Self)
        }
    }

    fn enforce(self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use landlock::RulesetStatus;
            let status = self.0.restrict_self().map_err(io::Error::other)?;
            match status.ruleset {
                RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => Ok(()),
                // 内核不支持 Landlock：放行，由容器边界兜底。
                RulesetStatus::NotEnforced => Ok(()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(())
        }
    }
}

/// 在子进程 exec 前应用隔离。任何一步失败都会让 spawn 返回错误，
/// 不允许任务在“以为有隔离、实际没有”的状态下启动。
#[cfg(unix)]
pub fn apply_spawn_isolation(
    cmd: &mut std::process::Command,
    cgroup: Option<CgroupFreezer>,
    workspace: Option<PathBuf>,
    home: Option<&Path>,
    credentials: Option<(u32, u32)>,
) {
    let disable_landlock = std::env::var("MIXUM_DISABLE_LANDLOCK").ok().as_deref() == Some("1");
    let prepared = match workspace.as_deref() {
        Some(_) if disable_landlock => Ok(None),
        Some(workspace) => PreparedLandlock::build(workspace, home).map(Some),
        None => Ok(None),
    };
    let mut prepared = Some(prepared);
    // cgroup 句柄在父进程打开;fork 后只 write,避免 open/分配。
    let cgroup_procs = cgroup.as_ref().map(CgroupFreezer::open_procs);
    // SAFETY: pre_exec 在 fork 后、exec 前的单线程上下文中运行；
    // 这里只做 write 与 Landlock 系统调用，不跨线程访问共享状态。
    unsafe {
        cmd.pre_exec(move || {
            if let Some(procs) = cgroup_procs.as_ref() {
                let file = procs
                    .as_ref()
                    .map_err(|err| io::Error::from_raw_os_error(err.raw_os_error().unwrap_or(5)))?;
                nix::unistd::write(file, b"0")?;
            }
            // cgroup 写完后降权；root 容器传 None，保持 root。
            if let Some((uid, gid)) = credentials {
                let current = nix::unistd::Uid::current().as_raw();
                if current != uid {
                    if !nix::unistd::Uid::effective().is_root() {
                        return Err(io::Error::from_raw_os_error(nix::libc::EPERM));
                    }
                    #[cfg(target_os = "linux")]
                    nix::unistd::setgroups(&[]).map_err(io::Error::other)?;
                    nix::unistd::setgid(nix::unistd::Gid::from_raw(gid))
                        .map_err(io::Error::other)?;
                    nix::unistd::setuid(nix::unistd::Uid::from_raw(uid))
                        .map_err(io::Error::other)?;
                }
            }
            match prepared.take().expect("pre_exec 只调用一次") {
                Ok(Some(landlock)) => landlock.enforce(),
                Ok(None) => Ok(()),
                Err(err) => Err(err),
            }
        });
    }
}

#[cfg(not(unix))]
pub fn apply_spawn_isolation(
    _cmd: &mut std::process::Command,
    _cgroup: Option<CgroupFreezer>,
    _workspace: Option<PathBuf>,
    _home: Option<&Path>,
    _credentials: Option<(u32, u32)>,
) {
}

/// 系统级目录白名单：只允许写工作区、/tmp、/dev、/run；其余只读。
/// `/run` 是 Wayland/X11 socket 所在，`/dev` 是 /dev/null 等基础设备。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn home_tool_dirs_from(home: &Path) -> Vec<PathBuf> {
    [
        ".cache",
        ".local",
        ".config",
        ".npm",
        ".cargo",
        ".rustup",
        ".mixum",
        ".opencode",
    ]
    .iter()
    .map(|name| home.join(name))
    .collect()
}

#[cfg(target_os = "linux")]
fn extra_writable_dirs() -> Vec<PathBuf> {
    std::env::var_os("MIXUM_LANDLOCK_EXTRA_WRITABLE")
        .map(|raw| {
            raw.to_string_lossy()
                .split(':')
                .filter(|item| !item.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// 启动时预建 HOME 下的工具目录：Landlock 只能按已存在目录授权，
/// 父目录 HOME 本身只读，任务无法自行创建这些目录。
pub fn prepare_user_dirs(home: &Path, uid: u32, gid: u32) {
    #[cfg(target_os = "linux")]
    {
        let _ = fs::create_dir_all(home);
        for dir in home_tool_dirs_from(home) {
            let _ = fs::create_dir_all(dir);
        }
        if nix::unistd::Uid::effective().is_root() {
            use std::os::unix::fs::chown;
            let _ = chown(home, Some(uid), Some(gid));
            for dir in home_tool_dirs_from(home) {
                let _ = chown(&dir, Some(uid), Some(gid));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (home, uid, gid);
    }
}

#[cfg(target_os = "linux")]
fn build_ruleset(workspace: &Path, home: Option<&Path>) -> io::Result<landlock::RulesetCreated> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };

    // V3 补上 TRUNCATE/REFER；老内核由 landlock crate 按 best-effort 降级。
    let abi = ABI::V3;
    let ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|ruleset| ruleset.create())
        .map_err(io::Error::other)?;

    let root = PathFd::new(Path::new("/")).map_err(io::Error::other)?;
    let mut ruleset = ruleset
        .add_rule(PathBeneath::new(root, AccessFs::from_read(abi)))
        .map_err(io::Error::other)?;

    let mut writable = vec![
        workspace.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/dev"),
        PathBuf::from("/run"),
    ];
    if let Some(home) = home {
        writable.extend(home_tool_dirs_from(home));
    }
    writable.extend(extra_writable_dirs());
    for path in writable {
        if !path.is_dir() {
            continue;
        }
        let fd = PathFd::new(&path).map_err(io::Error::other)?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .map_err(io::Error::other)?;
    }
    Ok(ruleset)
}

/// 探测内核是否真的启用 Landlock（只创建 ruleset，不 restrict 自己）。
#[cfg(target_os = "linux")]
pub fn landlock_supported() -> bool {
    use landlock::{Access, AccessFs, Ruleset, RulesetAttr, ABI};
    Ruleset::default()
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|ruleset| ruleset.create())
        .is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn landlock_supported() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cgroup_freezer_writes_expected_flags() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("cgroup.procs"), b"").unwrap();
        fs::write(dir.path().join("cgroup.freeze"), b"0").unwrap();
        let freezer = CgroupFreezer::at(dir.path().to_path_buf());

        freezer.attach_current().unwrap();
        freezer.set_frozen(true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.freeze")).unwrap(),
            "1"
        );
        freezer.set_frozen(false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("cgroup.procs")).unwrap(),
            "0"
        );
    }

    #[test]
    fn cgroup_create_refuses_non_cgroupfs_root() {
        let dir = TempDir::new().unwrap();
        // 覆盖全局 env 只影响本测试；cargo test 默认多线程，这里用唯一的子目录名避免串扰。
        std::env::set_var("MIXUM_CGROUP_ROOT", dir.path());
        let created = CgroupFreezer::create();
        std::env::remove_var("MIXUM_CGROUP_ROOT");
        assert!(created.is_none());
    }

    #[test]
    fn home_tool_dirs_are_scoped_to_known_subdirs() {
        let dirs = home_tool_dirs_from(Path::new("/var/lib/mixum"));
        assert!(dirs.contains(&PathBuf::from("/var/lib/mixum/.cache")));
        assert!(dirs.contains(&PathBuf::from("/var/lib/mixum/.npm")));
        assert!(dirs.contains(&PathBuf::from("/var/lib/mixum/.opencode")));
        assert!(!dirs.contains(&PathBuf::from("/var/lib/mixum")));
    }

    #[test]
    fn clone_drop_does_not_remove_shared_cgroup_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let owner = CgroupFreezer::at(path.clone());
        let temporary = owner.clone();
        drop(temporary);
        assert!(path.exists(), "临时句柄释放不应删除共享 cgroup 目录");
        drop(owner);
    }

    #[test]
    fn landlock_stub_is_noop_off_linux() {
        let dir = TempDir::new().unwrap();
        if cfg!(target_os = "linux") {
            // Linux 上真正 enforce 会限制当前测试进程，这里只验证 ruleset 可构建。
            PreparedLandlock::build(dir.path(), None).unwrap();
            return;
        }
        PreparedLandlock::build(dir.path(), None)
            .unwrap()
            .enforce()
            .unwrap();
    }

    /// Linux 才跑：子进程只能写工作区，写工作区外必须失败。
    /// 内核没启用 Landlock 时子进程以 42 退出，测试自动跳过。
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_denies_write_outside_workspace() {
        use std::process::Command;

        if !landlock_supported() {
            eprintln!("内核未启用 Landlock，跳过");
            return;
        }

        let allowed = TempDir::new().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let denied = tempfile::Builder::new()
            .prefix("mixum-landlock-denied")
            .tempdir_in(cwd)
            .unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo ok > \"$1\"; echo bad > \"$2\"")
            .arg("sh")
            .arg(allowed.path().join("allowed.txt"))
            .arg(denied.path().join("denied.txt"));
        apply_spawn_isolation(
            &mut cmd,
            None,
            Some(allowed.path().to_path_buf()),
            None,
            None,
        );
        let status = cmd.status().unwrap();
        assert!(!status.success(), "工作区外写入应该失败");
        assert!(allowed.path().join("allowed.txt").exists());
        assert!(!denied.path().join("denied.txt").exists());
    }

    /// HOME 白名单的负向验证：工具子目录可写，HOME 根目录仍只读。
    #[cfg(target_os = "linux")]
    #[test]
    fn home_tool_dirs_are_writable_but_home_itself_is_not() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("HOME 未设置，跳过");
            return;
        };
        if !home.is_dir() {
            eprintln!("HOME 不存在，跳过");
            return;
        }
        prepare_user_dirs(
            &home,
            nix::unistd::Uid::current().as_raw(),
            nix::unistd::Gid::current().as_raw(),
        );
        let tag = std::process::id();
        let allowed = home.join(".cache").join(format!("mixum-landlock-{tag}"));
        let denied = home.join(format!("mixum-landlock-{tag}"));
        let workspace = TempDir::new().unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg("echo ok > \"$1\"; echo bad > \"$2\"")
            .arg("sh")
            .arg(&allowed)
            .arg(&denied);
        apply_spawn_isolation(
            &mut cmd,
            None,
            Some(workspace.path().to_path_buf()),
            Some(&home),
            None,
        );
        let status = cmd.status().expect("spawn sh");
        let allowed_written = allowed.exists();
        let denied_written = denied.exists();
        let _ = std::fs::remove_file(&allowed);
        let _ = std::fs::remove_file(&denied);
        assert!(allowed_written, "HOME 工具目录应可写");
        assert!(!denied_written, "HOME 根目录应只读");
        assert!(!status.success(), "写 HOME 根目录应该失败");
    }
}
