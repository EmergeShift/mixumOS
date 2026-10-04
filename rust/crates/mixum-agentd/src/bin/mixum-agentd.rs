//! mixum-agentd 默认装配：配置、令牌签名器、任务引擎、快照引擎、broker 与 WSS。
//!
//! 快照后端由 `real-snapshot` 特性决定:开启时接 mixum-snapshot crate
//! (dataDir/snapshots 下 CAS + 树,装配同 reference/cmd/mixumd),
//! 否则使用内存桩(协议可测,文件回退不可测)。

use mixum_agentd::auth::{load_or_create_identity, Authorizer};
use mixum_agentd::display::Broker;
use mixum_agentd::server::{Config, Server};
use mixum_agentd::task::{
    Engine, ExternalDriver, LocalSandbox, NamedExternalDriver, Registry, ShellDriver,
};
use mixum_agentd::snapshot::SnapshotEngine;
use mixum_protocol::ops::Cap;
use mixum_protocol::token::Claims;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // --config <path> 与 --config=<path> 两种形式都接受(entrypoint 用空格形式)。
    let mut args = std::env::args().skip(1);
    let mut config_path = "/etc/mixum/mixumd.json".to_owned();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--config=") {
            config_path = value.to_owned();
        } else if arg == "--config" {
            if let Some(value) = args.next() {
                config_path = value;
            }
        }
    }
    // 空文件与缺失同义:使用默认配置(对齐 reference/cmd/mixumd 的 /dev/null 用法)。
    let mut config = match tokio::fs::read(&config_path).await {
        Ok(data) if !data.is_empty() => serde_json::from_slice::<Config>(&data)?,
        _ => Config::default(),
    };
    if config.hostname.is_empty() {
        config.hostname = nix::unistd::gethostname()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    let hosts = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
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
    let sandbox = Arc::new(LocalSandbox::new());

    // 快照引擎:git 式树 + 内容寻址块存储 + CRIU 探测。
    #[cfg(feature = "real-snapshot")]
    let (snapshots, sync_backend) = {
        let mut engine = mixum_snapshot::Engine::open(mixum_snapshot::EngineOptions {
            data_dir: std::path::Path::new(&config.data_dir).join("snapshots"),
            root: config.work_root.clone().into(),
            skip: vec![".mixum".to_owned()],
            encrypt: config.encrypt_snapshots,
        })?;
        eprintln!(
            "snapshot engine ready backend={} criu={}",
            engine.backend_name(),
            engine.criu.as_ref().is_some_and(|criu| criu.available())
        );
        // 快照冻结回调:冻结全部任务进程组,快照点即冻结点(无感)。
        let freeze_sandbox = Arc::clone(&sandbox);
        engine.freeze = Some(Box::new(move |action| {
            freeze_sandbox.freeze_all(true);
            let result = action();
            freeze_sandbox.freeze_all(false);
            result
        }));
        let adapter = mixum_agentd::snapshot::RealSnapshotEngine::from_engine(engine);
        let backend = Arc::new(mixum_agentd::sync::SyncBackend::new(adapter.shared()));
        (
            Arc::new(adapter) as Arc<dyn SnapshotEngine>,
            Some(backend),
        )
    };
    #[cfg(not(feature = "real-snapshot"))]
    let snapshots: Arc<dyn SnapshotEngine> =
        Arc::new(mixum_agentd::snapshot::MemorySnapshotEngine::new());

    let engine = Engine::with_snapshots(registry, sandbox, Some(snapshots.clone()));
    let broker = Broker::new("/run/mixum/display.sock");
    let _ = broker.start().await;
    // 接管状态机接线:任务运行→新窗口归 agent(接管需 claim);任务清零→回 free。
    let broker_for_tasks = broker.clone();
    engine.set_active_change_callback(Arc::new(move |active| {
        let b = broker_for_tasks.clone();
        tokio::spawn(async move { b.set_tasks_active(active).await });
    }));
    #[cfg_attr(not(feature = "real-snapshot"), allow(unused_mut))]
    let mut server = Server::new(config.clone(), authorizer, engine, broker, snapshots);
    #[cfg(feature = "real-snapshot")]
    {
        let assembled = Arc::get_mut(&mut server).expect("server 刚构造,无共享引用");
        assembled.sync = sync_backend;
    }
    let token_path = std::path::Path::new(&config.data_dir).join("admin-token");
    let token = match std::fs::read_to_string(&token_path) {
        Ok(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => {
            // bootstrap 管理令牌:10 年有效期,对齐 reference 的 MintToken 时长。
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default();
            let token = signer.mint(Claims {
                sub: "bootstrap-admin".into(),
                caps: vec![Cap::Admin],
                iat: now,
                exp: now + 10 * 365 * 24 * 3600,
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
