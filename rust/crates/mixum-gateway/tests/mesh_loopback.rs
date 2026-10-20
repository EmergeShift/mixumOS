//! M3b 回路:两个 gateway 实例(A/B)、共享注册表、节点只连 A。
//! 客户端连 B,请求经 mesh 到 A 的本地节点,应答/二进制帧原路回 B。

use mixum_gateway::{Gateway, MemoryRegistry, MeshHub, NodeRegistry, Outbound};
use mixum_protocol::token::Signer;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

async fn start_mesh(hub: Arc<MeshHub>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let hub = hub.clone();
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    let _ = hub.serve_connection(ws).await;
                }
            });
        }
    });
    port
}

async fn recv_text(rx: &mut mpsc::Receiver<Outbound>) -> Value {
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("等待节点消息超时")
        .expect("节点通道关闭")
    {
        Outbound::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("期望文本,实际 {other:?}"),
    }
}

#[tokio::test]
async fn cross_replica_routing_round_trip() {
    let registry: Arc<dyn NodeRegistry> = Arc::new(MemoryRegistry::new());

    let hub_a = MeshHub::new("gw-a", "mesh-secret");
    let port_a = start_mesh(hub_a.clone()).await;
    let gw_a = Gateway::with_options(
        Signer::generate(),
        registry.clone(),
        "gw-a".into(),
        format!("ws://127.0.0.1:{port_a}"),
        Some(hub_a.clone()),
    );
    gw_a.start_mesh(hub_a.clone());

    let hub_b = MeshHub::new("gw-b", "mesh-secret");
    let port_b = start_mesh(hub_b.clone()).await;
    let gw_b = Gateway::with_options(
        Signer::generate(),
        registry.clone(),
        "gw-b".into(),
        format!("ws://127.0.0.1:{port_b}"),
        Some(hub_b.clone()),
    );
    gw_b.start_mesh(hub_b.clone());

    // 节点 a 只连 A。
    let (node_tx, mut node_rx) = mpsc::channel::<Outbound>(32);
    gw_a.node_connected("a", "0.1.0", node_tx).await.unwrap();

    // 客户端连 B。
    let (client_tx, mut client_rx) = mpsc::channel::<Outbound>(32);
    let session = gw_b.client_connected("user:u1", client_tx).await;

    // B 上看到 A 注册的节点。
    assert_eq!(gw_b.node_offline().await, vec!["a".to_owned()]);

    let request = json!({"v":1,"id":"r1","op":"task.list","node":"a","payload":{}});
    let direct = gw_b.on_client_text(&session, &request.to_string()).await;
    assert!(direct.is_empty(), "跨副本路由不直接回客户端");

    // A 侧节点先收到 session.open,再收到带 ms- 会话标记的信封。
    let open = recv_text(&mut node_rx).await;
    assert_eq!(open["op"], "gateway.session.open");
    let mesh_session = open["payload"]["gatewaySession"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(mesh_session.starts_with("ms-gw-b-"), "{mesh_session}");
    let forwarded = recv_text(&mut node_rx).await;
    assert_eq!(forwarded["op"], "task.list");
    assert_eq!(forwarded["gatewaySession"], mesh_session);

    // 节点应答经 A→mesh→B→客户端。
    let reply = json!({
        "v":1,"id":"n1","op":"task.list.ok","replyTo":"r1",
        "gatewaySession": mesh_session, "payload":{"tasks":[]}
    });
    gw_a.on_node_text("a", &reply.to_string()).await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), client_rx.recv())
        .await
        .expect("等待跨副本应答超时")
        .expect("客户端通道关闭")
    {
        Outbound::Text(text) => {
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["op"], "task.list.ok");
            assert!(
                value.get("gatewaySession").is_none(),
                "回送客户端前应剥离标记"
            );
        }
        other => panic!("期望文本应答,实际 {other:?}"),
    }

    // 二进制帧同样按 mesh 会话回送。
    let frame = [0x01u8, 9, 8, 7, 6];
    let mut wire = (mesh_session.len() as u32).to_le_bytes().to_vec();
    wire.extend_from_slice(mesh_session.as_bytes());
    wire.extend_from_slice(&frame);
    gw_a.on_node_binary(&wire).await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), client_rx.recv())
        .await
        .expect("等待跨副本二进制超时")
        .expect("客户端通道关闭")
    {
        Outbound::Binary(bytes) => assert_eq!(bytes, frame),
        other => panic!("期望二进制帧,实际 {other:?}"),
    }

    // 客户端断开:B 发 mesh.close,A 侧关回环会话。
    gw_b.client_disconnected(&session).await;
    let mut saw_close = false;
    for _ in 0..4 {
        let Ok(Some(Outbound::Text(text))) =
            tokio::time::timeout(std::time::Duration::from_secs(2), node_rx.recv()).await
        else {
            break;
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        if value["op"] == "gateway.session.close" {
            saw_close = true;
            break;
        }
    }
    assert!(saw_close, "客户端断开后 owner 侧应收到 session.close");
}
