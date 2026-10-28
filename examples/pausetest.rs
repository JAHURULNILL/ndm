// Pause/resume functional test for the engine library.
// usage: pausetest <url> <out> <expected_sha256>
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (url, out, expect) = (&args[1], &args[2], &args[3]);
    let dl = ndm::start(url, std::path::Path::new(out), 4, ndm::DEFAULT_BLOCK).expect("start");
    // wait until at least ~20% is down, then pause
    loop {
        let p = dl.progress();
        if p.total > 0 && p.done_bytes as f64 / p.total as f64 >= 0.20 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    dl.control.pause();
    std::thread::sleep(Duration::from_millis(400));
    let at_pause = dl.progress().done_bytes;
    std::thread::sleep(Duration::from_secs(2));
    let after_wait = dl.progress().done_bytes;
    println!("paused at {at_pause} bytes; after 2s: {after_wait} bytes");
    assert_eq!(at_pause, after_wait, "download kept moving while paused");
    assert_eq!(dl.status(), ndm::Status::Paused);
    println!("PAUSE OK (no progress for 2s, status=paused)");
    dl.control.resume();
    let t0 = Instant::now();
    loop {
        if dl.is_terminal() {
            break;
        }
        if t0.elapsed() > Duration::from_secs(120) {
            panic!("timeout waiting for finish");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(dl.status(), ndm::Status::Finished);
    let sha = ndm::sha256_of(std::path::Path::new(out)).expect("sha");
    println!("finished; sha256 {sha}");
    assert_eq!(&sha, expect, "sha mismatch");
    println!("RESUME + FINISH + SHA OK");
}
