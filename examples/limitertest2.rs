// Limiter under a per-connection-throttled server (the starvation case):
// server caps each connection at 1 MiB/s, so workers read in slow 64 KiB
// pieces. 32 MiB at a 3 MiB/s global limit must take ~10.7 s (window 9–14 s).
use std::time::Instant;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).expect("url");
    let dir = args.get(2).expect("out dir");
    let out = std::path::Path::new(dir).join("limit2.bin");
    ndm::set_global_limit(3 << 20);
    let t = Instant::now();
    let dl = ndm::start(url, &out, 16, ndm::DEFAULT_BLOCK).expect("start");
    while !dl.is_terminal() {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let elapsed = t.elapsed().as_secs_f64();
    let p = dl.progress();
    assert_eq!(p.status, ndm::Status::Finished);
    let rate = p.total as f64 / elapsed / 1_048_576.0;
    println!("elapsed={elapsed:.1}s effective={rate:.2} MiB/s (limit 3.00)");
    assert!((9.0..=14.0).contains(&elapsed), "outside 9-14s window");
    println!("LIMITER2 OK");
}
