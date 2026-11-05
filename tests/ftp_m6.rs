//! M-6 witness (slice 7): an FTP server that answers 350 to
//! REST but streams RETR from byte 0 must NOT corrupt a
//! multi-connection download.
//!
//! Needs the lying server: python3 ~/workspace/ndm-rig/lying_ftp.py
//! serving a RANDOM bigftp.bin (random data — a periodic pattern
//! would mask offset corruption exactly at block boundaries).
//! Run: cargo test --test ftp_m6 -- --ignored --nocapture

use std::time::{Duration, Instant};

#[test]
#[ignore]
fn lying_rest_server_does_not_corrupt() {
    let url = "ftp://127.0.0.1:2121/bigftp.bin";
    let source = std::fs::read(
        std::path::Path::new(env!("HOME"))
            .join("workspace/ndm-rig/ftp-root/bigftp.bin"),
    )
    .expect("source file");
    let probe = ndm::probe(url).expect("probe");
    println!(
        "probe: total={} range_ok={} unknown_size={}",
        probe.total, probe.range_ok, probe.unknown_size
    );
    assert_eq!(probe.total as usize, source.len());

    let out = std::env::temp_dir().join("ndm-ftp-m6-out.bin");
    let _ = std::fs::remove_file(&out);
    let state = ndm::state_path_for(&out);
    let _ = std::fs::remove_file(&state);

    let dl = ndm::start_with_auth(url, &out, 8, 1024 * 1024, ndm::Auth::default())
        .expect("start");
    let t0 = Instant::now();
    loop {
        let p = dl.progress();
        if p.status == ndm::Status::Finished {
            break;
        }
        assert_ne!(p.status, ndm::Status::Error, "download errored");
        assert!(t0.elapsed() < Duration::from_secs(120), "download timed out");
        std::thread::sleep(Duration::from_millis(200));
    }
    let bytes = std::fs::read(&out).expect("read output");
    assert_eq!(bytes.len(), source.len(), "output size");
    assert!(
        bytes == source,
        "output differs from source — the lying REST corrupted the download \
         (first differing byte: {:?})",
        bytes.iter().zip(source.iter()).position(|(a, b)| a != b)
    );
    println!("all {} bytes true", bytes.len());
}
