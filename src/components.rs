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

/// The last yt-dlp release whose official Windows binaries still
/// run on Windows 7/8/8.1. The next release (2026.10.26, the pin in
/// the table above) switched its Windows builds to the newest
/// Python and its release notes state the binaries "now require
/// Windows 10 or later" — Win8 support was dropped there, so on an
/// older Windows the modern pin would download, verify and install
/// perfectly and then fail every single run. Hash taken from the
/// 2026.10.21 release's own SHA2-256SUMS file.
#[cfg(any(all(target_os = "windows", target_arch = "x86_64"), test))]
fn ytdlp_legacy_win_x64() -> ComponentSpec {
    ComponentSpec {
        name: "yt-dlp",
        asset: "yt-dlp.exe",
        url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.21/yt-dlp.exe",
        sha256: "33121d192d706e62f684a06f80918e3f3a8ba52ac3a65213b6951c39c7d9f555",
        zipped: false,
    }
}

/// Windows 10 is where the modern yt-dlp pin starts working;
/// below it the legacy pin above is the newest yt-dlp that runs.
#[cfg(any(all(target_os = "windows", target_arch = "x86_64"), test))]
fn use_legacy_ytdlp(windows_major: u32) -> bool {
    windows_major < 10
}

/// The running Windows' major version (6 = Vista/7/8 era, 10 =
/// Windows 10/11), read with RtlGetVersion: GetVersionEx reports
/// 6.2 to any process whose manifest declares no supportedOS —
/// which includes this app — so the usual version APIs cannot be
/// trusted to make the pin decision above.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn windows_major_version() -> Option<u32> {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    let mut info = OsVersionInfoW {
        size: 0,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd: [0; 128],
    };
    info.size = std::mem::size_of::<OsVersionInfoW>() as u32;
    if unsafe { RtlGetVersion(&mut info) } == 0 && info.major > 0 {
        Some(info.major)
    } else {
        None
    }
}

/// The fetch table for THIS machine: `specs()`, except that on
/// pre-Windows-10 x86_64 Windows the yt-dlp entry is swapped for
/// the last release that still runs there. Anything that fetches
/// components or reports them missing must use this rather than
/// `specs()` — otherwise an old-Windows machine fetches a yt-dlp
/// that can never start. When the OS version cannot be determined
/// the modern pin stays: the failure mode is the pre-fix
/// behaviour, not something worse. (No legacy swap exists for
/// ARM64 because there is no pre-10 64-bit ARM Windows.)
#[cfg_attr(
    not(all(target_os = "windows", target_arch = "x86_64")),
    allow(unused_mut)
)]
pub fn effective_specs() -> Vec<ComponentSpec> {
    let mut table = specs();
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        if let Some(major) = windows_major_version() {
            if use_legacy_ytdlp(major) {
                for spec in table.iter_mut() {
                    if spec.name == "yt-dlp" {
                        *spec = ytdlp_legacy_win_x64();
                    }
                }
            }
        }
    }
    table
}

/// One of the app-local UCRT files the app ships beside its own
/// executable: ucrtbase.dll itself or one of the api-ms-win-crt-*
/// forwarder stubs that resolve to it.
#[cfg(any(target_os = "windows", test))]
fn is_ucrt_companion(file_name: &str) -> bool {
    let n = file_name.to_ascii_lowercase();
    n == "ucrtbase.dll" || (n.starts_with("api-ms-win-crt-") && n.ends_with(".dll"))
}

/// Copy the UCRT companions from `src_dir` into `dst_dir`, skipping
/// any already there. Returns how many files were copied.
#[cfg(any(target_os = "windows", test))]
fn copy_ucrt_companions(src_dir: &Path, dst_dir: &Path) -> std::io::Result<usize> {
    let mut copied = 0;
    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_ucrt_companion(&name) {
            continue;
        }
        let target = dst_dir.join(&name);
        if target.exists() {
            continue;
        }
        std::fs::create_dir_all(dst_dir)?;
        std::fs::copy(entry.path(), &target)?;
        copied += 1;
    }
    Ok(copied)
}

/// Make sure the components folder carries the UCRT set that the
/// fetched Windows binaries need. ffmpeg.exe imports the
/// api-ms-win-crt-* contracts, which a stock Windows 7/8 machine
/// cannot resolve (that is exactly why the app itself ships the
/// set app-locally) — but app-local resolution only covers the
/// folder the executable lives in, and the components live in a
/// different one, so without this mirror ffmpeg would fail to
/// start on such a machine with a missing-DLL error the moment a
/// video download reached the merge. Mirrors only what is missing;
/// on Windows 10+ (UCRT in-box) and non-Windows hosts the copies
/// are simply never used. Returns how many files were copied.
pub fn ensure_ucrt_companions(components_dir: &Path) -> std::io::Result<usize> {
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe()?;
        let Some(src_dir) = exe.parent() else {
            return Ok(0);
        };
        copy_ucrt_companions(src_dir, components_dir)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = components_dir;
        Ok(0)
    }
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

/// Hard cap on one component body, enforced WHILE streaming: the
/// pinned SHA-256 is only checked after the whole body lands, so
/// without a cap a hostile or broken mirror could stream forever
/// and fill the disk before the hash ever gets a say. The largest
/// pinned asset (the Windows ffmpeg zip) is ~90 MB; 256 MiB leaves
/// generous headroom for future pinned releases.
pub const MAX_COMPONENT_BYTES: u64 = 256 * 1024 * 1024;

fn component_size_ok(downloaded: u64) -> bool {
    downloaded <= MAX_COMPONENT_BYTES
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
    if let Some(enc) = crate::response_encoding(&resp) {
        return Err(format!(
            "{} was served with Content-Encoding: {enc} — the download was discarded",
            spec.name
        ));
    }
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if total > 0 && !component_size_ok(total) {
        return Err(format!(
            "{} is larger than the {} MiB component size cap — the download was refused",
            spec.name,
            MAX_COMPONENT_BYTES / (1024 * 1024)
        ));
    }
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
                if !component_size_ok(downloaded) {
                    break Err(format!(
                        "{} exceeded the {} MiB component size cap — the download was aborted and discarded",
                        spec.name,
                        MAX_COMPONENT_BYTES / (1024 * 1024)
                    ));
                }
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
        // like every earlier failure path in this function, a
        // failed install rename must not leak the .part (W1 F10)
        if let Err(e) = std::fs::rename(&part, &final_path) {
            let _ = std::fs::remove_file(&part);
            return Err(format!("cannot install {}: {e}", spec.name));
        }
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
    write_extracted(&mut entry, &tmp, out, name)
}

/// Stream one extracted entry to the temp file beside `out` and
/// rename it into place. Every failure path removes the temp file:
/// a failed copy used to leave the `.extracting` file behind,
/// contradicting the cleanup this module promises (W1 F10).
fn write_extracted(
    entry: &mut impl Read,
    tmp: &Path,
    out: &Path,
    name: &str,
) -> Result<(), String> {
    let copied = (|| {
        let mut dst = std::fs::File::create(tmp)
            .map_err(|e| format!("cannot extract {name}: {e}"))?;
        std::io::copy(entry, &mut dst).map_err(|e| format!("cannot extract {name}: {e}"))
    })();
    if let Err(e) = copied {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(tmp, out) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!("cannot install {name}: {e}"));
    }
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
    fn component_size_cap_boundary() {
        // the cap that download_component enforces mid-stream (a
        // hostile mirror must be stopped during the download, not
        // after the disk is full): at the cap is fine, past it not
        assert!(component_size_ok(0));
        assert!(component_size_ok(MAX_COMPONENT_BYTES));
        assert!(!component_size_ok(MAX_COMPONENT_BYTES + 1));
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
    fn download_rename_failure_removes_part() {
        // W1 F10: the install target pre-created as a directory
        // makes the final rename fail — the .part must still be
        // cleaned up, like on every other failure path
        let dir = temp_dir("dl-rename-fail");
        let body = b"pretend yt-dlp binary".to_vec();
        let spec = ComponentSpec {
            name: "test-tool",
            asset: "test-tool.bin",
            url: Box::leak(serve_once(body.clone()).into_boxed_str()),
            sha256: Box::leak(sha256_hex(&body).into_boxed_str()),
            zipped: false,
        };
        std::fs::create_dir_all(dir.join(installed_name(&spec))).unwrap();
        let err = download_component(&spec, &dir, &|_| {}).unwrap_err();
        assert!(err.contains("cannot install"), "unexpected error: {err}");
        assert!(
            !dir.join(".test-tool.bin.part").exists(),
            "a failed install rename must not leak the .part file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader that yields a few bytes, then fails — the shape of
    /// a corrupt archive entry dying mid-copy.
    struct FailAfterFirstRead(bool);

    impl Read for FailAfterFirstRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "entry died mid-copy",
                ));
            }
            self.0 = true;
            buf[..4].copy_from_slice(b"part");
            Ok(4)
        }
    }

    #[test]
    fn extract_copy_failure_removes_extracting_temp() {
        // W1 F10: a failed io::copy must not leak the .extracting temp
        let dir = temp_dir("extract-copy-fail");
        let out = dir.join("tool");
        let tmp = out.with_extension("extracting");
        let err = write_extracted(&mut FailAfterFirstRead(false), &tmp, &out, "tool")
            .unwrap_err();
        assert!(err.contains("cannot extract"), "unexpected error: {err}");
        assert!(!tmp.exists(), "a failed copy must not leak the .extracting temp");
        assert!(!out.exists(), "a failed copy must not install anything");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_rename_failure_removes_extracting_temp() {
        // the same cleanup on extract_binary's install step: the
        // destination pre-created as a directory makes rename fail
        let dir = temp_dir("extract-rename-fail");
        let zip = make_zip(&dir, &[("tool", b"binary")]);
        let out = dir.join("tool");
        std::fs::create_dir_all(&out).unwrap();
        let tmp = out.with_extension("extracting");
        let err = extract_binary(&zip, "tool", &out).unwrap_err();
        assert!(err.contains("cannot install"), "unexpected error: {err}");
        assert!(!tmp.exists(), "a failed install rename must not leak the .extracting temp");
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

    /// The pre-Windows-10 fallback pin must be as well-formed as
    /// any table entry — it is verified against its hash on real
    /// machines exactly like the modern pin is.
    #[test]
    fn legacy_ytdlp_pin_is_well_formed() {
        let s = ytdlp_legacy_win_x64();
        assert_eq!(s.name, "yt-dlp");
        assert_eq!(s.asset, "yt-dlp.exe");
        assert!(!s.zipped);
        assert_eq!(s.sha256.len(), 64);
        assert!(
            s.sha256
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "pinned hash must be lowercase hex"
        );
        assert!(s.url.starts_with("https://"));
        assert!(s.url.ends_with(s.asset));
    }

    /// The legacy pin takes over strictly below Windows 10 — the
    /// release it replaces (2026.10.26) is the first whose Windows
    /// binaries require Windows 10.
    #[test]
    fn legacy_ytdlp_selected_below_windows_10_only() {
        assert!(use_legacy_ytdlp(5));
        assert!(use_legacy_ytdlp(6));
        assert!(!use_legacy_ytdlp(10));
        assert!(!use_legacy_ytdlp(11));
    }

    /// Off pre-10 x86_64 Windows the effective table must be the
    /// plain table, entry for entry — the swap may never leak
    /// onto a platform it was not meant for.
    #[test]
    fn effective_specs_match_specs_here() {
        let plain = specs();
        let effective = effective_specs();
        assert_eq!(plain.len(), effective.len());
        for (a, b) in plain.iter().zip(effective.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.url, b.url);
            assert_eq!(a.sha256, b.sha256);
        }
    }

    /// Only the CRT companions are mirrored into the components
    /// folder — core api-set stubs and unrelated files stay out —
    /// and a file already present (e.g. a newer OS copy) is never
    /// overwritten.
    #[test]
    fn ucrt_companions_copy_only_missing_crt_files() {
        let base = temp_dir("ucrt");
        let src = base.join("app");
        let dst = base.join("components");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("ucrtbase.dll"), b"ucrt").unwrap();
        std::fs::write(src.join("api-ms-win-crt-runtime-l1-1-0.dll"), b"stub").unwrap();
        std::fs::write(src.join("API-MS-Win-CRT-Heap-l1-1-0.dll"), b"stub2").unwrap();
        std::fs::write(src.join("api-ms-win-core-file-l1-1-0.dll"), b"core").unwrap();
        std::fs::write(src.join("ndm-app.exe"), b"app").unwrap();
        std::fs::write(dst.join("ucrtbase.dll"), b"keep-me").unwrap();
        let copied = copy_ucrt_companions(&src, &dst).unwrap();
        assert_eq!(copied, 2);
        assert_eq!(std::fs::read(dst.join("ucrtbase.dll")).unwrap(), b"keep-me");
        assert!(dst.join("api-ms-win-crt-runtime-l1-1-0.dll").exists());
        assert!(dst.join("API-MS-Win-CRT-Heap-l1-1-0.dll").exists());
        assert!(!dst.join("api-ms-win-core-file-l1-1-0.dll").exists());
        assert!(!dst.join("ndm-app.exe").exists());
        // a second pass finds nothing left to do
        assert_eq!(copy_ucrt_companions(&src, &dst).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}
