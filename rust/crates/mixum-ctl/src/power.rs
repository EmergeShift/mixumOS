//! 电力事件与主机配置。

use crate::transport::{Client, ConnectionOptions, Error};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub token_path: Option<String>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub policy: Policy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default = "default_true")]
    pub hybrid: bool,
    #[serde(default)]
    pub push_on_sleep: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            hybrid: true,
            push_on_sleep: false,
        }
    }
}

fn default_true() -> bool {
    true
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mixum/config.json")
}

pub fn load() -> Result<Config, Error> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn save(config: &Config) -> Result<(), Error> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub async fn simulate_sleep(
    options: ConnectionOptions,
    remote: Option<String>,
    remote_token: Option<String>,
) -> Result<(), Error> {
    let client = Client::connect(options).await?;
    let node = client
        .call(
            "snap.create",
            serde_json::json!({ "kind": "hybrid", "label": "合盖前迁移", "auto": true }),
        )
        .await?;
    let node_id = node.payload.unwrap_or_default()["id"]
        .as_str()
        .ok_or_else(|| Error::Protocol("snap.create 未返回节点 id".into()))?
        .to_owned();
    if let Some(remote) = remote {
        client
            .call(
                "snap.push",
                serde_json::json!({ "node": node_id, "remote": remote, "token": remote_token.unwrap_or_else(|| client.token().to_owned()) }),
            )
            .await?;
    }
    println!("合盖模拟完成: {node_id}");
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn watch_sleep(
    options: ConnectionOptions,
    remote: Option<String>,
    remote_token: Option<String>,
) -> Result<(), Error> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    std::thread::spawn(move || macos::run(tx));
    println!("已注册 macOS 合盖监听，等待电源事件");
    while let Some(event) = rx.recv().await {
        if event == macos::SleepEvent::WillSleep {
            simulate_sleep(options.clone(), remote.clone(), remote_token.clone()).await?;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub async fn watch_sleep(
    _options: ConnectionOptions,
    _remote: Option<String>,
    _remote_token: Option<String>,
) -> Result<(), Error> {
    Err(Error::Config(
        "当前平台没有 IOKit；Linux 请使用 systemd sleep.target 钩子，或使用 --simulate-sleep"
            .into(),
    ))
}

#[cfg(target_os = "macos")]
mod macos {
    use tokio::sync::mpsc::Sender;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SleepEvent {
        WillSleep,
    }

    type IoObject = u32;
    type IoConnect = u32;
    type KernReturn = i32;
    type Natural = u32;
    type CfSource = *const std::ffi::c_void;
    type CfString = *const std::ffi::c_void;

    const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: Natural = 0x0000_0005;
    const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: Natural = 0x0000_0006;
    const K_CF_RUN_LOOP_DEFAULT_MODE: CfString = std::ptr::null();

    type InterestCallback = extern "C" fn(
        refcon: *mut std::ffi::c_void,
        service: IoObject,
        message_type: Natural,
        message_argument: *mut std::ffi::c_void,
    );

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegisterForSystemPower(
            refcon: *mut std::ffi::c_void,
            notifier: *mut CfSource,
            callback: InterestCallback,
            root_port: *mut IoObject,
        ) -> IoConnect;
        fn IOAllowPowerChange(root_port: IoConnect, notification_id: isize) -> KernReturn;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRunLoopGetCurrent() -> *const std::ffi::c_void;
        fn CFRunLoopAddSource(run_loop: *const std::ffi::c_void, source: CfSource, mode: CfString);
        fn CFRunLoopRun();
    }

    extern "C" fn callback(
        refcon: *mut std::ffi::c_void,
        _service: IoObject,
        message_type: Natural,
        argument: *mut std::ffi::c_void,
    ) {
        let state = unsafe { &*(refcon as *const (IoConnect, Sender<SleepEvent>)) };
        if matches!(
            message_type,
            K_IO_MESSAGE_CAN_SYSTEM_SLEEP | K_IO_MESSAGE_SYSTEM_WILL_SLEEP
        ) {
            let _ = state.1.try_send(SleepEvent::WillSleep);
            unsafe {
                IOAllowPowerChange(state.0, argument as isize);
            }
        }
    }

    pub fn run(sender: Sender<SleepEvent>) {
        let state = Box::new((0 as IoConnect, sender));
        let raw = Box::into_raw(state);
        let mut source: CfSource = std::ptr::null();
        let mut root: IoObject = 0;
        let connect =
            unsafe { IORegisterForSystemPower(raw.cast(), &mut source, callback, &mut root) };
        unsafe {
            (*(raw)).0 = connect;
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, K_CF_RUN_LOOP_DEFAULT_MODE);
            CFRunLoopRun();
        }
    }
}

pub const PLATFORM_NOTES: &str = "macOS 使用 IOPMConnection/IOPMSleepInterestCallback 注册合盖前通知；该回调无需 root，回调内只做短事务（hybrid 快照与推送由异步任务完成）。Linux 使用 systemd sleep.target/prepare 休眠钩子，v1 仅记录安装方式。";
