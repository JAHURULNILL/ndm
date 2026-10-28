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

pub const MANIFEST_URL: &str =
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
    // build must only ever trust the pinned manifest address.
    let url = if cfg!(debug_assertions) {
        std::env::var("NDM_UPDATE_URL").unwrap_or_else(|_| MANIFEST_URL.to_string())
    } else {
        MANIFEST_URL.to_string()
    };
    let agent = crate::build_agent();
    let body = agent
        .get(&url)
        .call()
        .map_err(|e| format!("update check failed: {e}"))?
        .into_string()
        .map_err(|e| format!("update check failed: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("update info was unreadable: {e}"))
}

/// Download `url` to `dest`, verifying the SHA-256 when one is given.
/// Progress reports (downloaded, total). Used for the one-click
/// update package; a hash mismatch deletes the file and errors.
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
    if !sha256_expected.is_empty() {
        let digest = format!("{:x}", hasher.finalize());
        if digest != sha256_expected.to_lowercase() {
            let _ = std::fs::remove_file(&tmp);
            return Err("the update failed its integrity check and was discarded".to_string());
        }
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("cannot install file: {e}"))?;
    on_progress(downloaded, total.max(downloaded));
    Ok(())
}

/// Increment a shared counter (fire-and-forget, returns success).
/// One hit per installation is all the client ever sends.
pub fn count_hit(counter: &str) -> bool {
    let url = format!("{COUNT_BASE}/hit/{COUNT_NS}/{counter}");
    crate::build_agent().get(&url).call().is_ok()
}

