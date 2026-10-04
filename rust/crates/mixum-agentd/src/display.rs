//! mixcomp 控制通道和窗口流 broker。

use crate::wire::Window;
use mixum_protocol::frames;
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

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("窗口不存在：{0}")]
    NotFound(u32),
    #[error("窗口正由 agent 操作，请先接管")]
    AgentOwnsInput,
    #[error("接管冲突：{0}")]
    Conflict(String),
    #[error("控制通道：{0}")]
    Io(#[from] std::io::Error),
}

pub struct FrameSubscription {
    pub id: String,
    pub frames: FrameReceiver,
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
}

impl FrameQueue {
    fn pair(capacity: usize) -> (FrameSender, FrameReceiver) {
        let queue = Arc::new(Self {
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            notify: Notify::new(),
            capacity,
        });
        (
            FrameSender {
                queue: queue.clone(),
            },
            FrameReceiver { queue },
        )
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
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(frame) = self.queue.items.lock().await.pop_front() {
                return Some(frame);
            }
            self.queue.notify.notified().await;
        }
    }
}

struct WindowState {
    window: Window,
    subscribers: HashMap<String, FrameSender>,
}

#[derive(Clone)]
pub struct Broker {
    windows: Arc<Mutex<HashMap<u32, WindowState>>>,
    comp: Arc<Mutex<Option<mpsc::Sender<CompMessage>>>>,
    window_events: broadcast::Sender<Window>,
    socket_path: PathBuf,
    /// 任务引擎是否活跃:活跃时新窗口归 agent 模式(用户接管需 claim)。
    tasks_active: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
enum CompMessage {
    Json(serde_json::Value),
}

impl Broker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let (window_events, _) = broadcast::channel(128);
        Self {
            windows: Arc::new(Mutex::new(HashMap::new())),
            comp: Arc::new(Mutex::new(None)),
            window_events,
            socket_path: path.into(),
            tasks_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        self.comp.lock().await.is_some()
    }
    pub fn subscribe_windows(&self) -> broadcast::Receiver<Window> {
        self.window_events.subscribe()
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
                    (st.window.mode.is_empty() || st.window.mode == MODE_FREE)
                        .then_some(MODE_AGENT)
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

    pub async fn open_or_update(&self, window: Window) {
        let mut all = self.windows.lock().await;
        let tasks_active = self
            .tasks_active
            .load(std::sync::atomic::Ordering::SeqCst);
        let entry = all.entry(window.id).or_insert_with(|| {
            // 任务运行中打开的窗口归 agent:用户可见,接管需 display.takeover。
            let mut w = window.clone();
            if tasks_active && w.mode.is_empty() {
                w.mode = MODE_AGENT.into();
            }
            WindowState {
                window: w,
                subscribers: HashMap::new(),
            }
        });
        let mode = entry.window.mode.clone();
        entry.window = Window {
            mode: if mode.is_empty() {
                window.mode.clone()
            } else {
                mode
            },
            ..window
        };
        let changed = entry.window.clone();
        drop(all);
        let _ = self.window_events.send(changed);
    }
    pub async fn close(&self, id: u32) {
        self.windows.lock().await.remove(&id);
        let _ = self.window_events.send(Window {
            id,
            app_id: String::new(),
            title: "__closed__".into(),
            width: 0,
            height: 0,
            focused: false,
            mode: MODE_FREE.into(),
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
    ) -> Result<(u32, u32, u32, FrameReceiver), DisplayError> {
        let (tx, rx) = FrameQueue::pair(8);
        let subscriber_id = subscriber_id.into();
        let mut all = self.windows.lock().await;
        let state = all.get_mut(&id).ok_or(DisplayError::NotFound(id))?;
        state.subscribers.insert(subscriber_id, tx);
        drop(all);
        self.send_json(serde_json::json!({"type":"stream.start","id":id,"fps":fps.clamp(1,60)}))
            .await;
        let all = self.windows.lock().await;
        let state = all.get(&id).ok_or(DisplayError::NotFound(id))?;
        Ok((
            state.window.width,
            state.window.height,
            fps.clamp(1, 60),
            rx,
        ))
    }
    pub async fn detach(&self, id: u32, subscriber_id: &str) {
        if let Some(state) = self.windows.lock().await.get_mut(&id) {
            state.subscribers.remove(subscriber_id);
        }
        self.send_json(serde_json::json!({"type":"stream.stop","id":id}))
            .await;
    }
    pub async fn broadcast_frame(&self, id: u32, frame: Vec<u8>) {
        let subscribers = self
            .windows
            .lock()
            .await
            .get(&id)
            .map(|s| s.subscribers.values().cloned().collect::<Vec<_>>())
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
        self.send_json(serde_json::json!({"type":"input.mode","id":id,"mode":mode}))
            .await;
        Ok(mode.into())
    }
    pub async fn inject(&self, id: u32, event: serde_json::Value) -> Result<(), DisplayError> {
        let all = self.windows.lock().await;
        let state = all.get(&id).ok_or(DisplayError::NotFound(id))?;
        if state.window.mode == MODE_AGENT {
            return Err(DisplayError::AgentOwnsInput);
        }
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
        for key in ["x", "y", "button", "action", "key", "keysym", "mods"] {
            if let Some(value) = event.get(key) {
                control.insert(key.into(), value.clone());
            }
        }
        self.send_json(serde_json::Value::Object(control)).await;
        Ok(())
    }
    async fn send_json(&self, value: serde_json::Value) {
        if let Some(tx) = self.comp.lock().await.as_ref() {
            let _ = tx.send(CompMessage::Json(value)).await;
        }
    }
    async fn serve_comp(&self, stream: UnixStream) -> Result<(), DisplayError> {
        let (mut read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::channel(64);
        *self.comp.lock().await = Some(tx);
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
        loop {
            let mut header = [0u8; 5];
            read.read_exact(&mut header).await?;
            let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            if len == 0 || len > 16 * 1024 * 1024 {
                break;
            }
            let mut payload = vec![0u8; len];
            read.read_exact(&mut payload).await?;
            match header[4] {
                0x01 => self.handle_comp(&payload).await,
                0x02 => {
                    if payload.first() == Some(&frames::chan::VIDEO)
                        && payload.len() >= frames::VIDEO_HEADER_SIZE
                    {
                        if let Ok(frame) = frames::unmarshal_video(&payload) {
                            self.broadcast_frame(frame.window_id, payload).await;
                        }
                    }
                }
                _ => break,
            }
        }
        write_task.abort();
        *self.comp.lock().await = None;
        Ok(())
    }
    async fn handle_comp(&self, data: &[u8]) {
        if let Ok(message) = serde_json::from_slice::<CompMessageWire>(data) {
            match message.kind.as_str() {
                "window.open" | "window.update" => {
                    self.open_or_update(Window {
                        id: message.id,
                        app_id: message.app_id,
                        title: message.title,
                        width: message.width,
                        height: message.height,
                        focused: message.focused,
                        mode: MODE_FREE.into(),
                    })
                    .await;
                }
                "window.close" => self.close(message.id).await,
                _ => {}
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn takeover_state_machine_and_full_queue_policy() {
        let broker = Broker::new("/tmp/mixum-agentd-test.sock");
        broker
            .open_or_update(Window {
                id: 1,
                app_id: "a".into(),
                title: "A".into(),
                width: 800,
                height: 600,
                focused: true,
                mode: MODE_AGENT.into(),
            })
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
}
