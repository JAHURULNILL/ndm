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
        let fetched = agent
            .get(url)
            .call()
            .map_err(|e| format!("update check failed: {e}"))
            .and_then(|r| r.into_string().map_err(|e| format!("update check failed: {e}")));
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
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create folder: {e}"))?;
    }
    let agent = crate::build_agent();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
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

