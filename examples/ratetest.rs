// Rate-readout reproduction: samples Download::progress() exactly the
// way the app does (200 ms emit loop + the app's update_sample
// algorithm copied verbatim) and compares both the engine's own
// speed_bps and the app-sampler replica against the ground-truth rate
// computed from (time, done_bytes) over a trailing 5 s window.
// usage: ratetest <url> <out> <conns> <seconds>
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (url, out) = (&args[1], &args[2]);
    let conns: usize = args[3].parse().unwrap();
    let secs: u64 = args[4].parse().unwrap();
    let dl = ndm::start(url, std::path::Path::new(out), conns, ndm::DEFAULT_BLOCK).expect("start");
    // app sampler replica (main.rs update_sample + payload gate)
    let mut s: (Instant, u64, f64) = (Instant::now(), 0, 0.0);
    let t0 = Instant::now();
    let mut log: Vec<(f64, u64)> = Vec::new();
    while t0.elapsed() < Duration::from_secs(secs) {
        let p = dl.progress();
        // update_sample replica
        {
            let now = Instant::now();
            let dt = now.duration_since(s.0).as_secs_f64();
            if dt >= 0.2 {
                let inst = (p.done_bytes.saturating_sub(s.1)) as f64 / dt;
                s.2 = if s.2 == 0.0 { inst } else { 0.5 * s.2 + 0.5 * inst };
                if p.done_bytes != s.1 || p.status != ndm::Status::Downloading {
                    s.0 = now;
                }
                s.1 = p.done_bytes;
            }
        }
        let app_rate = s.2;
        let t = t0.elapsed().as_secs_f64();
        log.push((t, p.done_bytes));
        // ground truth: trailing 5 s slope of done_bytes
        let truth = {
            let from = log.iter().rev().find(|(tt, _)| t - tt >= 5.0).copied().unwrap_or(log[0]);
            if t - from.0 > 0.5 {
                (p.done_bytes.saturating_sub(from.1)) as f64 / (t - from.0)
            } else {
                0.0
            }
        };
        println!(
            "t={:5.1}s done={:>12} lib_speed={:>12.0} app_rate={:>12.0} truth5s={:>12.0}",
            t, p.done_bytes, p.speed_bps, app_rate, truth
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    dl.control.cancel();
}
