//! Self-update manifest fetching and anonymous usage counting.
//!
//! The release manifest is a small JSON file published alongside the
//! releases. Until the project is published, checks simply fail soft.
//! The counter is a privacy-free design by construction: the client
//! sends nothing but an empty GET to a shared counting endpoint —
//! no identifier, no IP logging on our side, no payload at all.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// The manifest travels WITH each release as an asset, fetched via
/// GitHub's always-latest download route — plain github.com, which
/// is reachable on networks where the raw host below times out
/// (seen live: a user's ISP blackholing raw.githubusercontent.com
/// while github.com itself worked fine, which silently killed every
/// update check). The repo copy stays as a fallback mirror.
pub const MANIFEST_URL: &str =
    "https://github.com/JAHURULNILL/ndm/releases/latest/download/release.json";
pub const MANIFEST_URL_FALLBACK: &str =
    "https://raw.githubusercontent.com/JAHURULNILL/ndm/main/release.json";
const COUNT_BASE: &str = "https://abacus.jasoncameron.dev";
const COUNT_NS: &str = "ndm-nill";

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    /// oldest version allowed to keep running; below it, updating
    /// is mandatory
    #[serde(default)]
    pub min_supported: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub deb_url: String,
    #[serde(default)]
    pub deb_sha256: String,
    /// Windows portable build (a ZIP the app swaps in itself)
    #[serde(default)]
    pub win_url: String,
    #[serde(default)]
    pub win_sha256: String,
    /// Windows installer (setup .exe, run silently)
    #[serde(default)]
    pub win_setup_url: String,
    #[serde(default)]
    pub win_setup_sha256: String,
    /// 32-bit Windows portable build (ZIP). Selected only by
    /// x86 builds; empty in feeds that predate the x86 tier.
    #[serde(default)]
    pub win_x86_url: String,
    #[serde(default)]
    pub win_x86_sha256: String,
    /// 32-bit Windows installer (setup .exe)
    #[serde(default)]
    pub win_x86_setup_url: String,
    #[serde(default)]
    pub win_x86_setup_sha256: String,
}

/// One parsed version: numeric components (saturating — a
/// component past u128 keeps a huge value instead of collapsing
/// to 0, W3-7) plus the pre-release suffix after the first '-',
/// if any ("2.3.2-beta" → components [2,3,2], suffix "beta").
fn parse_version(v: &str) -> (Vec<u128>, Option<String>) {
    let v = v.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, suffix) = match v.split_once('-') {
        Some((c, s)) if !s.is_empty() => (c, Some(s.to_string())),
        _ => (v, None),
    };
    let comps = core
        .split('.')
        .map(|p| {
            let mut n: u128 = 0;
            for c in p.chars().take_while(|c| c.is_ascii_digit()) {
                n = n
                    .saturating_mul(10)
                    .saturating_add(c as u128 - '0' as u128);
            }
            n
        })
        .collect();
    (comps, suffix)
}

/// Compare dotted numeric versions ("1.5.0" > "1.0.7").
/// A pre-release suffix sorts BEFORE the bare release
/// ("2.3.2-beta" < "2.3.2", semver-style, W3-7); two suffixes
/// compare lexically. Well-formed plain releases compare
/// exactly as before.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (va, sa) = parse_version(a);
    let (vb, sb) = parse_version(b);
    let n = va.len().max(vb.len());
    for i in 0..n {
        let (x, y) = (va.get(i).copied().unwrap_or(0), vb.get(i).copied().unwrap_or(0));
        if x != y {
            return x.cmp(&y);
        }
    }
    match (sa, sb) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// W3-8: feed-supplied URLs must not downgrade the update chain
/// to plain http. https is required for any remote host; plain
/// http is tolerated only for loopback, which no network
/// attacker can intercept (and which the test fixtures and the
/// debug manifest override run on).
pub fn update_url_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(h) = host.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn require_update_scheme(url: &str) -> Result<(), String> {
    if update_url_allowed(url) {
        Ok(())
    } else {
        Err(format!(
            "refused a non-https update URL ({url}): update packages and manifests are fetched over https only"
        ))
    }
}

/// Overall budget for ONE whole manifest check, across every
/// route it tries. The shared agent's per-operation timeouts
/// (15 s connect / 30 s per read) cannot bound a check on their
/// own: DNS resolution is untimed there, a server that dribbles
/// one byte at a time never trips a per-read timeout, and two
/// routes are tried back to back — on a blackholed network the
/// launch check once froze the app's window for minutes. The
/// manifest fetch therefore runs on its OWN agent and every
/// request carries the time left from this single budget, so no
/// caller of fetch_release can hang past it.
pub const MANIFEST_DEADLINE: Duration = Duration::from_secs(60);

/// The manifest fetch's dedicated agent — same shape as the
/// shared build_agent (proxy, UA, per-op timeouts) but private
/// to this module, so the deadline discipline below can never
/// leak into the download engine's agent.
fn manifest_agent() -> ureq::Agent {
    crate::apply_proxy(
        ureq::AgentBuilder::new().redirect_auth_headers(ureq::RedirectAuthHeaders::Never),
    )
    .timeout_connect(Duration::from_secs(15))
    .timeout_read(Duration::from_secs(30))
    .timeout_write(Duration::from_secs(30))
    .user_agent(crate::BROWSER_UA)
    .build()
}

/// Fetch one manifest URL's body as text. `timeout` is the
/// request's overall deadline (DNS through the last body byte),
/// not a per-operation budget — a dribbling body fails on it
/// even though every individual read succeeds. A response
/// carrying a real Content-Encoding is rejected (W1 F11): the
/// update paths request `identity` and ureq here does not
/// decode, so a gzipped manifest would otherwise be parsed as
/// garbage — or, worse on the download path, saved as the
/// package — instead of failing cleanly like every other
/// transfer path in the crate.
fn fetch_manifest_body(agent: &ureq::Agent, url: &str, timeout: Duration) -> Result<String, String> {
    require_update_scheme(url)?;
    let resp = agent
        .get(url)
        .timeout(timeout)
        .set("Accept-Encoding", "identity")
        .call()
        .map_err(|e| format!("update check failed: {e}"))?;
    // The scheme rule applies to where the fetch ENDED, not only
    // where it started: ureq follows redirects internally and no
    // hop is re-checked, so an https manifest URL could 302 to a
    // plain-http (or otherwise refused) address and its body
    // would be parsed as the feed. The final URL is validated
    // before the body is read; a refused landing discards it.
    require_update_scheme(resp.get_url())
        .map_err(|e| format!("update check failed: {e}"))?;
    if let Some(enc) = crate::response_encoding(&resp) {
        return Err(format!(
            "update check failed: server answered with Content-Encoding: {enc}"
        ));
    }
    // Read through the same streaming cap discipline as
    // download_file: a server that keeps sending must not be
    // read into memory without limit.
    let mut body = Vec::new();
    resp.into_reader()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("update check failed: {e}"))?;
    if body.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "update check failed: manifest exceeds the {} MiB size cap",
            MAX_MANIFEST_BYTES / (1024 * 1024)
        ));
    }
    String::from_utf8(body).map_err(|e| format!("update check failed: {e}"))
}

pub fn fetch_release() -> Result<ReleaseInfo, String> {
    // The env override exists for local update-flow tests; a release
    // build must only ever trust the pinned manifest addresses.
    let mut urls: Vec<String> = Vec::new();
    if cfg!(debug_assertions) {
        if let Ok(u) = std::env::var("NDM_UPDATE_URL") {
            urls.push(u);
        }
    }
    urls.push(MANIFEST_URL.to_string());
    urls.push(MANIFEST_URL_FALLBACK.to_string());
    fetch_release_within(&urls, MANIFEST_DEADLINE)
}

/// fetch_release against an explicit route list under one
/// overall budget shared by every route: each request gets the
/// time still left, and once the budget is spent no further
/// route is even attempted. Split out so the deadline is
/// testable against local servers.
fn fetch_release_within(urls: &[String], budget: Duration) -> Result<ReleaseInfo, String> {
    // Gather EVERY route's answer within the shared budget and
    // keep the HIGHEST version on offer. The old shape returned
    // the first route that merely parsed: the routes are mirrors
    // of one feed, but a stale mirror — or a release whose
    // manifest attach lagged on one route — silently won by
    // answering first, and every installed app behind it read
    // "up to date" through even a mandatory update. Equal
    // versions keep the earlier route's copy (route order is
    // preference order). A route that fails or won't parse is
    // noted and the next is still asked, as before.
    let agent = manifest_agent();
    let started = Instant::now();
    let mut best: Option<ReleaseInfo> = None;
    let mut last_err = String::new();
    for url in urls {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            if best.is_none() {
                last_err = "update check failed: timed out".to_string();
            }
            break;
        }
        // Once one route has answered, the rest are only asked
        // whether they know a NEWER version — a courtesy question
        // with a short grace, so a blackholed mirror cannot stall
        // every launch check for the whole budget. Before any
        // answer, a route gets whatever budget is left, exactly
        // as patient as the check has always been.
        let route_budget = if best.is_some() {
            remaining.min(Duration::from_secs(10))
        } else {
            remaining
        };
        let fetched = fetch_manifest_body(&agent, url, route_budget);
        match fetched {
            Ok(body) => match serde_json::from_str::<ReleaseInfo>(&body) {
                Ok(info) => {
                    let is_newer = match &best {
                        None => true,
                        Some(b) => {
                            version_cmp(&info.version, &b.version)
                                == std::cmp::Ordering::Greater
                        }
                    };
                    if is_newer {
                        best = Some(info);
                    }
                }
                Err(e) => last_err = format!("update info was unreadable: {e}"),
            },
            Err(e) => last_err = e,
        }
    }
    match best {
        Some(info) => Ok(info),
        None => Err(last_err),
    }
}

/// Absolute ceiling on one update package when the server gives
/// no trustworthy expected size, enforced WHILE streaming (W1
/// F11): the SHA-256 is only checked after the whole body lands,
/// so without a cap a broken or hostile source could fill the
/// disk before the hash ever gets a say — the exact hole the
/// components 256 MiB cap closed for the engine fetch. Real
/// update packages are tens of MiB; 2 GiB is generous headroom,
/// not a limit a genuine release can brush against.
pub const MAX_UPDATE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Overall deadline for ONE package download. The shared agent's
/// per-read timeout (30 s) cannot bound the whole transfer: a
/// server dribbling one byte per read-never-quite-30 s stretches
/// a tens-of-MiB package across hours of "downloading". Thirty
/// minutes is far beyond a genuine package on a slow link (it
/// covers the full 2 GiB ceiling at ~1.1 MB/s, and real packages
/// are tens of MiB) while still ending a dribble — the same
/// overall-deadline discipline as the manifest's (MANIFEST_DEADLINE).
pub const PACKAGE_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// The manifest is a small JSON record; anything past 1 MiB is
/// not a manifest, however it was served.
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Zip-bomb ceilings for extract_zip: a SHA-verified package can
/// still decompress without bound, so the entry count and the
/// total decompressed size are capped while extracting.
pub const MAX_ZIP_ENTRIES: usize = 10_000;
pub const MAX_ZIP_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

/// The mid-stream abort threshold for an update download whose
/// expected size is `expected_total` (the response's
/// Content-Length; 0 when the server did not send one, e.g. a
/// chunked body). With an expected size the cap is that size plus
/// a margin — the larger of 1 MiB or 10% — so a body that keeps
/// streaming well past what it announced is cut off; without one,
/// the absolute `ceiling` applies.
fn download_cap(expected_total: u64, ceiling: u64) -> u64 {
    if expected_total > 0 {
        expected_total.saturating_add((expected_total / 10).max(1024 * 1024))
    } else {
        ceiling
    }
}

/// Download `url` to `dest`, verifying the SHA-256 against the
/// feed's published hash. Progress reports (downloaded, total).
/// Used for the one-click update package; a hash mismatch deletes
/// the file and errors. An EMPTY expected hash is a broken feed,
/// not a weaker check: the install is refused outright (F-7) —
/// an update channel that cannot name its package's hash must
/// never silently downgrade to a length comparison.
pub fn download_file(
    url: &str,
    dest: &Path,
    sha256_expected: &str,
    on_progress: &dyn Fn(u64, u64),
) -> Result<(), String> {
    download_file_with_ceiling(url, dest, sha256_expected, on_progress, MAX_UPDATE_BYTES)
}

fn download_file_with_ceiling(
    url: &str,
    dest: &Path,
    sha256_expected: &str,
    on_progress: &dyn Fn(u64, u64),
    ceiling: u64,
) -> Result<(), String> {
    require_update_scheme(url)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create folder: {e}"))?;
    }
    let agent = crate::build_agent();
    let resp = agent
        .get(url)
        .set("Accept-Encoding", "identity")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    // As on the manifest fetch: the scheme rule is re-checked
    // against the FINAL URL after any redirects — an https
    // package URL that 302s to plain http is refused before a
    // single body byte is saved (the hash check below is the
    // integrity backstop, never a reason to accept a downgrade).
    require_update_scheme(resp.get_url())
        .map_err(|e| format!("download failed: {e}"))?;
    if let Some(enc) = crate::response_encoding(&resp) {
        return Err(format!(
            "download failed: server answered with Content-Encoding: {enc}"
        ));
    }
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if total > ceiling {
        return Err(format!(
            "the update package is larger than the {} MiB size cap — the download was refused",
            ceiling / (1024 * 1024)
        ));
    }
    let cap = download_cap(total, ceiling);
    let tmp = dest.with_extension("part");
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("cannot write file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    let mut last: u64 = 0;
    let started = Instant::now();
    let result = loop {
        // the overall package deadline, checked per chunk (each
        // read is itself bounded by the agent's per-read timeout,
        // so this fires promptly when the budget is spent)
        if started.elapsed() > PACKAGE_DEADLINE {
            break Err(format!(
                "the update download took longer than {} minutes and was aborted — the server is sending too slowly",
                PACKAGE_DEADLINE.as_secs() / 60
            ));
        }
        match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                hasher.update(&buf[..n]);
                if let Err(e) = file.write_all(&buf[..n]) {
                    break Err(format!("cannot write file: {e}"));
                }
                downloaded += n as u64;
                if downloaded > cap {
                    // the error path below removes the .part file:
                    // an aborted over-cap download leaves nothing
                    break Err(format!(
                        "the update exceeded the size cap for this download ({} bytes) — the download was aborted and discarded",
                        cap
                    ));
                }
                if downloaded - last >= 256 * 1024 {
                    last = downloaded;
                    on_progress(downloaded, total);
                }
            }
            Err(e) => break Err(format!("download interrupted: {e}")),
        }
    };
    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if sha256_expected.is_empty() {
        // F-7: no published hash — the feed is invalid for install
        // purposes. Discard the bytes and refuse; never fall back
        // to a Content-Length comparison as if it were integrity.
        let _ = std::fs::remove_file(&tmp);
        return Err(
            "the update feed carries no integrity hash for this package, so the update was refused"
                .to_string(),
        );
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != sha256_expected.to_lowercase() {
        let _ = std::fs::remove_file(&tmp);
        return Err("the update failed its integrity check and was discarded".to_string());
    }
    // The destination is a fixed name in the updates folder, so
    // every later update renames onto the previous attempt's
    // file — on Windows that rename fails when the old file is
    // read-only or still held by a just-finished installer/AV
    // scan. Clear it first: the verified bytes are already safe
    // in `tmp`, and a removal failure surfaces at the rename
    // with the same error either way.
    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("cannot install file: {e}"))?;
    on_progress(downloaded, total.max(downloaded));
    Ok(())
}

/// Extract a ZIP archive into `dest_dir`, returning the relative
/// paths written. Used by the Windows portable self-update: the
/// archive is the published portable tree, extracted to a staging
/// folder beside the installation and swapped in by a detached
/// helper. Entries with absolute paths or `..` components are
/// refused — a hostile or corrupt archive must never write outside
/// the staging folder.
pub fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    extract_zip_with_caps(
        zip_path,
        dest_dir,
        MAX_ZIP_ENTRIES,
        MAX_ZIP_DECOMPRESSED_BYTES,
    )
}

fn extract_zip_with_caps(
    zip_path: &Path,
    dest_dir: &Path,
    max_entries: usize,
    max_decompressed: u64,
) -> Result<Vec<std::path::PathBuf>, String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("cannot read the update package: {e}"))?;
    let mut arch = zip::ZipArchive::new(file)
        .map_err(|e| format!("the update package is unreadable: {e}"))?;
    if arch.len() > max_entries {
        return Err(format!(
            "the update package contains more than the {max_entries} entry cap and was discarded"
        ));
    }
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("cannot create the update folder: {e}"))?;
    let mut written = Vec::new();
    let mut decompressed: u64 = 0;
    for i in 0..arch.len() {
        let mut entry = arch
            .by_index(i)
            .map_err(|e| format!("the update package is unreadable: {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err("the update package contains an unsafe path and was discarded".to_string());
        };
        let rel = rel.to_path_buf();
        let out = dest_dir.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| format!("cannot create folder: {e}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create folder: {e}"))?;
        }
        let mut w = std::fs::File::create(&out)
            .map_err(|e| format!("cannot write file: {e}"))?;
        // The cap is enforced during the copy, not from the
        // declared entry size: a lying header must not widen it.
        let limit = max_decompressed.saturating_sub(decompressed) + 1;
        let n = std::io::copy(&mut entry.by_ref().take(limit), &mut w)
            .map_err(|e| format!("cannot extract the update package: {e}"))?;
        decompressed += n;
        if decompressed > max_decompressed {
            return Err(format!(
                "the update package decompresses past the {} MiB cap and was discarded",
                max_decompressed / (1024 * 1024)
            ));
        }
        written.push(rel);
    }
    Ok(written)
}

/// Increment a shared counter (fire-and-forget, returns success).
/// One hit per installation is all the client ever sends.
pub fn count_hit(counter: &str) -> bool {
    let url = format!("{COUNT_BASE}/hit/{COUNT_NS}/{counter}");
    crate::build_agent().get(&url).call().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ndm-test-updater-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    /// Serve one raw HTTP response (head + body, exactly as given)
    /// on a loopback port and return the URL.
    fn serve_raw(response: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = sock.write_all(&response);
            let _ = sock.flush();
        });
        format!("http://127.0.0.1:{port}/pkg")
    }

    fn serve_plain(body: &[u8], extra_headers: &str) -> String {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
            body.len()
        );
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        serve_raw(raw)
    }

    fn serve_chunked(body: &[u8]) -> String {
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            .to_vec();
        for chunk in body.chunks(64 * 1024) {
            raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            raw.extend_from_slice(chunk);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        serve_raw(raw)
    }

    #[test]
    fn feed_with_x86_keys_parses_all_fourteen() {
        let info: ReleaseInfo = serde_json::from_str(
            r#"{
                "version": "2.2.0",
                "min_supported": "2.0.0",
                "notes": "n",
                "deb_url": "https://x/ndm.deb",
                "deb_sha256": "d",
                "win_url": "https://x/win.zip",
                "win_sha256": "w",
                "win_setup_url": "https://x/setup.exe",
                "win_setup_sha256": "s",
                "win_x86_url": "https://x/win-x86.zip",
                "win_x86_sha256": "w86",
                "win_x86_setup_url": "https://x/setup-x86.exe",
                "win_x86_setup_sha256": "s86"
            }"#,
        )
        .unwrap();
        assert_eq!(info.win_x86_url, "https://x/win-x86.zip");
        assert_eq!(info.win_x86_sha256, "w86");
        assert_eq!(info.win_x86_setup_url, "https://x/setup-x86.exe");
        assert_eq!(info.win_x86_setup_sha256, "s86");
        assert_eq!(info.win_url, "https://x/win.zip");
    }

    #[test]
    fn feed_without_x86_keys_parses_with_x86_empty() {
        // a pre-x86 10-key feed must still parse; the x86 fields
        // default empty, which the per-arch asset rule reads as
        // "no usable package for an x86 build"
        let info: ReleaseInfo = serde_json::from_str(
            r#"{
                "version": "2.1.1",
                "min_supported": "2.1.0",
                "notes": "",
                "deb_url": "https://x/ndm.deb",
                "deb_sha256": "d",
                "win_url": "https://x/win.zip",
                "win_sha256": "w",
                "win_setup_url": "https://x/setup.exe",
                "win_setup_sha256": "s"
            }"#,
        )
        .unwrap();
        assert!(info.win_x86_url.is_empty());
        assert!(info.win_x86_sha256.is_empty());
        assert!(info.win_x86_setup_url.is_empty());
        assert!(info.win_x86_setup_sha256.is_empty());
    }

    #[test]
    fn version_cmp_saturates_huge_components_and_orders_prereleases() {
        use std::cmp::Ordering::*;
        // W3-7: a component beyond u64 used to collapse to 0 and
        // sort the whole version as 0.x
        assert_eq!(version_cmp("99999999999999999999999999.0", "1.0"), Greater);
        assert_eq!(version_cmp("1.0", "99999999999999999999999999.0"), Less);
        // a pre-release is LESS than its own release (semver-ish)
        assert_eq!(version_cmp("2.3.2-beta", "2.3.2"), Less);
        assert_eq!(version_cmp("2.3.2", "2.3.2-beta"), Greater);
        assert_eq!(version_cmp("2.3.2-rc1", "2.3.3"), Less);
        assert_eq!(version_cmp("2.3.2-beta", "2.3.2-beta"), Equal);
        // well-formed behaviour is unchanged
        assert_eq!(version_cmp("2.3.10", "2.3.2"), Greater);
        assert_eq!(version_cmp("2.3", "2.3.0"), Equal);
        assert_eq!(version_cmp("2.3.2.1", "2.3.2"), Greater);
        assert_eq!(version_cmp("v2.3.2", "2.3.2"), Equal);
    }

    #[test]
    fn update_urls_must_be_https_except_loopback() {
        // W3-8: a feed-supplied http URL to a remote host is
        // refused before any network traffic happens
        assert!(update_url_allowed("https://example.com/pkg"));
        assert!(update_url_allowed("http://127.0.0.1:8080/pkg"));
        assert!(update_url_allowed("http://localhost/pkg"));
        assert!(!update_url_allowed("http://example.com/pkg"));
        assert!(!update_url_allowed("http://192.0.2.1/pkg"));
        assert!(!update_url_allowed("ftp://example.com/pkg"));
        let dir = temp_dir("scheme");
        let dest = dir.join("pkg.bin");
        let err = download_file("http://192.0.2.1/pkg", &dest, "abc", &|_, _| {}).unwrap_err();
        assert!(err.contains("https"), "unexpected error: {err}");
        assert!(!dest.exists());
        let agent = crate::build_agent();
        let err = fetch_manifest_body(&agent, "http://192.0.2.1/release.json", Duration::from_secs(5))
            .unwrap_err();
        assert!(err.contains("https"), "unexpected error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_cap_math() {
        // no expected size -> the absolute ceiling decides
        assert_eq!(download_cap(0, MAX_UPDATE_BYTES), MAX_UPDATE_BYTES);
        assert_eq!(download_cap(0, 1_234), 1_234);
        // expected size -> expected + the larger of 1 MiB / 10%
        assert_eq!(download_cap(1_000, MAX_UPDATE_BYTES), 1_000 + 1024 * 1024);
        assert_eq!(
            download_cap(100 * 1024 * 1024, MAX_UPDATE_BYTES),
            110 * 1024 * 1024
        );
    }

    #[test]
    fn download_installs_a_small_verified_package() {
        let dir = temp_dir("ok");
        let dest = dir.join("pkg.bin");
        let body = b"genuine update package".to_vec();
        let url = serve_plain(&body, "");
        download_file(&url, &dest, &sha256_hex(&body), &|_, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // a second update lands on the SAME fixed destination
        // name: installing over the existing file must work
        let body2 = b"the next update package".to_vec();
        let url2 = serve_plain(&body2, "");
        download_file(&url2, &dest, &sha256_hex(&body2), &|_, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_discards_a_package_whose_hash_does_not_match() {
        // S-F2 regression lock: a staged .part whose digest fails
        // verification is deleted on the spot — left on disk, every
        // later check would re-download and re-fail against it
        let dir = temp_dir("badhash");
        let dest = dir.join("pkg.bin");
        let body = b"tampered update package".to_vec();
        let url = serve_plain(&body, "");
        let wrong = sha256_hex(b"the genuine article");
        let err = download_file(&url, &dest, &wrong, &|_, _| {}).unwrap_err();
        assert!(err.contains("integrity check"), "unexpected error: {err}");
        assert!(!dest.exists(), "a failed package must not install");
        assert!(
            !dest.with_extension("part").exists(),
            "a failed package must not leave its .part behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_rejects_an_encoded_response() {
        // W1 F11: a gzip-labelled body must fail cleanly before a
        // single byte is written, not be saved as the package
        let dir = temp_dir("enc");
        let dest = dir.join("pkg.bin");
        let body = b"not really a package".to_vec();
        let url = serve_plain(&body, "Content-Encoding: gzip\r\n");
        let err = download_file(&url, &dest, &sha256_hex(&body), &|_, _| {}).unwrap_err();
        assert!(err.contains("Content-Encoding"), "unexpected error: {err}");
        assert!(!dest.exists());
        assert!(!dest.with_extension("part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_aborts_a_chunked_body_past_the_cap_and_cleans_up() {
        // W1 F11: a chunked body has no Content-Length to pre-flight,
        // so the cap must bite MID-STREAM — a 2 MiB body against a
        // 1 MiB ceiling is aborted and its .part removed, instead of
        // being consumed in full before the hash check (the small
        // ceiling stands in for the 2 GiB production one)
        let dir = temp_dir("cap");
        let dest = dir.join("pkg.bin");
        let body = vec![b'x'; 2 * 1024 * 1024];
        let url = serve_chunked(&body);
        let err = download_file_with_ceiling(
            &url,
            &dest,
            &sha256_hex(&body),
            &|_, _| {},
            1024 * 1024,
        )
        .unwrap_err();
        assert!(err.contains("size cap"), "unexpected error: {err}");
        assert!(!dest.exists(), "an aborted download must not install");
        assert!(
            !dest.with_extension("part").exists(),
            "an aborted download must not leak its .part file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn make_zip(dir: &std::path::Path, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let path = dir.join("fixture.zip");
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in entries {
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file(name.to_string(), opts).unwrap();
            w.write_all(body).unwrap();
        }
        let cursor = w.finish().unwrap();
        std::fs::write(&path, cursor.into_inner()).unwrap();
        path
    }

    #[test]
    fn fetch_manifest_body_enforces_the_size_cap() {
        let agent = crate::build_agent();
        let body = vec![b'x'; MAX_MANIFEST_BYTES as usize + 1];
        let url = serve_plain(&body, "");
        let err = fetch_manifest_body(&agent, &url, Duration::from_secs(30)).unwrap_err();
        assert!(err.contains("size cap"), "unexpected error: {err}");
    }

    #[test]
    fn extract_zip_enforces_entry_and_decompressed_caps() {
        let dir = temp_dir("zipcaps");
        let zip = make_zip(&dir, &[("a.txt", b"aaaa"), ("b.txt", b"bbbb")]);
        let out = dir.join("out");
        let err = extract_zip_with_caps(&zip, &out, 1, 1024).unwrap_err();
        assert!(err.contains("entry cap"), "unexpected error: {err}");
        let err = extract_zip_with_caps(&zip, &out, 10, 4).unwrap_err();
        assert!(err.contains("cap"), "unexpected error: {err}");
        // under both caps the same archive extracts whole
        let written = extract_zip_with_caps(&zip, &out, 10, 1024).unwrap();
        assert_eq!(written.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that answers the headers (promising a full
    /// manifest-sized body) and then dribbles one byte every
    /// 200 ms forever: no single read ever stalls, so only an
    /// OVERALL deadline can end the fetch.
    fn serve_dribble() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_MANIFEST_BYTES
            );
            if sock.write_all(head.as_bytes()).is_err() {
                return;
            }
            loop {
                if sock.write_all(b"x").is_err() {
                    return; // the client hung up — the deadline did its job
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        format!("http://127.0.0.1:{port}/release.json")
    }

    #[test]
    fn manifest_fetch_fails_within_its_deadline_against_a_dribble_server() {
        // Regression lock for the launch-freeze: per-read
        // timeouts can never fire here (a byte lands every
        // 200 ms), so without the overall deadline this fetch
        // would run for the body's full dribble — hours.
        let agent = manifest_agent();
        let url = serve_dribble();
        let t0 = Instant::now();
        let err = fetch_manifest_body(&agent, &url, Duration::from_secs(2)).unwrap_err();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "dribble fetch must die on its 2 s deadline, took {elapsed:?} ({err})"
        );
    }

    #[test]
    fn manifest_check_budget_is_shared_across_routes() {
        // Two dribbling routes under one 3 s budget: the second
        // route must not get a fresh budget of its own — the
        // whole check is bounded by the one deadline.
        let urls = vec![serve_dribble(), serve_dribble()];
        let t0 = Instant::now();
        let err = fetch_release_within(&urls, Duration::from_secs(3)).unwrap_err();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(8),
            "two-route check must stay inside its shared budget, took {elapsed:?} ({err})"
        );
    }

    #[test]
    fn fetch_release_within_takes_the_highest_version_across_routes() {
        // Item 9 regression: the first PARSEABLE route used to
        // win outright, so a stale mirror answering first made
        // every client behind it read "up to date" — through
        // even a mandatory update. All routes are now asked and
        // the highest version wins, in either order.
        let older = br#"{"version":"2.3.4","notes":"old route"}"#;
        let newer = br#"{"version":"2.3.5","notes":"new route"}"#;
        let urls = vec![
            serve_plain(older, ""),
            serve_plain(newer, ""),
        ];
        let info = fetch_release_within(&urls, Duration::from_secs(30)).unwrap();
        assert_eq!(info.version, "2.3.5");
        assert_eq!(info.notes, "new route");
        // reversed: the newer route first must not be displaced
        // by the older one answering second
        let urls = vec![
            serve_plain(newer, ""),
            serve_plain(older, ""),
        ];
        let info = fetch_release_within(&urls, Duration::from_secs(30)).unwrap();
        assert_eq!(info.version, "2.3.5");
        assert_eq!(info.notes, "new route");
        // equal versions: the earlier route's copy stands (route
        // order is preference order)
        let same_a = br#"{"version":"2.3.5","notes":"route a"}"#;
        let same_b = br#"{"version":"2.3.5","notes":"route b"}"#;
        let urls = vec![
            serve_plain(same_a, ""),
            serve_plain(same_b, ""),
        ];
        let info = fetch_release_within(&urls, Duration::from_secs(30)).unwrap();
        assert_eq!(info.notes, "route a");
        // a route that fails outright does not poison the check
        let urls = vec![
            "http://127.0.0.1:1/release.json".to_string(),
            serve_plain(newer, ""),
        ];
        let info = fetch_release_within(&urls, Duration::from_secs(30)).unwrap();
        assert_eq!(info.version, "2.3.5");
    }

    #[test]
    fn fetch_manifest_body_rejects_an_encoded_response() {
        // W1 F11: fetch_release's body fetch must reject a real
        // Content-Encoding instead of parsing compressed bytes
        let agent = crate::build_agent();
        let body = br#"{"version":"9.9.9"}"#;
        let url = serve_plain(body, "Content-Encoding: gzip\r\n");
        let err = fetch_manifest_body(&agent, &url, Duration::from_secs(30)).unwrap_err();
        assert!(err.contains("Content-Encoding"), "unexpected error: {err}");
        // the same body unencoded passes through untouched
        let url = serve_plain(body, "");
        let text = fetch_manifest_body(&agent, &url, Duration::from_secs(30)).unwrap();
        assert_eq!(text.as_bytes(), body);
    }
}

