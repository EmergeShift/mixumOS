//! MAP 端到端验证宿主:在回环地址上起明文 WS,签发 admin 令牌,
//! 打印 `READY {url,token}` 供 TS SDK e2e 脚本连接,收到 Ctrl-C 退出。
//!
//! 只用于测试/演示:明文 WS + 临时 dataDir,不进入发行版。

use mixum_agentd::auth::Authorizer;
use mixum_agentd::display::Broker;
use mixum_agentd::server::{Config, Server};
use mixum_agentd::snapshot::MemorySnapshotEngine;
use mixum_agentd::task::{Engine, ExternalDriver, LocalSandbox, Registry, ShellDriver};
use mixum_protocol::ops::Cap;
use mixum_protocol::token::{Claims, Signer, Verifier};
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data_dir = std::env::var("MAP_HARNESS_DIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join(format!(
                "map-harness-{}",
                mixum_protocol::envelope::new_id()
            ))
            .to_string_lossy()
            .into_owned()
    });
    std::fs::create_dir_all(&data_dir)?;
    let config = Config {
        listen: "127.0.0.1:0".into(),
        data_dir: data_dir.clone(),
        work_root: format!("{data_dir}/work"),
        hostname: "map-harness".into(),
        encrypt_snapshots: false,
        ..Default::default()
    };

    let signer = Signer::generate();
    let authorizer = Authorizer::new(Verifier::new(signer.verifying_key()));
    let registry = Arc::new(Registry::new());
    registry.register(ShellDriver)?;
    registry.register(ExternalDriver { command: None })?;
    let engine = Engine::with_snapshots(
        registry,
        Arc::new(LocalSandbox::new()),
        Some(Arc::new(MemorySnapshotEngine::new())),
    );
    let broker = Broker::new(format!("{data_dir}/display.sock"));
    let server = Server::new(
        config,
        authorizer,
        engine,
        broker,
        Arc::new(MemorySnapshotEngine::new()),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    let token = signer.mint(Claims {
        sub: "map-harness".into(),
        caps: vec![Cap::Admin],
        iat: now,
        exp: now + 3600,
        jti: String::new(),
        task_id: String::new(),
        windows: vec![],
    })?;

    // e2e 脚本按行解析:必须显式冲刷(管道下 stdout 不是行缓冲)。
    println!(
        "READY {}",
        json!({ "url": format!("ws://{addr}/ws"), "token": token, "dataDir": data_dir })
    );
    std::io::stdout().flush()?;
    eprintln!("map harness listening on ws://{addr}/ws");

    tokio::select! {
        result = server.serve_plain_listener(listener) => result?,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
