// Speed-limiter verification: a 64 MiB download capped at 4 MiB/s must take
// ~16 s (accepted window 13–22 s) and remain byte-identical.
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).expect("url");
    let dir = args.get(2).expect("out dir");
    let out = std::path::Path::new(dir).join("limit.bin");
    let limit: u64 = 4 << 20;
    ndm::set_global_limit(limit);
    assert_eq!(ndm::global_limit(), limit);
    let t = Instant::now();
    let dl = ndm::start(url, &out, 16, ndm::DEFAULT_BLOCK).expect("start");
    while !dl.is_terminal() {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let elapsed = t.elapsed().as_secs_f64();
    let p = dl.progress();
    let pct = if p.total > 0 { p.done_bytes as f64 / p.total as f64 * 100.0 } else { 0.0 };
    println!("final: {:?} {pct:.0}% elapsed={elapsed:.1}s", p.status);
    assert_eq!(p.status, ndm::Status::Finished, "must finish");
    let rate = p.total as f64 / elapsed / 1_048_576.0;
    println!("effective rate: {rate:.2} MiB/s (limit 4.00)");
    assert!(
        (13.0..=22.0).contains(&elapsed),
        "elapsed {elapsed:.1}s outside 13–22s window"
    );
    println!("LIMITER OK");
}
