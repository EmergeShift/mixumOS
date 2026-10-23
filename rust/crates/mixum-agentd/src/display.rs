//! mixcomp 控制通道和窗口流 broker。

use crate::wire::Window;
use mixum_protocol::{frames, GuiSession};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

pub const MODE_AGENT: &str = "agent";
pub const MODE_USER: &str = "user";
pub const MODE_FREE: &str = "free";
pub const MAX_CLIPBOARD_BYTES: usize = 4 << 20;

/// JSON 字符串转义后的字节数（与 serde_json/json-c 的转义规则一致）：
/// 引号/反斜杠 2 字节，控制字符 6 字节，其余按 UTF-8 字节数。
fn json_escaped_len(text: &str) -> usize {
    text.chars()
        .map(|ch| match ch {
            '"' | '\\' => 2,
            ch if (ch as u32) < 0x20 => 6,
            ch => ch.len_utf8(),
        })
        .sum()
}

/// 下行文本必须能装进 agentd↔comp 的 16MiB 控制帧（留少量头部余量）。
pub fn clipboard_fits_control_frame(text: &str) -> bool {
    json_escaped_len(text) + 1024 <= MAX_COMP_FRAME
}
pub const CODEC_JPEG: &str = "jpeg";
pub const CODEC_H264: &str = "h264";
/// 无损 PNG + damage 补丁流（订阅者按需协商；旧客户端不受影响）。
pub const CODEC_PNG: &str = "png";

/// mixumd↔mixcomp 私有通道的记录类型(0x01 JSON 控制之外):
/// 0x02 = JPEG 视频帧,0x03 = H.264 视频帧(负载同为协议 §5 的
/// 0x01 + 27 字节头 + 编码数据,公有 WS 帧格式不区分)。
const COMP_REC_VIDEO: u8 = 0x02;
const COMP_REC_VIDEO_H264: u8 = 0x03;
/// PNG 全帧/补丁（DB 帧头 + 可选 20 字节 patch 头 + PNG）。
const COMP_REC_VIDEO_PNG: u8 = 0x04;
/// 单帧上限,超过视为控制通道 framing 损坏。
const MAX_COMP_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("窗口不存在：{0}")]
    NotFound(u32),
    #[error("窗口正由 agent 操作，请先接管")]
    AgentOwnsInput,
    #[error("接管冲突：{0}")]
    Conflict(String),
    /// 控制通道 framing/校验失败:显式错误路径(区别于「未知类型忽略」)。
    #[error("mixcomp 控制通道协议错误：{0}")]
    Protocol(String),
    #[error("控制通道：{0}")]
    Io(#[from] std::io::Error),
}

/// mixcomp 控制通道的健康计数(EmergeShift#8:未知记录类型可观测)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompStats {
    /// 未知记录类型被忽略的次数(前向兼容)。
    pub unknown_records: u64,
    /// 校验错误次数(framing/负载非法)。
    pub protocol_errors: u64,
}

pub struct FrameSubscription {
    pub id: String,
    pub frames: FrameReceiver,
    /// 订阅者协商的编码(broker 按 codec 路由帧)
    pub codec: String,
}

#[derive(Clone)]
struct FrameSender {
    queue: Arc<FrameQueue>,
}

#[derive(Clone)]
pub struct FrameReceiver {
    queue: Arc<FrameQueue>,
}

struct FrameQueue {
    items: Mutex<VecDeque<Vec<u8>>>,
    notify: Notify,
    capacity: usize,
    /// 关闭信号:订阅者被移除/窗口关闭时置位,recv 排干缓冲帧后返回 None,
    /// 令帧转发任务自然退出(否则每次 attach 泄漏一个永久等待的任务)。
    closed: std::sync::atomic::AtomicBool,
}

impl FrameQueue {
    fn pair(capacity: usize) -> (FrameSender, FrameReceiver) {
        let queue = Arc::new(Self {
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            notify: Notify::new(),
            capacity,
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        (
            FrameSender {
                queue: queue.clone(),
            },
            FrameReceiver { queue },
        )
    }
    fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_one();
    }
}

impl FrameSender {
    async fn push(&self, frame: Vec<u8>) {
        let mut items = self.queue.items.lock().await;
        if items.len() >= self.queue.capacity {
            let _ = items.pop_front();
        }
        items.push_back(frame);
        drop(items);
        self.queue.notify.notify_one();
    }
    fn close(&self) {
        self.queue.close();
    }
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(frame) = self.queue.items.lock().await.pop_front() {
                return Some(frame);
            }
            if self.queue.closed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            self.queue.notify.notified().await;
        }
    }
    pub fn try_recv(&self) -> Option<Vec<u8>> {
        self.queue.items.try_lock().ok()?.pop_front()
    }
}

struct Subscriber {
    sender: FrameSender,
    codec: String,
}

struct WindowState {
    window: Window,
    subscribers: HashMap<String, Subscriber>,
    /// 哪个 comp 连接注册了这个窗口；控制/输入/帧都按它路由。
    owner: u64,
}

#[derive(Clone)]
pub struct Broker {
    windows: Arc<Mutex<HashMap<u32, WindowState>>>,
    /// 允许多个 comp 源并存：mixcomp（A）与 B compositor 各自一个连接。
    comp: Arc<Mutex<HashMap<u64, CompConn>>>,
    comp_seq: Arc<std::sync::atomic::AtomicU64>,
    window_events: broadcast::Sender<Window>,
    /// 容器选区变化(mixcomp 上行),payload = {window, text}。
    clipboard_events: broadcast::Sender<serde_json::Value>,
    socket_path: PathBuf,
    /// 任务引擎是否活跃:活跃时新窗口归 agent 模式(用户接管需 claim)。
    tasks_active: Arc<std::sync::atomic::AtomicBool>,
    /// 窗口 id → 客户端 pid（mixcomp 在 window.open 里带上，仅用于会话元数据）。
    pids: Arc<Mutex<HashMap<u32, i32>>>,
    /// 未知记录类型 / 校验错误的观测计数。
    unknown_records: Arc<std::sync::atomic::AtomicU64>,
    protocol_errors: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone)]
struct CompConn {
    tx: mpsc::Sender<CompMessage>,
    /// comp 是否声明了 clipboard 能力（capabilities 握手）；未声明不发 set。
    clipboard: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
enum CompMessage {
    Json(serde_json::Value),
}

impl Broker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let (window_events, _) = broadcast::channel(128);
        let (clipboard_events, _) = broadcast::channel(128);
        Self {
            windows: Arc::new(Mutex::new(HashMap::new())),
            comp: Arc::new(Mutex::new(HashMap::new())),
            comp_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            window_events,
            clipboard_events,
            pids: Arc::new(Mutex::new(HashMap::new())),
            socket_path: path.into(),
            tasks_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            unknown_records: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            protocol_errors: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
    pub async fn start(&self) -> Result<(), DisplayError> {
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _ = tokio::fs::remove_file(&self.socket_path).await;
        let listener = UnixListener::bind(&self.socket_path)?;
        let this = self.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let broker = this.clone();
                tokio::spawn(async move {
                    let _ = broker.serve_comp(stream).await;
                });
            }
        });
        Ok(())
    }
    pub async fn list(&self) -> Vec<Window> {
        self.windows
            .lock()
            .await
            .values()
            .map(|w| w.window.clone())
            .collect()
    }
    pub async fn online(&self) -> bool {
        !self.comp.lock().await.is_empty()
    }

    /// 同步取窗口快照（快照 metadata 钩子用）。锁忙时返回空，下一次快照再收。
    pub fn try_windows(&self) -> Vec<Window> {
        self.windows
            .try_lock()
            .map(|all| all.values().map(|state| state.window.clone()).collect())
            .unwrap_or_default()
    }

    /// 同步取窗口 → pid 映射（仅用于会话元数据匹配）。
    pub fn try_window_pids(&self) -> HashMap<u32, i32> {
        self.pids
            .try_lock()
            .map(|pids| pids.clone())
            .unwrap_or_default()
    }
    pub fn subscribe_windows(&self) -> broadcast::Receiver<Window> {
        self.window_events.subscribe()
    }

    pub fn subscribe_clipboard(&self) -> broadcast::Receiver<serde_json::Value> {
        self.clipboard_events.subscribe()
    }

    pub async fn window_mode(&self, id: u32) -> Option<String> {
        self.windows
            .lock()
            .await
            .get(&id)
            .map(|state| state.window.mode.clone())
    }

    /// 下行剪贴板:把文本发给窗口所属 comp(多 comp 源按 owner 路由)。
    pub async fn clipboard_set(&self, id: u32, text: &str) -> Result<(), DisplayError> {
        let owner = {
            let all = self.windows.lock().await;
            all.get(&id).ok_or(DisplayError::NotFound(id))?.owner
        };
        let (tx, supported) = {
            let comps = self.comp.lock().await;
            match comps.get(&owner) {
                Some(conn) => (
                    conn.tx.clone(),
                    conn.clipboard.load(std::sync::atomic::Ordering::Relaxed),
                ),
                None => {
                    return Err(DisplayError::Protocol(format!(
                        "窗口 {id} 的 comp 连接已断开"
                    )))
                }
            }
        };
        if !supported {
            return Err(DisplayError::Protocol(
                "窗口所属 comp 未声明剪贴板能力".into(),
            ));
        }
        tx.send(CompMessage::Json(serde_json::json!({
            "type": "clipboard.set",
            "id": id,
            "text": text,
        })))
        .await
        .map_err(|_| DisplayError::Protocol("comp 控制通道已关闭".into()))?;
        Ok(())
    }
    /// 任务引擎活跃状态切换:开始→现有 free 窗口转 agent;结束→agent 窗口回 free。
    /// 用户已接管(user)的窗口不受影响。
    pub async fn set_tasks_active(&self, active: bool) {
        self.tasks_active
            .store(active, std::sync::atomic::Ordering::SeqCst);
        let mut changed = Vec::new();
        {
            let mut all = self.windows.lock().await;
            for st in all.values_mut() {
                let target = if active {
                    (st.window.mode.is_empty() || st.window.mode == MODE_FREE).then_some(MODE_AGENT)
                } else {
                    (st.window.mode == MODE_AGENT).then_some(MODE_FREE)
                };
                if let Some(m) = target {
                    st.window.mode = m.into();
                    changed.push(st.window.clone());
                }
            }
        }
        for w in changed {
            let _ = self.window_events.send(w);
        }
    }

    pub async fn open_or_update(&self, window: Window, owner: u64) {
        let mut all = self.windows.lock().await;
        let tasks_active = self.tasks_active.load(std::sync::atomic::Ordering::SeqCst);
        if let Some(entry) = all.get_mut(&window.id) {
            if entry.owner != owner {
                eprintln!(
                    "窗口 {} 已被 comp {} 占用，忽略 comp {} 的 update",
                    window.id, entry.owner, owner
                );
                return;
            }
            let mode = entry.window.mode.clone();
            let codec = entry.window.codec.clone();
            entry.window = Window {
                mode: if mode.is_empty() {
                    window.mode.clone()
                } else {
                    mode
                },
                codec: codec.or(window.codec),
                ..window
            };
            let changed = entry.window.clone();
            drop(all);
            let _ = self.window_events.send(changed);
            return;
        }
        // 任务运行中打开的窗口归 agent:用户可见,接管需 display.takeover。
        // 无任务时新窗口必须是 free——空 mode 不是合法协议值,console/视图
        // 页都按 free 展示,服务端需返回一致的值。
        let mut w = window;
        if w.mode.is_empty() {
            w.mode = if tasks_active { MODE_AGENT } else { MODE_FREE }.into();
        }
        let changed = w.clone();
        all.insert(
            w.id,
            WindowState {
                window: w,
                subscribers: HashMap::new(),
                owner,
            },
        );
        drop(all);
        let _ = self.window_events.send(changed);
    }
    pub async fn close(&self, id: u32) {
        if let Some(state) = self.windows.lock().await.remove(&id) {
            for sub in state.subscribers.values() {
                sub.sender.close();
            }
        }
        self.pids.lock().await.remove(&id);
        let _ = self.window_events.send(Window {
            id,
            app_id: String::new(),
            title: "__closed__".into(),
            width: 0,
            height: 0,
            focused: false,
            mode: MODE_FREE.into(),
            codec: None,
        });
    }
    /// 任务引擎在窗口生命周期变化时切换 agent 输入归属。
    pub async fn set_agent_mode(&self, id: u32, mode: &str) {
        let mut all = self.windows.lock().await;
        let Some(state) = all.get_mut(&id) else {
            return;
        };
        state.window.mode = mode.to_owned();
        let changed = state.window.clone();
        drop(all);
        let _ = self.window_events.send(changed);
    }
    pub async fn attach(
        &self,
        id: u32,
        subscriber_id: impl Into<String>,
        fps: u32,
        codec: &str,
        quality: u32,
    ) -> Result<(u32, u32, u32, FrameSubscription), DisplayError> {
        let codec = match codec {
            CODEC_H264 => CODEC_H264.to_owned(),
            CODEC_PNG => CODEC_PNG.to_owned(),
            _ => CODEC_JPEG.to_owned(),
        };
        let (tx, rx) = FrameQueue::pair(8);
        let subscriber_id = subscriber_id.into();
        // PR#6 遗留②:插入订阅者与读取窗口尺寸必须在同一个写锁区间内完成,
        // 否则 close() 可能落在两次查表之间,导致 stream.start 已发出却回
        // NotFound。这里不再做第二次查表(insert+lookup 单飞)。
        let (width, height, owner) = {
            let mut all = self.windows.lock().await;
            let state = all.get_mut(&id).ok_or(DisplayError::NotFound(id))?;
            state.subscribers.insert(
                subscriber_id.clone(),
                Subscriber {
                    sender: tx,
                    codec: codec.clone(),
                },
            );
            (state.window.width, state.window.height, state.owner)
        };
        let mut start = serde_json::json!({
            "type": "stream.start",
            "id": id,
            "fps": fps.clamp(1, 60),
            "codec": codec,
        });
        if quality > 0 {
            start["quality"] = serde_json::json!(quality.clamp(1, 100));
        }
        self.send_json_to(owner, start).await;
        Ok((
            width,
            height,
            fps.clamp(1, 60),
            FrameSubscription {
                id: subscriber_id,
                frames: rx,
                codec,
            },
        ))
    }
    pub async fn detach(&self, id: u32, subscriber_id: &str) {
        let (last_same_codec, owner) = {
            let mut all = self.windows.lock().await;
            let Some(state) = all.get_mut(&id) else {
                return;
            };
            let Some(removed) = state.subscribers.remove(subscriber_id) else {
                return;
            };
            removed.sender.close();
            (
                !state.subscribers.values().any(|s| s.codec == removed.codec),
                state.owner,
            )
        };
        // 同编码还有别的订阅者时不能停流,否则共享流被误停。
        if last_same_codec {
            self.send_json_to(owner, serde_json::json!({"type":"stream.stop","id":id}))
                .await;
        }
    }
    /// 连接断开时清掉该会话名下的全部窗口订阅(`{sessionId}-{window}`)。
    pub async fn detach_prefix(&self, prefix: &str) {
        let mut all = self.windows.lock().await;
        for state in all.values_mut() {
            state.subscribers.retain(|id, subscriber| {
                let keep = !id.starts_with(prefix);
                if !keep {
                    subscriber.sender.close();
                }
                keep
            });
        }
    }
    /// mixcomp 控制通道健康计数(未知记录类型/校验错误)。
    pub fn comp_stats(&self) -> CompStats {
        CompStats {
            unknown_records: self
                .unknown_records
                .load(std::sync::atomic::Ordering::Relaxed),
            protocol_errors: self
                .protocol_errors
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
    pub async fn broadcast_frame(&self, id: u32, frame: Vec<u8>, codec: &str) {
        let subscribers = self
            .windows
            .lock()
            .await
            .get(&id)
            .map(|s| {
                s.subscribers
                    .values()
                    .filter(|sub| sub.codec == codec)
                    .map(|sub| sub.sender.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for tx in subscribers {
            tx.push(frame.clone()).await;
        }
    }
    pub async fn takeover(
        &self,
        id: u32,
        action: &str,
        force: bool,
    ) -> Result<String, DisplayError> {
        let mut all = self.windows.lock().await;
        let state = all.get_mut(&id).ok_or(DisplayError::NotFound(id))?;
        let owner = state.owner;
        let mode = match action {
            "claim" => {
                if state.window.mode == MODE_AGENT && !force {
                    return Err(DisplayError::Conflict("agent 持有窗口".into()));
                }
                state.window.mode = MODE_USER.into();
                MODE_USER
            }
            "release" => {
                state.window.mode = MODE_FREE.into();
                MODE_FREE
            }
            other => return Err(DisplayError::Conflict(format!("未知接管动作：{other}"))),
        };
        drop(all);
        self.send_json_to(
            owner,
            serde_json::json!({"type":"input.mode","id":id,"mode":mode}),
        )
        .await;
        Ok(mode.into())
    }
    pub async fn inject(&self, id: u32, event: serde_json::Value) -> Result<(), DisplayError> {
        let all = self.windows.lock().await;
        let state = all.get(&id).ok_or(DisplayError::NotFound(id))?;
        if state.window.mode == MODE_AGENT {
            return Err(DisplayError::AgentOwnsInput);
        }
        let owner = state.owner;
        drop(all);
        let mut control = serde_json::Map::new();
        control.insert("type".into(), serde_json::json!("input"));
        control.insert("id".into(), serde_json::json!(id));
        control.insert(
            "kind".into(),
            event
                .get("type")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
        for key in ["x", "y", "button", "action", "key", "keysym", "mods", "text"] {
            if let Some(value) = event.get(key) {
                control.insert(key.into(), value.clone());
            }
        }
        self.send_json_to(owner, serde_json::Value::Object(control))
            .await;
        Ok(())
    }
    async fn send_json_to(&self, owner: u64, value: serde_json::Value) {
        let conn = self.comp.lock().await.get(&owner).cloned();
        if let Some(conn) = conn {
            let _ = conn.tx.send(CompMessage::Json(value)).await;
        }
    }

    /// 从同步上下文（快照 freeze 钩子）尽力通知所有 comp。
    /// 不等待、不阻塞：拿不到连接表或队列满就跳过。冻结本身仍生效，
    /// comp 最坏晚一个轮询节拍收到 pause，不会出现无限堆积。
    pub fn try_notify_comps(&self, value: serde_json::Value) {
        let Ok(comps) = self.comp.try_lock() else {
            return;
        };
        for conn in comps.values() {
            let _ = conn.tx.try_send(CompMessage::Json(value.clone()));
        }
    }
    async fn serve_comp(&self, stream: UnixStream) -> Result<(), DisplayError> {
        let (mut read, mut write) = stream.into_split();
        let id = self
            .comp_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, mut rx) = mpsc::channel(64);
        self.comp.lock().await.insert(
            id,
            CompConn {
                tx,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let write_task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let (kind, payload) = match message {
                    CompMessage::Json(v) => (0x01, serde_json::to_vec(&v).unwrap_or_default()),
                };
                let len = payload.len() as u32;
                if write.write_all(&len.to_le_bytes()).await.is_err()
                    || write.write_all(&[kind]).await.is_err()
                    || write.write_all(&payload).await.is_err()
                {
                    break;
                }
            }
        });
        let result = self.serve_comp_read(&mut read, id).await;
        write_task.abort();
        self.comp.lock().await.remove(&id);
        // 该 comp 断开后，它名下的窗口不应继续留在列表里。
        self.drop_comp(id).await;
        result
    }
    /// 关闭某个 comp 名下的全部窗口（连接断开时调用）。
    async fn drop_comp(&self, comp_id: u64) {
        let mut closed = Vec::new();
        {
            let mut all = self.windows.lock().await;
            let ids: Vec<u32> = all
                .iter()
                .filter(|(_, state)| state.owner == comp_id)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                self.pids.lock().await.remove(&id);
                if let Some(state) = all.remove(&id) {
                    for sub in state.subscribers.values() {
                        sub.sender.close();
                    }
                    closed.push(state.window);
                }
            }
        }
        for window in closed {
            let _ = self.window_events.send(Window {
                id: window.id,
                app_id: String::new(),
                title: "__closed__".into(),
                width: 0,
                height: 0,
                focused: false,
                mode: MODE_FREE.into(),
                codec: None,
            });
        }
    }
    async fn serve_comp_read(
        &self,
        read: &mut tokio::net::unix::OwnedReadHalf,
        comp_id: u64,
    ) -> Result<(), DisplayError> {
        loop {
            let mut header = [0u8; 5];
            match read.read_exact(&mut header).await {
                Ok(_) => {}
                // mixcomp 正常退出:EOF 不是错误。
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(error) => break Err(error.into()),
            }
            let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            if len == 0 || len > MAX_COMP_FRAME {
                // framing 已损坏,无法重新同步:显式错误路径(计数 + 断开)。
                self.protocol_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                break Err(DisplayError::Protocol(format!(
                    "非法帧长度 {len}(记录类型 0x{kind:02x})",
                    kind = header[4]
                )));
            }
            let mut payload = vec![0u8; len];
            if let Err(error) = read.read_exact(&mut payload).await {
                break Err(error.into());
            }
            match header[4] {
                0x01 => self.handle_comp(&payload, comp_id).await,
                COMP_REC_VIDEO => {
                    self.broadcast_video_record(&payload, CODEC_JPEG, comp_id)
                        .await;
                }
                COMP_REC_VIDEO_H264 => {
                    self.broadcast_video_record(&payload, CODEC_H264, comp_id)
                        .await;
                }
                COMP_REC_VIDEO_PNG => {
                    self.broadcast_video_record(&payload, CODEC_PNG, comp_id)
                        .await;
                }
                unknown => {
                    // EmergeShift#8:私有通道按增量演进,未知记录类型忽略并计数,
                    // 不再断连(新 mixcomp + 旧 mixumd 不再死链)。
                    let count = self
                        .unknown_records
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    if count == 1 || count.is_multiple_of(100) {
                        eprintln!(
                            "mixcomp 未知记录类型 0x{unknown:02x},已忽略(累计 {count});如有异常请检查 mixcomp/mixum-agentd 版本"
                        );
                    }
                }
            }
        }
    }
    /// 校验并广播一条视频记录(记录类型 0x02/0x03)。
    async fn broadcast_video_record(&self, payload: &[u8], codec: &str, comp_id: u64) {
        if payload.first() != Some(&frames::chan::VIDEO)
            || payload.len() < frames::VIDEO_HEADER_SIZE
        {
            self.protocol_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!("mixcomp 视频记录校验失败(通道字节/长度),已丢弃");
            return;
        }
        match frames::unmarshal_video(payload) {
            Ok(frame) => {
                let owner = self.window_owner(frame.window_id).await;
                if owner != Some(comp_id) {
                    return;
                }
                self.broadcast_frame(frame.window_id, payload.to_vec(), codec)
                    .await;
            }
            Err(error) => {
                self.protocol_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("mixcomp 视频帧头校验失败,已丢弃: {error}");
            }
        }
    }
    async fn window_owner(&self, id: u32) -> Option<u64> {
        self.windows.lock().await.get(&id).map(|state| state.owner)
    }
    async fn handle_comp(&self, data: &[u8], comp_id: u64) {
        match serde_json::from_slice::<CompMessageWire>(data) {
            Ok(message) => match message.kind.as_str() {
                "window.open" | "window.update" => {
                    // mode 留空:由 open_or_update 决定归属(任务活跃时新窗口归 agent)。
                    self.open_or_update(
                        Window {
                            id: message.id,
                            app_id: message.app_id,
                            title: message.title,
                            width: message.width,
                            height: message.height,
                            focused: message.focused,
                            mode: String::new(),
                            codec: None,
                        },
                        comp_id,
                    )
                    .await;
                    if let Some(pid) = message.pid {
                        self.pids.lock().await.insert(message.id, pid);
                    }
                }
                "window.close" => {
                    if self.window_owner(message.id).await == Some(comp_id) {
                        self.close(message.id).await;
                    } else {
                        eprintln!(
                            "comp {} 试图关闭不属于它的窗口 {}，忽略",
                            comp_id, message.id
                        );
                    }
                }
                // comp 能力握手（clipboard 等）：记录后决定是否接受对应操作。
                "capabilities" => {
                    if let Some(clipboard) = message.clipboard {
                        if let Some(conn) = self.comp.lock().await.get(&comp_id) {
                            conn.clipboard
                                .store(clipboard, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                // 容器选区变化(mixcomp 上行):校验能力与 owner 后广播。
                "clipboard" => {
                    let supported = self.comp.lock().await.get(&comp_id).is_some_and(|conn| {
                        conn.clipboard.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if !supported || self.window_owner(message.id).await != Some(comp_id) {
                        self.protocol_errors
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                    if let Some(text) = message.text {
                        if !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES {
                            let _ = self.clipboard_events.send(serde_json::json!({
                                "window": message.id,
                                "text": text,
                            }));
                        }
                    }
                }
                // mixcomp 能力回告:该窗口实际的流编码(如请求 h264 但
                // openh264 缺失时回 jpeg),更新订阅者与窗口并广播
                "stream.info" => {
                    if let Some(codec) = &message.codec {
                        let mut all = self.windows.lock().await;
                        if let Some(state) = all.get_mut(&message.id) {
                            if state.owner != comp_id {
                                return;
                            }
                            state.window.codec = Some(codec.clone());
                            for sub in state.subscribers.values_mut() {
                                sub.codec = codec.clone();
                            }
                            let changed = state.window.clone();
                            drop(all);
                            let _ = self.window_events.send(changed);
                        }
                    }
                }
                _ => {}
            },
            Err(error) => {
                self.protocol_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("mixcomp 控制记录 JSON 校验失败,已丢弃: {error}");
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompMessageWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    app_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    pid: Option<i32>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    clipboard: Option<bool>,
}

/// 把 spawn_gui 记录的启动信息与 broker 当前窗口按 pid 合并成快照元数据。
/// 找不到窗口的 launch 仍保留（restore 时至少能重放命令）。
pub fn merge_gui_sessions(
    launches: Vec<GuiSession>,
    windows: &[Window],
    pids: &HashMap<u32, i32>,
) -> Vec<GuiSession> {
    launches
        .into_iter()
        .map(|mut session| {
            if let Some(window) = windows
                .iter()
                .find(|window| pids.get(&window.id) == Some(&session.pid))
            {
                session.app_id = window.app_id.clone();
                session.title = window.title.clone();
                session.width = window.width;
                session.height = window.height;
            }
            session
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn takeover_state_machine_and_full_queue_policy() {
        let broker = Broker::new("/tmp/mixum-agentd-test.sock");
        broker
            .open_or_update(
                Window {
                    id: 1,
                    app_id: "a".into(),
                    title: "A".into(),
                    width: 800,
                    height: 600,
                    focused: true,
                    mode: MODE_AGENT.into(),
                    codec: None,
                },
                1,
            )
            .await;
        assert!(matches!(
            broker.inject(1, serde_json::json!({"type":"key"})).await,
            Err(DisplayError::AgentOwnsInput)
        ));
        assert!(broker.takeover(1, "claim", false).await.is_err());
        assert_eq!(broker.takeover(1, "claim", true).await.unwrap(), MODE_USER);
        broker
            .inject(1, serde_json::json!({"type":"key"}))
            .await
            .unwrap();
        assert_eq!(
            broker.takeover(1, "release", false).await.unwrap(),
            MODE_FREE
        );
    }

    #[test]
    fn clipboard_control_frame_bound() {
        assert!(clipboard_fits_control_frame(&"a".repeat(4 << 20)));
        assert!(clipboard_fits_control_frame(&"\"".repeat(4 << 20)));
        // 3MiB 控制字符 JSON 转义后 18MiB，超过 16MiB 控制帧。
        assert!(!clipboard_fits_control_frame(&"\u{1}".repeat(3 << 20)));
    }

    #[tokio::test]
    async fn clipboard_requires_capability_and_owner() {
        let broker = Broker::new("/tmp/mixum-agentd-test-clip.sock");
        let (tx1, mut rx1) = mpsc::channel(8);
        broker.comp.lock().await.insert(
            1,
            CompConn {
                tx: tx1,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let (tx2, _rx2) = mpsc::channel(8);
        broker.comp.lock().await.insert(
            2,
            CompConn {
                tx: tx2,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        broker
            .open_or_update(
                Window {
                    id: 5,
                    app_id: "a".into(),
                    title: "A".into(),
                    width: 10,
                    height: 10,
                    focused: true,
                    mode: MODE_FREE.into(),
                    codec: None,
                },
                1,
            )
            .await;

        // 未握手 → 拒绝，不下发假成功。
        assert!(broker.clipboard_set(5, "x").await.is_err());
        let cap = serde_json::to_vec(&serde_json::json!({
            "type": "capabilities", "clipboard": true
        }))
        .unwrap();
        broker.handle_comp(&cap, 1).await;
        broker.clipboard_set(5, "down").await.unwrap();
        let sent = rx1.recv().await.unwrap();
        assert!(matches!(sent, CompMessage::Json(v) if v["type"] == "clipboard.set"));

        let mut events = broker.subscribe_clipboard();
        // 非 owner 上报丢弃。
        let foreign = serde_json::to_vec(&serde_json::json!({
            "type": "clipboard", "id": 5, "text": "foreign"
        }))
        .unwrap();
        broker.handle_comp(&foreign, 2).await;
        assert!(events.try_recv().is_err());
        // owner 上报广播，payload 带窗口 id。
        let owned = serde_json::to_vec(&serde_json::json!({
            "type": "clipboard", "id": 5, "text": "up"
        }))
        .unwrap();
        broker.handle_comp(&owned, 1).await;
        let event = events.try_recv().unwrap();
        assert_eq!(event["window"], 5);
        assert_eq!(event["text"], "up");
    }

    #[tokio::test]
    async fn frames_route_by_subscriber_codec_and_stream_info_updates() {
        let broker = Broker::new("/tmp/mixum-agentd-test3.sock");
        broker
            .open_or_update(
                Window {
                    id: 7,
                    app_id: "a".into(),
                    title: "A".into(),
                    width: 640,
                    height: 480,
                    focused: true,
                    mode: MODE_FREE.into(),
                    codec: None,
                },
                1,
            )
            .await;
        let jpeg_sub = broker.attach(7, "s-jpeg", 30, CODEC_JPEG, 0).await.unwrap();
        let h264_sub = broker.attach(7, "s-h264", 30, CODEC_H264, 0).await.unwrap();
        assert_eq!(jpeg_sub.3.codec, CODEC_JPEG);
        assert_eq!(h264_sub.3.codec, CODEC_H264);
        // 能力回告前窗口 codec 未定
        assert_eq!(broker.list().await[0].codec, None);

        // H.264 帧只发给 h264 订阅者
        broker.broadcast_frame(7, vec![1, 2, 3], CODEC_H264).await;
        assert_eq!(
            h264_sub.3.frames.try_recv().as_deref(),
            Some(&[1u8, 2, 3][..])
        );
        assert!(jpeg_sub.3.frames.try_recv().is_none());

        // JPEG 帧只发给 jpeg 订阅者
        broker.broadcast_frame(7, vec![4, 5], CODEC_JPEG).await;
        assert_eq!(jpeg_sub.3.frames.try_recv().as_deref(), Some(&[4u8, 5][..]));
        assert!(h264_sub.3.frames.try_recv().is_none());

        // mixcomp 能力回告:请求 h264 但实际 jpeg → 订阅者与窗口统一切 jpeg
        let info = serde_json::to_vec(&serde_json::json!({
            "type":"stream.info","id":7,"codec":"jpeg"
        }))
        .unwrap();
        broker.handle_comp(&info, 1).await;
        assert_eq!(broker.list().await[0].codec.as_deref(), Some(CODEC_JPEG));
        // 回告后广播 jpeg,h264 订阅者也能收到(解码器据此切换)
        broker.broadcast_frame(7, vec![9], CODEC_JPEG).await;
        assert_eq!(h264_sub.3.frames.try_recv().as_deref(), Some(&[9u8][..]));
    }

    async fn write_comp_frame(stream: &mut UnixStream, kind: u8, payload: &[u8]) {
        stream
            .write_all(&(payload.len() as u32).to_le_bytes())
            .await
            .unwrap();
        stream.write_all(&[kind]).await.unwrap();
        stream.write_all(payload).await.unwrap();
        stream.flush().await.unwrap();
    }

    /// EmergeShift#8:未知记录类型忽略 + 计数,连接存活且后续消息仍被处理。
    #[tokio::test]
    async fn unknown_comp_record_is_ignored_and_counted() {
        let broker = Broker::new("/tmp/mixum-agentd-test-unknown.sock");
        let (server_side, mut client) = UnixStream::pair().unwrap();
        let serving = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.serve_comp(server_side).await })
        };
        write_comp_frame(&mut client, 0x7e, b"future-record").await;
        write_comp_frame(
            &mut client,
            0x01,
            br#"{"type":"window.open","id":9,"appId":"a","title":"A","width":10,"height":20,"focused":true}"#,
        )
        .await;

        let mut opened = false;
        for _ in 0..100 {
            if !broker.list().await.is_empty() {
                opened = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(opened, "未知记录类型之后连接应继续处理正常消息");
        let stats = broker.comp_stats();
        assert_eq!(stats.unknown_records, 1);
        assert_eq!(stats.protocol_errors, 0);

        drop(client);
        let _ = serving.await;
    }

    /// 校验错误走显式错误路径:framing 非法 → 计数 + 断开。
    #[tokio::test]
    async fn invalid_comp_frame_length_is_protocol_error_and_disconnects() {
        let broker = Broker::new("/tmp/mixum-agentd-test-protocol-err.sock");
        let (server_side, mut client) = UnixStream::pair().unwrap();
        let serving = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.serve_comp(server_side).await })
        };
        write_comp_frame(&mut client, 0x01, b"").await;
        let result = serving.await.expect("serve_comp 任务不应 panic");
        assert!(result.is_err(), "非法帧长度应结束控制通道");
        assert_eq!(broker.comp_stats().protocol_errors, 1);
        assert!(!broker.online().await);
    }

    /// PR#6 遗留②:attach 不再有「插入订阅者 → 二次查表」窗口。
    #[tokio::test]
    async fn attach_is_atomic_and_reports_not_found_without_stream_start() {
        let broker = Broker::new("/tmp/mixum-agentd-test-attach.sock");
        let (tx, mut rx) = mpsc::channel(8);
        broker.comp.lock().await.insert(
            1,
            CompConn {
                tx,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        assert!(matches!(
            broker.attach(42, "s", 30, CODEC_JPEG, 0).await,
            Err(DisplayError::NotFound(42))
        ));
        assert!(rx.try_recv().is_err(), "窗口不存在时不得发 stream.start");

        broker
            .open_or_update(
                Window {
                    id: 7,
                    app_id: "a".into(),
                    title: "A".into(),
                    width: 640,
                    height: 480,
                    focused: true,
                    mode: MODE_FREE.into(),
                    codec: None,
                },
                1,
            )
            .await;
        let (width, height, _, subscription) =
            broker.attach(7, "s", 30, CODEC_JPEG, 0).await.unwrap();
        assert_eq!((width, height), (640, 480));
        // close 紧跟在 attach 之后:attach 的返回值已经完整,不再二次查表。
        broker.close(7).await;
        assert!(matches!(
            broker.attach(7, "s2", 30, CODEC_JPEG, 0).await,
            Err(DisplayError::NotFound(7))
        ));
        drop(subscription);
    }

    /// codec=png + quality 要原样出现在给 mixcomp 的 stream.start 里。
    #[tokio::test]
    async fn attach_png_negotiates_quality_and_codec() {
        let broker = Broker::new("/tmp/mixum-agentd-test-png.sock");
        broker
            .open_or_update(
                Window {
                    id: 3,
                    app_id: "a".into(),
                    title: "A".into(),
                    width: 320,
                    height: 200,
                    focused: true,
                    mode: MODE_FREE.into(),
                    codec: None,
                },
                1,
            )
            .await;
        let (tx, mut rx) = mpsc::channel(8);
        broker.comp.lock().await.insert(
            1,
            CompConn {
                tx,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        let (_, _, _, subscription) = broker.attach(3, "s-png", 10, CODEC_PNG, 90).await.unwrap();
        assert_eq!(subscription.codec, CODEC_PNG);
        match rx.recv().await.expect("stream.start") {
            CompMessage::Json(value) => {
                assert_eq!(value["type"], "stream.start");
                assert_eq!(value["codec"], "png");
                assert_eq!(value["quality"], 90);
            }
        }
    }

    fn window(id: u32, title: &str) -> Window {
        Window {
            id,
            app_id: "a".into(),
            title: title.into(),
            width: 100,
            height: 50,
            focused: false,
            mode: MODE_FREE.into(),
            codec: None,
        }
    }

    fn video_payload(id: u32, png: &[u8]) -> Vec<u8> {
        let mut payload = vec![0u8; frames::VIDEO_HEADER_SIZE];
        payload[0] = frames::chan::VIDEO;
        payload[1..5].copy_from_slice(&id.to_le_bytes());
        payload[5..9].copy_from_slice(&100u32.to_le_bytes());
        payload[9..13].copy_from_slice(&50u32.to_le_bytes());
        payload[13..17].copy_from_slice(&(png.len() as u32).to_le_bytes());
        payload[17..19].copy_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(png);
        payload
    }

    /// A 的 mixcomp 与 B compositor 各占一个连接：stream.start/stop 只发给
    /// 窗口 owner，不能把 B 的 stream.start 发到 mixcomp。
    #[tokio::test]
    async fn routes_stream_lifecycle_to_window_owner() {
        let broker = Broker::new("/tmp/mixum-agentd-test-owners.sock");
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        broker.comp.lock().await.insert(
            10,
            CompConn {
                tx: tx_a,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        broker.comp.lock().await.insert(
            20,
            CompConn {
                tx: tx_b,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        broker.open_or_update(window(1, "A"), 10).await;
        broker.open_or_update(window(1000, "B"), 20).await;

        broker.attach(1, "s-a", 10, CODEC_PNG, 0).await.unwrap();
        broker.attach(1000, "s-b", 10, CODEC_PNG, 0).await.unwrap();
        match rx_a.recv().await.expect("A stream.start") {
            CompMessage::Json(value) => assert_eq!(value["id"], 1),
        }
        match rx_b.recv().await.expect("B stream.start") {
            CompMessage::Json(value) => assert_eq!(value["id"], 1000),
        }
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());

        broker.detach(1, "s-a").await;
        match rx_a.recv().await.expect("A stream.stop") {
            CompMessage::Json(value) => assert_eq!(value["type"], "stream.stop"),
        }
        assert!(rx_b.try_recv().is_err());
    }

    /// 帧只接受 owner comp；另一个 comp 不能往同一 window id 推帧。
    #[tokio::test]
    async fn ignores_frames_from_non_owner() {
        let broker = Broker::new("/tmp/mixum-agentd-test-frame-owner.sock");
        broker.open_or_update(window(1, "A"), 10).await;
        let sub = broker.attach(1, "s", 10, CODEC_PNG, 0).await.unwrap();
        let payload = video_payload(1, b"png");

        broker.broadcast_video_record(&payload, CODEC_PNG, 20).await;
        assert!(sub.3.frames.try_recv().is_none());
        broker.broadcast_video_record(&payload, CODEC_PNG, 10).await;
        assert_eq!(sub.3.frames.try_recv(), Some(payload));
    }

    /// 一个 comp 断开只清理它自己的窗口，另一个 comp 的窗口继续存在。
    #[tokio::test]
    async fn drops_only_owner_windows_when_comp_disconnects() {
        let broker = Broker::new("/tmp/mixum-agentd-test-drop-owner.sock");
        broker.open_or_update(window(1, "A"), 10).await;
        broker.open_or_update(window(1000, "B"), 20).await;
        broker.drop_comp(10).await;
        let ids: Vec<u32> = broker.list().await.iter().map(|window| window.id).collect();
        assert_eq!(ids, vec![1000]);
    }

    #[tokio::test]
    async fn frame_queue_close_delivers_buffered_then_none() {
        let (tx, mut rx) = FrameQueue::pair(4);
        tx.push(vec![1, 2]).await;
        tx.push(vec![3]).await;
        tx.close();
        // 关闭前排队的帧仍按序送达
        assert_eq!(rx.recv().await.as_deref(), Some(&[1u8, 2][..]));
        assert_eq!(rx.recv().await.as_deref(), Some(&[3u8][..]));
        // 排干后 recv 终结,帧转发任务据此退出
        assert_eq!(rx.recv().await, None);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn window_open_during_active_task_is_agent_owned() {
        let broker = Broker::new("/tmp/mixum-agentd-test4.sock");
        broker.set_tasks_active(true).await;
        let open = serde_json::to_vec(&serde_json::json!({
            "type":"window.open","id":9,"app_id":"a","title":"A",
            "width":640,"height":480,"focused":false
        }))
        .unwrap();
        broker.handle_comp(&open, 1).await;
        assert_eq!(broker.list().await[0].mode, MODE_AGENT);
        // agent 持有期间,控制台注入必须先接管
        assert!(matches!(
            broker.inject(9, serde_json::json!({"type":"key"})).await,
            Err(DisplayError::AgentOwnsInput)
        ));
        broker.set_tasks_active(false).await;
        assert_eq!(broker.list().await[0].mode, MODE_FREE);
    }

    #[tokio::test]
    async fn window_open_without_active_task_is_free() {
        let broker = Broker::new("/tmp/mixum-agentd-test-free.sock");
        let open = serde_json::to_vec(&serde_json::json!({
            "type":"window.open","id":10,"app_id":"a","title":"A",
            "width":640,"height":480,"focused":false
        }))
        .unwrap();
        broker.handle_comp(&open, 1).await;
        assert_eq!(broker.list().await[0].mode, MODE_FREE);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detach_keeps_stream_until_last_same_codec_subscriber_leaves() {
        use std::time::Duration;
        let path = format!("/tmp/mixum-agentd-detach-{}.sock", std::process::id());
        let broker = Broker::new(path.clone());
        broker.start().await.unwrap();
        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        for _ in 0..50 {
            if broker.online().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(broker.online().await);
        broker
            .open_or_update(
                Window {
                    id: 7,
                    app_id: "a".into(),
                    title: "A".into(),
                    width: 640,
                    height: 480,
                    focused: true,
                    mode: MODE_FREE.into(),
                    codec: None,
                },
                1,
            )
            .await;
        broker.attach(7, "s1", 30, CODEC_JPEG, 0).await.unwrap();
        broker.attach(7, "s2", 30, CODEC_JPEG, 0).await.unwrap();
        broker.attach(7, "s3", 30, CODEC_H264, 0).await.unwrap();
        // 三次 attach 的 stream.start
        for _ in 0..3 {
            let (kind, payload) = read_record(&mut client).await;
            assert_eq!(kind, 0x01);
            assert_eq!(payload["type"], "stream.start");
        }
        // 还有同编码订阅者:不得停流
        broker.detach(7, "s1").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(150), read_record(&mut client))
                .await
                .is_err()
        );
        // 最后一个 jpeg 订阅者退出:停流
        broker.detach(7, "s2").await;
        let (kind, payload) = read_record(&mut client).await;
        assert_eq!(kind, 0x01);
        assert_eq!(payload["type"], "stream.stop");
        // 最后一个 h264 订阅者退出:同样停流
        broker.detach(7, "s3").await;
        let (kind, payload) = read_record(&mut client).await;
        assert_eq!(kind, 0x01);
        assert_eq!(payload["type"], "stream.stop");
    }

    #[tokio::test]
    async fn sync_notify_reaches_comp_queue() {
        let broker = Broker::new("/tmp/mixum-agentd-test-notify.sock");
        let (tx, mut rx) = mpsc::channel(4);
        broker.comp.lock().await.insert(
            7,
            CompConn {
                tx,
                clipboard: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        broker.try_notify_comps(serde_json::json!({"type": "snapshot.freeze"}));
        match rx.recv().await.expect("应收到冻结通知") {
            CompMessage::Json(value) => assert_eq!(value["type"], "snapshot.freeze"),
        }
    }

    #[cfg(unix)]
    async fn read_record(client: &mut tokio::net::UnixStream) -> (u8, serde_json::Value) {
        let mut header = [0u8; 5];
        client.read_exact(&mut header).await.unwrap();
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        client.read_exact(&mut payload).await.unwrap();
        (header[4], serde_json::from_slice(&payload).unwrap())
    }
}

#[cfg(test)]
mod gui_session_tests {
    use super::*;

    #[test]
    fn merge_gui_sessions_matches_pid() {
        let launches = vec![GuiSession {
            pid: 7,
            command: "/usr/bin/foot".into(),
            ..Default::default()
        }];
        let windows = vec![Window {
            id: 1,
            app_id: "foot".into(),
            title: "term".into(),
            width: 640,
            height: 400,
            focused: true,
            mode: MODE_FREE.into(),
            codec: None,
        }];
        let pids = HashMap::from([(1, 7)]);
        let merged = merge_gui_sessions(launches, &windows, &pids);
        assert_eq!(merged[0].app_id, "foot");
        assert_eq!(merged[0].width, 640);
    }

    #[tokio::test]
    async fn window_pid_map_tracks_open_and_close() {
        let broker = Broker::new("/tmp/mixum-agentd-test-pid.sock");
        let open = serde_json::to_vec(&serde_json::json!({
            "type":"window.open","id":3,"app_id":"a","title":"A",
            "width":10,"height":20,"focused":false,"pid":4242
        }))
        .unwrap();
        broker.handle_comp(&open, 1).await;
        assert_eq!(broker.try_window_pids().get(&3), Some(&4242));

        let close = serde_json::to_vec(&serde_json::json!({"type":"window.close","id":3})).unwrap();
        broker.handle_comp(&close, 1).await;
        assert!(broker.try_window_pids().is_empty());
    }
}
