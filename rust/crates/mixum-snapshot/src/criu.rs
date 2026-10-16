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
    #[cfg(target_os = "linux")]
    pub fn probe() -> Option<Self> {
        for candidate in ["criu", "/usr/sbin/criu", "/usr/bin/criu"] {
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
        std::fs::create_dir_all(dir.as_ref())?;
        let mut cmd = Command::new(&self.binary);
        // --shell-job:任务进程用进程组托管而非会话首,转储必须带此参数。
        cmd.args(["dump", "--tree", &pid.to_string(), "--images-dir"])
            .arg(dir.as_ref())
            .args(["--shell-job", "-v4", "-o", "dump.log"]);
        if leave_running {
            cmd.arg("--leave-running");
        }
        run(cmd)
    }
    pub fn restore(&self, dir: impl AsRef<Path>) -> Result<(), CriuError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(["restore", "--images-dir"])
            .arg(dir.as_ref())
            .args(["-v4", "-o", "restore.log", "--shell-job"]);
        run(cmd)
    }
}

fn run(mut cmd: Command) -> Result<(), CriuError> {
    let output = cmd.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CriuError::Command(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
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
