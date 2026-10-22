//! 容器音频采集与广播。
//!
//! 采集命令由 `audio_command` 配置（默认读 PulseAudio/PipeWire 的 parec），
//! 输出定长 s16le 交织 PCM；这里切成 20ms 块，加音频帧头后广播给订阅连接。
//! 第一个订阅者启动采集进程，最后一个离开时终止。

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mixum_protocol::frames;
use tokio::sync::broadcast;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const CHUNK_MS: u32 = 20;
pub const CHUNK_BYTES: usize =
    (SAMPLE_RATE as usize / 1000 * CHUNK_MS as usize) * CHANNELS as usize * 2;

pub struct AudioSubscription {
    pub rx: broadcast::Receiver<Arc<[u8]>>,
    pub sample_rate: u32,
    pub channels: u16,
    pub chunk_ms: u32,
}

pub struct AudioHub {
    command: Option<String>,
    subscribers: AtomicUsize,
    child: Mutex<Option<Child>>,
    tx: broadcast::Sender<Arc<[u8]>>,
}

impl AudioHub {
    pub fn new(command: Option<String>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(32);
        Arc::new(Self {
            command,
            subscribers: AtomicUsize::new(0),
            child: Mutex::new(None),
            tx,
        })
    }

    /// 订阅音频帧；返回 (接收端, 采样率, 声道数, 块时长)。
    pub fn subscribe(self: &Arc<Self>) -> Result<AudioSubscription, String> {
        let Some(command) = self.command.clone() else {
            return Err("未配置音频采集命令(audio_command)".into());
        };
        let rx = self.tx.subscribe();
        if self.subscribers.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Err(e) = self.start(&command) {
                self.subscribers.fetch_sub(1, Ordering::SeqCst);
                return Err(e);
            }
        }
        Ok(AudioSubscription {
            rx,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            chunk_ms: CHUNK_MS,
        })
    }

    pub fn unsubscribe(&self) {
        if self.subscribers.fetch_sub(1, Ordering::SeqCst) <= 1 {
            self.stop();
        }
    }

    fn start(self: &Arc<Self>, command: &str) -> Result<(), String> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动音频采集失败: {e}"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "音频采集无 stdout".to_owned())?;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut pcm = vec![0u8; CHUNK_BYTES];
            let mut seq: u32 = 0;
            while stdout.read_exact(&mut pcm).is_ok() {
                let frame = frames::marshal_audio(seq, &pcm);
                seq = seq.wrapping_add(1);
                // 没有订阅者时 send 返回 Err；保留采集进程等待新订阅者。
                let _ = tx.send(Arc::from(frame));
            }
        });
        *self.child.lock().expect("音频子进程锁") = Some(child);
        Ok(())
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().expect("音频子进程锁").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 会话侧守卫：Drop 时退订，最后一个订阅者离开会终止采集进程。
pub struct AudioGuard(Arc<AudioHub>);

impl AudioGuard {
    pub fn new(hub: Arc<AudioHub>) -> Self {
        Self(hub)
    }
}

impl Drop for AudioGuard {
    fn drop(&mut self) {
        self.0.unsubscribe();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_is_twenty_milliseconds() {
        assert_eq!(CHUNK_BYTES, 3840);
        assert_eq!(CHUNK_BYTES, (48000 * 20 / 1000) * 2 * 2);
    }

    #[test]
    fn subscribe_without_command_is_unsupported() {
        let hub = AudioHub::new(None);
        assert!(hub.subscribe().is_err());
    }
}
