//! First-run fetching of the video sidecar components (yt-dlp + deno).
//!
//! The installer stays IDM-sized — app, browser bridge and extension
//! only. The video engine (~83 MB on the wire) arrives once, on first
//! use, into the app-data components folder (the first place
//! `find_tool` looks), and every downloaded byte is verified against
//! a pinned SHA-256 before it is allowed to run: a component that
//! does not match its hash is deleted, never executed.
//!
//! Two support/testing knobs redirect the source (mirrors, offline
//! tests): in DEBUG builds only, `NDM_COMPONENTS_BASE` replaces the
//! download base URL and `NDM_COMPONENTS_SHA256_<NAME>` overrides a
//! pinned hash. Release builds ignore both — a shipped binary must
//! fetch exactly the pinned artifacts, or the hash guarantee is void.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct ComponentSpec {
    /// the name `find_tool` looks up ("yt-dlp", "deno")
    pub name: &'static str,
    /// asset file name appended to the base URL
    pub asset: &'static str,
    /// pinned release URL the asset normally comes from
    pub url: &'static str,
    /// SHA-256 of the downloaded file itself (for deno: of the zip)
    pub sha256: &'static str,
    /// the download is a zip holding the binary under `name`
    pub zipped: bool,
}

#[cfg(target_arch = "x86_64")]
pub fn specs() -> Vec<ComponentSpec> {
    vec![
        ComponentSpec {
            name: "yt-dlp",
            asset: "yt-dlp_linux",
            url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp_linux",
            sha256: "767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795",
            zipped: false,
        },
        ComponentSpec {
            name: "deno",
            asset: "deno-x86_64-unknown-linux-gnu.zip",
            url: "https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-unknown-linux-gnu.zip",
            sha256: "e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609",
            zipped: true,
        },
    ]
}

#[cfg(target_arch = "aarch64")]
pub fn specs() -> Vec<ComponentSpec> {
    vec![
        ComponentSpec {
            name: "yt-dlp",
            asset: "yt-dlp_linux_aarch64",
            url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp_linux_aarch64",
            sha256: "e87e9533e89fe5889abaa21f5114dfe7756d6aa6219b83551add04d48b18e483",
            zipped: false,
        },
        ComponentSpec {
            name: "deno",
            asset: "deno-aarch64-unknown-linux-gnu.zip",
            url: "https://github.com/denoland/deno/releases/download/v2.9.8/deno-aarch64-unknown-linux-gnu.zip",
            sha256: "d49596c20b9788cf8f5d8ddf644103d4bde56ed239753f0bdb92ba9361d51905",
            zipped: true,
        },
    ]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn specs() -> Vec<ComponentSpec> {
    Vec::new()
}

/// One progress report while a component downloads. `phase` is
/// "downloading", "verifying" or "extracting"; the caller adds the
/// terminal "done" / "error" reports around the whole run.
#[derive(Clone)]
pub struct Progress {
    pub name: String,
    pub downloaded: u64,
    pub total: u64,
    pub phase: String,
}

fn download_url(spec: &ComponentSpec) -> String {
    if !cfg!(debug_assertions) {
        return spec.url.to_string();
    }
    match std::env::var("NDM_COMPONENTS_BASE") {
        Ok(base) if !base.trim().is_empty() => {
            format!("{}/{}", base.trim_end_matches('/'), spec.asset)
        }
        _ => spec.url.to_string(),
    }
}

fn expected_sha256(spec: &ComponentSpec) -> String {
    if !cfg!(debug_assertions) {
        return spec.sha256.to_string();
    }
    let key = format!(
        "NDM_COMPONENTS_SHA256_{}",
        spec.name.to_uppercase().replace('-', "_")
    );
    match std::env::var(&key) {
        Ok(v) if v.trim().len() == 64 => v.trim().to_lowercase(),
        _ => spec.sha256.to_string(),
    }
}

/// Download one component into `dir`, verify it, unpack it if needed
/// and leave the runnable binary at `dir/<name>`. Any failure cleans
/// up its partial files — a half-fetched component must never be
/// mistaken for an installed one.
pub fn download_component(
    spec: &ComponentSpec,
    dir: &Path,
    on_progress: &dyn Fn(Progress),
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create components folder: {e}"))?;
    let url = download_url(spec);
    let part = dir.join(format!(".{}.part", spec.asset));
    let agent = crate::build_agent();
    let resp = agent
        .get(&url)
        .call()
        .map_err(|e| format!("could not download {}: {e}", spec.name))?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut file =
        std::fs::File::create(&part).map_err(|e| format!("cannot write {}: {e}", spec.name))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_report: u64 = 0;
    let read_result = loop {
        match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                hasher.update(&buf[..n]);
                if let Err(e) = file.write_all(&buf[..n]) {
                    break Err(format!("cannot write {}: {e}", spec.name));
                }
                downloaded += n as u64;
                if downloaded - last_report >= 512 * 1024 {
                    last_report = downloaded;
                    on_progress(Progress {
                        name: spec.name.to_string(),
                        downloaded,
                        total,
                        phase: "downloading".to_string(),
                    });
                }
            }
            Err(e) => break Err(format!("download of {} interrupted: {e}", spec.name)),
        }
    };
    drop(file);
    if let Err(e) = read_result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    on_progress(Progress {
        name: spec.name.to_string(),
        downloaded,
        total,
        phase: "verifying".to_string(),
    });
    let digest = format!("{:x}", hasher.finalize());
    if digest != expected_sha256(spec) {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "{} failed its integrity check (SHA-256 mismatch) — the download was discarded",
            spec.name
        ));
    }
    let final_path = dir.join(spec.name);
    if spec.zipped {
        on_progress(Progress {
            name: spec.name.to_string(),
            downloaded,
            total,
            phase: "extracting".to_string(),
        });
        if let Err(e) = extract_binary(&part, spec.name, &final_path) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        let _ = std::fs::remove_file(&part);
    } else {
        std::fs::rename(&part, &final_path)
            .map_err(|e| format!("cannot install {}: {e}", spec.name))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&final_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(final_path)
}

/// Pull the one binary named `name` out of a downloaded zip into
/// `out` (via a temp file + rename, so a crash mid-extract can never
/// leave a truncated binary at the final path).
fn extract_binary(zip_path: &Path, name: &str, out: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("cannot open archive: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("downloaded archive is unreadable: {e}"))?;
    let mut entry = archive
        .by_name(name)
        .map_err(|_| format!("the archive does not contain {name}"))?;
    let tmp = out.with_extension("extracting");
    {
        let mut dst = std::fs::File::create(&tmp)
            .map_err(|e| format!("cannot extract {name}: {e}"))?;
        std::io::copy(&mut entry, &mut dst).map_err(|e| format!("cannot extract {name}: {e}"))?;
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("cannot install {name}: {e}"))?;
    Ok(())
}
