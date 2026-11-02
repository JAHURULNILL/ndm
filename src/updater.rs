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
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| {
            let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

/// Compare dotted numeric versions ("1.5.0" > "1.0.7").
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (va, vb) = (parse_version(a), parse_version(b));
    let n = va.len().max(vb.len());
    for i in 0..n {
        let (x, y) = (va.get(i).copied().unwrap_or(0), vb.get(i).copied().unwrap_or(0));
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

/// Fetch one manifest URL's body as text. A response carrying a
/// real Content-Encoding is rejected (W1 F11): the update paths
/// request `identity` and ureq here does not decode, so a gzipped
/// manifest would otherwise be parsed as garbage — or, worse on
/// the download path, saved as the package — instead of failing
/// cleanly like every other transfer path in the crate.
fn fetch_manifest_body(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let resp = agent
        .get(url)
        .set("Accept-Encoding", "identity")
        .call()
        .map_err(|e| format!("update check failed: {e}"))?;
    if let Some(enc) = crate::response_encoding(&resp) {
        return Err(format!(
            "update check failed: server answered with Content-Encoding: {enc}"
        ));
    }
    resp.into_string()
        .map_err(|e| format!("update check failed: {e}"))
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
    // Try each route in turn: one unreachable host must not silently
    // kill the update check when the other route works.
    let agent = crate::build_agent();
    let mut last_err = String::new();
    for url in &urls {
        let fetched = fetch_manifest_body(&agent, url);
        match fetched {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(info) => return Ok(info),
                Err(e) => last_err = format!("update info was unreadable: {e}"),
            },
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
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
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create folder: {e}"))?;
    }
    let agent = crate::build_agent();
    let resp = agent
        .get(url)
        .set("Accept-Encoding", "identity")
        .call()
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
    let result = loop {
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
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("cannot read the update package: {e}"))?;
    let mut arch = zip::ZipArchive::new(file)
        .map_err(|e| format!("the update package is unreadable: {e}"))?;
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("cannot create the update folder: {e}"))?;
    let mut written = Vec::new();
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
        std::io::copy(&mut entry, &mut w)
            .map_err(|e| format!("cannot extract the update package: {e}"))?;
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

    #[test]
    fn fetch_manifest_body_rejects_an_encoded_response() {
        // W1 F11: fetch_release's body fetch must reject a real
        // Content-Encoding instead of parsing compressed bytes
        let agent = crate::build_agent();
        let body = br#"{"version":"9.9.9"}"#;
        let url = serve_plain(body, "Content-Encoding: gzip\r\n");
        let err = fetch_manifest_body(&agent, &url).unwrap_err();
        assert!(err.contains("Content-Encoding"), "unexpected error: {err}");
        // the same body unencoded passes through untouched
        let url = serve_plain(body, "");
        let text = fetch_manifest_body(&agent, &url).unwrap();
        assert_eq!(text.as_bytes(), body);
    }
}

