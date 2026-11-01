//! First-run fetching of the video sidecar components.
//!
//! The installer stays IDM-sized — app, browser bridge and extension
//! only. The video engine arrives once, silently in the background
//! on first launch, into the app-data components folder (the first
//! place `find_tool` looks), and every downloaded byte is verified
//! against a pinned SHA-256 before it is allowed to run: a component
//! that does not match its hash is deleted, never executed.
//!
//! The fetched set is yt-dlp + deno on every platform, plus ffmpeg
//! on Windows (there is no package manager to supply it there). On
//! Linux ffmpeg deliberately stays OUT of this table: the .deb
//! declares it as a package dependency, so `find_tool` picks the
//! distro's build up from PATH — fetching a second copy would only
//! shadow it.
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

#[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
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

#[cfg(all(not(target_os = "windows"), target_arch = "aarch64"))]
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

// Windows assets are different files entirely — an OS-blind table
// fetched the Linux binaries there and installed them without the
// .exe names find_tool looks for, so the video engine could never
// become ready. Hashes below were taken from the publishers'
// own checksum files for these exact releases.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub fn specs() -> Vec<ComponentSpec> {
    vec![
        ComponentSpec {
            name: "yt-dlp",
            asset: "yt-dlp.exe",
            url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp.exe",
            sha256: "f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d",
            zipped: false,
        },
        ComponentSpec {
            name: "deno",
            asset: "deno-x86_64-pc-windows-msvc.zip",
            url: "https://github.com/denoland/deno/releases/download/v2.9.8/deno-x86_64-pc-windows-msvc.zip",
            sha256: "cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf",
            zipped: true,
        },
        // ffmpeg joins the fetched set on Windows only: no package
        // manager supplies it there, and the slim packages no longer
        // bundle it. The source is the canonical Windows build
        // (Gyan, linked from ffmpeg.org), pinned to the permanent
        // versioned 8.1.2 release — BtbN/yt-dlp builds publish only
        // rolling or pruned tags, which a baked-in hash cannot
        // survive. The hash is the publisher's own checksum
        // (gyan.dev), verified against a full local download of
        // this byte-identical file from the publisher's mirror.
        // The essentials zip nests the binary under
        // ffmpeg-8.1.2-essentials_build/bin/ — extract_binary's
        // file-name match finds it; ffplay/ffprobe are discarded.
        ComponentSpec {
            name: "ffmpeg",
            asset: "ffmpeg-8.1.2-essentials_build.zip",
            url: "https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-essentials_build.zip",
            sha256: "db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec",
            zipped: true,
        },
    ]
}

// No ffmpeg entry here: there is no permanently-hosted ARM64
// Windows ffmpeg build to pin (Gyan ships x86_64 only; the ARM64
// builds elsewhere are rolling-only). x86_64 is the shipped
// Windows target — this table is completeness, like it was before.
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
pub fn specs() -> Vec<ComponentSpec> {
    vec![
        ComponentSpec {
            name: "yt-dlp",
            asset: "yt-dlp_arm64.exe",
            url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp_arm64.exe",
            sha256: "0066dbac3c124028e2dcae2a197fccee33cd91f456313d24f7af89fd54590e98",
            zipped: false,
        },
        ComponentSpec {
            name: "deno",
            asset: "deno-aarch64-pc-windows-msvc.zip",
            url: "https://github.com/denoland/deno/releases/download/v2.9.8/deno-aarch64-pc-windows-msvc.zip",
            sha256: "b413b92b735b50c198e8fe3b85af1aa50e29d5f72e229aab701d132286e14cef",
            zipped: true,
        },
    ]
}

#[cfg(not(any(
    all(not(target_os = "windows"), target_arch = "x86_64"),
    all(not(target_os = "windows"), target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "aarch64"),
)))]
pub fn specs() -> Vec<ComponentSpec> {
    Vec::new()
}

/// The file name the binary is installed under in the components
/// dir — and, for zipped components, the entry name inside the
/// archive. `find_tool` appends `.exe` on Windows, so the installed
/// file must carry it there (the deno archives ship `deno.exe`).
fn installed_name(spec: &ComponentSpec) -> String {
    if cfg!(target_os = "windows") {
        format!("{}.exe", spec.name)
    } else {
        spec.name.to_string()
    }
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
    let final_path = dir.join(installed_name(spec));
    if spec.zipped {
        on_progress(Progress {
            name: spec.name.to_string(),
            downloaded,
            total,
            phase: "extracting".to_string(),
        });
        if let Err(e) = extract_binary(&part, &installed_name(spec), &final_path) {
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
/// leave a truncated binary at the final path). The binary may sit
/// at the archive root (deno ships it there) or nested inside a
/// versioned folder (ffmpeg's essentials zip nests it under
/// `<version>-essentials_build/bin/`): an exact path match wins,
/// otherwise the first entry whose file name matches is used.
fn extract_binary(zip_path: &Path, name: &str, out: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("cannot open archive: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("downloaded archive is unreadable: {e}"))?;
    let mut entry_name: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("downloaded archive is unreadable: {e}"))?;
        let ename = entry.name().to_string();
        if ename == name {
            entry_name = Some(ename);
            break;
        }
        if entry_name.is_none() && !ename.ends_with('/') && ename.rsplit('/').next() == Some(name)
        {
            entry_name = Some(ename);
        }
    }
    let entry_name =
        entry_name.ok_or_else(|| format!("the archive does not contain {name}"))?;
    let mut entry = archive
        .by_name(&entry_name)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ndm-test-components-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_zip(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
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
    fn spec_table_is_well_formed() {
        let specs = specs();
        assert!(!specs.is_empty(), "this platform must have a fetch table");
        let mut names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), specs.len(), "component names must be unique");
        for s in &specs {
            assert_eq!(s.sha256.len(), 64, "{}: pinned hash must be 64 hex chars", s.name);
            assert!(
                s.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{}: pinned hash must be lowercase hex",
                s.name
            );
            assert!(s.url.starts_with("https://"), "{}: url must be https", s.name);
            assert!(
                s.url.ends_with(s.asset),
                "{}: pinned url must name the asset it pins",
                s.name
            );
        }
        // the two core components are fetched on every platform
        assert!(specs.iter().any(|s| s.name == "yt-dlp"));
        assert!(specs.iter().any(|s| s.name == "deno"));
    }

    /// The Linux contract the slim-Windows work must not break:
    /// ffmpeg is the .deb's package dependency there, never a
    /// fetched component (a fetched copy would shadow the distro's).
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn linux_specs_never_fetch_ffmpeg() {
        assert!(
            !specs().iter().any(|s| s.name == "ffmpeg"),
            "ffmpeg must stay out of the Linux fetch table"
        );
    }

    /// The Windows engine set is the whole engine: nothing may be
    /// dropped from the table or the slim packages lose a capability
    /// the bundled ones had. (Runs in the Windows CI test steps.)
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_specs_cover_the_whole_engine() {
        let mut names: Vec<&str> = specs().iter().map(|s| s.name).collect();
        names.sort_unstable();
        assert_eq!(names, ["deno", "ffmpeg", "yt-dlp"]);
    }

    #[test]
    fn extract_finds_binary_at_zip_root() {
        // the deno shape: the binary sits at the archive root,
        // beside unrelated files
        let dir = temp_dir("root");
        let zip = make_zip(
            &dir,
            &[("deno.exe", b"root-binary"), ("README.txt", b"noise")],
        );
        let out = dir.join("deno.exe");
        extract_binary(&zip, "deno.exe", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"root-binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_finds_binary_nested_in_a_versioned_folder() {
        // the ffmpeg shape: <ver>-essentials_build/bin/ffmpeg.exe,
        // with sibling tools that must not be picked up
        let dir = temp_dir("nested");
        let zip = make_zip(
            &dir,
            &[
                ("ffmpeg-8.1.2-essentials_build/bin/ffprobe.exe", b"probe"),
                ("ffmpeg-8.1.2-essentials_build/bin/ffmpeg.exe", b"nested-binary"),
                ("ffmpeg-8.1.2-essentials_build/doc/ffmpeg.html", b"docs"),
            ],
        );
        let out = dir.join("ffmpeg.exe");
        extract_binary(&zip, "ffmpeg.exe", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"nested-binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_prefers_the_exact_path_over_a_nested_namesake() {
        let dir = temp_dir("exact");
        let zip = make_zip(
            &dir,
            &[
                ("bundle/bin/tool.exe", b"nested"),
                ("tool.exe", b"exact"),
            ],
        );
        let out = dir.join("tool.exe");
        extract_binary(&zip, "tool.exe", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"exact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_reports_a_missing_binary() {
        let dir = temp_dir("missing");
        let zip = make_zip(&dir, &[("other.exe", b"x")]);
        let out = dir.join("ffmpeg.exe");
        let err = extract_binary(&zip, "ffmpeg.exe", &out).unwrap_err();
        assert!(err.contains("does not contain"), "unexpected error: {err}");
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serve `body` once over HTTP on a loopback port and return the
    /// URL. The bootstrap's whole contract — fetch, verify, install —
    /// is exercised against this, never against the real network.
    fn serve_once(body: Vec<u8>) -> String {
        use std::io::Read as _;
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
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&body);
            let _ = sock.flush();
        });
        format!("http://127.0.0.1:{port}/asset")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn download_installs_a_verified_plain_binary() {
        let dir = temp_dir("dl-plain");
        let body = b"pretend yt-dlp binary".to_vec();
        let spec = ComponentSpec {
            name: "test-tool",
            asset: "test-tool.bin",
            url: Box::leak(serve_once(body.clone()).into_boxed_str()),
            sha256: Box::leak(sha256_hex(&body).into_boxed_str()),
            zipped: false,
        };
        let installed = download_component(&spec, &dir, &|_| {}).unwrap();
        assert_eq!(installed, dir.join(installed_name(&spec)));
        assert_eq!(std::fs::read(&installed).unwrap(), body);
        assert!(
            !dir.join(".test-tool.bin.part").exists(),
            "the .part file must not survive a successful install"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_discards_a_binary_that_fails_its_hash() {
        let dir = temp_dir("dl-badhash");
        let body = b"tampered bytes".to_vec();
        let spec = ComponentSpec {
            name: "test-tool",
            asset: "test-tool.bin",
            url: Box::leak(serve_once(body).into_boxed_str()),
            sha256: Box::leak(sha256_hex(b"the genuine article").into_boxed_str()),
            zipped: false,
        };
        let err = download_component(&spec, &dir, &|_| {}).unwrap_err();
        assert!(err.contains("integrity check"), "unexpected error: {err}");
        assert!(
            !dir.join(installed_name(&spec)).exists(),
            "a component that fails its hash must never be installed"
        );
        assert!(!dir.join(".test-tool.bin.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_extracts_a_verified_nested_binary() {
        // the ffmpeg shape end-to-end: a zip whose binary is nested
        // under a versioned folder lands at dir/<name>. The entry
        // must carry the platform's installed name — test-tool.exe
        // on Windows, where the real archives likewise ship
        // ffmpeg.exe / deno.exe; a bare "test-tool" entry is a
        // fixture only a non-Windows lookup could ever find.
        let dir = temp_dir("dl-zip");
        let entry = format!(
            "pkg-1.0/bin/{}",
            if cfg!(target_os = "windows") {
                "test-tool.exe"
            } else {
                "test-tool"
            }
        );
        let zip_path = make_zip(&dir, &[(&entry, b"zipped-binary")]);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        let spec = ComponentSpec {
            name: "test-tool",
            asset: "test-tool.zip",
            url: Box::leak(serve_once(zip_bytes.clone()).into_boxed_str()),
            sha256: Box::leak(sha256_hex(&zip_bytes).into_boxed_str()),
            zipped: true,
        };
        let installed = download_component(&spec, &dir, &|_| {}).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"zipped-binary");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
