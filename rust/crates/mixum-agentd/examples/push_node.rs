//! 把本地快照节点推送到远端 mixumd/agentd(协议 §6)。
//!
//! 用法示例(向已运行的远端推最近一个非 genesis 节点):
//! ```text
//! cargo run -p mixum-agentd --features real-snapshot --example push_node -- \
//!   --data-dir /tmp/src/data --work-root /tmp/src/work \
//!   --remote wss://127.0.0.1:17601 --token <远端能力令牌(含 snap.sync)>
//! ```
//! 远端自签证书直接跳过校验(与 reference client.go 一致)。

use mixum_agentd::snapshot::{RealSnapshotEngine, SnapshotEngine};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut data_dir = String::new();
    let mut work_root = String::new();
    let mut remote = String::new();
    let mut token = String::new();
    let mut node = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => data_dir = args.next().unwrap_or_default(),
            "--work-root" => work_root = args.next().unwrap_or_default(),
            "--remote" => remote = args.next().unwrap_or_default(),
            "--token" => token = args.next().unwrap_or_default(),
            "--node" => node = args.next().unwrap_or_default(),
            other => return Err(format!("未知参数:{other}").into()),
        }
    }
    if remote.is_empty() || token.is_empty() || data_dir.is_empty() || work_root.is_empty() {
        return Err("需要 --data-dir --work-root --remote --token".into());
    }

    let engine = RealSnapshotEngine::open(mixum_snapshot::EngineOptions {
        data_dir: data_dir.clone().into(),
        root: work_root.into(),
        skip: vec![".mixum".into()],
        encrypt: false,
    })?;
    let node_id = if node.is_empty() {
        // 缺省取最新节点(树中 createdAt 最大者)。
        let tree = engine.tree().await?;
        let mut nodes: Vec<_> = tree.nodes;
        nodes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        nodes.first().map(|n| n.id.clone()).ok_or("本地树为空")?
    } else {
        node
    };

    let stats = mixum_agentd::sync::push_node(
        &engine.shared(),
        &node_id,
        &remote,
        &token,
        &nix::unistd::gethostname()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
    .await?;
    println!(
        "已推送 {} → {remote} chunks={} bytes={} dedup={}",
        stats.node, stats.chunks_sent, stats.bytes_sent, stats.dedup_ratio
    );
    Ok(())
}
