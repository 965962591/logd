//! 生成 N GB 的合成日志，用来做索引/筛选的性能验收。
//!
//! ```text
//! cargo run --release -p logd-core --example gen_big_log -- 10 D:/tmp/big.log
//! ```
//!
//! 行的形态照抄真实 AE logcat：时间戳 + pid/tid + tag + 载荷，
//! 并按 `tat/ae_log.tat` 里的关键字分布掺入命中行，这样筛选耗时才有参考价值。

use std::io::{BufWriter, Write};

/// 掺进去的关键字，取自 tat/ae_log.tat。命中率约 1/12。
const HITS: &[&str] = &[
    "updateAEInfo2ISP  i4AOEGain=1024 i4ISPGain=256",
    "ae_mgr : [doAEMode] eAEMode=0",
    "AEtable idx=42 exp=33000 gain=1024",
    "MagicNum: 0x1a2b3c4d",
    "doafae converged=1",
    "doCapAE flash=0 target=180",
    "Final AE Target = 176",
    "[getAEPLineMappingID] id=3",
    "AEINIT done in 12ms",
    "Magic: 998877",
    "takepicture shot2shot=310ms",
    "StrobeDrvFlashlight: setOnOff() on=1",
];

const TAGS: &[&str] = &[
    "AeAlgo",
    "Hal3Av3",
    "MtkCam",
    "CamAdapter",
    "ISP",
    "SensorDrv",
];

fn main() {
    let mut args = std::env::args().skip(1);
    let gb: f64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| usage());
    let path = args.next().unwrap_or_else(|| usage());

    let target = (gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let file = std::fs::File::create(&path).expect("创建输出文件失败");
    // 大块缓冲，别让 write 系统调用成为瓶颈
    let mut w = BufWriter::with_capacity(8 << 20, file);

    let mut written = 0u64;
    let mut line = 0u64;
    let mut buf = String::with_capacity(256);
    let started = std::time::Instant::now();

    while written < target {
        buf.clear();
        let ms = line * 7 % 1000;
        let sec = (line / 143) % 60;
        let min = (line / 8580) % 60;
        let tag = TAGS[(line % TAGS.len() as u64) as usize];
        let pid = 1000 + (line % 40);
        let tid = 2000 + (line % 97);

        if line % 12 == 0 {
            let hit = HITS[(line / 12 % HITS.len() as u64) as usize];
            buf.push_str(&format!(
                "01-01 12:{min:02}:{sec:02}.{ms:03}  {pid}  {tid} I {tag:<10}: {hit}\n"
            ));
        } else {
            buf.push_str(&format!(
                "01-01 12:{min:02}:{sec:02}.{ms:03}  {pid}  {tid} D {tag:<10}: \
                 frame={line} req={} status=ok latency={}us\n",
                line % 65536,
                line % 8000
            ));
        }

        w.write_all(buf.as_bytes()).expect("写入失败");
        written += buf.len() as u64;
        line += 1;

        if line % (1 << 22) == 0 {
            eprint!(
                "\r{:.2} / {gb:.2} GB  {line} 行  {:.0} MB/s",
                written as f64 / (1 << 30) as f64,
                written as f64 / (1 << 20) as f64 / started.elapsed().as_secs_f64()
            );
        }
    }
    w.flush().expect("flush 失败");

    eprintln!(
        "\n完成：{path}  {:.2} GB  {line} 行  耗时 {:.1}s",
        written as f64 / (1 << 30) as f64,
        started.elapsed().as_secs_f64()
    );
}

fn usage() -> ! {
    eprintln!("用法: gen_big_log <GB> <输出路径>");
    eprintln!("例如: cargo run --release -p logd-core --example gen_big_log -- 10 D:/tmp/big.log");
    std::process::exit(2)
}
