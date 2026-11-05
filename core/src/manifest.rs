//! Native HLS/DASH manifest parsing (slice 6 of the 2.3.6
//! rebuild) — the piece that lets the app's own multi-connection
//! engine download playlist streams instead of handing the whole
//! download to yt-dlp's fragment path. IDM ships its own parsers
//! for both formats (the 6.43 post-mortem: a full HLS parser in
//! NetMon, a typed MPD parser in VMPrs); this module is the
//! clean-room equivalent for the subsets real sites serve.
//!
//! Pure functions only — fetching and segment download live in
//! the caller, so every rule here is unit-testable offline.

/// Resolve a possibly-relative playlist/segment reference
/// against the URL of the document that named it. Handles
/// absolute URLs, protocol-relative `//host/…`, absolute paths
/// and relative paths with `.`/`..` segments — the shapes HLS
/// and DASH documents actually use.
pub fn resolve_url(base: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let (scheme, rest) = match base.split_once("://") {
        Some((s, r)) => (s, r),
        None => return rel.to_string(),
    };
    if let Some(r) = rel.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    let (host, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if rel.starts_with('/') {
        return format!("{scheme}://{host}{}", normalize_path_keep_slash(rel));
    }
    // Relative: resolve against the base document's directory,
    // with any query/fragment of the base dropped (the reference
    // brings its own, or none).
    let base_path = base_path.split(['?', '#']).next().unwrap_or("/");
    let dir = base_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let joined = format!("{dir}/{rel}");
    // A query/fragment on the REFERENCE survives normalization
    // of its path part only.
    let (path_part, suffix) = match joined.find(['?', '#']) {
        Some(i) => (joined[..i].to_string(), joined[i..].to_string()),
        None => (joined, String::new()),
    };
    format!(
        "{scheme}://{host}{}{suffix}",
        normalize_path_keep_slash(&path_part)
    )
}

/// normalize_path, preserving a trailing slash: a base that
/// ends in '/' is a DIRECTORY (BaseURLs almost always are),
/// and dropping the slash makes the next resolution treat the
/// last segment as a file and lose it.
fn normalize_path_keep_slash(p: &str) -> String {
    let trailing = p.ends_with('/');
    let mut out = normalize_path(p);
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn normalize_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// Parse an attribute list (`KEY=value,KEY="quoted,value",…`)
/// into (key, value) pairs. Values are unquoted; a quoted value
/// may contain commas.
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        rest = &rest[eq + 1..];
        let (val, next) = if let Some(stripped) = rest.strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => (stripped[..end].to_string(), &stripped[end + 1..]),
                None => (stripped.to_string(), ""),
            }
        } else {
            match rest.find(',') {
                Some(end) => (rest[..end].to_string(), &rest[end..]),
                None => (rest.to_string(), ""),
            }
        };
        out.push((key, val.trim().to_string()));
        rest = next.strip_prefix(',').unwrap_or(next);
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------- HLS

#[derive(Debug, Clone, PartialEq)]
pub struct HlsVariant {
    pub uri: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub codecs: String,
    pub audio_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HlsAudio {
    pub uri: String,
    pub group: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HlsMaster {
    pub variants: Vec<HlsVariant>,
    pub audios: Vec<HlsAudio>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HlsSegment {
    pub uri: String,
    /// (offset, len) byte range within the resource, when the
    /// playlist addresses segments by range (EXT-X-BYTERANGE).
    pub range: Option<(u64, u64)>,
    pub duration: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HlsMedia {
    pub segments: Vec<HlsSegment>,
    /// EXT-X-MAP initialisation segment: (uri, range).
    pub init: Option<(String, Option<(u64, u64)>)>,
    /// The EXT-X-KEY method when the media is encrypted
    /// ("AES-128", …). None = METHOD=NONE or no key tag.
    pub key_method: Option<String>,
    pub media_sequence: u64,
    pub ended: bool,
}

fn parse_byte_range(s: &str, prev_end: u64) -> Option<(u64, u64)> {
    // "len[@offset]" — offset defaults to the previous segment's
    // end (RFC 8216 §4.3.2.2).
    let (len_s, off_s) = match s.split_once('@') {
        Some((l, o)) => (l, Some(o)),
        None => (s, None),
    };
    let len: u64 = len_s.trim().parse().ok()?;
    let off: u64 = match off_s {
        Some(o) => o.trim().parse().ok()?,
        None => prev_end,
    };
    Some((off, len))
}

/// Parse a master playlist. `base` is the playlist's own URL,
/// used to resolve every relative reference it carries.
pub fn parse_hls_master(text: &str, base: &str) -> HlsMaster {
    let mut master = HlsMaster::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attrs(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attrs(rest);
            if attr(&attrs, "TYPE") == Some("AUDIO") {
                if let Some(uri) = attr(&attrs, "URI") {
                    master.audios.push(HlsAudio {
                        uri: resolve_url(base, uri),
                        group: attr(&attrs, "GROUP-ID").unwrap_or("").to_string(),
                        name: attr(&attrs, "NAME").unwrap_or("").to_string(),
                        is_default: attr(&attrs, "DEFAULT") == Some("YES"),
                    });
                }
            }
            pending = None;
        } else if line.starts_with('#') {
            // Other tags don't consume a pending STREAM-INF; the
            // variant URI still follows it. (A stray tag between
            // the two is malformed but seen in the wild.)
        } else if !line.is_empty() {
            if let Some(attrs) = pending.take() {
                let (width, height) = attr(&attrs, "RESOLUTION")
                    .and_then(|r| r.split_once('x'))
                    .map(|(w, h)| (w.parse().unwrap_or(0), h.parse().unwrap_or(0)))
                    .unwrap_or((0, 0));
                master.variants.push(HlsVariant {
                    uri: resolve_url(base, line),
                    bandwidth: attr(&attrs, "BANDWIDTH")
                        .and_then(|b| b.parse().ok())
                        .unwrap_or(0),
                    width,
                    height,
                    codecs: attr(&attrs, "CODECS").unwrap_or("").to_string(),
                    audio_group: attr(&attrs, "AUDIO").map(|s| s.to_string()),
                });
            }
        }
    }
    master
}

/// Pick the variant the app's quality story asks for: the
/// highest-bandwidth variant at or below `max_height` (0 = no
/// cap), else the highest-bandwidth variant overall.
pub fn pick_variant(master: &HlsMaster, max_height: u32) -> Option<&HlsVariant> {
    if max_height > 0 {
        if let Some(v) = master
            .variants
            .iter()
            .filter(|v| v.height > 0 && v.height <= max_height)
            .max_by_key(|v| v.bandwidth)
        {
            return Some(v);
        }
    }
    master.variants.iter().max_by_key(|v| v.bandwidth)
}

/// The default audio rendition for a variant's audio group.
pub fn pick_audio<'m>(master: &'m HlsMaster, variant: &HlsVariant) -> Option<&'m HlsAudio> {
    let group = variant.audio_group.as_deref()?;
    let in_group: Vec<&HlsAudio> = master.audios.iter().filter(|a| a.group == group).collect();
    in_group.iter().find(|a| a.is_default).copied().or_else(|| in_group.first().copied())
}

/// Parse a media playlist into its ordered segment list.
pub fn parse_hls_media(text: &str, base: &str) -> HlsMedia {
    let mut media = HlsMedia::default();
    let mut pending_duration = 0.0f64;
    let mut pending_range: Option<(u64, u64)> = None;
    let mut prev_end: u64 = 0;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media.media_sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_attrs(rest);
            if let Some(uri) = attr(&attrs, "URI") {
                let range = attr(&attrs, "BYTERANGE")
                    .and_then(|r| parse_byte_range(r, 0));
                media.init = Some((resolve_url(base, uri), range));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_attrs(rest);
            match attr(&attrs, "METHOD") {
                Some("NONE") | None => {}
                Some(m) => media.key_method = Some(m.to_string()),
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = parse_byte_range(rest.trim(), prev_end);
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_duration = rest
                .split(',')
                .next()
                .and_then(|d| d.trim().parse().ok())
                .unwrap_or(0.0);
        } else if line == "#EXT-X-ENDLIST" {
            media.ended = true;
        } else if line.starts_with('#') {
            // Discontinuity / program-date-time / target duration:
            // irrelevant to a VOD segment walk.
        } else if !line.is_empty() {
            let range = pending_range.take();
            if let Some((off, len)) = range {
                prev_end = off + len;
            }
            media.segments.push(HlsSegment {
                uri: resolve_url(base, line),
                range,
                duration: pending_duration,
            });
            pending_duration = 0.0;
        }
    }
    media
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_handles_the_playlist_shapes() {
        let base = "https://cdn.example.com/hls/master.m3u8?tok=1";
        assert_eq!(resolve_url(base, "v720/pl.m3u8"),
                   "https://cdn.example.com/hls/v720/pl.m3u8");
        assert_eq!(resolve_url(base, "/abs/seg.ts"),
                   "https://cdn.example.com/abs/seg.ts");
        assert_eq!(resolve_url(base, "//other.cdn/x.m3u8"),
                   "https://other.cdn/x.m3u8");
        assert_eq!(resolve_url(base, "https://full.url/a.ts"),
                   "https://full.url/a.ts");
        assert_eq!(resolve_url("https://h/a/b/pl.m3u8", "../seg1.ts?x=2"),
                   "https://h/a/seg1.ts?x=2");
        assert_eq!(resolve_url("https://h/a/b/pl.m3u8", "./seg.ts"),
                   "https://h/a/b/seg.ts");
    }

    const MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Hindi\",DEFAULT=NO,URI=\"audio/hi.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"aud\"\n\
v360/pl.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"aud\"\n\
v720/pl.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\"\n\
v1080/pl.m3u8\n";

    #[test]
    fn master_variants_and_audio_parse() {
        let m = parse_hls_master(MASTER, "https://cdn.example.com/hls/master.m3u8");
        assert_eq!(m.variants.len(), 3);
        assert_eq!(m.variants[1].height, 720);
        assert_eq!(m.variants[1].uri, "https://cdn.example.com/hls/v720/pl.m3u8");
        assert_eq!(m.audios.len(), 2);
        let v = pick_variant(&m, 720).unwrap();
        assert_eq!(v.height, 720);
        let v = pick_variant(&m, 0).unwrap();
        assert_eq!(v.height, 1080);
        let a = pick_audio(&m, v).unwrap();
        assert_eq!(a.name, "English");
        assert_eq!(a.uri, "https://cdn.example.com/hls/audio/en.m3u8");
    }

    const MEDIA: &str = "#EXTM3U\n\
#EXT-X-TARGETDURATION:6\n\
#EXT-X-MEDIA-SEQUENCE:42\n\
#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
#EXTINF:6.006,\n\
seg42.m4s\n\
#EXTINF:5.995,\n\
#EXT-X-BYTERANGE:1000@5000\n\
media.mp4\n\
#EXTINF:6.0,\n\
#EXT-X-BYTERANGE:900\n\
media.mp4\n\
#EXT-X-ENDLIST\n";

    #[test]
    fn media_segments_ranges_and_init_parse() {
        let m = parse_hls_media(MEDIA, "https://cdn.example.com/hls/v720/pl.m3u8");
        assert_eq!(m.media_sequence, 42);
        assert!(m.ended);
        assert_eq!(m.key_method, None);
        let (init_uri, init_range) = m.init.clone().unwrap();
        assert_eq!(init_uri, "https://cdn.example.com/hls/v720/init.mp4");
        assert_eq!(init_range, Some((0, 720)));
        assert_eq!(m.segments.len(), 3);
        assert_eq!(m.segments[0].range, None);
        assert_eq!(m.segments[1].range, Some((5000, 1000)));
        // no explicit offset: continues from the previous end
        assert_eq!(m.segments[2].range, Some((6000, 900)));
    }

    #[test]
    fn encrypted_media_is_flagged_not_silently_accepted() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n#EXTINF:4,\na.ts\n";
        let m = parse_hls_media(text, "https://h/pl.m3u8");
        assert_eq!(m.key_method.as_deref(), Some("AES-128"));
    }
}

// ---------------------------------------------------------------- DASH

#[derive(Debug, Clone, PartialEq)]
pub struct DashSegment {
    pub uri: String,
    /// (offset, len) byte range within the resource, when known.
    pub range: Option<(u64, u64)>,
    pub duration: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DashRep {
    pub id: String,
    pub mime: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub codecs: String,
    /// Initialisation segment: (uri, range) — for SegmentBase
    /// single-file reps this is None and the one media segment
    /// IS the whole file, init included.
    pub init: Option<(String, Option<(u64, u64)>)>,
    pub segments: Vec<DashSegment>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DashMpd {
    pub video: Vec<DashRep>,
    pub audio: Vec<DashRep>,
}

/// A minimal XML event scanner — just enough for MPD's regular
/// structure: elements, attributes (single/double quoted, the
/// five standard entities decoded), comments, PIs and CDATA.
#[derive(Debug, Clone, PartialEq)]
enum XmlEv {
    Start(String, Vec<(String, String)>, bool),
    End(String),
    Text(String),
}

fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_tokenize(xml: &str) -> Vec<XmlEv> {
    let b = xml.as_bytes();
    let mut evs = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'<' {
            if xml[i..].starts_with("<!--") {
                i = xml[i..].find("-->").map(|e| i + e + 3).unwrap_or(b.len());
            } else if xml[i..].starts_with("<![CDATA[") {
                let start = i + 9;
                let end = xml[start..].find("]]>").map(|e| start + e).unwrap_or(b.len());
                evs.push(XmlEv::Text(xml_unescape(&xml[start..end])));
                i = (end + 3).min(b.len());
            } else if xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
                i = xml[i..].find('>').map(|e| i + e + 1).unwrap_or(b.len());
            } else if xml[i..].starts_with("</") {
                let end = xml[i..].find('>').map(|e| i + e).unwrap_or(b.len());
                evs.push(XmlEv::End(xml[i + 2..end].trim().to_string()));
                i = (end + 1).min(b.len());
            } else {
                // start element: scan to '>' with quote awareness
                let mut j = i + 1;
                let mut q: Option<u8> = None;
                while j < b.len() {
                    match q {
                        Some(c) if b[j] == c => q = None,
                        Some(_) => {}
                        None if b[j] == b'"' || b[j] == b'\'' => q = Some(b[j]),
                        None if b[j] == b'>' => break,
                        None => {}
                    }
                    j += 1;
                }
                let inner = &xml[i + 1..j.min(b.len())];
                let self_closing = inner.trim_end().ends_with('/');
                let inner = inner.trim_end().trim_end_matches('/');
                let mut parts = inner.splitn(2, |c: char| c.is_whitespace());
                let name = parts.next().unwrap_or("").to_string();
                let attrs = parse_attrs_xml(parts.next().unwrap_or(""));
                evs.push(XmlEv::Start(name, attrs, self_closing));
                i = (j + 1).min(b.len());
            }
        } else {
            let end = xml[i..].find('<').map(|e| i + e).unwrap_or(b.len());
            let text = xml[i..end].trim();
            if !text.is_empty() {
                evs.push(XmlEv::Text(xml_unescape(text)));
            }
            i = end;
        }
    }
    evs
}

fn parse_attrs_xml(s: &str) -> Vec<(String, String)> {
    // name="value" / name='value' pairs; MPD attribute lists
    // never carry commas, unlike HLS's.
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let name_start = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        let name = s[name_start..i].to_string();
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || (b[i] != b'"' && b[i] != b'\'') {
            continue;
        }
        let quote = b[i];
        i += 1;
        let val_start = i;
        while i < b.len() && b[i] != quote {
            i += 1;
        }
        out.push((name, xml_unescape(&s[val_start..i])));
        i = (i + 1).min(b.len());
    }
    out
}

/// Parse an ISO-8601 duration ("PT1H2M3.5S", "PT90S", "P1DT2H")
/// into seconds — the subset MPD uses.
pub fn parse_iso_duration(s: &str) -> Option<f64> {
    let s = s.trim().strip_prefix('P')?;
    let (days, rest) = match s.split_once('T') {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };
    let mut secs = 0.0f64;
    for (part, is_time) in [(days, false), (rest, true)] {
        let mut num = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let v: f64 = num.parse().ok()?;
                num.clear();
                secs += match (is_time, c) {
                    (false, 'D') => v * 86400.0,
                    (false, 'W') => v * 604800.0,
                    (false, 'Y') => v * 31536000.0,
                    (false, 'M') => v * 2592000.0,
                    (true, 'H') => v * 3600.0,
                    (true, 'M') => v * 60.0,
                    (true, 'S') => v,
                    _ => return None,
                };
            }
        }
    }
    Some(secs)
}

fn parse_range(s: &str) -> Option<(u64, u64)> {
    // DASH ranges are "start-end" INCLUSIVE → (start, len).
    let (a, b) = s.trim().split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end: u64 = b.parse().ok()?;
    if end < start {
        return None;
    }
    Some((start, end - start + 1))
}

/// Expand a SegmentTemplate URL pattern: $RepresentationID$,
/// $Number$ / $Number%0Nd$, $Time$, $Bandwidth$, $$ escape.
fn expand_template(tpl: &str, rep_id: &str, number: u64, time: u64, bandwidth: u64) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
            continue;
        }
        let Some(end) = after.find('$') else {
            out.push('$');
            rest = after;
            break;
        };
        let token = &after[..end];
        let (name, fmt) = match token.find('%') {
            Some(p) => (&token[..p], Some(&token[p..])),
            None => (token, None),
        };
        let value = match name {
            "RepresentationID" => rep_id.to_string(),
            "Number" => pad_number(number, fmt),
            "Time" => pad_number(time, fmt),
            "Bandwidth" => pad_number(bandwidth, fmt),
            _ => format!("${token}$"),
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn pad_number(n: u64, fmt: Option<&str>) -> String {
    // Only the "%0Nd" zero-pad shape is defined for templates.
    if let Some(w) = fmt
        .and_then(|f| f.strip_prefix("%0"))
        .and_then(|f| f.strip_suffix('d'))
        .and_then(|w| w.parse::<usize>().ok())
    {
        return format!("{n:0w$}");
    }
    n.to_string()
}

#[derive(Default, Clone)]
struct Template {
    media: Option<String>,
    initialization: Option<String>,
    start_number: u64,
    timescale: u64,
    duration: u64,
    timeline: Vec<(u64, u64, i64)>, // (t, d, r)
}

#[derive(Default, Clone)]
struct SegList {
    init: Option<(String, Option<(u64, u64)>)>,
    urls: Vec<(String, Option<(u64, u64)>)>,
}

#[derive(Default, Clone)]
struct SegAddr {
    template: Option<Template>,
    list: Option<SegList>,
    base_single: bool, // SegmentBase present
}

#[derive(Default)]
struct RepCtx {
    id: String,
    bandwidth: u64,
    width: u32,
    height: u32,
    codecs: String,
    mime: String,
    base: String,
    addr: SegAddr,
}

fn build_rep(ctx: &RepCtx, adapt_base: &str, adapt_addr: &SegAddr, period_secs: f64) -> Option<DashRep> {
    let base = if ctx.base.is_empty() { adapt_base } else { &ctx.base };
    // Representation-level addressing beats adaptation-level.
    let addr = if ctx.addr.template.is_some() || ctx.addr.list.is_some() || ctx.addr.base_single {
        &ctx.addr
    } else {
        adapt_addr
    };
    let head = |init: Option<(String, Option<(u64, u64)>)>, segments: Vec<DashSegment>| DashRep {
        id: ctx.id.clone(),
        mime: ctx.mime.clone(),
        bandwidth: ctx.bandwidth,
        width: ctx.width,
        height: ctx.height,
        codecs: ctx.codecs.clone(),
        init,
        segments,
    };
    if let Some(list) = &addr.list {
        let init = list.init.as_ref().map(|(u, r)| (resolve_url(base, u), *r));
        let segments = list
            .urls
            .iter()
            .map(|(u, r)| DashSegment {
                uri: resolve_url(base, u),
                range: *r,
                duration: 0.0,
            })
            .collect();
        return Some(head(init, segments));
    }
    if let Some(tpl) = &addr.template {
        let timescale = if tpl.timescale == 0 { 1 } else { tpl.timescale };
        let init = tpl.initialization.as_ref().map(|u| {
            (
                resolve_url(base, &expand_template(u, &ctx.id, tpl.start_number, 0, ctx.bandwidth)),
                None,
            )
        });
        let media = tpl.media.as_ref()?;
        let mut segments = Vec::new();
        if !tpl.timeline.is_empty() {
            let mut number = tpl.start_number;
            let mut cur_t: Option<u64> = None;
            for (t, d, r) in &tpl.timeline {
                if *d == 0 {
                    continue;
                }
                let start_t = cur_t.unwrap_or(*t);
                let count = if *r < 0 {
                    // repeats until the period ends
                    if period_secs <= 0.0 {
                        0
                    } else {
                        let end_t = (period_secs * timescale as f64).ceil() as u64;
                        if end_t <= start_t {
                            0
                        } else {
                            (end_t - start_t).div_ceil(*d)
                        }
                    }
                } else {
                    (*r + 1) as u64
                };
                for k in 0..count {
                    let seg_t = start_t + k * d;
                    segments.push(DashSegment {
                        uri: resolve_url(
                            base,
                            &expand_template(media, &ctx.id, number, seg_t, ctx.bandwidth),
                        ),
                        range: None,
                        duration: *d as f64 / timescale as f64,
                    });
                    number += 1;
                }
                if count > 0 {
                    cur_t = Some(start_t + count * d);
                }
            }
        } else if tpl.duration > 0 && period_secs > 0.0 {
            let count = (period_secs * timescale as f64 / tpl.duration as f64).ceil() as u64;
            for k in 0..count {
                segments.push(DashSegment {
                    uri: resolve_url(
                        base,
                        &expand_template(
                            media,
                            &ctx.id,
                            tpl.start_number + k,
                            k * tpl.duration,
                            ctx.bandwidth,
                        ),
                    ),
                    range: None,
                    duration: tpl.duration as f64 / timescale as f64,
                });
            }
        }
        return Some(head(init, segments));
    }
    if addr.base_single {
        // One file: the initialisation lives inside it, so the
        // single segment is the whole resource.
        return Some(head(
            None,
            vec![DashSegment {
                uri: base.to_string(),
                range: None,
                duration: period_secs,
            }],
        ));
    }
    None
}

/// One open element on the walk stack. `base` is the effective
/// BaseURL for this element's children; a <BaseURL> child
/// rewrites its parent's slot when it closes, so later siblings
/// resolve against it.
struct Frame {
    name: String,
    base: String,
}

/// Parse an MPD document into per-kind representation lists.
/// `base` is the MPD's own URL. Only the first Period is walked
/// — for a VOD download its representations are the file; later
/// periods are a streaming concern.
pub fn parse_mpd(text: &str, base: &str) -> DashMpd {
    let evs = xml_tokenize(text);
    let mut out = DashMpd::default();
    let mut stack: Vec<Frame> = Vec::new();
    let mut mpd_duration = 0.0f64;
    let mut period_duration = 0.0f64;
    let mut in_period = false;
    let mut period_done = false;
    let mut adapt_mime = String::new();
    let mut adapt_base = String::new();
    let mut adapt_addr = SegAddr::default();
    let mut rep: Option<RepCtx> = None;
    let mut cur_template: Option<Template> = None;
    let mut cur_list: Option<SegList> = None;
    let mut in_timeline = false;
    let mut baseurl_text = String::new();

    let effective_base = |stack: &Vec<Frame>| {
        stack.last().map(|f| f.base.clone()).unwrap_or_else(|| base.to_string())
    };

    for ev in &evs {
        match ev {
            XmlEv::Start(name, attrs, self_closing) => {
                let a = |k: &str| attr(attrs, k).map(|s| s.to_string());
                let parent_base = effective_base(&stack);
                match name.as_str() {
                    "MPD" => {
                        if let Some(d) = a("mediaPresentationDuration") {
                            mpd_duration = parse_iso_duration(&d).unwrap_or(0.0);
                        }
                    }
                    "Period" => {
                        if !period_done {
                            in_period = true;
                            period_duration = a("duration")
                                .and_then(|d| parse_iso_duration(&d))
                                .unwrap_or(0.0);
                        }
                    }
                    "AdaptationSet" if in_period => {
                        adapt_mime = a("mimeType")
                            .or_else(|| a("contentType"))
                            .unwrap_or_default();
                        adapt_base = parent_base.clone();
                        adapt_addr = SegAddr::default();
                    }
                    "Representation" if in_period => {
                        rep = Some(RepCtx {
                            id: a("id").unwrap_or_default(),
                            bandwidth: a("bandwidth").and_then(|v| v.parse().ok()).unwrap_or(0),
                            width: a("width").and_then(|v| v.parse().ok()).unwrap_or(0),
                            height: a("height").and_then(|v| v.parse().ok()).unwrap_or(0),
                            codecs: a("codecs").unwrap_or_default(),
                            mime: a("mimeType").unwrap_or_else(|| adapt_mime.clone()),
                            base: String::new(),
                            addr: SegAddr::default(),
                        });
                    }
                    "BaseURL" => {
                        baseurl_text.clear();
                        if let Some(rc) = rep.as_mut() {
                            if rc.base.is_empty() {
                                // representation-level BaseURL:
                                // its text event fills this in
                                rc.base = "\u{1}".to_string(); // marker: from BaseURL child
                            }
                        }
                    }
                    "SegmentTemplate" => {
                        cur_template = Some(Template {
                            start_number: a("startNumber").and_then(|v| v.parse().ok()).unwrap_or(1),
                            timescale: a("timescale").and_then(|v| v.parse().ok()).unwrap_or(1),
                            duration: a("duration").and_then(|v| v.parse().ok()).unwrap_or(0),
                            media: a("media"),
                            initialization: a("initialization"),
                            timeline: Vec::new(),
                        });
                    }
                    "SegmentTimeline" => in_timeline = true,
                    "S" if in_timeline => {
                        if let Some(tpl) = cur_template.as_mut() {
                            tpl.timeline.push((
                                a("t").and_then(|v| v.parse().ok()).unwrap_or(0),
                                a("d").and_then(|v| v.parse().ok()).unwrap_or(0),
                                a("r").and_then(|v| v.parse().ok()).unwrap_or(0),
                            ));
                        }
                    }
                    "SegmentList" => cur_list = Some(SegList::default()),
                    "Initialization" => {
                        let range = a("range").and_then(|r| parse_range(&r));
                        if let Some(l) = cur_list.as_mut() {
                            if let Some(u) = a("sourceURL") {
                                l.init = Some((u, range));
                            }
                        }
                    }
                    "SegmentURL" => {
                        if let Some(l) = cur_list.as_mut() {
                            if let Some(u) = a("media") {
                                let range = a("mediaRange").and_then(|r| parse_range(&r));
                                l.urls.push((u, range));
                            }
                        }
                    }
                    "SegmentBase" => {
                        if let Some(rc) = rep.as_mut() {
                            rc.addr.base_single = true;
                        } else {
                            adapt_addr.base_single = true;
                        }
                    }
                    _ => {}
                }
                stack.push(Frame {
                    name: name.clone(),
                    base: parent_base,
                });
                if *self_closing {
                    close_element(
                        name, &mut stack, &mut in_period, &mut period_done, &mut rep,
                        &mut adapt_addr, &mut cur_template, &mut cur_list, &mut in_timeline,
                        &mut out, &adapt_base, &adapt_mime, period_duration, mpd_duration,
                        &baseurl_text, base,
                    );
                }
            }
            XmlEv::Text(t) => {
                if stack.last().map(|f| f.name.as_str()) == Some("BaseURL") {
                    baseurl_text = t.clone();
                }
            }
            XmlEv::End(name) => {
                close_element(
                    name, &mut stack, &mut in_period, &mut period_done, &mut rep,
                    &mut adapt_addr, &mut cur_template, &mut cur_list, &mut in_timeline,
                    &mut out, &adapt_base, &adapt_mime, period_duration, mpd_duration,
                    &baseurl_text, base,
                );
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn close_element(
    name: &str,
    stack: &mut Vec<Frame>,
    in_period: &mut bool,
    period_done: &mut bool,
    rep: &mut Option<RepCtx>,
    adapt_addr: &mut SegAddr,
    cur_template: &mut Option<Template>,
    cur_list: &mut Option<SegList>,
    in_timeline: &mut bool,
    out: &mut DashMpd,
    adapt_base: &str,
    adapt_mime: &str,
    period_duration: f64,
    mpd_duration: f64,
    baseurl_text: &str,
    doc_base: &str,
) {
    match name {
        "BaseURL" => {
            // The BaseURL frame is on top; pop it and rewrite
            // the parent frame's base for later siblings.
            stack.pop();
            if let Some(parent) = stack.last_mut() {
                parent.base = resolve_url(&parent.base, baseurl_text);
            }
            if let Some(rc) = rep.as_mut() {
                if rc.base == "\u{1}" {
                    let parent = stack
                        .last()
                        .map(|f| f.base.clone())
                        .unwrap_or_else(|| doc_base.to_string());
                    rc.base = parent;
                }
            }
        }
        "SegmentTimeline" => *in_timeline = false,
        "SegmentTemplate" => {
            if let Some(tpl) = cur_template.take() {
                if let Some(rc) = rep.as_mut() {
                    rc.addr.template = Some(tpl);
                } else {
                    adapt_addr.template = Some(tpl);
                }
            }
            stack.pop();
        }
        "SegmentList" => {
            if let Some(l) = cur_list.take() {
                if let Some(rc) = rep.as_mut() {
                    rc.addr.list = Some(l);
                } else {
                    adapt_addr.list = Some(l);
                }
            }
            stack.pop();
        }
        "Representation" => {
            if let Some(rc) = rep.take() {
                let period_secs = if period_duration > 0.0 {
                    period_duration
                } else {
                    mpd_duration
                };
                // The adaptation base is read from the LIVE
                // stack, not the value captured when the
                // AdaptationSet opened: an AdaptationSet-level
                // <BaseURL> child rewrites its frame's base
                // after the open, and representations that
                // follow it must resolve against the rewrite.
                let live_adapt_base = stack
                    .iter()
                    .rev()
                    .find(|f| f.name == "AdaptationSet")
                    .map(|f| f.base.as_str())
                    .unwrap_or(adapt_base);
                if let Some(r) = build_rep(&rc, live_adapt_base, adapt_addr, period_secs) {
                    let mime = if !r.mime.is_empty() {
                        r.mime.clone()
                    } else {
                        adapt_mime.to_string()
                    };
                    if mime.starts_with("audio") {
                        out.audio.push(r);
                    } else {
                        out.video.push(r);
                    }
                }
            }
            stack.pop();
        }
        "Period" => {
            if *in_period {
                *in_period = false;
                *period_done = true;
            }
            stack.pop();
        }
        _ => {
            stack.pop();
        }
    }
}

/// Pick the video representation the quality story asks for and
/// the best audio representation (highest bandwidth).
pub fn pick_dash<'m>(mpd: &'m DashMpd, max_height: u32) -> (Option<&'m DashRep>, Option<&'m DashRep>) {
    let video = if max_height > 0 {
        mpd.video
            .iter()
            .filter(|r| r.height > 0 && r.height <= max_height)
            .max_by_key(|r| r.bandwidth)
            .or_else(|| mpd.video.iter().max_by_key(|r| r.bandwidth))
    } else {
        mpd.video.iter().max_by_key(|r| r.bandwidth)
    };
    let audio = mpd.audio.iter().max_by_key(|r| r.bandwidth);
    (video, audio)
}

#[cfg(test)]
mod dash_tests {
    use super::*;

    const MPD_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD mediaPresentationDuration="PT0H1M0S" type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate timescale="1000" startNumber="1"
        initialization="vinit-$RepresentationID$.mp4" media="v$Number%05d$.m4s">
        <SegmentTimeline>
          <S t="0" d="10000" r="2"/>
          <S d="10000" r="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v720" bandwidth="2500000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="v360" bandwidth="800000" width="640" height="360" codecs="avc1.4d401e"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio">
      <SegmentTemplate timescale="48000" startNumber="1"
        initialization="ainit.mp4" media="a$Time$.m4s">
        <SegmentTimeline>
          <S t="0" d="48000" r="5"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a1" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn template_timeline_expands_with_numbers_and_times() {
        let mpd = parse_mpd(MPD_TEMPLATE, "https://cdn.example.com/movie/manifest.mpd");
        assert_eq!(mpd.video.len(), 2);
        assert_eq!(mpd.audio.len(), 1);
        let v = &mpd.video[0];
        assert_eq!(v.id, "v720");
        assert_eq!(v.segments.len(), 6);
        assert_eq!(
            v.init.as_ref().unwrap().0,
            "https://cdn.example.com/movie/vinit-v720.mp4"
        );
        assert_eq!(
            v.segments[0].uri,
            "https://cdn.example.com/movie/v00001.m4s"
        );
        assert_eq!(
            v.segments[5].uri,
            "https://cdn.example.com/movie/v00006.m4s"
        );
        assert_eq!(v.segments[0].duration, 10.0);
        let a = &mpd.audio[0];
        assert_eq!(a.segments.len(), 6);
        assert_eq!(
            a.segments[3].uri,
            "https://cdn.example.com/movie/a144000.m4s"
        );
        let (pv, pa) = pick_dash(&mpd, 360);
        assert_eq!(pv.unwrap().id, "v360");
        assert_eq!(pa.unwrap().id, "a1");
    }

    const MPD_LIST: &str = r#"<MPD mediaPresentationDuration="PT30S" type="static">
  <BaseURL>https://cdn2.example.com/root/</BaseURL>
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <BaseURL>vid/</BaseURL>
      <Representation id="r1" bandwidth="1000" height="480">
        <BaseURL>rep1.mp4</BaseURL>
        <SegmentList>
          <Initialization sourceURL="init.mp4" range="0-99"/>
          <SegmentURL media="s1.m4s" mediaRange="100-199"/>
          <SegmentURL media="s2.m4s" mediaRange="200-349"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn segment_list_and_nested_baseurls_resolve() {
        let mpd = parse_mpd(MPD_LIST, "https://origin.example.com/x/manifest.mpd");
        assert_eq!(mpd.video.len(), 1);
        let v = &mpd.video[0];
        // MPD BaseURL replaces the origin entirely; the
        // adaptation "vid/" and representation file append.
        let (init_uri, init_range) = v.init.clone().unwrap();
        assert_eq!(init_uri, "https://cdn2.example.com/root/vid/init.mp4");
        assert_eq!(init_range, Some((0, 100)));
        assert_eq!(v.segments.len(), 2);
        assert_eq!(v.segments[0].uri, "https://cdn2.example.com/root/vid/s1.m4s");
        assert_eq!(v.segments[0].range, Some((100, 100)));
        assert_eq!(v.segments[1].range, Some((200, 150)));
    }

    #[test]
    fn duration_forms_and_template_duration_count() {
        assert_eq!(parse_iso_duration("PT0H1M30.5S"), Some(90.5));
        assert_eq!(parse_iso_duration("PT90S"), Some(90.0));
        assert_eq!(parse_iso_duration("P1DT2H"), Some(93600.0));
        let text = r#"<MPD mediaPresentationDuration="PT25S" type="static"><Period>
          <AdaptationSet mimeType="video/mp4">
            <SegmentTemplate timescale="1" duration="10" startNumber="3"
              initialization="i.mp4" media="s$Number$.m4s"/>
            <Representation id="x" bandwidth="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mpd = parse_mpd(text, "https://h/m.mpd");
        let v = &mpd.video[0];
        assert_eq!(v.segments.len(), 3); // ceil(25/10)
        assert_eq!(v.segments[0].uri, "https://h/s3.m4s");
        assert_eq!(v.segments[2].uri, "https://h/s5.m4s");
    }
}
