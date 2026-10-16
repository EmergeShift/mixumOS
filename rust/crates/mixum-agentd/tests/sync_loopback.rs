//! 跨节点推送回环:两个 agentd 服务实例(明文 WS 回环),A 把快照推给 B。
//! 验证协议 §6 全流程:import 对账 → 缺失块二进制流 → commit 落树。
//! 仅在 `real-snapshot` 特性下编译。

#![cfg(feature = "real-snapshot")]

use mixum_agentd::auth::Authorizer;
use mixum_agentd::display::Broker;
use mixum_agentd::server::{Config, Server};
use mixum_agentd::snapshot::{RealSnapshotEngine, SharedEngine, SnapshotEngine};
use mixum_agentd::sync;
use mixum_agentd::task::{Engine, LocalSandbox, Registry, ShellDriver};
use mixum_protocol::ops::Cap;
use mixum_protocol::token::{Claims, Signer, Verifier};
use std::sync::Arc;

fn admin_token(signer: &Signer, sub: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    signer
        .mint(Claims {
            sub: sub.into(),
            caps: vec![Cap::Admin],
            iat: now,
            exp: now + 3600,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![],
        })
        .unwrap()
}

/// 起一个装配完整的 agentd(真实快照引擎 + 同步后端),返回监听端口。
async fn spawn_node(tag: &str) -> (tempfile::TempDir, SharedEngine, String, u16) {
    let dir = tempfile::tempdir().unwrap();
    let engine = mixum_snapshot::Engine::open(mixum_snapshot::EngineOptions {
        data_dir: dir.path().join("snapshots"),
        root: dir.path().join("work"),
        skip: vec![".mixum".into()],
        encrypt: false,
    })
    .unwrap_or_else(|e| panic!("{tag}: {e}"));
    let shared: SharedEngine = Arc::new(std::sync::Mutex::new(engine));

    let signer = Signer::generate();
    let authorizer = Authorizer::new(Verifier::new(signer.verifying_key()));
    let token = admin_token(&signer, tag);

    let snapshots: Arc<dyn SnapshotEngine> =
        Arc::new(RealSnapshotEngine::from_shared(shared.clone()));

    let registry = Arc::new(Registry::new());
    registry.register(ShellDriver).unwrap();
    let tasks =
        Engine::with_snapshots(registry, Arc::new(LocalSandbox::new()), Some(snapshots.clone()));
    let broker = Broker::new(dir.path().join("display.sock"));

    let mut server = Server::new(
        Config {
            listen: "127.0.0.1:0".into(),
            data_dir: dir.path().to_string_lossy().into_owned(),
            work_root: dir.path().join("work").to_string_lossy().into_owned(),
            hostname: tag.into(),
            encrypt_snapshots: false,
            ..Config::default()
        },
        authorizer,
        tasks,
        broker,
        snapshots,
    );
    Arc::get_mut(&mut server).unwrap().sync =
        Some(Arc::new(sync::SyncBackend::new(shared.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = server.serve_plain_listener(listener).await;
    });
    (dir, shared, token, port)
}

#[tokio::test(flavor = "multi_thread")]
async fn push_moves_node_between_nodes_and_dedups() {
    let (_dir_a, engine_a, _token_a, port_a) = spawn_node("node-a").await;
    let (_dir_b, engine_b, token_b, port_b) = spawn_node("node-b").await;
    let _ = port_a; // 仅保持节点 A 存活对比用途

    // A 端造内容并快照(父节点是 A 的 genesis,B 端没有 → 应标记远端根)。
    let node_id = {
        let mut engine = engine_a.lock().unwrap();
        std::fs::write(engine.root.join("marker.txt"), b"e2e-payload").unwrap();
        engine
            .snapshot(mixum_snapshot::KIND_DISK, "来自 A", false)
            .unwrap()
            .id
    };

    // 第一次推送:全量缺失,逐块送达,commit 落树。
    let stats = sync::push_node(
        &engine_a,
        &node_id,
        &format!("ws://127.0.0.1:{port_b}"),
        &token_b,
        "node-a",
    )
    .await
    .unwrap();
    assert_eq!(stats.node, node_id);
    assert!(stats.chunks_sent >= 1, "至少推送一个块:{stats:?}");
    assert_eq!(stats.dedup_ratio, "1.00");
    assert!(stats.bytes_sent > 0);

    // B 端树上出现该节点:父不在 → 新分支根;块已入 CAS(不物化工作区)。
    {
        let engine = engine_b.lock().unwrap();
        let node = engine.tree.get(&node_id).unwrap();
        assert_eq!(node.parent, "");
        assert!(node.label.contains("远端根"), "label:{}", node.label);
        assert_eq!(node.origin, "node-a");
        let manifest = node.disk.as_ref().unwrap();
        for hash in manifest.chunks() {
            assert!(engine.store.has(&hash), "块 {hash} 应已在 B 端 CAS");
        }
    }

    // 第二次推送:全部块已在 B 端,零发送且不报错(幂等)。
    let again = sync::push_node(
        &engine_a,
        &node_id,
        &format!("ws://127.0.0.1:{port_b}"),
        &token_b,
        "node-a",
    )
    .await
    .unwrap();
    assert_eq!(again.chunks_sent, 0);
    assert_eq!(again.dedup_ratio, "1.00");
}

#[tokio::test(flavor = "multi_thread")]
async fn push_reports_missing_node() {
    let (_dir_a, engine_a, _token_a, _port_a) = spawn_node("node-a2").await;
    let (_dir_b, _engine_b, token_b, _port_b) = spawn_node("node-b2").await;
    let err = sync::push_node(&engine_a, "snap_nope", "ws://127.0.0.1:1", &token_b, "node-a")
        .await
        .unwrap_err();
    assert!(matches!(err, sync::SyncError::NotFound(_)));
}
