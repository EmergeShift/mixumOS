//! mixum-agentd 默认装配：配置、令牌签名器、任务引擎、快照桩、broker 与 WSS。

use mixum_agentd::auth::{load_or_create_identity, Authorizer};
use mixum_agentd::display::Broker;
use mixum_agentd::server::{Config, Server};
use mixum_agentd::snapshot::MemorySnapshotEngine;
use mixum_agentd::task::{
    Engine, ExternalDriver, LocalSandbox, NamedExternalDriver, Registry, ShellDriver,
};
use mixum_protocol::ops::Cap;
use mixum_protocol::token::Claims;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config_path = std::env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix("--config=").map(str::to_owned))
        .unwrap_or_else(|| "/etc/mixum/mixumd.json".into());
    let config = match tokio::fs::read(&config_path).await {
        Ok(data) => serde_json::from_slice::<Config>(&data)?,
        Err(_) => Config::default(),
    };

    let hosts = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        config.hostname.clone(),
    ];
    let identity = load_or_create_identity(&config.data_dir, &hosts)?;
    let signer = identity.signer;
    let authorizer = Authorizer::new(mixum_protocol::token::Verifier::new(signer.verifying_key()));
    let registry = Arc::new(Registry::new());
    registry
        .register(ShellDriver)
        .map_err(std::io::Error::other)?;
    registry
        .register(ExternalDriver { command: None })
        .map_err(std::io::Error::other)?;
    for agent in &config.agents {
        registry
            .register(NamedExternalDriver {
                name: agent.name.clone(),
                command: agent.command.clone(),
            })
            .map_err(std::io::Error::other)?;
    }
    let snapshots = Arc::new(MemorySnapshotEngine::new());
    let engine = Engine::with_snapshots(
        registry,
        Arc::new(LocalSandbox::new()),
        Some(snapshots.clone()),
    );
    let broker = Broker::new("/run/mixum/display.sock");
    let _ = broker.start().await;
    let server = Server::new(config.clone(), authorizer, engine, broker, snapshots);
    let token_path = std::path::Path::new(&config.data_dir).join("admin-token");
    let token = match std::fs::read_to_string(&token_path) {
        Ok(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => {
            let token = signer.mint(Claims {
                sub: "bootstrap-admin".into(),
                caps: vec![Cap::Admin],
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })?;
            std::fs::write(&token_path, &token)?;
            token
        }
    };
    eprintln!("mixum-agentd listening on {}", config.listen);
    eprintln!("fingerprint: {}", identity.fingerprint);
    eprintln!("bootstrap token: {token}");
    let listener: std::net::SocketAddr = config.listen.parse()?;
    tokio::select! { result = server.serve_tls(listener, identity.tls) => result.map_err(Into::into), _ = tokio::signal::ctrl_c() => Ok(()) }
}
