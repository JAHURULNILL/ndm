// NDM CLI — thin shell over the engine library (same code the app uses).
use ndm::{fmt_mb, sha256_of, Status, DEFAULT_BLOCK};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: ndm <url> [-o file] [-c max_connections] [--block-mib N]");
        std::process::exit(2);
    }
    let url = args[1].clone();
    if url.starts_with('-') {
        // a flag in the URL position is a usage error, not a host name
        eprintln!("usage: ndm <url> [-o file] [-c connections] [--block-mib N]");
        std::process::exit(2);
    }
    let mut out: Option<PathBuf> = None;
    let mut max_conn = 16usize;
    let mut block_size = DEFAULT_BLOCK;
    // every option takes a value; a missing or malformed one is a usage
    // error (exit 2), never a panic
    let value = |args: &Vec<String>, i: usize, opt: &str| -> String {
        args.get(i + 1).cloned().unwrap_or_else(|| {
            eprintln!("option {opt} needs a value");
            std::process::exit(2);
        })
    };
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                out = Some(PathBuf::from(value(&args, i, "-o")));
                i += 2;
            }
            "-c" => {
                max_conn = value(&args, i, "-c").parse().unwrap_or_else(|_| {
                    eprintln!("invalid -c value");
                    std::process::exit(2);
                });
                i += 2;
            }
            "--block-mib" => {
                let mib: u64 = value(&args, i, "--block-mib")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("invalid --block-mib value");
                        std::process::exit(2);
                    });
                block_size = mib.checked_shl(20).unwrap_or_else(|| {
                    eprintln!("--block-mib value too large");
                    std::process::exit(2);
                });
                i += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                std::process::exit(2);
            }
        }
    }
    let out = out.unwrap_or_else(|| PathBuf::from(ndm::file_name_of(&url)));

    let probed = match ndm::probe(&url) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[ndm] {e}");
            std::process::exit(1);
        }
    };
    println!(
        "[ndm] size {:.1} MiB | range support: {}",
        fmt_mb(probed.total),
        if probed.range_ok { "yes" } else { "no" }
    );
    if !probed.range_ok {
        println!("[ndm] server cannot resume/accelerate — falling back to single stream");
    }

    let t0 = Instant::now();
    let dl = match ndm::start(&url, &out, max_conn, block_size) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[ndm] {e}");
            std::process::exit(1);
        }
    };
    // a resumed download announces itself through its first progress snapshot
    {
        let p = dl.progress();
        if p.done_bytes > 0 && probed.range_ok && probed.total > 0 {
            println!(
                "[ndm] RESUME: {:.1}% already on disk",
                100.0 * p.done_bytes as f64 / probed.total as f64
            );
        }
    }
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let p = dl.progress();
        print!(
            "\r[ndm] {} | {:.1}/{:.1} MiB | {:.2} MB/s | conns {}   ",
            if probed.total > 0 {
                format!("{:5.1}%", 100.0 * p.done_bytes as f64 / probed.total as f64)
            } else {
                "    —".to_string()
            },
            fmt_mb(p.done_bytes),
            fmt_mb(probed.total),
            p.speed_bps / (1u64 << 20) as f64,
            p.conns
        );
        let _ = io::stdout().flush();
        if dl.is_terminal() {
            break;
        }
    }
    let p = dl.progress();
    match p.status {
        Status::Finished => {
            println!(
                "\n[ndm] DONE {:.1} MiB in {:.2}s | fetched this run {:.1} MiB | avg {:.2} MB/s",
                fmt_mb(probed.total),
                t0.elapsed().as_secs_f64(),
                fmt_mb(p.fetched_session),
                fmt_mb(p.fetched_session) / t0.elapsed().as_secs_f64()
            );
            match sha256_of(&out) {
                Ok(h) => println!("[ndm] sha256 {h}"),
                Err(e) => eprintln!("[ndm] sha256 failed: {e}"),
            }
        }
        Status::Error => {
            println!(
                "\n[ndm] FAILED — progress saved, run again to resume ({})",
                p.error.unwrap_or_else(|| "unknown error".to_string())
            );
            std::process::exit(1);
        }
        Status::Cancelled => {
            println!("\n[ndm] CANCELLED — progress saved, run again to resume");
            std::process::exit(1);
        }
        _ => {}
    }
}
