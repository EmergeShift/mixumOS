//! mixum-operator 入口:连接集群并启动调谐循环。

use mixum_operator::reconciler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // workspace 的 rustls 关掉 default 只启用 ring,而依赖树可能带入
    // aws-lc-rs;进程级显式钉住 provider(幂等)。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = kube::Client::try_default().await?;
    let version = client.apiserver_version().await?;
    tracing::info!("已连接 apiserver {}.{},开始监听 MixumTask", version.major, version.minor);

    // v1 单命名空间(mixumos);多租户随阶段 5 扩展。
    reconciler::run(client, "mixumos").await?;
    Ok(())
}
