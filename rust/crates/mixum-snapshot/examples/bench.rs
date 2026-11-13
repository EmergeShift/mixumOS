//! 快照引擎延迟基准:验证「前置快照毫秒级」与恢复延迟的量级。
//!
//! 运行:
//!   cargo run -p mixum-snapshot --example bench           # code + assets
//!   cargo run -p mixum-snapshot --example bench -- many   # 追加 10k 小文件轮廓
//!
//! 三种工作区轮廓:
//!   code   = 300 × 2 KiB 源码型小文件(嵌套目录)
//!   assets = 1 × 50 MiB 二进制大文件
//!   many   = 10k × 1 KiB 小文件(100 目录 × 100 文件),复现论文 §10(1) 的规模
//!
//! 每种测四件事:冷打包、无变更热快照(增量去重)、部分变更快照、恢复物化。
//!
//! `many` 默认不跑:改批量化之前冷打包要几十秒(每个块对象一次 fsync),
//! 每次都带上会让基准没法用。要对比批量化收益时显式传 `many`。

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
        "many" => {
            // 10k 个 1 KiB 文件:块数最多、单块最小,压测的是每块固定开销。
            for d in 0..100 {
                let sub = dir.join(format!("pkg{d}"));
                std::fs::create_dir_all(&sub).unwrap();
                for f in 0..100 {
                    let body = format!("// {d}/{f}\n{}", "let x = 1;\n".repeat(90));
                    std::fs::write(sub.join(format!("m{f}.rs")), body).unwrap();
                }
            }
        }
        _ => unreachable!(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut profiles: Vec<(&str, &str)> = vec![("code", "rewrite-half"), ("assets", "append")];
    if args.iter().any(|a| a == "many") {
        profiles.push(("many", "rewrite-percent"));
    }

    for (profile, mutate) in profiles {
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

        // 无变更:全部块去重,清单重扫仍要走一遍目录。`many` 的每次快照都很贵,
        // 少跑几轮,中位数仍然成立。
        let rounds = if profile == "many" { 3 } else { 10 };
        let mut hot = Vec::new();
        for _ in 0..rounds {
            let t = Instant::now();
            engine.snapshot(KIND_DISK, "无变更", true).unwrap();
            hot.push(ms(t));
        }

        // 部分变更:改一半小文件 / 给大文件追加 1 MiB / 改 1% 小文件。
        let t = Instant::now();
        match mutate {
            "rewrite-half" => {
                for i in (0..300).step_by(2) {
                    let sub = dir.path().join(format!("work/src/mod{}", i % 20));
                    std::fs::write(sub.join(format!("file{i}.rs")), format!("// changed {i}\n"))
                        .unwrap();
                }
            }
            "rewrite-percent" => {
                for d in 0..100 {
                    let sub = dir.path().join(format!("work/pkg{d}"));
                    std::fs::write(sub.join("m0.rs"), format!("// touched {d}\n")).unwrap();
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
