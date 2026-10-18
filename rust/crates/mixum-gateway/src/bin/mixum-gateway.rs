//! mixum-gateway 二进制:M1 单实例,WSS 明文入口(M1 限定,见 gateway.md §9)。
//! 启动即打印 bootstrap 管理令牌;节点以 gateway.register 出站注册,
//! 客户端 hello 握手后按 envelope.node 路由。

use futures_util::{SinkExt, StreamExt};
use mixum_gateway::{Gateway, OutTx, Outbound};
use mixum_protocol::envelope::Envelope;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut listen = "0.0.0.0:7701".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--listen" {
            listen = args.next().unwrap_or(listen);
        }
    }

    let gw = Gateway::new();
    let bootstrap = gw.bootstrap_token()?;
    eprintln!("mixum-gateway listening on ws://{listen}");
    eprintln!("bootstrap token: {bootstrap}");

    let listener = TcpListener::bind(&listen).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let gw = Arc::clone(&gw);
        tokio::spawn(async move {
            let ws = tokio_tungstenite::accept_async(stream).await;
            if let Ok(ws) = ws {
                let _ = handle_connection(gw, ws).await;
            }
        });
    }
}

#[allow(clippy::large_enum_variant)]
enum Side {
    /// 未分类:等待首条消息(gateway.register 或 hello)。
    Pending,
    Node(String),
    Client(String),
}

async fn handle_connection(
    gw: Arc<Gateway>,
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Outbound>(256);

    let mut side = Side::Pending;

    // 写泵
    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let msg = match out {
                Outbound::Text(t) => Message::Text(t.into()),
                Outbound::Binary(b) => Message::Binary(b.into()),
                Outbound::Close => {
                    let _ = sink.close().await;
                    break;
                }
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 读泵:首条消息分类连接
    while let Some(msg) = source.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            // Ping 的 Pong 由 tungstenite 在写入时自动附带,这里不处理
            Message::Text(t) => {
                let t = t.to_string();
                match &side {
                    Side::Pending => {
                        side = classify(&gw, &t, out_tx.clone()).await;
                        if let Side::Pending = side {
                            // 分类失败:回错误并断开
                            break;
                        }
                    }
                    Side::Node(name) => {
                        gw.on_node_text(name, &t).await; // 核心直投客户端
                    }
                    Side::Client(session) => {
                        let session = session.clone();
                        for out in gw.on_client_text(&session, &t).await {
                            let _ = out_tx.send(out).await;
                        }
                    }
                }
            }
            Message::Binary(b) => {
                if let Side::Node(_) = &side {
                    gw.on_node_binary(&b).await; // 核心直投客户端
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // 连接结束:注销
    match side {
        Side::Node(name) => gw.node_disconnected(&name).await,
        Side::Client(session) => gw.client_disconnected(&session).await,
        Side::Pending => {}
    }
    writer.abort();
    Ok(())
}

/// 首条消息分类:gateway.register(节点令牌)→ Node;hello(任意有效令牌)→ Client。
/// 分类失败时向对端回错误信封并返回 Pending(调用方断开)。
async fn classify(gw: &Arc<Gateway>, text: &str, out: OutTx) -> Side {
    let env: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let op = env.get("op").and_then(Value::as_str).unwrap_or_default();
    let token = env
        .pointer("/payload/token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match op {
        "gateway.register" => {
            let name = env
                .pointer("/payload/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let version = env
                .pointer("/payload/version")
                .and_then(Value::as_str)
                .unwrap_or("0")
                .to_owned();
            if name.is_empty() {
                let _ = out
                    .send(Outbound::Text(err_text(None, "name 必填")))
                    .await;
                return Side::Pending;
            }
            match gw.verify(token).await {
                Ok(claims) if claims.sub.starts_with("node:") => {
                    if let Err(e) = gw.node_connected(&name, &version, out.clone()).await {
                        let _ = out.send(Outbound::Text(err_text(None, &e))).await;
                        return Side::Pending;
                    }
                    eprintln!("节点已注册: {name} ({version})");
                    let _ = out
                        .send(Outbound::Text(
                            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.register.ok","replyTo":env.get("id").cloned().unwrap_or_default(),"payload":{"lease":30}})
                                .to_string(),
                        ))
                        .await;
                    Side::Node(name)
                }
                _ => {
                    let _ = out
                        .send(Outbound::Text(err_text(None, "节点令牌无效")))
                        .await;
                    Side::Pending
                }
            }
        }
        "hello" => {
            let claims = match gw.verify(token).await {
                Ok(c) if c.sub.starts_with("user:") || c.sub == "gateway-admin" => c,
                _ => {
                    let _ = out
                        .send(Outbound::Text(err_text(None, "用户令牌无效")))
                        .await;
                    return Side::Pending;
                }
            };
            let session = gw.client_connected(&claims.sub, out.clone()).await;
            eprintln!("客户端接入: {} ({})", claims.sub, session);
            let mut hello = Envelope::new(
                "hello.ok",
                Some(json!({
                    "sessionId": session,
                    "server": "mixum-gateway/0.1",
                    "arch": std::env::consts::ARCH,
                    "kernel": "gateway",
                    "caps": claims.caps,
                    "versions": [1],
                    "heartbeat": 30,
                })),
            );
            hello.reply_to = env.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let _ = out.send(Outbound::Text(hello.to_json())).await;
            Side::Client(session)
        }
        _ => {
            let _ = out
                .send(Outbound::Text(err_text(
                    None,
                    "首条消息须为 gateway.register 或 hello",
                )))
                .await;
            Side::Pending
        }
    }
}

fn err_text(env: Option<&Value>, message: &str) -> String {
    let id = env
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() {
        json!({"v":1,"id":Value::String(mixum_protocol::envelope::new_id()),"op":"error.err","payload":{"code":"unauthorized","message":message}}).to_string()
    } else {
        json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"error.err","replyTo":id,"payload":{"code":"unauthorized","message":message}}).to_string()
    }
}
