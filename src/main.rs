// NDM CLI — thin shell over the engine library (same code the app uses).
use ndm::{fmt_mb, sha256_of, Status, DEFAULT_BLOCK};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// F2: the mandatory-update latch, mirrored from the app's
/// `do_check_update` (src-tauri/src/main.rs) using the SAME public
/// feed helpers it uses (`ndm::updater::fetch_release` +
/// `version_cmp`) — the CLI has no long-lived bootstrap to arm a
/// latch in, so the decision is recomputed per invocation from
/// the feed, in the app's own order:
/// 1. an ill-formed feed version never blocks (the app returns an
///    error status and leaves the latch as it was — unarmed at
///    bootstrap);
/// 2. a feed offering nothing newer than the running copy never
///    blocks (the app's #19 recompute clears the latch there);
/// 3. blocking requires a non-empty `min_supported` strictly
///    above the running version AND a package this platform can
///    actually install (the app's usable-asset rule; on Windows
///    the CLI accepts either Windows asset rather than
///    distinguishing installer vs portable flavour by the
///    app-only `installed.flag` contract).
fn version_well_formed(v: &str) -> bool {
    let v = v.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    if v.is_empty() {
        return false;
    }
    v.split('.')
        .all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
}

fn mandatory_block(info: &ndm::updater::ReleaseInfo, current: &str) -> bool {
    if !version_well_formed(&info.version) {
        return false;
    }
    if ndm::updater::version_cmp(&info.version, current) != std::cmp::Ordering::Greater {
        return false;
    }
    if info.min_supported.is_empty()
        || ndm::updater::version_cmp(current, &info.min_supported)
            != std::cmp::Ordering::Less
    {
        return false;
    }
    #[cfg(windows)]
    return !info.win_url.is_empty() || !info.win_setup_url.is_empty();
    #[cfg(not(windows))]
    return !info.deb_url.is_empty();
}

/// Refuse a download start/resume while the feed's mandatory
/// update is outstanding. Fail-soft exactly like the app: a feed
/// that cannot be fetched arms nothing there, so it blocks
/// nothing here either (an offline machine keeps its CLI).
fn enforce_mandatory_update() {
    let Ok(info) = ndm::updater::fetch_release() else {
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    if mandatory_block(&info, current) {
        // the app's own latch wording (LATCH_BLOCKED), verbatim
        eprintln!("[ndm] downloads are paused until the required update is installed");
        eprintln!(
            "[ndm] this copy is {current}; the update feed requires at least {} (latest {}) — update NDM, then run this again",
            info.min_supported, info.version
        );
        std::process::exit(1);
    }
}

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
                // W3-15: 0 connections is a usage error, not a
                // silent clamp to 1 inside the engine
                if max_conn == 0 {
                    eprintln!("invalid -c value: connections must be at least 1");
                    std::process::exit(2);
                }
                i += 2;
            }
            "--block-mib" => {
                let mib: u64 = value(&args, i, "--block-mib")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("invalid --block-mib value");
                        std::process::exit(2);
                    });
                // W3-15: a 0 MiB block is a usage error here, not
                // a late download failure after probe + update check
                if mib == 0 {
                    eprintln!("invalid --block-mib value: block size must be at least 1 MiB");
                    std::process::exit(2);
                }
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
    // F2: every invocation of this CLI is a download start or
    // resume (there are no read-only subcommands to spare), so
    // the mandatory-update gate runs here — after usage errors
    // (exit 2 above) and before the probe/start it protects
    enforce_mandatory_update();
    // a CLI invocation IS a download start, not the File Info
    // dialog: it runs the engine's download probe (F5 — one HEAD,
    // tentative range verdict, no separate verify round trip)
    let probed = match ndm::probe_for_download(&url) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[ndm] {e}");
            std::process::exit(1);
        }
    };
    let out = out.unwrap_or_else(|| {
        PathBuf::from(
            probed
                .suggested_name
                .clone()
                .unwrap_or_else(|| ndm::file_name_of(&url)),
        )
    });
    println!(
        "[ndm] size {:.1} MiB | range support: {}",
        fmt_mb(probed.total),
        if probed.range_ok { "yes" } else { "no" }
    );
    if !probed.range_ok {
        println!("[ndm] server cannot resume/accelerate — falling back to single stream");
    }

    let t0 = Instant::now();
    // the probe above was made for this exact URL a moment ago, with
    // no state change in between: hand it to the start instead of
    // paying for a second identical probe inside the engine
    let dl = match ndm::start_with_auth_preprobed(
        &url,
        &out,
        max_conn,
        block_size,
        ndm::Auth::default(),
        Some(probed.clone()),
    ) {
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
    // is_terminal flips when the status settles, BEFORE the
    // supervisor's final journal save lands — exiting on it alone
    // could cut that save off and overstate what a resume would
    // find. Wait for the settled flag: the supervisor sets it only
    // after the final save, on every outcome. The wait itself has a
    // hard backstop (C-2): the engine's settlement is bounded, but
    // the CLI must never hang forever on any engine state — after
    // the deadline it reports and exits instead of waiting into
    // eternity on a wedged download.
    let settle_deadline = Instant::now() + Duration::from_secs(120);
    while !dl.is_settled() {
        if Instant::now() >= settle_deadline {
            eprintln!(
                "\n[ndm] FAILED — the engine did not settle within 120s; abandoning this run (run again to resume)"
            );
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(50));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A feed record; both platforms' assets are filled so the
    /// usable-asset arm holds on either CI platform.
    fn info(version: &str, min_supported: &str) -> ndm::updater::ReleaseInfo {
        ndm::updater::ReleaseInfo {
            version: version.to_string(),
            min_supported: min_supported.to_string(),
            notes: String::new(),
            deb_url: "https://example.invalid/ndm.deb".to_string(),
            deb_sha256: String::new(),
            win_url: "https://example.invalid/ndm.zip".to_string(),
            win_sha256: String::new(),
            win_setup_url: String::new(),
            win_setup_sha256: String::new(),
            win_x86_url: String::new(),
            win_x86_sha256: String::new(),
            win_x86_setup_url: String::new(),
            win_x86_setup_sha256: String::new(),
        }
    }

    #[test]
    fn blocks_when_below_min_and_a_newer_update_is_offered() {
        assert!(mandatory_block(&info("2.0.9", "2.0.9"), "2.0.8"));
        assert!(mandatory_block(&info("2.1.0", "2.0.9"), "2.0.8"));
    }

    #[test]
    fn does_not_block_at_or_above_the_minimum() {
        assert!(!mandatory_block(&info("2.0.9", "2.0.8"), "2.0.8"));
        assert!(!mandatory_block(&info("2.0.9", "2.0.7"), "2.0.8"));
        assert!(!mandatory_block(&info("2.0.9", ""), "2.0.8"));
    }

    #[test]
    fn does_not_block_when_the_feed_offers_nothing_newer() {
        // the app's #19 rule: no newer version on the feed means
        // no latch, even against a (stale/contradictory) minimum
        assert!(!mandatory_block(&info("2.0.8", "2.0.9"), "2.0.8"));
        assert!(!mandatory_block(&info("2.0.7", "2.0.9"), "2.0.8"));
    }

    #[test]
    fn does_not_block_without_a_usable_asset_or_on_garbage() {
        let mut bare = info("2.0.9", "2.0.9");
        bare.deb_url.clear();
        bare.win_url.clear();
        assert!(!mandatory_block(&bare, "2.0.8"));
        assert!(!mandatory_block(&info("", "2.0.9"), "2.0.8"));
        assert!(!mandatory_block(&info("2.x", "2.0.9"), "2.0.8"));
    }
}
