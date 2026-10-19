//! gateway 出站 relay(mixumd --gateway <url>):节点接入云端枢纽的唯一新增。
//!
//! 模型:gateway 为每个客户端连接分配 gatewaySession;relay 收到
//! `gateway.session.open` 后开一条到本机 mixumd 的回环 WS 会话(明文,
//! 仅 127.0.0.1,复用 server.serve_plain_listener),之后信封/帧双向搬运,
//! 全部带 gatewaySession 标记——mixumd 的订阅/广播是会话级的,按会话开
//! 回环天然隔离多客户端。节点通道私有约定见 docs/gateway.md。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// relay 配置与本机回环信息(writer 任务里惰性开回环会话要用)。
pub struct GatewayRelay {
    pub url: String,
    pub name: String,
    pub token: String,
    /// 本地管理令牌(回环会话 hello 鉴权,装配层用本机 signer 铸造)。
    pub local_token: String,
    /// 本机 mixumd 的 Server 句柄:relay 在其上起回环明文监听。
    pub server: Arc<crate::server::Server>,
    /// 期望的 gateway 证书指纹(--gateway-fingerprint);缺省 TOFU。
    pub fingerprint: Option<String>,
    /// TOFU 指纹落盘路径(dataDir/gateway-fingerprint)。
    pub pin_path: Option<std::path::PathBuf>,
}

pub fn start(
    relay: GatewayRelay,
) -> tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
    tokio::spawn(async move { run(relay).await })
}

async fn run(relay: GatewayRelay) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 回环监听(仅 127.0.0.1):每条 gatewaySession 一条 mixumd 普通客户端会话。
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    let server = Arc::clone(&relay.server);
    tokio::spawn(async move {
        if let Err(e) = server.serve_plain_listener(listener).await {
            eprintln!("gateway relay 回环服务退出: {e}");
        }
    });
    eprintln!("gateway relay 回环口: 127.0.0.1:{local_port}(仅本机)");

    // 出站重连循环(指数退避,封顶 30s)。重连后旧会话全部失效,
    // 客户端自行重连/重订阅(与直连 mixumd 的重连语义一致)。
    let mut backoff = 1u64;
    loop {
        // wss 时用不校验自签证书的 connector(节点→gateway 属同一信任域,
        // M2 限定;公网部署换受信证书后可去)。
        let connector = if relay.url.starts_with("wss://") {
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(
                crate::auth::insecure_tls_client(),
            )))
        } else {
            None
        };
        match tokio_tungstenite::connect_async_tls_with_config(
            &relay.url,
            None,
            false,
            connector,
        )
        .await
        {
            Ok((ws, _)) => {
                eprintln!("gateway 已连接: {}", relay.url);
                if let Err(e) = pump(&relay, ws, local_port).await {
                    eprintln!("gateway 会话结束: {e}");
                }
                backoff = 1;
            }
            Err(e) => eprintln!("gateway 连接失败({e}),{backoff}s 后重试"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn pump(
    relay: &GatewayRelay,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    local_port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut sink, mut source) = ws.split();
    // 上行口:回环会话 → gateway(全部带 gatewaySession 标记)。
    let (up_tx, mut up_rx) = mpsc::channel::<Message>(256);

    // 注册
    let register = json!({
        "v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.register",
        "payload":{"name":relay.name,"token":relay.token,"version":env!("CARGO_PKG_VERSION")}
    });
    sink.send(Message::Text(register.to_string().into())).await?;

    // 会话表:gatewaySession → 回环写入端
    let sessions: Arc<Mutex<HashMap<String, Loopback>>> = Arc::new(Mutex::new(HashMap::new()));

    // 下行泵:gateway → 回环(会话开关/业务信封)
    let down = {
        let sessions = Arc::clone(&sessions);
        let local_token = relay.local_token.clone();
        async move {
            while let Some(Ok(Message::Text(text))) = source.next().await {
                let Ok(env) = serde_json::from_str::<Value>(&text) else { continue };
                let op = env.get("op").and_then(Value::as_str).unwrap_or_default();
                if op == "gateway.session.open" {
                    let Some(gs) = env
                        .pointer("/payload/gatewaySession")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    else {
                        continue;
                    };
                    let has = sessions.lock().await.contains_key(&gs);
                    if !has {
                        match Loopback::connect(local_port, &local_token, &gs, up_tx.clone()).await {
                            Ok(lb) => {
                                sessions.lock().await.insert(gs.clone(), lb);
                            }
                            Err(e) => eprintln!("gateway relay 回环建立失败: {e}"),
                        }
                    }
                    continue;
                }
                if op == "gateway.session.close" {
                    if let Some(gs) = env
                        .pointer("/payload/gatewaySession")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    {
                        if let Some(lb) = sessions.lock().await.remove(&gs) {
                            lb.read_task.abort();
                        }
                    }
                    continue;
                }
                // 业务信封:带 gatewaySession 转发到对应回环
                let gs = env
                    .get("gatewaySession")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let target = sessions.lock().await.get(&gs).cloned();
                match target {
                    Some(lb) => {
                        if let Err(e) = lb.send_text(&text).await {
                            eprintln!("gateway relay 回环发送失败: {e}");
                        }
                    }
                    None => eprintln!("gateway relay 无会话,丢弃信封(op={op})"),
                }
            }
        }
    };

    // 上行泵:gateway ← 回环(文本补 gatewaySession 字段,二进制加前缀)
    let up = async {
        while let Some(msg) = up_rx.recv().await {
            let out = match msg {
                Message::Text(t) => {
                    let Ok(mut env) = serde_json::from_str::<Value>(&t) else { continue };
                    let Some(obj) = env.as_object_mut() else { continue };
                    // 会话归属:gatewaySession 由 Loopback 写泵按所属回环填好
                    obj.insert("gatewaySession".into(), json!(""));
                    let _ = obj; // gatewaySession 实际值见 Loopback(下行同类逻辑)
                    Message::Text(t)
                }
                b @ Message::Binary(_) => b,
                m => m,
            };
            sink.send(out).await?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };

    tokio::select! {
        _ = down => {}
        r = up => r?,
    }
    Ok(())
}

/// 回环会话:到本机 mixumd 的一条普通客户端连接(自带 hello 鉴权)。
/// 读泵把 mixumd 下行(应答/事件/视频帧)转发到 gateway 上行口。
#[derive(Clone)]
struct Loopback {
    tx: mpsc::Sender<Message>,
    /// 读泵句柄:会话关闭/重连时 abort,避免任务泄漏。
    read_task: Arc<tokio::task::JoinHandle<()>>,
}


impl Loopback {
    async fn connect(
        port: u16,
        token: &str,
        session: &str,
        up_tx: mpsc::Sender<Message>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws")).await?;
        let (mut sink, mut source) = ws.split();
        let (tx, mut rx) = mpsc::channel::<Message>(256);
        let hello = json!({
            "v":1,"id":mixum_protocol::envelope::new_id(),"op":"hello",
            "payload":{"token":token,"client":"mixum-gateway-relay/0.1","device":"gateway"}
        });
        sink.send(Message::Text(hello.to_string().into())).await?;
        let mut authenticated = false;
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(250), source.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    if t.contains("hello.ok") {
                        authenticated = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        if !authenticated {
            return Err("回环 hello 失败".into());
        }
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });
        // 读泵:mixumd → gateway(文本补 gatewaySession 字段;二进制加
        // [u32 LE len][session] 前缀——节点通道私有约定,gateway 剥离后回送)
        let session_id = session.to_owned();
        let read_task = Arc::new(tokio::spawn(async move {
            while let Some(Ok(msg)) = source.next().await {
                let tagged = match msg {
                    Message::Text(t) => {
                        let Ok(mut env) = serde_json::from_str::<Value>(&t) else { continue };
                        if let Some(obj) = env.as_object_mut() {
                            obj.insert("gatewaySession".into(), json!(session_id));
                        }
                        Message::Text(env.to_string().into())
                    }
                    Message::Binary(b) => {
                        let mut wire = ((session_id.len()) as u32).to_le_bytes().to_vec();
                        wire.extend_from_slice(session_id.as_bytes());
                        wire.extend_from_slice(&b);
                        Message::Binary(wire.into())
                    }
                    m => m,
                };
                if up_tx.send(tagged).await.is_err() {
                    break;
                }
            }
        }));
        Ok(Self { tx, read_task })
    }

    async fn send_text(&self, text: &str) -> Result<(), String> {
        self.tx
            .send(Message::Text(text.to_owned().into()))
            .await
            .map_err(|e| e.to_string())
    }
}

