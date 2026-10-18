//! 快照引擎延迟基准:验证「前置快照毫秒级」与恢复延迟的量级。
//!
//! 运行:
//!   cargo run -p mixum-snapshot --example bench
//!
//! 两种工作区轮廓:
//!   code   = 300 × 2 KiB 源码型小文件(嵌套目录)
//!   assets = 1 × 50 MiB 二进制大文件
//! 每种测四件事:冷打包、无变更热快照(增量去重)、部分变更快照、恢复物化。

use mixum_snapshot::{Engine, EngineOptions, KIND_DISK};
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn workspace(dir: &std::path::Path, profile: &str) {
    match profile {
        "code" => {
            for i in 0..300 {
                let sub = dir.join(format!("src/mod{}", i % 20));
                std::fs::create_dir_all(&sub).unwrap();
                let body = format!("// module {i}\n{}", "fn demo() {}\n".repeat(60));
                std::fs::write(sub.join(format!("file{i}.rs")), body).unwrap();
            }
        }
        "assets" => {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            let blob: Vec<u8> = (0..50 * 1024 * 1024).map(|i| (i / 7) as u8).collect();
            std::fs::write(dir.join("bin/blob.bin"), blob).unwrap();
        }
        _ => unreachable!(),
    }
}

fn main() {
    for (profile, mutate) in [
        ("code", "rewrite-half"),
        ("assets", "append"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(EngineOptions {
            data_dir: dir.path().join("snap"),
            root: dir.path().join("work"),
            skip: vec![],
            encrypt: false,
        })
        .unwrap();
        workspace(&dir.path().join("work"), profile);

        let t = Instant::now();
        let n1 = engine.snapshot(KIND_DISK, "冷打包", false).unwrap();
        let cold = ms(t);

        // 无变更:全部块去重,清单重扫仍要走一遍目录。
        let mut hot = Vec::new();
        for _ in 0..10 {
            let t = Instant::now();
            engine.snapshot(KIND_DISK, "无变更", true).unwrap();
            hot.push(ms(t));
        }

        // 部分变更:改一半小文件 / 给大文件追加 1 MiB。
        let t = Instant::now();
        match mutate {
            "rewrite-half" => {
                for i in (0..300).step_by(2) {
                    let sub = dir.path().join(format!("work/src/mod{}", i % 20));
                    std::fs::write(sub.join(format!("file{i}.rs")), format!("// changed {i}\n"))
                        .unwrap();
                }
            }
            _ => {
                let mut blob = std::fs::read(dir.path().join("work/bin/blob.bin")).unwrap();
                blob.extend_from_slice(&vec![0xAAu8; 1024 * 1024]);
                std::fs::write(dir.path().join("work/bin/blob.bin"), blob).unwrap();
            }
        }
        let n3 = engine.snapshot(KIND_DISK, "部分变更", false).unwrap();
        let delta = ms(t);

        // 恢复物化:回到「冷打包」节点。
        let t = Instant::now();
        engine.restore(&n1.id, "inplace", "").unwrap();
        let restore = ms(t);

        let med = median(&mut hot);
        println!(
            "{profile:>6}: 冷打包 {cold:8.1} ms | 无变更(中位) {med:6.1} ms | 部分变更 {delta:8.1} ms | 恢复 {restore:8.1} ms   [{} → {}]",
            n1.id,
            n3.id,
        );
    }
}
