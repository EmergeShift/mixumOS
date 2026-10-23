use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CriuError {
    #[error("CRIU is unavailable")]
    Unavailable,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("CRIU command failed: {0}")]
    Command(String),
}

#[derive(Debug, Clone)]
pub struct Criu {
    binary: PathBuf,
}

impl Criu {
    /// 指定二进制路径（测试或自定义安装）。
    pub fn from_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn probe() -> Option<Self> {
        let override_path = std::env::var("MIXUM_CRIU_BIN").ok();
        let candidates = override_path.iter().map(String::as_str).chain([
            "criu",
            "/usr/sbin/criu",
            "/usr/bin/criu",
        ]);
        for candidate in candidates {
            if Command::new(candidate)
                .arg("--version")
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
            {
                return Some(Self {
                    binary: candidate.into(),
                });
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    pub fn probe() -> Option<Self> {
        None
    }

    #[cfg(target_os = "linux")]
    pub fn available(&self) -> bool {
        true
    }
    #[cfg(not(target_os = "linux"))]
    pub fn available(&self) -> bool {
        let _ = self;
        false
    }
    pub fn dump(
        &self,
        pid: i32,
        dir: impl AsRef<Path>,
        leave_running: bool,
    ) -> Result<(), CriuError> {
        self.dump_roots(&[pid], dir, leave_running)
    }

    /// 多根 dump：GUI 场景要同时包含任务树与合成器（Wayland 连接两端）。
    pub fn dump_roots(
        &self,
        pids: &[i32],
        dir: impl AsRef<Path>,
        leave_running: bool,
    ) -> Result<(), CriuError> {
        let pids: Vec<i32> = pids.iter().copied().filter(|pid| *pid > 0).collect();
        if pids.is_empty() {
            return Err(CriuError::Unavailable);
        }
        std::fs::create_dir_all(dir.as_ref())?;
        let mut cmd = Command::new(&self.binary);
        // --shell-job:任务进程用进程组托管而非会话首,转储必须带此参数。
        cmd.args(["dump"]);
        for pid in &pids {
            cmd.args(["--tree", &pid.to_string()]);
        }
        cmd.arg("--images-dir").arg(dir.as_ref()).args([
            "--shell-job",
            // 幽灵文件上限放大以容纳 wlroots shm。
            "--file-locks",
            "--ghost-limit",
            "2147483648",
            "-v4",
            "-o",
            "dump.log",
        ]);
        // Wayland 需要把任务与 compositor 两端的外部 unix socket 都放行。
        // 单任务 dump 带上该标志会在当前内核的 parasite 阶段卡住。
        if pids.len() > 1 {
            cmd.arg("--ext-unix-sk");
        }
        if leave_running {
            cmd.arg("--leave-running");
        }
        run(cmd)
    }
    pub fn restore(&self, dir: impl AsRef<Path>) -> Result<(), CriuError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(["restore", "--images-dir"])
            .arg(dir.as_ref())
            .args(["-v4", "-o", "restore.log", "--shell-job", "--ext-unix-sk"]);
        run(cmd)
    }
}

fn run(mut cmd: Command) -> Result<(), CriuError> {
    let mut child = cmd.spawn()?;
    let timeout = std::env::var("MIXUM_CRIU_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(120);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(CriuError::Command(format!("criu exited with {status}")))
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CriuError::Command(format!(
                "criu exceeded {timeout}s (MIXUM_CRIU_TIMEOUT_SECS)"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub fn env_fingerprint() -> String {
    format!("{}/{}", std::env::consts::ARCH, kernel_release())
}
pub fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().into())
        .unwrap_or_else(|_| "unknown".into())
}
