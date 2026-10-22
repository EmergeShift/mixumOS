//! 电力事件与主机配置。

use crate::transport::{Client, ConnectionOptions, Error};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// WillSleep 事件:notification_id 必须在迁移结束后经 IOAllowPowerChange 归还,
/// 系统在等待确认期间保持唤醒——合盖迁移正发生在这一窗口内。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub struct SleepEvent {
    pub notification_id: isize,
}

/// --watch 模式下迁移时长上限;超时后仍放行睡眠,任何失败都不挂死系统。
pub const WATCH_DEADLINE: Duration = Duration::from_secs(30);

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

/// 配置目录:MIXUM_CONFIG_DIR 可重定位——systemd 钩子以 root 运行时指向
/// 用户配置、e2e 隔离临时目录,都靠它。
pub fn config_dir() -> PathBuf {
    resolve_config_dir(
        std::env::var("MIXUM_CONFIG_DIR").ok().as_deref(),
        dirs::home_dir(),
    )
}

fn resolve_config_dir(explicit: Option<&str>, home: Option<PathBuf>) -> PathBuf {
    match explicit.filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home.unwrap_or_else(|| PathBuf::from(".")).join(".mixum"),
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
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

/// 盒盖迁移:hybrid 快照(auto=true)后推送到远端。
/// deadline 非 0 时整体限时——systemd 钩子场景 suspend 会等脚本退出,必须可界。
pub async fn simulate_sleep(
    options: ConnectionOptions,
    remote: Option<String>,
    remote_token: Option<String>,
    deadline: Duration,
) -> Result<(), Error> {
    let run = async {
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
        let mut stats_note = "未配置远端,仅本地快照".to_owned();
        if let Some(remote) = remote {
            let push = client
                .call(
                    "snap.push",
                    serde_json::json!({ "node": node_id, "remote": remote, "token": remote_token.unwrap_or_else(|| client.token().to_owned()) }),
                )
                .await?;
            let stats = push.payload.unwrap_or_default();
            stats_note = format!(
                "推送 {} 块 / {} 字节",
                stats["chunksSent"], stats["bytesSent"]
            );
        }
        println!("合盖模拟完成: {node_id} ({stats_note})");
        Ok(())
    };
    if deadline.is_zero() {
        run.await
    } else {
        tokio::time::timeout(deadline, run)
            .await
            .map_err(|_| Error::Timeout("合盖迁移".into()))?
    }
}

/// 平台中立的睡眠监听循环。不变量(有单测锁定):ack 严格晚于迁移结束;
/// 迁移失败或超时也必须 ack,否则系统永远等不到放行。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub async fn sleep_watch_loop<Fut>(
    mut rx: tokio::sync::mpsc::Receiver<SleepEvent>,
    allow: impl Fn(isize) + Send + 'static,
    mut work: impl FnMut() -> Fut + Send,
    deadline: Duration,
) where
    Fut: std::future::Future<Output = Result<(), Error>> + Send,
{
    while let Some(event) = rx.recv().await {
        let outcome = tokio::time::timeout(deadline, work()).await;
        match outcome {
            Ok(Ok(())) => eprintln!("合盖迁移完成,放行睡眠"),
            Ok(Err(e)) => eprintln!("合盖迁移失败,仍放行睡眠: {e}"),
            Err(_) => eprintln!("合盖迁移超时,仍放行睡眠"),
        }
        allow(event.notification_id);
    }
}

#[cfg(target_os = "macos")]
pub async fn watch_sleep(
    options: ConnectionOptions,
    remote: Option<String>,
    remote_token: Option<String>,
    deadline: Duration,
) -> Result<(), Error> {
    let (tx, rx) = tokio::sync::mpsc::channel::<SleepEvent>(4);
    let root = macos::run(tx).map_err(|e| Error::Config(format!("IOKit 注册失败: {e}")))?;
    // IOAllowPowerChange 是 Mach 调用,任意线程可归还确认;ack 线程消费迁移
    // 完成后的放行请求,让 IOKit 回调只做投递。
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<isize>();
    std::thread::spawn(move || {
        for id in ack_rx {
            macos::allow_sleep(root, id);
        }
    });
    println!(
        "已注册 macOS 合盖监听,等待电源事件(迁移上限 {}s)",
        deadline.as_secs().max(1)
    );
    sleep_watch_loop(
        rx,
        move |id| {
            let _ = ack_tx.send(id);
        },
        move || {
            simulate_sleep(
                options.clone(),
                remote.clone(),
                remote_token.clone(),
                Duration::ZERO,
            )
        },
        deadline,
    )
    .await;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub async fn watch_sleep(
    _options: ConnectionOptions,
    _remote: Option<String>,
    _remote_token: Option<String>,
    _deadline: Duration,
) -> Result<(), Error> {
    Err(Error::Config(
        "当前平台没有 IOKit;Linux 请用 power install 安装 systemd 合盖钩子,或用 --simulate-sleep 手动触发"
            .into(),
    ))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SYSTEMD_SLEEP_PATH: &str = "/usr/lib/systemd/system-sleep/mixum-ctl";

/// 渲染 systemd system-sleep 钩子脚本(pure,便于单测)。pre 阶段同步执行迁移,
/// suspend 会等脚本退出;--deadline-secs 20 保证不无限阻塞睡眠。
fn systemd_script(config_dir: &str, bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# mixum-ctl 电力钩子(systemd sleep.target 的 system-sleep 脚本,power install 生成)。
# pre 阶段执行盒盖迁移(hybrid 快照+推送),迁移完成或超时后 suspend 才继续。
LOG=/var/log/mixum-power.log
case "$1" in
  pre)
    echo "[$(date '+%F %T')] 盒盖迁移开始" >>"$LOG"
    MIXUM_CONFIG_DIR={config_dir} {bin} power --simulate-sleep --deadline-secs 20 >>"$LOG" 2>&1 \
      || echo "[$(date '+%F %T')] 迁移失败,仍继续睡眠" >>"$LOG"
    ;;
esac
"#
    )
}

/// power --install / --remove / --print。--print 在任意平台可用;
/// 写入/移除钩子文件需要 root,仅 Linux。
pub fn systemd_manage(install: bool, remove: bool, print_only: bool) -> Result<(), Error> {
    let script = systemd_script(
        &config_dir().to_string_lossy(),
        &std::env::current_exe()?.to_string_lossy(),
    );
    if print_only {
        print!("{script}");
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        systemd_linux(install, remove, &script)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (install, remove);
        Err(Error::Config(
            "systemd 钩子仅支持 Linux;--print 可预览脚本内容".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
fn systemd_linux(install: bool, remove: bool, script: &str) -> Result<(), Error> {
    let path = std::path::Path::new(SYSTEMD_SLEEP_PATH);
    if install {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, script)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        }
        println!("已安装 {SYSTEMD_SLEEP_PATH}(需 root);卸载:mixum-ctl power --remove");
        return Ok(());
    }
    if remove {
        match std::fs::remove_file(path) {
            Ok(()) => println!("已移除 {SYSTEMD_SLEEP_PATH}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("未安装 {SYSTEMD_SLEEP_PATH}")
            }
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    Err(Error::Config("power 需指定 --install 或 --remove".into()))
}

pub const PLATFORM_NOTES: &str = "电力钩子平台差异:
- macOS:power --watch 经 IOKit IORegisterForSystemPower 注册(无需 root)。CanSystemSleep(闲置倒计时)仅放行;WillSleep 触发盒盖迁移(hybrid 快照+推送),系统保持唤醒等待迁移完成或超时(--deadline-secs,默认 30s)后放行,失败不会挂死睡眠。
- Linux:power --install 生成 systemd system-sleep 钩子(/usr/lib/systemd/system-sleep/mixum-ctl,pre 阶段同步迁移,deadline 20s,日志 /var/log/mixum-power.log);--print 预览,--remove 卸载。钩子以 root 运行,配置目录经 MIXUM_CONFIG_DIR 指向用户 ~/.mixum,安装前需 connect 配置 remote 与 token_path。
- 其他平台:--simulate-sleep 手动触发同一迁移链路。";

#[cfg(target_os = "macos")]
mod macos {
    use super::SleepEvent;
    use tokio::sync::mpsc::Sender;

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

    struct Shared {
        /// IORegisterForSystemPower 的返回值,即 IOAllowPowerChange 所需的 root_port。
        root: IoConnect,
        events: Sender<SleepEvent>,
    }

    extern "C" fn callback(
        refcon: *mut std::ffi::c_void,
        _service: IoObject,
        message_type: Natural,
        argument: *mut std::ffi::c_void,
    ) {
        let state = unsafe { &*(refcon as *const Shared) };
        let id = argument as isize;
        match message_type {
            // 闲置倒计时:不迁移,直接放行;真正入睡时还会收到 WillSleep。
            K_IO_MESSAGE_CAN_SYSTEM_SLEEP => unsafe {
                IOAllowPowerChange(state.root, id);
            },
            // 不在回调里放行:系统保持唤醒等待确认,迁移在等待窗口内完成
            // (sleep_watch_loop 的 deadline 兜底);确认经通道由 watch 侧归还。
            K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
                let _ = state.events.try_send(SleepEvent {
                    notification_id: id,
                });
            }
            _ => {}
        }
    }

    /// 独立线程注册电源通知并起 run loop;阻塞返回注册所得 root_port。
    pub fn run(events: Sender<SleepEvent>) -> Result<IoConnect, String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let state = Box::into_raw(Box::new(Shared { root: 0, events }));
            let mut source: CfSource = std::ptr::null();
            let mut notifier: IoObject = 0;
            let root = unsafe {
                IORegisterForSystemPower(state.cast(), &mut source, callback, &mut notifier)
            };
            unsafe { (*state).root = root };
            if root == 0 {
                let _ = ready_tx.send(Err("IORegisterForSystemPower 失败".to_owned()));
                return;
            }
            let _ = ready_tx.send(Ok(root));
            unsafe {
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source, K_CF_RUN_LOOP_DEFAULT_MODE);
                CFRunLoopRun();
            }
        });
        ready_rx
            .recv()
            .unwrap_or_else(|_| Err("电源监听线程意外退出".to_owned()))
    }

    pub fn allow_sleep(root: IoConnect, id: isize) {
        unsafe {
            IOAllowPowerChange(root, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_prefers_env_override() {
        assert_eq!(
            resolve_config_dir(Some("/tmp/mixum-e2e"), None),
            PathBuf::from("/tmp/mixum-e2e")
        );
        assert_eq!(
            resolve_config_dir(Some(""), Some(PathBuf::from("/home/u"))),
            PathBuf::from("/home/u/.mixum")
        );
        assert_eq!(
            resolve_config_dir(None, Some(PathBuf::from("/home/u"))),
            PathBuf::from("/home/u/.mixum")
        );
    }

    #[test]
    fn systemd_script_pins_config_dir_bin_and_deadline() {
        let script = systemd_script("/home/u/.mixum", "/usr/bin/mixum-ctl");
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("case \"$1\" in"));
        assert!(script.contains("pre)"));
        assert!(script.contains("MIXUM_CONFIG_DIR=/home/u/.mixum"));
        assert!(script.contains("/usr/bin/mixum-ctl power --simulate-sleep --deadline-secs 20"));
    }

    mod watch {
        use super::*;
        use std::sync::{Arc, Mutex};

        #[allow(clippy::type_complexity)]
        fn spawn_loop(
            rx: tokio::sync::mpsc::Receiver<SleepEvent>,
            trace: Arc<Mutex<Vec<&'static str>>>,
            acks: Arc<Mutex<Vec<isize>>>,
            succeed: bool,
            work_delay: Duration,
            deadline: Duration,
        ) -> tokio::task::JoinHandle<()> {
            let work_trace = Arc::clone(&trace);
            let ack_trace = Arc::clone(&trace);
            tokio::spawn(sleep_watch_loop(
                rx,
                move |id| {
                    acks.lock().unwrap().push(id);
                    ack_trace.lock().unwrap().push("ack");
                },
                move || {
                    let t = Arc::clone(&work_trace);
                    async move {
                        tokio::time::sleep(work_delay).await;
                        t.lock().unwrap().push("work");
                        if succeed {
                            Ok(())
                        } else {
                            Err(Error::Config("测试注入的迁移失败".into()))
                        }
                    }
                },
                deadline,
            ))
        }

        #[tokio::test]
        async fn ack_waits_for_work_completion() {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let trace = Arc::new(Mutex::new(Vec::new()));
            let acks = Arc::new(Mutex::new(Vec::new()));
            let handle = spawn_loop(
                rx,
                Arc::clone(&trace),
                Arc::clone(&acks),
                true,
                Duration::ZERO,
                Duration::from_secs(5),
            );
            tx.send(SleepEvent { notification_id: 7 }).await.unwrap();
            drop(tx);
            handle.await.unwrap();
            // 核心不变量:先迁移后放行。
            assert_eq!(&*trace.lock().unwrap(), &["work", "ack"]);
            assert_eq!(&*acks.lock().unwrap(), &[7]);
        }

        #[tokio::test]
        async fn ack_fires_even_when_work_fails_or_times_out() {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let acks = Arc::new(Mutex::new(Vec::new()));
            let handle = spawn_loop(
                rx,
                Arc::new(Mutex::new(Vec::new())),
                Arc::clone(&acks),
                false,
                Duration::ZERO,
                Duration::from_secs(5),
            );
            tx.send(SleepEvent { notification_id: 1 }).await.unwrap();
            drop(tx);
            handle.await.unwrap();
            assert_eq!(&*acks.lock().unwrap(), &[1]);

            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let acks = Arc::new(Mutex::new(Vec::new()));
            let handle = spawn_loop(
                rx,
                Arc::new(Mutex::new(Vec::new())),
                Arc::clone(&acks),
                true,
                Duration::from_millis(200),
                Duration::from_millis(20),
            );
            tx.send(SleepEvent { notification_id: 9 }).await.unwrap();
            drop(tx);
            handle.await.unwrap();
            assert_eq!(&*acks.lock().unwrap(), &[9]);
        }
    }
}
