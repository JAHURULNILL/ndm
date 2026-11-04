//! The browser-capture door (2.3.0) — BROWSER-DOOR-DESIGN.md.
//!
//! The main door stays yt-dlp: a pasted link is resolved by the
//! app itself. This door is the IDM-shaped extra: the browser
//! already unlocked and played the video, the extension watched
//! the real media requests go by, and — only when the user clicks
//! — hands those stream URLs to the app. It exists for the
//! machines yt-dlp cannot fully serve (pre-Windows-10, where no
//! JavaScript runtime can run and YouTube's JS challenge stays
//! shut): there the browser is the only thing that can open the
//! lock, so its streams are the way in.
//!
//! The two doors meet in the video picker under ONE identity
//! (page URL / video id): the same video arriving through both is
//! one picker entry with two sources, and a download job is
//! created exactly once, from the source the user picked. A
//! captured option's selector is `ndmcap:<group key>` — the key
//! is a hash of the group's own stream URLs, so it names the same
//! group at picker time, at add time and after an app restart,
//! whatever other groups expired in between.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::video::{Preresolved, PreresolvedStream};
use ndm::video_probe::{QualityOption, VideoInfo};

pub const SELECTOR_PREFIX: &str = "ndmcap:";
/// What the user reads when a captured link died — never a raw
/// engine or HTTP error (design: expired/invalid captured URLs
/// produce a plain-language message).
pub const EXPIRED_MESSAGE: &str = "The links captured from your browser have expired. Play the video in the browser tab again, then capture it once more.";
pub const MISSING_MESSAGE: &str = "The video captured from your browser is no longer available. Play it in the browser tab, then capture it again.";
/// A captured set is a snapshot of signed, perishable URLs; a day
/// is far past any of them, so older sets are dropped on sight.
const SET_MAX_AGE_SECS: u64 = 24 * 3600;
const SETS_CAP: usize = 32;
const SETS_FILE: &str = "captured.json";

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One media stream the browser actually requested, as the
/// extension reports it (the queue line's `streams` entries use
/// the same camelCase wire names).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedStream {
    pub url: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub quality_label: Option<String>,
    #[serde(default)]
    pub has_audio: bool,
    #[serde(default)]
    pub has_video: bool,
    /// the capturing browser's own User-Agent, stamped by the
    /// host from the capture message (audit-232 S3): capture sets
    /// stored before this field existed parse with None and keep
    /// the engine's fallback UA.
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// Everything captured for one page, stamped when the app
/// received it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedSet {
    pub page_url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub streams: Vec<CapturedStream>,
    #[serde(default)]
    pub received_unix: u64,
    /// 2.3.2 (WS-B): the recorder's per-video grouping key
    /// ("fb:<id>") when this set names ONE Facebook video whose
    /// page URL does not (a feed). The store identity derives
    /// from it (see set_identity), so two videos captured on the
    /// same feed page are two sets and can never merge. Empty
    /// for every unkeyed set (the page-data door, YouTube, and
    /// all pre-2.3.2 captures).
    #[serde(default)]
    pub video_key: String,
}

/* ------------------------------------------------------------------ */
/* Identity: one video, one entry — whichever door it came through    */
/* ------------------------------------------------------------------ */

/// The picker's dedupe identity for a page: YouTube URLs reduce
/// to the video id (watch / shorts / live / embed / youtu.be all
/// name the same video); anything else is the URL with its
/// fragment and tracking parameters removed and the remaining
/// query sorted, so a re-shared link still lands on the same
/// entry.
pub fn video_identity(page_url: &str) -> String {
    if let Some(id) = youtube_video_id(page_url) {
        return format!("yt:{id}");
    }
    format!("url:{}", normalize_url(page_url))
}

/// The numeric video id a Facebook URL names — /videos/<id>,
/// /reel/<id> (also /<page>/videos/<id>), watch?v=<digits> — or
/// None when the URL names no specific video (feed, profile,
/// shortlink). The mirror of the extension's
/// fb-classify::facebookVideoId; the two must agree, because the
/// extension keys a capture by this id and the app looks the set
/// up by it.
pub fn facebook_video_id(url: &str) -> Option<String> {
    let (_, host, _, path, query) = split_url(url)?;
    if !(host == "facebook.com" || host.ends_with(".facebook.com") || host == "fb.watch") {
        return None;
    }
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for pair in segs.windows(2) {
        if (pair[0] == "videos" || pair[0] == "reel")
            && !pair[1].is_empty()
            && pair[1].chars().all(|c| c.is_ascii_digit())
        {
            return Some(pair[1].to_string());
        }
    }
    let v = query_param(&query, "v")?;
    (!v.is_empty() && v.chars().all(|c| c.is_ascii_digit())).then(|| v.to_string())
}

/// The store identity of ONE captured set (2.3.2 WS-B): a page
/// URL that names its video (a Facebook permalink) keys the set
/// by that video, whatever door it arrived through; a set
/// carrying a recorder video key keys by page identity + key, so
/// it coexists with — and never merges into — other videos'
/// sets from the same page; anything else keeps the standing
/// page identity.
pub fn set_identity(set: &CapturedSet) -> String {
    if let Some(id) = facebook_video_id(&set.page_url) {
        return format!("fb:{id}");
    }
    if !set.video_key.is_empty() {
        return format!("{}#vg:{}", video_identity(&set.page_url), set.video_key);
    }
    video_identity(&set.page_url)
}

/// The identity a URL-only lookup (the probe merge, a
/// preresolved start) must use — the same derivation
/// set_identity applies when the page URL names its video, so a
/// Facebook permalink finds the set its recorder capture stored.
pub fn identity_for_url(url: &str) -> String {
    if let Some(id) = facebook_video_id(url) {
        return format!("fb:{id}");
    }
    video_identity(url)
}

/// Split a URL into (scheme, host, port, path, query) without a
/// URL crate — the same hand-rolled shape the host bridge uses.
/// The port is kept apart from the host: host comparisons (the
/// YouTube check) must not see it, but the identity must — two
/// servers on one host at different ports are different pages.
fn split_url(url: &str) -> Option<(String, String, String, String, String)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_ascii_lowercase(), p.to_string())
        }
        _ => (hostport.to_ascii_lowercase(), String::new()),
    };
    let tail = tail.split('#').next().unwrap_or("");
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (tail.to_string(), String::new()),
    };
    Some((scheme.to_ascii_lowercase(), host, port, path, query))
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

fn youtube_video_id(url: &str) -> Option<String> {
    let (_, host, _, path, query) = split_url(url)?;
    let valid = |id: &str| {
        id.len() >= 6
            && id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if host == "youtu.be" || host.ends_with(".youtu.be") {
        let id = path.trim_start_matches('/').split('/').next().unwrap_or("");
        return valid(id).then(|| id.to_string());
    }
    let is_yt = host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com");
    if !is_yt {
        return None;
    }
    if path == "/watch" {
        let id = query_param(&query, "v")?;
        return valid(id).then(|| id.to_string());
    }
    for prefix in ["/shorts/", "/live/", "/embed/", "/v/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = rest.split('/').next().unwrap_or("");
            return valid(id).then(|| id.to_string());
        }
    }
    None
}

const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "si", "feature", "pp", "igsh",
];

fn normalize_url(url: &str) -> String {
    let Some((scheme, host, port, path, query)) = split_url(url) else {
        return url.trim().to_string();
    };
    let mut params: Vec<(&str, &str)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            if k.starts_with("utm_") || TRACKING_PARAMS.contains(&k) {
                None
            } else {
                Some((k, v))
            }
        })
        .collect();
    params.sort();
    // A non-default port is part of the page's identity (a dev
    // server on :8931 is not the site on :443); the default port
    // is not — the same page is routinely linked both ways.
    let default_port = (scheme == "http" && port == "80") || (scheme == "https" && port == "443");
    let authority = if port.is_empty() || default_port {
        host
    } else {
        format!("{host}:{port}")
    };
    let base = format!("{scheme}://{authority}{path}");
    if params.is_empty() {
        base
    } else {
        let q: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("{base}?{}", q.join("&"))
    }
}

/* ------------------------------------------------------------------ */
/* Grouping: the DASH parts of one video belong to one picker row     */
/* ------------------------------------------------------------------ */

/// One selectable picker row: a combined stream on its own, one
/// video stream paired with the best captured audio stream, or a
/// lone audio stream.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedGroup {
    pub streams: Vec<CapturedStream>,
    pub audio_only: bool,
    pub label: String,
    pub height: u64,
    pub ext: String,
}

fn height_of(label: &str) -> u64 {
    let digits: String = label.chars().take_while(|c| c.is_ascii_digit()).collect();
    if label.chars().nth(digits.len()) == Some('p') {
        digits.parse().unwrap_or(0)
    } else {
        0
    }
}

/// The badge for a stream: its MIME type first, the URL's file
/// extension as the fallback, upper-cased for display.
fn badge_of(mime: &str, url: &str) -> String {
    let from_mime = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let from_url = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let pick = |s: &str| -> Option<String> {
        match s {
            "mp4" | "m4a" => Some("MP4".to_string()),
            "webm" => Some("WEBM".to_string()),
            "mpeg" | "mp3" => Some("MP3".to_string()),
            "ogg" | "opus" => Some("OGG".to_string()),
            "aac" => Some("AAC".to_string()),
            "flac" => Some("FLAC".to_string()),
            "wav" | "x-wav" => Some("WAV".to_string()),
            "mov" | "quicktime" => Some("MOV".to_string()),
            _ => None,
        }
    };
    pick(&from_mime)
        .or_else(|| pick(&from_url))
        .unwrap_or_else(|| "MP4".to_string())
}

/// The file-extension token a stream's part file carries: a plain
/// short alphanumeric, like the resolve path's own cleaning.
fn ext_token(mime: &str, url: &str) -> String {
    let badge = badge_of(mime, url).to_ascii_lowercase();
    match badge.as_str() {
        "mp4" => {
            // an audio/mp4 stream is an .m4a part, not .mp4
            if mime.starts_with("audio/") {
                "m4a".to_string()
            } else {
                "mp4".to_string()
            }
        }
        "mp3" => "mp3".to_string(),
        "ogg" => "ogg".to_string(),
        other => other.to_string(),
    }
}

/// Group a page's captured streams into picker rows. Pure and
/// deterministic (streams are canonically sorted first), so the
/// picker, the add path and a post-restart resume all derive the
/// SAME groups — and the same group keys — from the same set.
pub fn group_streams(streams: &[CapturedStream]) -> Vec<CapturedGroup> {
    let mut combined: Vec<&CapturedStream> = Vec::new();
    let mut videos: Vec<&CapturedStream> = Vec::new();
    let mut audios: Vec<&CapturedStream> = Vec::new();
    for s in streams {
        match (s.has_video, s.has_audio) {
            (true, true) => combined.push(s),
            (true, false) => videos.push(s),
            (false, true) => audios.push(s),
            (false, false) => {
                // the observer could not tell: an audio/* MIME is
                // an audio stream, anything else a media element
                // played whole is treated as a combined stream
                if s.mime_type.starts_with("audio/") {
                    audios.push(s);
                } else {
                    combined.push(s);
                }
            }
        }
    }
    let height = |s: &CapturedStream| height_of(s.quality_label.as_deref().unwrap_or(""));
    videos.sort_by(|a, b| height(b).cmp(&height(a)).then_with(|| a.url.cmp(&b.url)));
    combined.sort_by(|a, b| height(b).cmp(&height(a)).then_with(|| a.url.cmp(&b.url)));
    // the best audio pairs with every video: prefer an mp4/m4a
    // track (it muxes into the mp4 output without surprises),
    // then the URL for determinism
    audios.sort_by(|a, b| {
        let rank = |s: &CapturedStream| {
            if s.mime_type.starts_with("audio/mp4") {
                0
            } else if s.mime_type.starts_with("audio/") {
                1
            } else {
                2
            }
        };
        rank(a).cmp(&rank(b)).then_with(|| a.url.cmp(&b.url))
    });
    let best_audio = audios.first().copied();

    let mut groups: Vec<CapturedGroup> = Vec::new();
    for s in combined {
        let label = s
            .quality_label
            .clone()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "Video".to_string());
        groups.push(CapturedGroup {
            streams: vec![(*s).clone()],
            audio_only: false,
            height: height(s),
            ext: badge_of(&s.mime_type, &s.url),
            label,
        });
    }
    for v in videos {
        let mut parts = vec![(*v).clone()];
        if let Some(a) = best_audio {
            parts.push((*a).clone());
        }
        let label = v
            .quality_label
            .clone()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "Video".to_string());
        groups.push(CapturedGroup {
            streams: parts,
            audio_only: false,
            height: height(v),
            ext: badge_of(&v.mime_type, &v.url),
            label,
        });
    }
    if groups.is_empty() {
        if let Some(a) = best_audio {
            groups.push(CapturedGroup {
                streams: vec![(*a).clone()],
                audio_only: true,
                label: "Audio only".to_string(),
                height: 0,
                ext: badge_of(&a.mime_type, &a.url),
            });
        }
    }
    groups
}

/// A group's stable key: FNV-1a over its streams' URLs. The
/// picker writes it into the option's selector (`ndmcap:<key>`);
/// the add path re-derives it from the stored set, so the two
/// agree without any index bookkeeping — and an expired group
/// dropping out can never shift another group's key.
pub fn group_key(group: &CapturedGroup) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for s in &group.streams {
        for b in s.url.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn selector_for(group: &CapturedGroup) -> String {
    format!("{SELECTOR_PREFIX}{}", group_key(group))
}

pub fn is_captured_selector(selector: &str) -> bool {
    selector.starts_with(SELECTOR_PREFIX)
}

/* ------------------------------------------------------------------ */
/* Expiry: signed stream URLs are perishable                          */
/* ------------------------------------------------------------------ */

/// The expiry a signed stream URL declares about itself
/// (googlevideo's `expire` parameter), if it declares one.
pub fn stream_expiry(url: &str) -> Option<u64> {
    let query = split_url(url)?.4;
    query_param(&query, "expire")?.parse().ok()
}

/// A group is dead when ANY of its streams is past its own
/// declared expiry — a video/audio pair with one dead half is
/// not half a download.
pub fn group_expired(group: &CapturedGroup, now: u64) -> bool {
    group
        .streams
        .iter()
        .any(|s| stream_expiry(&s.url).map(|e| e <= now).unwrap_or(false))
}

/// The groups of a set that can still be downloaded.
pub fn live_groups(set: &CapturedSet, now: u64) -> Vec<CapturedGroup> {
    group_streams(&set.streams)
        .into_iter()
        .filter(|g| !group_expired(g, now))
        .collect()
}

/* ------------------------------------------------------------------ */
/* The store: memory first, a small JSON file as the durable copy     */
/* ------------------------------------------------------------------ */

static SETS: Mutex<Vec<(String, CapturedSet)>> = Mutex::new(Vec::new());

fn sets_path(dir: &Path) -> PathBuf {
    dir.join(SETS_FILE)
}

fn load_sets(dir: &Path) -> Vec<(String, CapturedSet)> {
    let Ok(raw) = std::fs::read_to_string(sets_path(dir)) else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, CapturedSet>>(&raw) else {
        return Vec::new();
    };
    map.into_iter().collect()
}

fn persist_sets(dir: &Path, sets: &[(String, CapturedSet)]) {
    let map: HashMap<&String, &CapturedSet> = sets.iter().map(|(k, v)| (k, v)).collect();
    let Ok(body) = serde_json::to_string(&map) else {
        return;
    };
    let path = sets_path(dir);
    // temp + rename, the same torn-write discipline as the
    // engine's state saves
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Record a freshly captured set (replacing any older set for
/// the same video — the newest capture is the live one) and keep
/// the durable copy in step. `dir` is the app-data dir; None is
/// memory-only (tests, pre-init callers).
pub fn store_set(dir: Option<&Path>, set: CapturedSet) {
    let identity = set_identity(&set);
    let mut sets = SETS.lock().unwrap();
    if sets.is_empty() {
        if let Some(d) = dir {
            *sets = load_sets(d);
        }
    }
    sets.retain(|(k, _)| k != &identity);
    sets.push((identity, set));
    while sets.len() > SETS_CAP {
        sets.remove(0);
    }
    if let Some(d) = dir {
        persist_sets(d, &sets);
    }
}

/// The stored set for one identity, if any — and not older than
/// a set may ever be. Memory first, the durable copy behind it
/// (a picker opened after an app restart still finds the
/// capture; the streams themselves may of course have expired,
/// which the caller judges with live_groups / group_expired).
pub fn get_set(dir: Option<&Path>, identity: &str) -> Option<CapturedSet> {
    let now = now_unix();
    let fresh = |s: &CapturedSet| now.saturating_sub(s.received_unix) <= SET_MAX_AGE_SECS;
    {
        let sets = SETS.lock().unwrap();
        if let Some((_, s)) = sets.iter().find(|(k, _)| k == identity) {
            return if fresh(s) { Some(s.clone()) } else { None };
        }
    }
    let d = dir?;
    let loaded = load_sets(d);
    let found = loaded
        .iter()
        .find(|(k, _)| k == identity)
        .map(|(_, s)| s.clone());
    if found.is_some() {
        // warm memory with the durable copy for the next lookup
        let mut sets = SETS.lock().unwrap();
        if sets.is_empty() {
            *sets = loaded;
        }
    }
    found.filter(fresh)
}

/* ------------------------------------------------------------------ */
/* Picker presentation: merge, or stand alone when yt-dlp failed      */
/* ------------------------------------------------------------------ */

/// The picker rows one set contributes — live groups only, each
/// carrying the source marker and the "From your browser tab"
/// note the shells render.
pub fn captured_options(set: &CapturedSet, now: u64) -> Vec<QualityOption> {
    live_groups(set, now)
        .iter()
        .map(|g| QualityOption {
            selector: selector_for(g),
            label: g.label.clone(),
            ext: g.ext.clone(),
            note: "From your browser tab".to_string(),
            size_bytes: 0,
            size_approx: false,
            size_text: String::new(),
            audio: g.audio_only,
            height: g.height,
            source: "browser".to_string(),
        })
        .collect()
}

/// Merge a captured set into a probed picker's options: the SAME
/// video stays ONE picker entry with two sources (the design's
/// locked dedupe). The default order keeps yt-dlp first;
/// `foreground` (yt-dlp has no JS runtime here, so its list is
/// the weak one) puts the captured rows first, where the
/// picker's preselect lands. Returns how many rows were added.
pub fn merge_into_info(info: &mut VideoInfo, set: &CapturedSet, foreground: bool, now: u64) -> usize {
    let opts = captured_options(set, now);
    let n = opts.len();
    if n == 0 {
        return 0;
    }
    if foreground {
        let mut merged = opts;
        merged.append(&mut info.options);
        info.options = merged;
    } else {
        info.options.extend(opts);
    }
    n
}

/// The picker content when yt-dlp could not read the page at all
/// but the browser could play it: the captured set on its own,
/// under the page's own title.
pub fn bare_info(set: &CapturedSet, now: u64) -> VideoInfo {
    VideoInfo {
        title: set.title.clone(),
        duration: 0,
        thumbnail: String::new(),
        uploader: String::new(),
        options: captured_options(set, now),
    }
}

/* ------------------------------------------------------------------ */
/* Starting a download from a captured option                         */
/* ------------------------------------------------------------------ */

/// Resolve a picker selector to the streams a job must download.
/// Ok(None) for an ordinary yt-dlp selector; Err with a
/// plain-language message when a captured selector names a group
/// that is gone or whose links have expired — never a raw error.
pub fn preresolved_for(
    dir: Option<&Path>,
    page_url: &str,
    selector: &str,
    now: u64,
) -> Result<Option<Preresolved>, String> {
    let Some(key) = selector.strip_prefix(SELECTOR_PREFIX) else {
        return Ok(None);
    };
    let identity = identity_for_url(page_url);
    let set = get_set(dir, &identity).ok_or_else(|| MISSING_MESSAGE.to_string())?;
    let group = group_streams(&set.streams)
        .into_iter()
        .find(|g| group_key(g) == key)
        .ok_or_else(|| MISSING_MESSAGE.to_string())?;
    if group_expired(&group, now) {
        return Err(EXPIRED_MESSAGE.to_string());
    }
    let streams = group
        .streams
        .iter()
        .map(|s| PreresolvedStream {
            url: s.url.clone(),
            ext: ext_token(&s.mime_type, &s.url),
            has_audio: s.has_audio,
            cookie: None,
            referer: Some(set.page_url.clone()),
            // an empty UA is no UA: never let a blank string
            // replace the engine's fallback on the wire
            user_agent: s
                .user_agent
                .clone()
                .filter(|u| !u.trim().is_empty()),
        })
        .collect();
    Ok(Some(Preresolved {
        title: set.title.clone(),
        streams,
        expect_audio: group.streams.iter().any(|s| s.has_audio),
    }))
}

/* ------------------------------------------------------------------ */
/* Cookies: the Netscape jar the extension captured, read for one URL */
/* ------------------------------------------------------------------ */

/// Convert a Netscape cookie export into a Cookie header for ONE
/// target URL — the same rules the host bridge applies (domain
/// flag, '/'-boundary path match, Secure stays on https, expired
/// cookies are never sent), with the clock injected for tests.
pub fn cookie_header_from_netscape(text: &str, url: &str, now: u64) -> Option<String> {
    let after = url.split("://").nth(1).unwrap_or("");
    let (hostport, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    let host = hostport
        .split(':')
        .next()
        .unwrap_or(hostport)
        .to_ascii_lowercase();
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let is_https = url.starts_with("https://");
    let now = now as i64;
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        let dom = f[0].trim_start_matches('.').to_ascii_lowercase();
        let include_subdomains = f[1].trim().eq_ignore_ascii_case("true");
        let domain_match = !dom.is_empty()
            && (host == dom || (include_subdomains && host.ends_with(&format!(".{dom}"))));
        let cookie_path = if f[2].is_empty() { "/" } else { f[2] };
        let path_match = path == cookie_path
            || (path.starts_with(cookie_path)
                && (cookie_path.ends_with('/') || path[cookie_path.len()..].starts_with('/')));
        if !domain_match || !path_match {
            continue;
        }
        if f[3].eq_ignore_ascii_case("true") && !is_https {
            continue;
        }
        if let Ok(expiry) = f[4].trim().parse::<i64>() {
            if expiry > 0 && expiry <= now {
                continue;
            }
        }
        pairs.push(format!("{}={}", f[5], f[6]));
    }
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(url: &str, mime: &str, label: &str, has_audio: bool, has_video: bool) -> CapturedStream {
        CapturedStream {
            url: url.to_string(),
            mime_type: mime.to_string(),
            quality_label: if label.is_empty() {
                None
            } else {
                Some(label.to_string())
            },
            has_audio,
            has_video,
            user_agent: None,
        }
    }

    fn dash_set(page: &str, expire: u64) -> CapturedSet {
        CapturedSet {
            page_url: page.to_string(),
            title: "Some Video".to_string(),
            streams: vec![
                stream(
                    &format!("https://rr1.googlevideo.com/videoplayback?expire={expire}&itag=137&mime=video%2Fmp4"),
                    "video/mp4", "1080p", false, true,
                ),
                stream(
                    &format!("https://rr1.googlevideo.com/videoplayback?expire={expire}&itag=136&mime=video%2Fmp4"),
                    "video/mp4", "720p", false, true,
                ),
                stream(
                    &format!("https://rr1.googlevideo.com/videoplayback?expire={expire}&itag=140&mime=audio%2Fmp4"),
                    "audio/mp4", "", true, false,
                ),
            ],
            received_unix: 1_000,
            video_key: String::new(),
        }
    }

    #[test]
    fn identity_unifies_every_youtube_shape() {
        let watch = video_identity("https://www.youtube.com/watch?v=abcDEF123_-&utm_source=x&t=4");
        assert_eq!(watch, "yt:abcDEF123_-");
        assert_eq!(video_identity("https://youtu.be/abcDEF123_-?si=zzz"), watch);
        assert_eq!(video_identity("https://www.youtube.com/shorts/abcDEF123_-"), watch);
        assert_eq!(video_identity("https://www.youtube.com/live/abcDEF123_-?feature=share"), watch);
        assert_eq!(video_identity("https://www.youtube.com/embed/abcDEF123_-"), watch);
        assert_ne!(video_identity("https://www.youtube.com/watch?v=otherID999"), watch);
    }

    #[test]
    fn identity_normalizes_generic_urls() {
        let a = video_identity("https://Example.com/watch/42?utm_medium=m&id=7#frag");
        let b = video_identity("https://example.com/watch/42?id=7");
        assert_eq!(a, b);
        assert_ne!(a, video_identity("https://example.com/watch/43?id=7"));
        // a non-default port is a different page; the default
        // port spelled out is the same page
        assert_ne!(
            video_identity("http://127.0.0.1:8931/"),
            video_identity("http://127.0.0.1:9999/")
        );
        assert_eq!(
            video_identity("https://example.com:443/watch/42?id=7"),
            b
        );
    }

    #[test]
    fn facebook_video_id_names_one_video_or_none() {
        // the same shapes the extension's facebookVideoId accepts
        assert_eq!(
            facebook_video_id("https://www.facebook.com/SomePage/videos/1234567890123456/").as_deref(),
            Some("1234567890123456")
        );
        assert_eq!(
            facebook_video_id("https://www.facebook.com/reel/9876543210987654").as_deref(),
            Some("9876543210987654")
        );
        assert_eq!(
            facebook_video_id("https://www.facebook.com/watch/?v=1234567890123456").as_deref(),
            Some("1234567890123456")
        );
        // a feed, a profile and a shortlink name NO video
        assert_eq!(facebook_video_id("https://www.facebook.com/"), None);
        assert_eq!(facebook_video_id("https://www.facebook.com/SomePage"), None);
        assert_eq!(facebook_video_id("https://fb.watch/abcDEF/"), None);
        assert_eq!(facebook_video_id("https://www.youtube.com/watch?v=abcDEF123_-"), None);
    }

    #[test]
    fn fb_permalink_set_identity_matches_the_url_lookup() {
        // a set captured on a permalink page and a probe of that
        // page URL must land on the SAME identity, keyed by the
        // video — not by the tracking-laden URL it arrived as
        let page = "https://www.facebook.com/SomePage/videos/1234567890123456/?utm_source=x";
        assert_eq!(identity_for_url(page), "fb:1234567890123456");
        let set = CapturedSet {
            page_url: page.to_string(),
            title: "Permalink video".to_string(),
            streams: vec![stream(
                "https://video.example.fbcdn.net/v.mp4",
                "video/mp4",
                "720p",
                true,
                true,
            )],
            received_unix: now_unix(),
            video_key: "fb:1234567890123456".to_string(),
        };
        assert_eq!(set_identity(&set), identity_for_url(page));
    }

    #[test]
    fn fb_feed_sets_with_different_video_keys_never_merge() {
        // two videos recorded on ONE feed page (the recorder's
        // per-video grouping key is the only thing told apart):
        // each set is stored under its own identity, and the bare
        // feed identity holds NO merged set at all
        let feed = format!("https://www.facebook.com/wsb-feed-{}", std::process::id());
        let mk = |key: &str, url: &str| CapturedSet {
            page_url: feed.clone(),
            title: "Feed video".to_string(),
            streams: vec![stream(url, "video/mp4", "720p", true, true)],
            received_unix: now_unix(),
            video_key: key.to_string(),
        };
        let a = mk("fb:111111111111111", "https://video.example.fbcdn.net/a-only.mp4");
        let b = mk("fb:222222222222222", "https://video.example.fbcdn.net/b-only.mp4");
        assert_ne!(set_identity(&a), set_identity(&b));
        store_set(None, a.clone());
        store_set(None, b.clone());
        let got_a = get_set(None, &set_identity(&a)).expect("set A stored");
        let got_b = get_set(None, &set_identity(&b)).expect("set B stored");
        assert_eq!(got_a.streams.len(), 1);
        assert_eq!(got_a.streams[0].url, a.streams[0].url);
        assert_eq!(got_b.streams.len(), 1);
        assert_eq!(got_b.streams[0].url, b.streams[0].url);
        assert!(
            get_set(None, &video_identity(&feed)).is_none(),
            "no feed-wide merged set may exist"
        );
    }

    #[test]
    fn grouping_pairs_each_video_with_the_best_audio() {
        let set = dash_set("https://www.youtube.com/watch?v=abcDEF123_-", 9_999_999_999);
        let groups = group_streams(&set.streams);
        assert_eq!(groups.len(), 2, "one row per video quality: {groups:?}");
        assert_eq!(groups[0].label, "1080p");
        assert_eq!(groups[0].streams.len(), 2, "video + audio parts together");
        assert!(groups[0].streams[0].has_video, "the video part leads the pair");
        assert!(groups[0].streams[1].has_audio);
        assert_eq!(groups[1].label, "720p");
        assert_eq!(groups[1].streams.len(), 2);
    }

    #[test]
    fn grouping_combined_and_audio_only() {
        let combined = vec![stream("https://cdn.example.com/v.mp4", "video/mp4", "", true, true)];
        let g = group_streams(&combined);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].streams.len(), 1);
        assert!(!g[0].audio_only);
        let audio = vec![stream("https://cdn.example.com/a.m4a", "audio/mp4", "", true, false)];
        let g = group_streams(&audio);
        assert_eq!(g.len(), 1);
        assert!(g[0].audio_only);
    }

    #[test]
    fn grouping_is_deterministic_under_shuffle() {
        let set = dash_set("https://www.youtube.com/watch?v=abcDEF123_-", 9_999_999_999);
        let mut shuffled = set.streams.clone();
        shuffled.reverse();
        let keys = |ss: &[CapturedStream]| group_streams(ss).iter().map(group_key).collect::<Vec<_>>();
        assert_eq!(keys(&set.streams), keys(&shuffled));
        let sels = |ss: &[CapturedStream]| {
            group_streams(ss).iter().map(selector_for).collect::<Vec<_>>()
        };
        assert_eq!(sels(&set.streams), sels(&shuffled));
    }

    #[test]
    fn expiry_comes_from_the_urls_own_expire_param() {
        let past = dash_set("https://www.youtube.com/watch?v=abcDEF123_-", 500);
        assert!(live_groups(&past, 1_000).is_empty(), "expired groups are not offered");
        let future = dash_set("https://www.youtube.com/watch?v=abcDEF123_-", 9_999_999_999);
        assert_eq!(live_groups(&future, 1_000).len(), 2);
        // one dead half kills the pair
        let mut mixed = dash_set("https://www.youtube.com/watch?v=abcDEF123_-", 9_999_999_999);
        mixed.streams[2] = stream(
            "https://rr1.googlevideo.com/videoplayback?expire=500&itag=140",
            "audio/mp4", "", true, false,
        );
        assert!(live_groups(&mixed, 1_000).is_empty());
    }

    #[test]
    fn merge_keeps_one_entry_with_two_sources() {
        let set = dash_set("https://www.youtube.com/watch?v=abcDEF123_-", 9_999_999_999);
        let mut info = VideoInfo {
            title: "Some Video".to_string(),
            duration: 60,
            thumbnail: String::new(),
            uploader: String::new(),
            options: vec![QualityOption {
                selector: "bv+ba".to_string(),
                label: "1080p".to_string(),
                ext: "MP4".to_string(),
                note: String::new(),
                size_bytes: 10,
                size_approx: false,
                size_text: String::new(),
                audio: false,
                height: 1080,
                source: String::new(),
            }],
        };
        // default: yt-dlp first, captured appended
        assert_eq!(merge_into_info(&mut info, &set, false, 1_000), 2);
        assert_eq!(info.options.len(), 3);
        assert_eq!(info.options[0].source, "");
        assert_eq!(info.options[1].source, "browser");
        assert!(info.options[1].selector.starts_with(SELECTOR_PREFIX));
        // foregrounded: captured rows lead
        let mut info2 = info.clone();
        info2.options.truncate(1);
        assert_eq!(merge_into_info(&mut info2, &set, true, 1_000), 2);
        assert_eq!(info2.options[0].source, "browser");
        assert_eq!(info2.options[2].source, "");
    }

    #[test]
    fn capture_user_agent_field_is_backwards_compatible() {
        // a set stored by a build from before the userAgent field
        // existed (audit-232 S3) has no such key anywhere and must
        // still parse, with the UA absent; a set carrying one
        // round-trips it verbatim
        let old: CapturedSet = serde_json::from_str(
            r#"{"pageUrl":"https://www.youtube.com/watch?v=OLDtest999","title":"t",
                "streams":[{"url":"https://rr1.googlevideo.com/videoplayback?itag=136",
                             "mimeType":"video/mp4","qualityLabel":"720p",
                             "hasAudio":false,"hasVideo":true}],
                "receivedUnix":1000}"#,
        )
        .expect("an old-format set must parse");
        assert_eq!(old.streams[0].user_agent, None);

        let mut set = dash_set("https://www.youtube.com/watch?v=ZZZtest999", 9_999_999_999);
        set.streams[0].user_agent = Some("Brave/131 (test)".to_string());
        let raw = serde_json::to_string(&set).unwrap();
        let back: CapturedSet = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.streams[0].user_agent.as_deref(), Some("Brave/131 (test)"));
        assert_eq!(back.streams[1].user_agent, None);
    }

    #[test]
    fn store_round_trip_and_preresolved_resolution() {
        let dir = std::env::temp_dir().join(format!("ndm-capture-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut set = dash_set("https://www.youtube.com/watch?v=ZZZtest999", 9_999_999_999);
        set.received_unix = now_unix();
        store_set(Some(&dir), set.clone());
        let got = get_set(Some(&dir), &video_identity(&set.page_url)).expect("set stored");
        assert_eq!(got, set);
        // a picker option's selector resolves back to its streams
        let opts = captured_options(&set, 1_000);
        assert_eq!(opts.len(), 2);
        let pre = preresolved_for(Some(&dir), &set.page_url, &opts[0].selector, 1_000)
            .unwrap()
            .expect("captured selector resolves");
        assert_eq!(pre.streams.len(), 2, "video + audio parts");
        assert!(!pre.streams[0].has_audio, "the first part is the video stream");
        assert!(pre.streams[1].has_audio, "the second part is the audio stream");
        assert!(pre.expect_audio);
        assert_eq!(pre.streams[0].referer.as_deref(), Some(set.page_url.as_str()));
        // an ordinary selector is not this door's business
        assert!(preresolved_for(Some(&dir), &set.page_url, "bv+ba", 1_000)
            .unwrap()
            .is_none());
        // an unknown captured key is a plain-language miss
        let err = preresolved_for(Some(&dir), &set.page_url, "ndmcap:0000000000000000", 1_000)
            .unwrap_err();
        assert_eq!(err, MISSING_MESSAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preresolved_reports_expired_links_in_plain_language() {
        let dir = std::env::temp_dir().join(format!("ndm-capture-exp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut set = dash_set("https://www.youtube.com/watch?v=EXPtest999", 500);
        set.received_unix = now_unix();
        store_set(Some(&dir), set.clone());
        let groups = group_streams(&set.streams);
        let sel = selector_for(&groups[0]);
        let now = now_unix();
        let err = preresolved_for(Some(&dir), &set.page_url, &sel, now).unwrap_err();
        assert_eq!(err, EXPIRED_MESSAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cookie_header_follows_the_hosts_rules() {
        let jar = [
            "googlevideo.com\tTRUE\t/\tTRUE\t9999999999\tsess\tabc",
            "googlevideo.com\tTRUE\t/\tFALSE\t100\texpired\tzzz",
            "other.com\tTRUE\t/\tFALSE\t9999999999\tnope\tn1",
        ]
        .join("\n");
        let h = cookie_header_from_netscape(&jar, "https://rr1.googlevideo.com/videoplayback?x=1", 1_000)
            .expect("the session cookie applies");
        assert!(h.contains("sess=abc"), "{h}");
        assert!(!h.contains("expired"), "expired cookies are never sent: {h}");
        assert!(!h.contains("nope"), "another host's cookies stay home: {h}");
        // Secure cookies never travel to http://
        assert!(cookie_header_from_netscape(&jar, "http://rr1.googlevideo.com/videoplayback?x=1", 1_000).is_none());
    }
}
