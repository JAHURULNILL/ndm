// Video downloads via yt-dlp (sidecar process) — YouTube, Facebook and the
// 1,700+ other sites yt-dlp supports. NDM never parses those sites itself;
// yt-dlp + ffmpeg do the extraction and merging, NDM owns the list, the
// progress, pause/resume and persistence — exactly like the HTTP engine.
//
// Pause = stop the yt-dlp process (its .part files stay on disk); resume =
// start it again with the same arguments, yt-dlp continues the partial files.

pub use ndm::video_probe::{base_args_for_url, find_tool, probe, VideoInfo};

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime};

/// Poison-tolerant lock for the settle/delete paths (mirrors the
/// app side's LockOk): a delete must never die — on Windows as a
/// SIGABRT — because some earlier panic poisoned a job mutex.
fn lock_ok<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Build a Command for a bundled console tool (yt-dlp/ffmpeg).
/// On Windows the app is a GUI-subsystem process with no console
/// of its own, so a console child would allocate a flashing
/// console window unless created with CREATE_NO_WINDOW;
/// elsewhere this is a plain Command::new.
fn tool_cmd<S: AsRef<std::ffi::OsStr>>(prog: S) -> Command {
    #[allow(unused_mut)] // mut is only exercised by the Windows branch
    let mut c = Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Parse `ffmpeg -hide_banner -i <file>` stderr to check `(has_video, has_audio)`.
pub fn streams_in_ffmpeg_info(text: &str) -> (bool, bool) {
    let mut video = false;
    let mut audio = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("Stream #") {
            if t.contains(": Video:") {
                video = true;
            } else if t.contains(": Audio:") {
                audio = true;
            }
        }
    }
    (video, audio)
}

/// Inspect a finished media file with `ffmpeg -hide_banner -i <path>` to
/// verify which stream types (`(has_video, has_audio)`) are actually present.
pub fn probe_streams(path: &Path, components_dir: &Path) -> Option<(bool, bool)> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = tool_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    Some(streams_in_ffmpeg_info(&stderr))
}

/// The codec name of the first audio stream in `ffmpeg -i`
/// output (e.g. "aac", "opus", "mp3"), lower-cased. Pure parse,
/// unit-tested. None when the file reports no audio stream.
pub fn audio_codec_in_ffmpeg_info(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("Stream #") {
            if let Some(rest) = t.split(": Audio: ").nth(1) {
                let codec: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !codec.is_empty() {
                    return Some(codec.to_lowercase());
                }
            }
        }
    }
    None
}

/// The first audio stream's codec in a media file, via the same
/// `ffmpeg -i` inspection probe_streams uses. None when the
/// file cannot be probed or carries no audio.
pub fn probe_audio_codec(path: &Path, components_dir: &Path) -> Option<String> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = tool_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    audio_codec_in_ffmpeg_info(&String::from_utf8_lossy(&out.stderr))
}

/// The merge's audio arguments for an input whose audio may
/// already be AAC (WS-D D3). The pair/single merges have always
/// normalized audio to AAC 192k so an MP4 never carries a codec
/// players refuse (Opus-in-MP4) — but when the source audio IS
/// AAC already, the re-encode is pure cost: on the audit's pair
/// fixture it owned ~9.0 s of the ~10 s last-byte-to-finished
/// tail (a 128 s audio track re-encoded at ~14x realtime),
/// while copying the same track through the same merge took
/// 0.16 s. Copying also avoids a lossy-to-lossy generation.
/// Non-AAC (or unprobeable) audio keeps the re-encode exactly
/// as before.
fn merge_audio_args(audio_source: &Path, components_dir: &Path) -> Vec<String> {
    if probe_audio_codec(audio_source, components_dir).as_deref() == Some("aac") {
        vec!["-c:a".into(), "copy".into()]
    } else {
        vec![
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "192k".into(),
        ]
    }
}

/// Completion size reconciliation for MERGE/REMUX jobs (F-EXT-04,
/// R3-EXT-01). The question at the finish line is whether the merged
/// output holds the media this job selected — so the baseline is
/// `expected_media`, the expected size of the SELECTED media streams
/// (the chosen video + audio payloads), and never the raw downloaded
/// byte count: a container may legitimately carry a large non-media
/// payload (padding, embedded data, extra renditions) that a remux
/// drops, so a correct merge can be a small fraction of what was
/// downloaded. The earlier form of this guard compared against the
/// download total and declared byte-perfect merges broken (the R3
/// fixture: 100 MB container, 2.3 MB of media, errored 3×). A merged
/// file under half of the expected media is still the drastic case
/// it exists for — a stub where the real movie should be.
///
/// `expected_media == None` (or 0) means the selected streams'
/// sizes are genuinely unknown — the source reported no bitrates to
/// estimate from — and the guard then SKIPS the job rather than
/// fabricating a verdict. `downloaded` is accepted so callers and
/// tests can state the full scenario; it is deliberately never
/// compared against.
///
/// SCOPE (R2-EXT-02): callers apply this ONLY to merge/remux jobs.
/// Audio extraction/transcode jobs (`self.audio`) are never judged
/// by it: their correct output is routinely a small fraction of the
/// source container, and the guard used to fail every one of them
/// after a perfect download.
fn drastic_shrink(
    expected_media: Option<u64>,
    downloaded: u64,
    final_len: u64,
) -> Option<String> {
    let _ = downloaded; // context only — see the note above
    let expected = expected_media?;
    if expected > 0 && final_len.saturating_mul(2) < expected {
        Some(format!(
            "the merged file is much smaller than what was downloaded ({final_len} of {expected} bytes of media in the selected streams) — the source streams may be incomplete"
        ))
    } else {
        None
    }
}

/// The container duration in `ffmpeg -i` text ("Duration:
/// HH:MM:SS.xx, ..."), in seconds. None when unreported.
fn duration_in_ffmpeg_info(text: &str) -> Option<f64> {
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Duration: ") {
            let token = rest.split(',').next()?.trim();
            if token == "N/A" {
                return None;
            }
            let mut parts = token.split(':');
            let h: f64 = parts.next()?.parse().ok()?;
            let m: f64 = parts.next()?.parse().ok()?;
            let s: f64 = parts.next()?.parse().ok()?;
            return Some(h * 3600.0 + m * 60.0 + s);
        }
    }
    None
}

/// Run `ffmpeg -i` over a file and read its container duration.
fn probe_duration_secs(path: &Path, components_dir: &Path) -> Option<f64> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = tool_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    duration_in_ffmpeg_info(&String::from_utf8_lossy(&out.stderr))
}

/// W-2 finish-line contradiction check. `drastic_shrink` judges a
/// merge against the RESOLVED streams; this judges the produced
/// file against what the user was SHOWN and picked at add time —
/// the chosen option's probed size and the page video's probed
/// duration (0 = unknown, that half of the check then sits out).
/// A plain contradiction — the delivered file runs half the
/// expected length or less (or twice it), or lands under a quarter
/// of the expected size — means a different stream than the one
/// probed was delivered (the walkthrough's silent-substitution
/// case), and the entry must say so instead of presenting a clean
/// success. Deliberately NOT judged: the raw downloaded byte count
/// (a padded container legitimately remuxes down to its media —
/// R3-EXT-01), and the size half never applies to audio jobs,
/// whose transcodes legitimately change size (only their duration
/// is checked).
/// The duration half of the W-2 contradiction check, on its own:
/// a produced file running half the expected length or less (or
/// twice it) contradicts the probed page video. Shared by
/// `contradiction_verdict` and the M-15-aware finish line in
/// `VideoJob::finish_contradiction`.
fn duration_contradiction(
    expect_duration: f64,
    produced_duration: Option<f64>,
) -> Option<String> {
    if expect_duration >= 1.0 {
        if let Some(d) = produced_duration {
            if d >= 0.5 && (d < expect_duration * 0.5 || d > expect_duration * 2.0) {
                return Some(format!(
                    "the downloaded file runs about {}s but the page's video is about {}s — the wrong stream may have been delivered; please retry",
                    d.round() as u64,
                    expect_duration.round() as u64
                ));
            }
        }
    }
    None
}

fn contradiction_verdict(
    expect_size: u64,
    expect_duration: f64,
    produced_size: u64,
    produced_duration: Option<f64>,
    audio: bool,
) -> Option<String> {
    if let Some(msg) = duration_contradiction(expect_duration, produced_duration) {
        return Some(msg);
    }
    if !audio && expect_size > 0 && produced_size > 0 && produced_size.saturating_mul(4) < expect_size {
        return Some(format!(
            "the downloaded file is {produced_size} bytes but the quality you picked was about {expect_size} bytes — the wrong stream may have been delivered; please retry"
        ));
    }
    None
}

/// M-15 finish-line size judgement. The old finish line compared
/// the merged file against `expect_size` alone — and `expect_size`
/// can itself be a poisoned figure (a journal total written by the
/// pre-fix fragment accounting, restored into the job on resume),
/// which branded byte-perfect merges as "the wrong stream". The
/// trustworthy baselines, in order:
///
/// 1. `parts_sum` — the bytes THIS run actually downloaded (the
///    tracker's per-file exact totals). A merged file in
///    [parts/4, parts×4] IS what was downloaded; no expectation,
///    exact or estimated, can overrule that.
/// 2. `format_size` when EXACT (the selected formats' `filesize`
///    sum from the probe): judged with the same 4× rule.
/// 3. `format_size` as an ESTIMATE (`filesize_approx`): an
///    estimate must never brand a byte-valid file an error — when
///    the produced file passes stream/duration sanity
///    (`sanity_ok`), the size half is waived entirely.
/// 4. The legacy rule against `expect_size`, for jobs with no
///    better baseline.
fn completion_size_error(
    expect_size: u64,
    format_size: Option<u64>,
    format_exact: bool,
    parts_sum: u64,
    produced_len: u64,
    sanity_ok: bool,
) -> Option<String> {
    if produced_len == 0 {
        return None;
    }
    if parts_sum > 0
        && produced_len.saturating_mul(4) >= parts_sum
        && produced_len <= parts_sum.saturating_mul(4)
    {
        return None;
    }
    if let Some(f) = format_size {
        if format_exact {
            if produced_len.saturating_mul(4) < f {
                return Some(format!(
                    "the downloaded file is {produced_len} bytes but the quality you picked was about {f} bytes — the wrong stream may have been delivered; please retry"
                ));
            }
            return None;
        }
        if sanity_ok {
            return None;
        }
    }
    if expect_size > 0 && produced_len.saturating_mul(4) < expect_size {
        return Some(format!(
            "the downloaded file is {produced_len} bytes but the quality you picked was about {expect_size} bytes — the wrong stream may have been delivered; please retry"
        ));
    }
    None
}

/// M-4 adoption gate for a final file found on disk (a kept merge
/// recorded by a previous run, or a complete file already sitting
/// at the job's expected final path): adopt only when an expected
/// size is known and the file's length is in the same order as
/// that expectation. A merged file is legitimately a little
/// larger than the downloaded parts (container overhead — the
/// hunt's 155,201,613-byte merge of 154,471,113 bytes of parts),
/// so exact equality is too strict; a file half the expected size
/// or less, or over twice it, is a different file and must not be
/// adopted on size trust alone. The finalize that follows still
/// runs the stream/duration guards before anything is published.
fn adoptable_final(len: u64, expected: u64) -> bool {
    expected > 0
        && len > 0
        && (len as u128) * 2 >= expected as u128
        && (len as u128) <= (expected as u128) * 2
}

/// One parsed NDMPROG line (see the --progress-template the
/// yt-dlp fallback is launched with). Every field is optional:
/// yt-dlp renders a missing progress key as "NA", and which keys
/// exist depends on the downloader — a plain HTTP file reports an
/// exact `total_bytes` and no fragment fields; a fragment (DASH/
/// HLS) file reports `fragment_index`/`fragment_count` plus a
/// `total_bytes_estimate`, and its exact total only on the final
/// 'finished' report for the file.
#[derive(Clone, Debug, Default)]
struct NdmProgReport {
    downloaded: Option<u64>,
    total: Option<u64>,
    estimate: Option<u64>,
    speed: Option<f64>,
    frag_index: Option<u64>,
    frag_count: Option<u64>,
    finished: bool,
    file: String,
}

/// Parse the part after "NDMPROG:". Field order is the template's:
/// downloaded|total|estimate|speed|frag_index|frag_count|status|
/// filename — the filename comes LAST and is rejoined verbatim,
/// because a title may itself contain '|'. Legacy 4-field lines
/// (the pre-M-15 template) still parse: the missing fields simply
/// stay None and the report lands in the anonymous file bucket.
fn parse_ndmprog(rest: &str) -> NdmProgReport {
    let mut it = rest.splitn(8, '|');
    let num = |s: Option<&str>| -> Option<u64> {
        s.and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v as u64)
    };
    let downloaded = num(it.next());
    let total = num(it.next()).filter(|t| *t > 0);
    let estimate = num(it.next()).filter(|t| *t > 0);
    let speed = it
        .next()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0);
    let frag_index = num(it.next());
    let frag_count = num(it.next()).filter(|t| *t > 0);
    let status = it.next().unwrap_or("").trim().to_string();
    let file = it.next().unwrap_or("").trim().to_string();
    NdmProgReport {
        downloaded,
        total,
        estimate,
        speed,
        frag_index,
        frag_count,
        finished: status == "finished",
        file: if file == "NA" { String::new() } else { file },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FragMode {
    /// No fragment report seen yet for this file.
    Unknown,
    /// The file's reports carry a file-level AGGREGATE
    /// downloaded_bytes (yt-dlp's fragment state dict): the
    /// latest report IS the file's progress, and summing reports
    /// would count the same bytes again and again.
    Aggregate,
    /// The file's reports are PER-FRAGMENT cumulatives (each
    /// carries its fragment's own total): progress lives in the
    /// per-fragment slots and is summed over them.
    Fragment,
}

/// Per-file NDMPROG accounting (M-15). The pre-fix parser kept a
/// single (base, current) pair for the WHOLE yt-dlp run: with
/// several files (video + audio) reporting into one stream and
/// fragment estimates spiking under --concurrent-fragments, every
/// drop of the reported counter folded the current (possibly
/// spiked) total into `base` — the journal total snowballed to
/// ~37× the real size, nondeterministically, and the finish line
/// then branded the perfect merge an error. This tracker keys
/// every report by its FILE and, inside a fragment file, by
/// fragment identity — it only ever stores the LATEST report per
/// key and sums those, never a running sum of cumulative reports.
#[derive(Default)]
struct NdmFileTrack {
    mode: FragMode,
    /// per-fragment latest (downloaded, total) — Fragment mode
    slots: HashMap<u64, (u64, Option<u64>)>,
    /// largest aggregate downloaded_bytes seen — Aggregate mode
    agg_done: u64,
    exact_total: Option<u64>,
    /// latest QUALIFIED estimate reported (see apply): the
    /// estimate formula divides the bytes so far by
    /// (finished + 1), so with 8 fragments in flight and none
    /// finished it overshoots wildly; those early spikes are
    /// UNqualified and never land here at all. A qualified
    /// estimate may refine UP or DOWN as fragments finish — the
    /// latest one stands (208B total rule ii); it is an estimate,
    /// never a floor the downloaded count can raise (the old
    /// monotonic-max / min-sticky treatments both froze a
    /// transient figure in as the run's total)
    estimate_qualified: Option<u64>,
    frag_count: Option<u64>,
    finished: bool,
    speed: f64,
}

impl Default for FragMode {
    fn default() -> Self {
        FragMode::Unknown
    }
}

impl NdmFileTrack {
    fn apply(&mut self, r: &NdmProgReport) {
        if let Some(c) = r.frag_count {
            self.frag_count = Some(c);
        }
        // Mode is decided by the first DOWNLOADING fragment
        // report: one that also carries the fragment's own total
        // is a per-fragment report; one without is the aggregate
        // state dict. (The file's 'finished' report carries an
        // exact file total in the aggregate world and decides
        // nothing.)
        if r.frag_index.is_some() && self.mode == FragMode::Unknown {
            if !r.finished {
                // 208B classification guard: a report whose
                // downloaded count EXCEEDS its supposed fragment
                // total cannot be a per-fragment report — a
                // fragment never downloads more than its own size.
                // It is the aggregate state dict (file-cumulative
                // downloaded_bytes) that happens to carry a
                // fragment-sized total_bytes, and it decides
                // Aggregate no matter what else it carries.
                self.mode = match (r.downloaded, r.total) {
                    (Some(d), Some(t)) if d > t => FragMode::Aggregate,
                    (_, Some(_)) => FragMode::Fragment,
                    _ => FragMode::Aggregate,
                };
            } else if r.frag_count.map(|c| c > 1).unwrap_or(false) {
                // a fragment's own 'finished' report arriving
                // before any downloading report was sampled (a
                // tiny init segment can complete that fast): it
                // must land in its slot, not be mistaken for the
                // whole file's exact total
                self.mode = FragMode::Fragment;
            }
        }
        if let Some(d) = r.downloaded {
            self.agg_done = self.agg_done.max(d);
        }
        // 208B contradiction reclassification: the first report
        // was ambiguous (downloaded <= its fragment-sized total
        // fits both readings) and Fragment was chosen, but a later
        // report proves the aggregate reading the same way as
        // above — downloaded beyond the fragment's own total.
        // The slots built so far are sums of cumulative snapshots
        // (the runaway multiplication behind the 110 MB card), so
        // they are discarded wholesale; `agg_done` was maintained
        // from every report's downloaded field all along, and in
        // the aggregate world that max IS the true byte count, so
        // done() is correct from this line on.
        if self.mode == FragMode::Fragment && !r.finished {
            if let (Some(d), Some(t)) = (r.downloaded, r.total) {
                if d > t {
                    self.mode = FragMode::Aggregate;
                    self.slots.clear();
                }
            }
        }
        match self.mode {
            FragMode::Fragment => {
                if let Some(idx) = r.frag_index {
                    let slot = self.slots.entry(idx).or_insert((0, None));
                    if r.finished {
                        // this fragment is complete: its total is
                        // its size, and it downloaded all of it
                        if let Some(t) = r.total.or(r.downloaded) {
                            *slot = (t, Some(t));
                        }
                    } else {
                        if let Some(d) = r.downloaded {
                            slot.0 = d;
                        }
                        if r.total.is_some() {
                            slot.1 = r.total;
                        }
                    }
                }
                if r.finished {
                    // the file is complete once every fragment's
                    // slot is fully downloaded (its own reports
                    // reached its total); the slots then sum to
                    // the file's exact size
                    if let Some(c) = self.frag_count {
                        let done_slots = self
                            .slots
                            .values()
                            .filter(|(d, t)| t.map(|t| *d >= t).unwrap_or(false))
                            .count() as u64;
                        if done_slots >= c {
                            self.finished = true;
                            self.exact_total =
                                Some(self.slots.values().map(|(d, _)| *d).sum());
                        }
                    }
                }
            }
            _ => {
                // 208B: a fragment-bearing report's total is a
                // FRAGMENT's size, never the file's — taking it as
                // the file's exact total is how a total of exactly
                // 82,047 B (fragment 0's content length) could ever
                // be displayed for a 24.393 MB job. A total counts
                // as the file's exact total only on a plain
                // (fragment-less) report, or on the file's own
                // finished report, whose total is the file's size
                // by construction.
                if let Some(t) = r.total {
                    if r.frag_index.is_none() || r.finished {
                        self.exact_total = Some(t);
                    }
                }
                if r.finished {
                    self.finished = true;
                    if let Some(t) = r.total {
                        self.agg_done = self.agg_done.max(t);
                    } else if let Some(d) = r.downloaded {
                        self.agg_done = self.agg_done.max(d);
                    }
                }
            }
        }
        if let Some(e) = r.estimate {
            // qualified = safe to show as a total: either the
            // file is not fragmented at all (a plain estimate,
            // which has no spike formula behind it), or at least
            // one fragment has finished, which tames the fragment
            // formula's denominator
            if r.frag_index.map(|i| i >= 1).unwrap_or(true) {
                self.estimate_qualified = Some(e);
            }
        }
        if let Some(s) = r.speed {
            self.speed = s;
        }
    }

    fn done(&self) -> u64 {
        if self.finished {
            if let Some(t) = self.exact_total {
                return t;
            }
        }
        match self.mode {
            FragMode::Fragment => self.slots.values().map(|(d, _)| *d).sum(),
            _ => self.agg_done,
        }
    }

    fn total(&self) -> u64 {
        // V3-a (208B): a total is NEVER derived from the downloaded
        // count. The pre-fix fallback returned `done()` whenever no
        // trustworthy figure existed, so the status layer's total
        // chased its own numerator in lockstep (always exactly
        // 100%, climbing past the real size as later files reported
        // in). Unknown is 0; a trustworthy figure (an exact reported
        // total, the fragments' own totals, or a QUALIFIED estimate)
        // is returned exactly as reported — never inflated to the
        // downloaded count either.
        if let Some(t) = self.exact_total {
            return t;
        }
        let est = match self.mode {
            FragMode::Fragment => {
                // sum of the fragments' own totals; fragments
                // whose total is not yet known are estimated at
                // the mean of the known ones
                let known: Vec<u64> = self.slots.values().filter_map(|(_, t)| *t).collect();
                if known.is_empty() {
                    None
                } else {
                    let mean = known.iter().sum::<u64>() / known.len() as u64;
                    let count = self
                        .frag_count
                        .map(|c| c.max(self.slots.len() as u64))
                        .unwrap_or(self.slots.len() as u64);
                    let mut sum = 0u64;
                    for i in 0..count {
                        sum = sum.saturating_add(
                            self.slots
                                .get(&i)
                                .and_then(|(_, t)| *t)
                                .unwrap_or(mean),
                        );
                    }
                    Some(sum)
                }
            }
            // only QUALIFIED estimates may stand as a total (see
            // estimate_qualified): an unqualified fragment
            // estimate is the M-15 spike itself. With no qualified
            // estimate the total is UNKNOWN (0) — the job-level
            // seeded total (see NdmProgTracker::set_format_total
            // and the spawn path) is what the status shows then,
            // never the downloaded count (V3-a)
            _ => self.estimate_qualified,
        };
        est.unwrap_or(0)
    }

    /// The file's final size once its download is over: the exact
    /// total when one was reported, else everything it reported
    /// downloading. The finish line's parts sum is built from this.
    fn final_size(&self) -> u64 {
        self.exact_total.unwrap_or_else(|| self.done())
    }
}

/// Whole-run NDMPROG accounting: the per-file tracks plus the
/// format-level size learned from the probe (the selected
/// formats' `filesize`/`filesize_approx`), which outranks every
/// fragment estimate as the run's total. When the probe learned
/// no format size, the spawn path seeds the tracker's format
/// total from the job's known total instead — the picked
/// option's size (`expect_size`, the picker's row total) — so a
/// trustworthy denominator exists from the first progress line
/// and is never synthesised from downloaded bytes (V3-a, 208B).
#[derive(Default)]
struct NdmProgTracker {
    files: HashMap<String, NdmFileTrack>,
    format_total: Option<u64>,
    format_exact: bool,
}

impl NdmProgTracker {
    fn set_format_total(&mut self, total: Option<u64>, exact: bool) {
        self.format_total = total.filter(|t| *t > 0);
        self.format_exact = exact;
    }

    fn apply(&mut self, r: NdmProgReport) {
        self.files.entry(r.file.clone()).or_default().apply(&r);
    }

    fn done(&self) -> u64 {
        self.files.values().map(|f| f.done()).sum()
    }

    fn total(&self) -> u64 {
        // V3-a (208B): the seeded format/job total stands exactly
        // as seeded for the whole run (the finish line corrects it
        // from parts_sum / the merged length at completion); it is
        // never raised to the downloaded count mid-run, and with no
        // seeded total the per-file sum is 0 while every file's
        // total is still unknown — the status layer then keeps the
        // job's previously seeded total instead of inventing one
        // from downloaded bytes
        if let Some(f) = self.format_total {
            return f;
        }
        self.files.values().map(|f| f.total()).sum::<u64>()
    }

    fn speed(&self) -> f64 {
        self.files
            .values()
            .filter(|f| !f.finished)
            .map(|f| f.speed)
            .sum()
    }

    /// Sum of the files' final sizes — what this run actually
    /// downloaded, the finish line's trustworthy baseline.
    fn parts_sum(&self) -> u64 {
        self.files.values().map(|f| f.final_size()).sum()
    }

    /// True only when at least one file is tracked and EVERY
    /// tracked file has reported finished (208B: the [Merger]
    /// branch's licence to claim "merging" / done == total).
    fn all_finished(&self) -> bool {
        if self.files.is_empty() {
            return false;
        }
        // The "" bucket holds nameless reports (an NA filename);
        // in Fragment mode it can never prove itself finished, so
        // when any named file is tracked it must not veto the
        // licence the named files have earned. A run whose ONLY
        // bucket is "" keeps the plain rule — there the nameless
        // finished report is the only evidence there is.
        if self.files.keys().any(|k| !k.is_empty()) {
            return self
                .files
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .all(|(_, f)| f.finished);
        }
        self.files.values().all(|f| f.finished)
    }
}

/// Estimate a media file's true media payload in bytes from its
/// `ffmpeg -i` text: the container duration × the sum of its
/// streams' OWN bitrates. Per-stream bitrates describe the media
/// itself, so container padding and other non-media payload do not
/// inflate the estimate the way the file's byte count would (the
/// Duration line's bitrate is derived from that byte count and is
/// useless for this — 134217 kb/s for the padded R3 fixture, whose
/// streams report their true 3010 + 69 kb/s). Returns None when
/// the duration or any stream's bitrate is unreported: an estimate
/// is never fabricated from partial data.
pub fn media_payload_estimate(text: &str) -> Option<u64> {
    let mut duration_secs: Option<f64> = None;
    let mut total_kbps: u64 = 0;
    let mut streams = 0usize;
    for line in text.lines() {
        let t = line.trim_start();
        if duration_secs.is_none() {
            if let Some(rest) = t.strip_prefix("Duration: ") {
                let token = rest.split(',').next()?.trim();
                if token != "N/A" {
                    let mut parts = token.split(':');
                    let h: f64 = parts.next()?.parse().ok()?;
                    let m: f64 = parts.next()?.parse().ok()?;
                    let s: f64 = parts.next()?.parse().ok()?;
                    duration_secs = Some(h * 3600.0 + m * 60.0 + s);
                }
            }
        }
        if t.starts_with("Stream #") {
            streams += 1;
            // the stream's own bitrate is the first "kb/s" figure
            // on its line; a stream that reports none leaves the
            // file's payload unknowable, so the estimate fails
            let (before, _) = t.split_once(" kb/s")?;
            let kbps: u64 = before
                .rsplit(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            if kbps == 0 {
                return None;
            }
            total_kbps = total_kbps.saturating_add(kbps);
        }
    }
    let duration = duration_secs?;
    if streams == 0 || duration <= 0.0 || total_kbps == 0 {
        return None;
    }
    let bytes = (duration * total_kbps as f64 * 1000.0 / 8.0) as u64;
    (bytes > 0).then_some(bytes)
}

/// Run `ffmpeg -i` over a downloaded file and estimate its media
/// payload (see `media_payload_estimate`).
fn estimate_media_payload(path: &Path, components_dir: &Path) -> Option<u64> {
    let ffmpeg = find_tool("ffmpeg", components_dir)?;
    let out = tool_cmd(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    media_payload_estimate(&stderr)
}

/// Estimate the selected formats' combined media payload from
/// yt-dlp's resolution: each format's bitrate (tbr, else vbr+abr)
/// × the media duration. yt-dlp's `filesize` fields are
/// deliberately NOT used — for direct containers they report the
/// container's byte count, which can far exceed the media inside
/// (R3-EXT-01; for the padded fixture they are absent entirely).
/// None when any selected format reports no bitrate or the
/// duration is unknown.
fn formats_media_bytes(duration_secs: f64, formats: &[&serde_json::Value]) -> Option<u64> {
    if duration_secs <= 0.0 || formats.is_empty() {
        return None;
    }
    let mut total_kbps = 0.0f64;
    for f in formats {
        let br = f["tbr"].as_f64().filter(|b| *b > 0.0).or_else(|| {
            let v = f["vbr"].as_f64().unwrap_or(0.0);
            let a = f["abr"].as_f64().unwrap_or(0.0);
            (v + a > 0.0).then_some(v + a)
        })?;
        total_kbps += br;
    }
    let bytes = (total_kbps * 1000.0 / 8.0 * duration_secs) as u64;
    (bytes > 0).then_some(bytes)
}

/// The selected formats' combined size from the probe info, for
/// the yt-dlp fallback's accounting (M-15): the sum of `filesize`
/// when EVERY selected format reports one (exact, true), else the
/// sum of `filesize`/`filesize_approx` when every format reports
/// at least one of them (estimate, false), else (None, false).
/// A format-level figure — even an approximate one — outranks
/// fragment estimates, whose formula overshoots wildly while the
/// first fragments are still in flight.
fn formats_filesize(formats: &[&serde_json::Value]) -> (Option<u64>, bool) {
    if formats.is_empty() {
        return (None, false);
    }
    let field = |f: &serde_json::Value, k: &str| -> Option<u64> {
        f[k].as_u64().filter(|n| *n > 0)
    };
    if formats.iter().all(|f| field(f, "filesize").is_some()) {
        return (
            Some(formats.iter().filter_map(|f| field(f, "filesize")).sum()),
            true,
        );
    }
    if formats
        .iter()
        .all(|f| field(f, "filesize").or_else(|| field(f, "filesize_approx")).is_some())
    {
        return (
            Some(
                formats
                    .iter()
                    .filter_map(|f| field(f, "filesize").or_else(|| field(f, "filesize_approx")))
                    .sum(),
            ),
            false,
        );
    }
    (None, false)
}

/// (name -> (len, mtime)) for every file in `dir`, taken just before a
/// download starts so the finished file can be recognised afterwards.
fn dir_snapshot(dir: &Path) -> HashMap<String, (u64, SystemTime)> {
    let mut out = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    out.insert(
                        e.file_name().to_string_lossy().into_owned(),
                        (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
                    );
                }
            }
        }
    }
    out
}

/// Remove THIS job's intermediate files from its save folder —
/// BOTH engines' working files for `stem`: the parallel engine's
/// hidden parts and their resume-state journals (".{stem}.
/// ndm_stream*" — the journal rides the same prefix), its mux /
/// audio-pairing temps, and yt-dlp's "{stem}.f<digits>…" split
/// parts and "{stem}.<ext>…" single-format parts (.part / .ytdl).
/// Called when a run publishes "finished": an earlier attempt of
/// the SAME job may have run on the OTHER engine (pause kills the
/// yt-dlp child; the resume tries the parallel engine first), and
/// each engine's own finish cleanup only ever knew its own temps
/// — the abandoned attempt's partial survived next to the
/// finished file (R4 side observation: an orphaned 13.9 MB
/// big2.mp4.part after a pause/resume → finish cycle). The
/// finished file itself never matches: it carries no .part/.ytdl
/// suffix and no leading dot. Strictly this stem — the same rule
/// the discard sweep in main.rs applies.
fn sweep_job_temps(dir: &Path, stem: &str) {
    if stem.is_empty() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for f in rd.flatten() {
        let n = f.file_name().to_string_lossy().to_string();
        let engine_temp = n.starts_with(&format!(".{stem}.ndm_stream"))
            || n.starts_with(&format!(".{stem}.ndm_mux"))
            || n.starts_with(&format!(".{stem}.ndm_audio"));
        let ytdlp_part = (n.ends_with(".part") || n.ends_with(".ytdl")) && {
            let split = n
                .strip_prefix(&format!("{stem}.f"))
                .map(|rest| {
                    rest.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let single = n
                .strip_prefix(&format!("{stem}."))
                .map(|rest| {
                    ["mp4", "m4a", "mp3", "webm", "mkv", "ts"]
                        .iter()
                        .any(|e| rest == *e || rest.starts_with(&format!("{e}.")))
                })
                .unwrap_or(false);
            split || single
        };
        if engine_temp || ytdlp_part {
            let _ = std::fs::remove_file(f.path());
        }
    }
}

/// "<title>.f<format-id>.<ext>" stream files wait for the merge step and
/// are never the deliverable; .part/.ytdl are yt-dlp's working files.
fn is_intermediate(name: &str) -> bool {
    if name.ends_with(".part") || name.ends_with(".ytdl") || name.ends_with(".temp.mp4") {
        return true;
    }
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    if let Some((_, tail)) = stem.rsplit_once('.') {
        let t = tail.as_bytes();
        if tail.len() <= 24
            && t.first() == Some(&b'f')
            && tail[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return true;
        }
    }
    false
}

/// The file a successful run must have produced: newly appeared or
/// changed since `before`, not a stream intermediate. The merged/
/// extracted extension wins; otherwise the newest candidate.
fn find_final_file(
    dir: &Path,
    before: &HashMap<String, (u64, SystemTime)>,
    audio: bool,
    stem_hint: &str,
) -> Option<PathBuf> {
    let mut cands: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_intermediate(&name) {
                continue;
            }
            if let Ok(m) = e.metadata() {
                if !m.is_file() {
                    continue;
                }
                let key = (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH));
                if before.get(&name) == Some(&key) {
                    continue; // untouched by this run
                }
                cands.push((key.1, e.path()));
            }
        }
    }
    cands.sort_by_key(|(t, _)| *t);
    let want = if audio { "mp3" } else { "mp4" };
    let pick = |pool: &[&(SystemTime, PathBuf)]| -> Option<PathBuf> {
        pool.iter()
            .rev()
            .find(|(_, p)| {
                p.extension().map(|x| x.to_string_lossy().to_lowercase())
                    == Some(want.to_string())
            })
            .or_else(|| pool.last())
            .map(|(_, p)| p.clone())
    };
    // This job's own stem wins first: with two video jobs sharing one
    // download folder (the normal case), the directory-wide newest
    // file can be the OTHER job's result, and certifying it would
    // report this download finished with someone else's video.
    if !stem_hint.is_empty() {
        let prefix = format!("{stem_hint}.");
        let own: Vec<&(SystemTime, PathBuf)> = cands
            .iter()
            .filter(|(_, p)| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .collect();
        if let Some(p) = pick(&own) {
            return Some(p);
        }
    }
    let all: Vec<&(SystemTime, PathBuf)> = cands.iter().collect();
    pick(&all)
}

/// Parse the output file path from yt-dlp status lines when present.
fn parse_output_path_line(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("[Merger] Merging formats into \"") {
        if let Some(path_str) = rest.strip_suffix('"') {
            return Some(PathBuf::from(path_str));
        }
    }
    if let Some(rest) = line.strip_prefix("[ExtractAudio] Destination: ") {
        return Some(PathBuf::from(rest.trim()));
    }
    if let Some(rest) = line.strip_prefix("[ExtractAudio] Not converting audio ") {
        if let Some((path_str, _)) = rest.split_once("; ") {
            return Some(PathBuf::from(path_str.trim()));
        }
    }
    if let Some(rest) = line.strip_prefix("[download] ") {
        if let Some(path_str) = rest.strip_suffix(" has already been downloaded") {
            return Some(PathBuf::from(path_str.trim()));
        }
        if let Some(dest) = rest.strip_prefix("Destination: ") {
            let p = PathBuf::from(dest.trim());
            let fname = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !fname.is_empty() && !is_intermediate(&fname) {
                return Some(p);
            }
        }
    }
    None
}

#[derive(Clone)]
pub struct VideoProgress {
    pub status: String, // starting | downloading | merging | paused | finished | error | cancelled
    pub total: u64,
    /// A1: set once every engine part has reported a probed total:
    /// `total` then holds the parts' sum and the engine poll loop
    /// never rewrites it (before the lock, `total` keeps whatever
    /// hint it already had instead of tracking partial per-part
    /// sums). The finalize paths may still replace it — together
    /// with `done` — with the merged file's length.
    pub total_locked: bool,
    pub done: u64,
    pub speed_bps: f64,
    pub conns: usize,
    pub final_path: Option<PathBuf>,
    pub error: Option<String>,
    /// when `done` last changed: a "downloading" job whose bytes
    /// have not moved for a few seconds is stalled, whatever its
    /// last sampled speed says — payload_of decays the reported
    /// rate to 0 from this (F-15 state honesty)
    pub last_progress_at: Instant,
}

impl Default for VideoProgress {
    fn default() -> Self {
        VideoProgress {
            status: "downloading".into(),
            total: 0,
            total_locked: false,
            done: 0,
            speed_bps: 0.0,
            conns: 0,
            final_path: None,
            error: None,
            last_progress_at: Instant::now(),
        }
    }
}

/// Sanitize a user-supplied or page-supplied video title into a safe
/// cross-platform filename stem capped at 150 UTF-8 bytes so yt-dlp's
/// `.f<id>.mp4.part` suffix never hits the 255-byte filesystem limit.
pub fn safe_filename_stem(title: &str) -> String {
    let t = title.trim();
    let t = t
        .strip_suffix(".mp4")
        .or_else(|| t.strip_suffix(".MP4"))
        .or_else(|| t.strip_suffix(".mp3"))
        .or_else(|| t.strip_suffix(".MP3"))
        .or_else(|| t.strip_suffix(".mkv"))
        .or_else(|| t.strip_suffix(".webm"))
        .or_else(|| t.strip_suffix(".m4a"))
        .unwrap_or(t);
    let cleaned: String = t
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '%') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() || trimmed == "index" || trimmed == "watch" {
        return String::new();
    }
    let mut end = trimmed.len().min(150);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].trim().trim_matches('.').to_string()
}

/// The filename extensions a job of one kind can actually land on
/// disk with: video jobs mux to MP4 (MKV/WebM are the other video
/// containers a same-title file may already occupy); audio jobs save
/// MP3 (yt-dlp extraction) or M4A (the chosen stream's own
/// container). A stem only collides within its OWN kind — a finished
/// `big2.mp4` must not push the audio half of a download-all pair to
/// "big2 (1).m4a" when no `big2.m4a` exists (R3-EXT-04).
pub fn own_media_exts(audio: bool) -> &'static [&'static str] {
    if audio {
        &["mp3", "m4a"]
    } else {
        &["mp4", "mkv", "webm"]
    }
}

/// True when `cand` is already used inside `dir` by a finished file
/// (or an in-flight `.part`) whose extension belongs to this kind —
/// i.e. the new job's own final filename would collide.
pub fn stem_taken_on_disk(dir: &Path, cand: &str, audio: bool) -> bool {
    own_media_exts(audio).iter().any(|ext| {
        dir.join(format!("{cand}.{ext}")).exists()
            || dir
                .join(format!(".{cand}.ndm_stream0.{ext}.part"))
                .exists()
    })
}

/// Choose a non-colliding filename stem inside `dir` so two downloads
/// of the same kind with the same title (or two qualities of the same
/// video) never overwrite each other or collide on `.part` files.
pub fn unique_stem_on_disk(dir: &Path, base_stem: &str, audio: bool) -> String {
    let stem = safe_filename_stem(base_stem);
    if stem.is_empty() {
        return String::new();
    }
    if !stem_taken_on_disk(dir, &stem, audio) {
        return stem;
    }
    for n in 1..10_000 {
        let cand = format!("{stem} ({n})");
        if !stem_taken_on_disk(dir, &cand, audio) {
            return cand;
        }
    }
    stem
}

pub struct VideoJob {
    pub url: String,
    pub dir: PathBuf,
    pub selector: String,
    pub audio: bool,
    pub title: String,
    pub cookies: Option<PathBuf>,
    pub progress: Arc<Mutex<VideoProgress>>,
    chosen_stem: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    active_downloads: Mutex<Vec<Arc<ndm::Download>>>,
    stop_flag: Mutex<Option<String>>, // "paused" | "cancelled" when WE stopped it
    spawn_lock: Mutex<()>,
    gen: std::sync::atomic::AtomicU64,
    /// true while this job's spawn thread is in flight (resolving,
    /// downloading, merging — any phase). Set synchronously by
    /// spawn() and cleared by the thread's exit guard, so
    /// payload_of can tell a live run's real (still-zero) numbers
    /// from a settled job's (A3)
    alive: std::sync::atomic::AtomicBool,
    /// the chosen formats carry a sound stream (learned from yt-dlp's own
    /// resolution) — a "finished" video without sound is then a failure,
    /// never a success
    expect_audio: Mutex<bool>,
    /// the selected formats' combined media payload in bytes,
    /// learned from yt-dlp's resolution (bitrates × duration) —
    /// the finish-line size guard's baseline for the yt-dlp
    /// finalize path. None when the resolution reported no usable
    /// bitrates: the guard then skips the job instead of guessing
    /// (R3-EXT-01)
    expected_media: Mutex<Option<u64>>,
    /// separate HLS audio rendition (master-playlist EXT-X-MEDIA) that the
    /// app must fetch and mux itself; yt-dlp's generic extractor cannot
    /// pair it with the chosen variant
    pair_audio_url: Mutex<Option<String>>,
    /// this entry's own speed limit in bytes/sec (0 = none) — applied
    /// to every engine stream and to the yt-dlp fallback
    limit_bps: std::sync::atomic::AtomicU64,
    /// saved Site Login for this page: passed to yt-dlp as
    /// --username/--password whenever non-empty, the way the HTTP
    /// engine applies the same login as Basic credentials
    login: String,
    password: String,
    /// set when a pause landed DURING the ffmpeg merge: the merged
    /// output file, kept on disk and recorded here, which the next
    /// resume finalizes instead of re-downloading everything
    merged_final: Mutex<Option<PathBuf>>,
    /// M-15: the selected formats' combined size from the probe
    /// info (`filesize` sum when exact, else `filesize_approx`),
    /// learned at resolution time. The yt-dlp fallback's
    /// accounting prefers it over fragment estimates, and the
    /// finish line judges the merged file against it — exact
    /// figures with the usual tolerance, estimates never as a
    /// hard error (see completion_size_error)
    format_size: Mutex<Option<u64>>,
    format_size_exact: Mutex<bool>,
    /// the bytes this run actually downloaded (the parts sum):
    /// set by the engine path from its locked total when the
    /// merge starts, and by the yt-dlp path from the tracker's
    /// per-file final sizes when the child exits. The finish
    /// line's trustworthy baseline (see completion_size_error)
    downloaded_total: Mutex<u64>,
    /// M-4: a completed final file a PREVIOUS run left on disk,
    /// handed in at construction (the journal's recorded
    /// final_path, via start_with_recorded_final, or a complete
    /// file found at this job's expected final path when the job
    /// was built). Step 0 of the spawn thread tries to finalize
    /// (adopt) it instead of re-downloading every part; a
    /// candidate that fails the finalize guards is NOT an error —
    /// the job falls through to a normal download
    recorded_final: Option<PathBuf>,
    /// the browser-capture door's pre-resolved streams, when this
    /// job was started from a captured picker option (see
    /// Preresolved); None for every yt-dlp-resolved job
    preresolved: Option<Preresolved>,
    /// what the user was shown and chose at add time: the picked
    /// option's probed size in bytes and the page video's probed
    /// duration in seconds (0 = unknown). The finish line
    /// cross-checks the produced file against them (W-2): a
    /// delivery that plainly contradicts the probed expectation is
    /// surfaced as an error, never presented as a clean success.
    expect_size: u64,
    expect_duration: f64,
}

#[derive(Clone)]
struct DirectStream {
    url: String,
    ext: String,
    has_audio: bool,
    cookie: Option<String>,
    referer: Option<String>,
    user_agent: Option<String>,
}

/// One stream of a caller-supplied resolution (see Preresolved).
#[derive(Clone, Debug)]
pub struct PreresolvedStream {
    pub url: String,
    pub ext: String,
    pub has_audio: bool,
    pub cookie: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
}

/// A resolution supplied by the caller instead of a yt-dlp run —
/// the browser-capture door's streams (crate::capture): the
/// browser already unlocked and played these, so there is nothing
/// to resolve; the job downloads them through the same parallel
/// engine + ffmpeg mux as a resolved selection, and never falls
/// back to the yt-dlp DOWNLOADER (its selector names a capture
/// group, not a yt-dlp format — feeding it to yt-dlp would only
/// manufacture a raw format error where the user deserves the
/// plain capture-expired message). One WS-A exception: when the
/// captured streams AUTH-DIE (403/410) mid-download, the job
/// re-resolves the PAGE URL through yt-dlp — with the standard
/// best selector, not the capture key — and continues on the
/// fresh streams through the engine (see
/// heal_reresolve_preresolved).
#[derive(Clone, Debug)]
pub struct Preresolved {
    pub title: String,
    pub streams: Vec<PreresolvedStream>,
    pub expect_audio: bool,
}

/// A completed stream resolution: everything a job start needs to
/// skip the yt-dlp resolve run. Stream URLs are signed and perishable,
/// so resolutions are cached only briefly (see RESOLUTION_TTL) and the
/// cache key covers every input that can change the result — the full
/// page URL, the effective selector, the audio flag, the site login,
/// a hash of its password (m-6: the login NAME alone used to key it,
/// so a corrected password kept being served the resolution the wrong
/// one produced), the proxy, and the canonical signature of the
/// cookie file's cookie SET (a re-login or a refreshed session
/// changes values, so it can never be served a resolution made
/// under the old cookies — but a mere reordering of the same set
/// between two captures of one session must NOT change the key:
/// the picker's warm capture and the click's capture are two
/// reads of one jar, see cookie_text_signature). A cached
/// resolution can therefore only be wrong if the STREAM URL itself
/// died inside the TTL window — and that case is already handled: the
/// engine's start or stream error path falls back to the yt-dlp
/// download, which re-resolves independently. A stale hit can cost
/// one fast failed engine attempt, never a wrong file.
#[derive(Clone)]
struct Resolution {
    title: String,
    streams: Vec<DirectStream>,
    expect_audio: bool,
    expected_media: Option<u64>,
    pair_audio_url: Option<String>,
    /// M-15: the selected formats' combined probe size and
    /// whether it is exact (see formats_filesize)
    format_size: Option<u64>,
    format_size_exact: bool,
}

#[derive(PartialEq, Eq, Clone, Hash)]
struct ResolutionKey {
    url: String,
    sel: String,
    audio: bool,
    login: String,
    password_sig: u64,
    cookies_sig: u64,
    proxy: String,
}

/// FNV-1a over the password's bytes — the cache key must change when
/// the password does, without ever holding the password itself. The
/// empty password hashes to the same sentinel family as a missing
/// cookie file: 0, which no real password's hash is remapped to.
fn password_signature(password: &str) -> u64 {
    if password.is_empty() {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in password.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// How long a resolution stays reusable. Signed CDN URLs live for
/// hours; ten minutes is deliberately far inside that, while still
/// covering the flows that re-resolve today: pause-killed yt-dlp
/// jobs, error retries, restored entries after an app restart, a
/// repeat add of the same quality, and a click that follows a
/// flyout-open warm resolve (see warm_resolution).
const RESOLUTION_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const RESOLUTION_CACHE_CAP: usize = 64;
/// 2.3.1 stall recovery (bugs 2+3): an engine attempt whose
/// combined downloaded bytes have not advanced for this long is
/// wedged — throttled connections dribble too slowly to trip
/// the engine's own 30 s pool policy (any byte refreshes it),
/// so the video job runs its own tighter clock and recovers in
/// place instead of abandoning the transfer.
const VIDEO_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(12);
/// The least combined progress over one stall window that
/// still counts as a living transfer (~2.7 KB/s): a throttled
/// stream's dribble stays under it, a slow-but-real one clears
/// it.
const VIDEO_STALL_MIN_ADVANCE: u64 = 32 * 1024;
/// Recovery attempts per engine run: fresh connections, fresh
/// connections again, then one re-resolve of the stream URLs.
/// Consecutive BARREN recovery attempts tolerated before the
/// job falls back to yt-dlp (attempts that gain real bytes do
/// not spend this budget — see the attempt loop).
const MAX_VIDEO_RECOVERIES: u32 = 3;

/// How a cancelled-stream settle wait ended (see
/// VideoJob::settle_streams).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StreamSettle {
    Done,
    Stopped,
    TimedOut,
}

/// How one engine attempt ended (see poll_engine_attempt).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EngineAttempt {
    /// Every stream finished; the merge may begin.
    Finished,
    /// Advance stayed under the floor for a whole
    /// VIDEO_STALL_WINDOW mid-transfer (throttle dribble).
    Stalled,
    /// A stream reported a terminal engine error.
    Errored,
    /// The job was stopped (pause/cancel/newer generation):
    /// the caller must not touch progress further.
    Stopped,
}
/// Wall-clock bound for one resolve: the picker probe already
/// budgets 45 s for yt-dlp to read a page, and a challenged resolve
/// must not hold a job in "starting" past that same budget.
const RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

static RESOLUTION_CACHE: Mutex<Vec<(ResolutionKey, Instant, Resolution)>> =
    Mutex::new(Vec::new());

/// Content signature of the cookie file a resolution was made with:
/// the CANONICAL signature of its text (see
/// ndm::video_probe::cookie_text_signature) — the warm resolve and
/// the Download click capture the same browser session into
/// different files whose byte order can differ, and the cache key
/// must see them as the same session. No file and an unreadable
/// file get distinct sentinels so neither can alias a real (or
/// empty) file's hash.
fn cookies_signature(cookies: Option<&Path>) -> u64 {
    let Some(p) = cookies else { return 0 };
    let Ok(bytes) = std::fs::read(p) else {
        return u64::MAX;
    };
    ndm::video_probe::cookie_text_signature(&String::from_utf8_lossy(&bytes))
}

fn resolution_cache_get(key: &ResolutionKey, now: Instant) -> Option<Resolution> {
    let mut cache = RESOLUTION_CACHE.lock().unwrap();
    cache.retain(|(_, at, _)| now.duration_since(*at) < RESOLUTION_TTL);
    cache
        .iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, _, r)| r.clone())
}

fn resolution_cache_store(key: ResolutionKey, res: Resolution, now: Instant) {
    let mut cache = RESOLUTION_CACHE.lock().unwrap();
    cache.retain(|(k, at, _)| k != &key && now.duration_since(*at) < RESOLUTION_TTL);
    cache.push((key, now, res));
    while cache.len() > RESOLUTION_CACHE_CAP {
        cache.remove(0);
    }
}

/// Drop one cached resolution: stall recovery's last resort
/// re-resolves the stream URLs, and the cache would otherwise
/// hand back the very URLs that just wedged (2.3.1 bugs 2+3).
fn resolution_cache_evict(key: &ResolutionKey) {
    RESOLUTION_CACHE
        .lock()
        .unwrap()
        .retain(|(k, _, _)| k != key);
}

/// Keys with a resolve run currently in flight, plus the condvar
/// waiters sleep on. The picker seeding, the add flow's warm
/// thread and the job's own resolve all chase the SAME key in
/// the seconds around a Download click; without this they could
/// each spawn their own yt-dlp for it — the exact duplicate the
/// caches exist to remove, merely raced instead of serialised.
static RESOLVE_INFLIGHT: (Mutex<Vec<ResolutionKey>>, Condvar) =
    (Mutex::new(Vec::new()), Condvar::new());

/// Rebuild the probe a cache hit stands for, so a resolve that
/// waited on another thread's run applies the same learned
/// fields as one that ran yt-dlp itself.
fn probe_from_resolution(res: Resolution) -> ResolutionProbe {
    ResolutionProbe {
        expect_audio: res.expect_audio,
        pair_audio_url: res.pair_audio_url,
        expected_media: res.expected_media,
        format_size: res.format_size,
        format_size_exact: res.format_size_exact,
        resolved: Some((res.title, res.streams)),
    }
}

/// RAII: an in-flight claim is released — and its waiters woken —
/// on every exit path, panic included.
struct InflightClaim(ResolutionKey);

impl Drop for InflightClaim {
    fn drop(&mut self) {
        let (lock, cvar) = &RESOLVE_INFLIGHT;
        lock.lock().unwrap().retain(|k| k != &self.0);
        cvar.notify_all();
    }
}

/// Run `produce` (one yt-dlp resolve) for `key`, collapsing
/// concurrent duplicates: a caller that finds the same key
/// already in flight WAITS for that run and is then served from
/// the resolution cache it leaves behind, instead of spawning a
/// second yt-dlp for the same page + selector. A wait that
/// outlives the resolve budget (the owner died without leaving
/// a cache entry) falls through and runs the resolve itself —
/// a duplicate is wasteful, a lost download is worse.
fn resolve_streams_once(
    key: &ResolutionKey,
    produce: impl FnOnce() -> Option<ResolutionProbe>,
) -> Option<ResolutionProbe> {
    if let Some(res) = resolution_cache_get(key, Instant::now()) {
        return Some(probe_from_resolution(res));
    }
    let (lock, cvar) = &RESOLVE_INFLIGHT;
    {
        let mut inflight = lock.lock().unwrap();
        let deadline = Instant::now() + RESOLVE_TIMEOUT + std::time::Duration::from_secs(15);
        while inflight.contains(key) {
            let (guard, _) = cvar
                .wait_timeout(inflight, std::time::Duration::from_millis(100))
                .unwrap();
            inflight = guard;
            if let Some(res) = resolution_cache_get(key, Instant::now()) {
                return Some(probe_from_resolution(res));
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        if inflight.contains(key) {
            // the owner overran its budget without producing a
            // cache entry: take over rather than wait forever
            inflight.retain(|k| k != key);
        }
        inflight.push(key.clone());
    }
    let _claim = InflightClaim(key.clone());
    let probe = produce()?;
    // Leave the result where a waiter (or the next caller) finds
    // it — the callers ALSO store it (built from the job's own
    // fields, identically), which is idempotent.
    if let Some((title, streams)) = &probe.resolved {
        resolution_cache_store(
            key.clone(),
            Resolution {
                title: title.clone(),
                streams: streams.clone(),
                expect_audio: probe.expect_audio,
                expected_media: probe.expected_media,
                pair_audio_url: probe.pair_audio_url.clone(),
                format_size: probe.format_size,
                format_size_exact: probe.format_size_exact,
            },
            Instant::now(),
        );
    }
    Some(probe)
}

/// The full outcome of one yt-dlp resolve run. `resolved` is None
/// when the selection is not eligible for the parallel engine (the
/// caller then takes the yt-dlp download path) — the learned fields
/// are still meaningful in that case and the job wrapper applies
/// them exactly as the inline code did before this split.
#[derive(Clone)]
struct ResolutionProbe {
    expect_audio: bool,
    pair_audio_url: Option<String>,
    expected_media: Option<u64>,
    format_size: Option<u64>,
    format_size_exact: bool,
    resolved: Option<(String, Vec<DirectStream>)>,
}

/// True when a yt-dlp format is a playlist (HLS/DASH) rather than
/// a directly downloadable media file.
fn format_is_playlist(f: &serde_json::Value) -> bool {
    let proto = f["protocol"].as_str().unwrap_or("");
    if proto.contains("m3u8") || proto.contains("dash") {
        return true;
    }
    let u = f["url"].as_str().unwrap_or("").to_ascii_lowercase();
    u.contains(".m3u8") || u.contains(".mpd")
}

/// The best DIRECT media format in a probe's full format list —
/// the page's own `<video>` file, as opposed to a playlist variant
/// a script or a CDN ladder offered alongside it. None when the
/// list holds no direct video file. Ranking follows the app's
/// usual preference: avc-in-mp4 first, then height, then bitrate.
fn best_direct_format<'a>(v: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
    let formats = v["formats"].as_array()?;
    let mut best: Option<(&serde_json::Value, (u8, u64, u64))> = None;
    for f in formats {
        if format_is_playlist(f) {
            continue;
        }
        let proto = f["protocol"].as_str().unwrap_or("");
        if proto != "http" && proto != "https" {
            continue;
        }
        let u = f["url"].as_str().unwrap_or("");
        if !u.starts_with("http://") && !u.starts_with("https://") {
            continue;
        }
        let ext = f["ext"].as_str().unwrap_or("");
        if !matches!(ext, "mp4" | "webm" | "mkv" | "mov") {
            continue;
        }
        // an entry that DECLARES itself audio-only is not the
        // page's video; undeclared codecs (generic pages) pass
        let vc = f["vcodec"].as_str().unwrap_or("");
        if vc == "none" {
            continue;
        }
        if f["format_id"].as_str().unwrap_or("").contains("storyboard") {
            continue;
        }
        let avc_mp4 = u8::from(ext == "mp4" && vc.starts_with("avc"));
        let height = f["height"].as_u64().unwrap_or(0);
        let tbr = f["tbr"].as_f64().unwrap_or(0.0) as u64;
        let rank = (avc_mp4, height, tbr);
        if best.map(|(_, r)| rank > r).unwrap_or(true) {
            best = Some((f, rank));
        }
    }
    best.map(|(f, _)| f)
}

/// One yt-dlp `-j -f <sel>` run against `url`, parsed into a
/// ResolutionProbe. Free-standing (no job) so the flyout's warm
/// resolve can populate the cache before any job exists.
#[allow(clippy::too_many_arguments)]
fn resolve_streams_core(
    url: &str,
    sel: &str,
    audio: bool,
    cookies: Option<&Path>,
    login: &str,
    password: &str,
    ytdlp: &Path,
    components_dir: &Path,
) -> Option<ResolutionProbe> {
    // WS-B derivation: when the picker probe already fetched
    // this page (its `-J` dump sits in the probe cache) and the
    // page offers exactly ONE format, the selector's outcome is
    // already known — every picker selector is a fallback chain
    // whose last resort is that sole format — so the streams are
    // derived from the cached dump and yt-dlp is NOT spawned a
    // second time for the same page. This is the common direct-
    // media / single-file case (a bare .mp4, a page with one
    // <video>). Multi-format pages keep the real resolve run:
    // their selectors are yt-dlp filter expressions and picking
    // from them here would be a fork of yt-dlp's own selection.
    // A site login gates this out: the picker probe ran WITHOUT
    // the login, so its dump may describe a different (public)
    // view of the page than the job's authenticated resolve.
    if login.is_empty() {
        if let Some(raw) = ndm::video_probe::cached_probe_json(url, components_dir, cookies) {
            if single_format_probe(&raw) {
                if let Some(p) = resolution_from_json(&raw, url, sel, audio) {
                    return Some(p);
                }
            }
        }
    }
    let mut args = base_args_for_url(url, components_dir, cookies);
    if !login.is_empty() {
        args.push("--username".into());
        args.push(login.to_string());
        args.push("--password".into());
        args.push(password.to_string());
    }
    args.push("-j".into());
    args.push("-f".into());
    args.push(sel.to_string());
    args.push("--".into());
    args.push(url.to_string());

    let mut child = tool_cmd(ytdlp)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out_pipe = child.stdout.take()?;
    // Drain stdout on a thread, as the picker probe does: polling
    // try_wait without draining would let a large -j dump fill the
    // pipe and stall the child until the deadline.
    let out_thread = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut v = Vec::new();
        let _ = out_pipe.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + RESOLVE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // None is the engine-miss path: the yt-dlp
                    // fallback still downloads, so a timed-out
                    // resolve never loses the download.
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    if !status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&stdout)
        .lines()
        .find(|l| l.trim_start().starts_with('{'))?
        .to_string();
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    resolution_from_json(&v, url, sel, audio)
}

/// True when a probe dump describes exactly one usable format —
/// the case where a selector's outcome needs no second yt-dlp
/// run to know (see the derivation in `resolve_streams_core`).
/// Storyboards and playlist formats never count: they are not
/// the page's video, whatever the selector would say.
fn single_format_probe(v: &serde_json::Value) -> bool {
    let formats = ndm::video_probe::probe_formats(v);
    formats.len() == 1
        && formats[0]["ext"].as_str().unwrap_or("") != "mhtml"
        && !format_is_playlist(&formats[0])
}

/// Interpret one yt-dlp JSON dump (a resolve run's `-j` output,
/// or the picker probe's cached `-J` dump when it suffices) into
/// a ResolutionProbe: the selected formats' streams when they
/// are directly downloadable, plus everything learned about the
/// selection either way.
fn resolution_from_json(
    v: &serde_json::Value,
    url: &str,
    sel: &str,
    audio: bool,
) -> Option<ResolutionProbe> {
    let resolved_title = v["title"].as_str().unwrap_or("").to_string();
    let mut formats: Vec<&serde_json::Value> = match v["requested_formats"].as_array() {
        Some(arr) if !arr.is_empty() => arr.iter().collect(),
        _ => vec![v],
    };
    // W-2: a page's own direct media source outranks an indirect
    // variant. When the app's best-quality selection resolved to
    // playlist streams only — the shape a page gets when a script
    // planted an HLS/DASH variant next to a plain <video> element —
    // while the probe's full format list holds a direct media file
    // for the same content, the direct file is what the user means
    // by "this video": resolve THAT for the engine instead of
    // letting the yt-dlp fallback fetch the variant. Explicit
    // per-quality selectors keep yt-dlp's own choice — the user
    // picked that variant deliberately.
    if !audio && sel == ndm::video_probe::best_selector() {
        let selection_is_playlist =
            !formats.is_empty() && formats.iter().all(|f| format_is_playlist(f));
        if selection_is_playlist {
            if let Some(direct) = best_direct_format(v) {
                formats = vec![direct];
            }
        }
    }
    // Learn what this download SHOULD contain before any early
    // return below can lose the information: whether the chosen
    // formats carry sound, and — for HLS sources whose sound is a
    // separate rendition in the master playlist — where it lives.
    let any_audio = formats.iter().any(|f| {
        let ac = f["acodec"].as_str().unwrap_or("none");
        !ac.is_empty() && ac != "none"
    });
    let mut pair_audio_url: Option<String> = None;
    if !any_audio && !audio {
        let clean_url = url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_hls = clean_url.ends_with(".m3u8")
            || formats
                .iter()
                .any(|f| f["protocol"].as_str().unwrap_or("").contains("m3u8"));
        if is_hls {
            let hdr = &formats[0]["http_headers"];
            let cookie = hdr["Cookie"]
                .as_str()
                .or_else(|| hdr["cookie"].as_str())
                .unwrap_or("")
                .to_string();
            let referer = hdr["Referer"]
                .as_str()
                .or_else(|| hdr["referer"].as_str())
                .unwrap_or("")
                .to_string();
            if let Some(u) = ndm::video_probe::fetch_hls_default_audio(url, &referer, &cookie) {
                pair_audio_url = Some(u);
            }
        }
    }
    // M-15: the selected formats' combined probe size — learned
    // for EVERY outcome below, including the ineligible ones:
    // the yt-dlp fallback downloads this same selection, and its
    // accounting prefers this figure over fragment estimates.
    let (format_size, format_size_exact) = formats_filesize(&formats);
    if formats.is_empty() || formats.len() > 2 {
        return Some(ResolutionProbe {
            expect_audio: any_audio,
            pair_audio_url,
            expected_media: None,
            format_size,
            format_size_exact,
            resolved: None,
        });
    }
    // Learn the selected streams' combined media payload for
    // the finish-line guard (see the field's note): recorded
    // only when every selected format reported a bitrate.
    // (Recorded before the stream checks below on purpose: if
    // this resolution is rejected, the yt-dlp fallback still
    // downloads this same selection, so the estimate stands.)
    let expected_media =
        formats_media_bytes(v["duration"].as_f64().unwrap_or(0.0), &formats);
    let mut streams = Vec::new();
    let mut saw_vcodec = false;
    let mut any_video = false;
    for f in formats {
        let proto = f["protocol"].as_str().unwrap_or("");
        if proto != "https" && proto != "http" {
            return Some(ResolutionProbe {
                expect_audio: any_audio,
                pair_audio_url,
                expected_media,
                format_size,
                format_size_exact,
                resolved: None,
            });
        }
        let u = f["url"].as_str().unwrap_or("");
        if !u.starts_with("http://") && !u.starts_with("https://") {
            return Some(ResolutionProbe {
                expect_audio: any_audio,
                pair_audio_url,
                expected_media,
                format_size,
                format_size_exact,
                resolved: None,
            });
        }
        let lower = u.to_ascii_lowercase();
        if lower.contains(".m3u8") || lower.contains(".mpd") {
            return Some(ResolutionProbe {
                expect_audio: any_audio,
                pair_audio_url,
                expected_media,
                format_size,
                format_size_exact,
                resolved: None,
            });
        }
        // The extension comes from the network: keep it a plain
        // short token before it is ever used inside a file path.
        let raw_ext = f["ext"].as_str().unwrap_or("mp4");
        let cleaned: String = raw_ext
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(5)
            .collect::<String>()
            .to_ascii_lowercase();
        let ext = if cleaned.len() < 2 {
            "mp4".to_string()
        } else {
            cleaned
        };
        let has_audio = {
            let ac = f["acodec"].as_str().unwrap_or("none");
            !ac.is_empty() && ac != "none"
        };
        {
            let vc = f["vcodec"].as_str().unwrap_or("");
            if !vc.is_empty() {
                saw_vcodec = true;
                if vc != "none" {
                    any_video = true;
                }
            }
        }
        let headers = &f["http_headers"];
        let cookie = headers["Cookie"]
            .as_str()
            .or_else(|| headers["cookie"].as_str())
            .map(|s| s.to_string());
        let referer = headers["Referer"]
            .as_str()
            .or_else(|| headers["referer"].as_str())
            .map(|s| s.to_string());
        streams.push(DirectStream {
            url: u.to_string(),
            ext,
            has_audio,
            cookie,
            referer,
            user_agent: None,
        });
    }
    if !audio && saw_vcodec && !any_video {
        // Defensive guard: the selector resolved to an
        // audio-only set for a VIDEO job (every format declares
        // vcodec "none"). The parallel engine would download
        // the whole set and only fail at the finish line
        // ("no playable video stream") — refuse here instead,
        // so the caller falls back to yt-dlp, which re-resolves
        // independently. Formats that declare no codec info at
        // all (saw_vcodec == false) keep the old behaviour.
        return Some(ResolutionProbe {
            expect_audio: any_audio,
            pair_audio_url,
            expected_media,
            format_size,
            format_size_exact,
            resolved: None,
        });
    }
    Some(ResolutionProbe {
        expect_audio: any_audio,
        pair_audio_url,
        expected_media,
        format_size,
        format_size_exact,
        resolved: Some((resolved_title, streams)),
    })
}

/// Resolve a page URL + selector in the background and cache the
/// result, so the download click that follows (seconds later, once
/// the user picks a quality in the flyout) starts without paying a
/// second yt-dlp run. Called on a dedicated thread by the incoming
/// queue drain for {"kind":"warm"} lines; a no-op when a fresh
/// resolution for the same key already exists.
pub fn warm_resolution(
    url: &str,
    sel: &str,
    audio: bool,
    cookies: Option<PathBuf>,
    login: &str,
    password: &str,
    components_dir: &Path,
) {
    let key = ResolutionKey {
        url: url.to_string(),
        sel: sel.to_string(),
        audio,
        login: login.to_string(),
        password_sig: password_signature(password),
        cookies_sig: cookies_signature(cookies.as_deref()),
        proxy: ndm::global_proxy(),
    };
    let Some(ytdlp) = find_tool("yt-dlp", components_dir) else {
        return;
    };
    // resolve_streams_once owns the cache check, the in-flight
    // dedup and the store: a warm that races the job's own
    // resolve (or another warm of the same row) spawns yt-dlp
    // once between them, never twice.
    let _ = resolve_streams_once(&key, || {
        resolve_streams_core(
            url,
            sel,
            audio,
            cookies.as_deref(),
            login,
            password,
            &ytdlp,
            components_dir,
        )
    });
}

/// Which picker rows the seeding below warms: the row the picker
/// PRESELECTS for the user's quality preference (mirroring the
/// picker's own pickQualityIndex: "best" → the first row, "audio"
/// → the audio row, a height → that height's row or the first row
/// at/below it), plus the first row when it differs (the best
/// quality is always one click away). Bounded at two so opening a
/// picker never fans out into a yt-dlp storm; a pick outside the
/// warmed set simply resolves on click, exactly as before.
fn picker_warm_indices(options: &[ndm::video_probe::QualityOption], quality_pref: &str) -> Vec<usize> {
    if options.is_empty() {
        return Vec::new();
    }
    let preselect = if quality_pref == "audio" {
        options.iter().position(|o| o.audio).unwrap_or(0)
    } else if quality_pref == "best" || quality_pref.is_empty() {
        0
    } else if let Ok(target) = quality_pref.parse::<u64>() {
        options
            .iter()
            .position(|o| !o.audio && o.label.starts_with(quality_pref))
            .or_else(|| {
                options
                    .iter()
                    .position(|o| !o.audio && o.height > 0 && o.height <= target)
            })
            .unwrap_or(0)
    } else {
        0
    };
    let mut idx = vec![preselect];
    if preselect != 0 {
        idx.push(0);
    }
    idx
}

/// Change 5 (2.0.4): seed the resolution cache from the in-app
/// picker's probe, so the Download click that follows does not pay
/// a second full yt-dlp resolve (START-LATENCY.md R2: the picker
/// probe costs one resolve, the click used to cost another
/// ~13–15 s).
///
/// Why a warm resolve and not a derivation from the picker's `-J`
/// dump: every picker selector is a yt-dlp filter/fallback
/// EXPRESSION (`bv*[height=720][vcodec^=avc][ext=mp4]+ba[ext=m4a]/…`),
/// never a concrete format id, so the streams a selector yields
/// exist only by running yt-dlp's own selection — reimplementing
/// it here would be a fork that can silently pick different
/// streams than the job's own resolve. This seeding instead runs
/// the EXISTING warm_resolution (identical keying, identical
/// resolve) in the background while the user reads the picker;
/// the warmed entries are byte-for-byte what the click's own
/// resolve would have produced, and anything not warmed (or
/// expired) behaves exactly as before.
pub fn seed_picker_resolutions(
    url: &str,
    options: &[ndm::video_probe::QualityOption],
    quality_pref: &str,
    cookies: Option<PathBuf>,
    login: &str,
    password: &str,
    components_dir: &Path,
) {
    for i in picker_warm_indices(options, quality_pref) {
        let o = &options[i];
        warm_resolution(
            url,
            &o.selector,
            o.audio,
            cookies.clone(),
            login,
            password,
            components_dir,
        );
    }
}

/// Clears `VideoJob::alive` when the spawn thread exits — on
/// EVERY path, including the thread body's many early returns and
/// a panic: the flag claims "a run is in flight", and a dead
/// thread must never leave that claim standing (A3).
struct AliveGuard(Arc<VideoJob>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0
            .alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl VideoJob {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        title: &str,
        cookies: Option<PathBuf>,
        login: &str,
        password: &str,
        components_dir: &Path,
        expect_size: u64,
        expect_duration: f64,
    ) -> Result<Arc<VideoJob>, String> {
        Self::start_impl(
            url,
            dir,
            selector,
            audio,
            title,
            cookies,
            login,
            password,
            components_dir,
            expect_size,
            expect_duration,
            None,
            None,
        )
    }

    /// M-4: start a job for an entry whose previous run may have
    /// completed a merge that was never finalized (the app quit
    /// during the merge). `recorded_final` is the journal's
    /// recorded final_path; when the file is still on disk at a
    /// size consistent with `expect_size`, step 0 of the spawn
    /// thread finalizes (adopts) it instead of re-downloading
    /// every part from zero.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_recorded_final(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        title: &str,
        cookies: Option<PathBuf>,
        login: &str,
        password: &str,
        components_dir: &Path,
        expect_size: u64,
        expect_duration: f64,
        recorded_final: Option<PathBuf>,
    ) -> Result<Arc<VideoJob>, String> {
        Self::start_impl(
            url,
            dir,
            selector,
            audio,
            title,
            cookies,
            login,
            password,
            components_dir,
            expect_size,
            expect_duration,
            recorded_final,
            None,
        )
    }

    /// Start a job whose streams are already known — the
    /// browser-capture door (crate::capture): no yt-dlp resolve,
    /// no yt-dlp fallback; the engine downloads the supplied
    /// streams and muxes them exactly as it does a resolved
    /// selection.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_preresolved(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        title: &str,
        cookies: Option<PathBuf>,
        login: &str,
        password: &str,
        components_dir: &Path,
        expect_size: u64,
        expect_duration: f64,
        recorded_final: Option<PathBuf>,
        preresolved: Preresolved,
    ) -> Result<Arc<VideoJob>, String> {
        Self::start_impl(
            url,
            dir,
            selector,
            audio,
            title,
            cookies,
            login,
            password,
            components_dir,
            expect_size,
            expect_duration,
            recorded_final,
            Some(preresolved),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_impl(
        url: &str,
        dir: &Path,
        selector: &str,
        audio: bool,
        title: &str,
        cookies: Option<PathBuf>,
        login: &str,
        password: &str,
        components_dir: &Path,
        expect_size: u64,
        expect_duration: f64,
        recorded_final: Option<PathBuf>,
        preresolved: Option<Preresolved>,
    ) -> Result<Arc<VideoJob>, String> {
        // M-4: no recorded path handed in — a complete file
        // already sitting at this job's expected final path (its
        // title stem in the save folder) at a size consistent
        // with the expectation is the same kept-merge shape:
        // remember it as an adoption candidate for step 0. (The
        // stem is uniquified BELOW precisely because that file
        // exists; the candidate keeps the original path.)
        let recorded_final = recorded_final.or_else(|| {
            if expect_size == 0 {
                return None;
            }
            let s = safe_filename_stem(title);
            if s.is_empty() {
                return None;
            }
            own_media_exts(audio).iter().find_map(|ext| {
                let cand = dir.join(format!("{s}.{ext}"));
                let len = std::fs::metadata(&cand)
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| m.len())?;
                adoptable_final(len, expect_size).then_some(cand)
            })
        });
        let initial_stem = {
            let s = safe_filename_stem(title);
            if s.is_empty() {
                None
            } else {
                let final_exists = own_media_exts(audio)
                    .iter()
                    .any(|ext| dir.join(format!("{s}.{ext}")).exists());
                if final_exists {
                    Some(unique_stem_on_disk(dir, &s, audio))
                } else {
                    Some(s)
                }
            }
        };
        let effective_title = initial_stem.clone().unwrap_or_else(|| title.to_string());
        let job = Arc::new(VideoJob {
            url: url.to_string(),
            dir: dir.to_path_buf(),
            selector: selector.to_string(),
            audio,
            title: effective_title,
            cookies,
            // V3-a (208B): the progress total is seeded ONCE, here
            // at job start, from the job's known total — the picked
            // option's probed size (`expect_size`). No progress
            // handler may ever (re)derive it from downloaded bytes;
            // the engine poll loop may only LOCK it to the parts'
            // probed sum, and the finalize paths may correct it to
            // the merged file's true length at completion
            progress: Arc::new(Mutex::new(VideoProgress {
                total: expect_size,
                ..VideoProgress::default()
            })),
            chosen_stem: Mutex::new(initial_stem),
            child: Mutex::new(None),
            active_downloads: Mutex::new(Vec::new()),
            stop_flag: Mutex::new(None),
            spawn_lock: Mutex::new(()),
            gen: std::sync::atomic::AtomicU64::new(0),
            alive: std::sync::atomic::AtomicBool::new(false),
            expect_audio: Mutex::new(false),
            expected_media: Mutex::new(None),
            pair_audio_url: Mutex::new(None),
            limit_bps: std::sync::atomic::AtomicU64::new(0),
            login: login.to_string(),
            password: password.to_string(),
            merged_final: Mutex::new(None),
            format_size: Mutex::new(None),
            format_size_exact: Mutex::new(false),
            downloaded_total: Mutex::new(0),
            recorded_final,
            preresolved,
            expect_size,
            expect_duration,
        });
        job.spawn(components_dir)?;
        Ok(job)
    }

    fn resolve_or_lock_stem(&self, fallback_title: &str) -> String {
        let mut guard = self.chosen_stem.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            if !existing.is_empty() {
                return existing.clone();
            }
        }
        let raw = if !self.title.trim().is_empty() {
            &self.title
        } else {
            fallback_title
        };
        let unique = unique_stem_on_disk(&self.dir, raw, self.audio);
        if !unique.is_empty() {
            *guard = Some(unique.clone());
        }
        unique
    }

    /// The cache key for this job's resolution under `sel`: every
    /// input that can change which streams yt-dlp would pick.
    fn resolution_key(&self, sel: &str) -> ResolutionKey {
        ResolutionKey {
            url: self.url.clone(),
            sel: sel.to_string(),
            audio: self.audio,
            login: self.login.clone(),
            password_sig: password_signature(&self.password),
            cookies_sig: cookies_signature(self.cookies.as_deref()),
            proxy: ndm::global_proxy(),
        }
    }

    /// Resolve direct HTTP(S) stream URLs via yt-dlp JSON so we can download
    /// them over NDM's 32-connection parallel Rust engine instead of yt-dlp's
    /// single-threaded Python HTTP loop.
    fn resolve_direct_streams(
        &self,
        ytdlp: &Path,
        components_dir: &Path,
        sel: &str,
    ) -> Option<(String, Vec<DirectStream>)> {
        let key = self.resolution_key(sel);
        let probe = resolve_streams_once(&key, || {
            resolve_streams_core(
                &self.url,
                sel,
                self.audio,
                self.cookies.as_deref(),
                &self.login,
                &self.password,
                ytdlp,
                components_dir,
            )
        })?;
        // The learned fields are applied even when the selection
        // turns out ineligible for the engine: the yt-dlp fallback
        // downloads this same selection, so its guards still need
        // them. They are applied UNCONDITIONALLY (m-7): this probe
        // describes THIS selection, so a field it does not report
        // must CLEAR the value a previous selection left behind —
        // the old conditional writes let a fresh probe inherit the
        // earlier selector's pair_audio_url / expected_media, and
        // the cache store in run_parallel_engine then persisted
        // those stale fields under the new selection's key.
        *self.expect_audio.lock().unwrap() = probe.expect_audio;
        *self.pair_audio_url.lock().unwrap() = probe.pair_audio_url.clone();
        *self.expected_media.lock().unwrap() = probe.expected_media;
        *self.format_size.lock().unwrap() = probe.format_size;
        *self.format_size_exact.lock().unwrap() = probe.format_size_exact;
        probe.resolved
    }

    /// WS-A (audit-232 S5): re-resolve this job's PAGE URL
    /// through yt-dlp after a preresolved (browser-captured)
    /// job's streams auth-died. The job's own selector names a
    /// capture group — it is not a yt-dlp format expression — so
    /// the heal resolves with the standard best selector for the
    /// job's kind, and returns it alongside the fresh streams:
    /// later stall re-resolves keep using that selector, never
    /// the capture key. The cache entry for the heal selector is
    /// evicted first — a heal must resolve LIVE, not inherit a
    /// warm resolution whose URLs may be the ones that just died.
    fn heal_reresolve_preresolved(
        &self,
        ytdlp: &Path,
        components_dir: &Path,
    ) -> Option<(Vec<DirectStream>, String)> {
        let heal_sel = if self.audio {
            "ba[ext=m4a]/ba/b".to_string()
        } else {
            ndm::video_probe::best_selector()
        };
        resolution_cache_evict(&self.resolution_key(&heal_sel));
        let (_, fresh) = self.resolve_direct_streams(ytdlp, components_dir, &heal_sel)?;
        Some((fresh, heal_sel))
    }

    /// Wait for cancelled streams to fully settle before their
    /// part files are reopened (the engine's one-writer guard is
    /// released only after the final state save; restarting
    /// earlier is refused and kills the engine path). Bounded by
    /// the engine's own watchdog horizon; a stop landing during
    /// the wait is reported, never waited out.
    fn settle_streams(&self, dls: &[Arc<ndm::Download>], my_gen: u64) -> StreamSettle {
        let deadline = Instant::now() + std::time::Duration::from_secs(35);
        while !dls.iter().all(|d| d.is_settled()) {
            if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
                || self.stop_flag.lock().unwrap().is_some()
            {
                return StreamSettle::Stopped;
            }
            if Instant::now() >= deadline {
                return StreamSettle::TimedOut;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        StreamSettle::Done
    }

    fn run_parallel_engine(
        self: &Arc<Self>,
        ytdlp: &Path,
        ffmpeg: &Path,
        components_dir: &Path,
        sel: &str,
        my_gen: u64,
    ) -> bool {
        // A caller-supplied resolution (the browser-capture door)
        // skips both the cache and the yt-dlp resolve: the streams
        // are already the real ones. Its learned state is exactly
        // what the capture declared — nothing else is known about
        // the selection, so every other learned field starts
        // cleared (the m-7 rule: assign, never merge).
        let (resolved_title, streams) = if let Some(pre) = &self.preresolved {
            *self.expect_audio.lock().unwrap() = pre.expect_audio;
            *self.expected_media.lock().unwrap() = None;
            *self.pair_audio_url.lock().unwrap() = None;
            *self.format_size.lock().unwrap() = None;
            *self.format_size_exact.lock().unwrap() = false;
            let mut streams: Vec<DirectStream> = pre
                .streams
                .iter()
                .map(|s| DirectStream {
                    url: s.url.clone(),
                    ext: s.ext.clone(),
                    has_audio: s.has_audio,
                    cookie: s.cookie.clone(),
                    referer: s.referer.clone(),
                    user_agent: s.user_agent.clone(),
                })
                .collect();
            // Streams the capture left cookieless inherit the
            // job's captured session (the 2.2.0 cookie jar's
            // source file): googlevideo-class hosts answer a
            // bare request, but a session-gated CDN may not, and
            // the browser's cookies are what it played with.
            if let Some(path) = &self.cookies {
                if let Ok(text) = std::fs::read_to_string(path) {
                    let now = crate::capture::now_unix();
                    for st in &mut streams {
                        if st.cookie.is_none() {
                            st.cookie = crate::capture::cookie_header_from_netscape(
                                &text, &st.url, now,
                            );
                        }
                    }
                }
            }
            (pre.title.clone(), streams)
        } else {
            // A fresh cached resolution for this exact URL + selector +
            // cookie set (left by an earlier start of the same selection
            // or by the flyout's warm resolve) skips the yt-dlp run
            // entirely — that run is the whole click-to-first-byte wait.
            // A miss resolves live and leaves its result behind.
            let res_key = self.resolution_key(sel);
            match resolution_cache_get(&res_key, Instant::now()) {
            Some(res) => {
                // a cache hit defines this selection's learned
                // state completely (m-7): assign, never merge with
                // whatever a previous selection left on the job
                *self.expect_audio.lock().unwrap() = res.expect_audio;
                *self.expected_media.lock().unwrap() = res.expected_media;
                *self.pair_audio_url.lock().unwrap() = res.pair_audio_url.clone();
                *self.format_size.lock().unwrap() = res.format_size;
                *self.format_size_exact.lock().unwrap() = res.format_size_exact;
                (res.title, res.streams)
            }
            None => {
                let Some(fresh) = self.resolve_direct_streams(ytdlp, components_dir, sel)
                else {
                    return false;
                };
                resolution_cache_store(
                    res_key,
                    Resolution {
                        title: fresh.0.clone(),
                        streams: fresh.1.clone(),
                        expect_audio: *self.expect_audio.lock().unwrap(),
                        expected_media: *self.expected_media.lock().unwrap(),
                        pair_audio_url: self.pair_audio_url.lock().unwrap().clone(),
                        format_size: *self.format_size.lock().unwrap(),
                        format_size_exact: *self.format_size_exact.lock().unwrap(),
                    },
                    Instant::now(),
                );
                fresh
            }
            }
        };
        // The resolved streams are the ground truth for what the
        // merged file must contain (the finish-line guard reads this).
        if streams.iter().any(|s| s.has_audio) {
            *self.expect_audio.lock().unwrap() = true;
        }
        if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
            || self.stop_flag.lock().unwrap().is_some()
        {
            return true;
        }

        let mut stem = self.resolve_or_lock_stem(&resolved_title);
        if stem.is_empty() {
            stem = unique_stem_on_disk(&self.dir, "video", self.audio);
        }
        let final_ext = if self.audio {
            if sel.contains("m4a") && !sel.contains("mp3") {
                "m4a"
            } else {
                "mp3"
            }
        } else {
            "mp4"
        };
        let final_path = self.dir.join(format!("{stem}.{final_ext}"));

        let max_conn = ndm::global_connections().clamp(1, 32);
        // Finding 2 (speed investigation): in a video+audio pair the
        // video stream carries ~90% of the bytes, and googlevideo-class
        // hosts cap throughput per connection — the old even split
        // (4 + 4 at the default 8) starved exactly the stream that
        // needed the connections. The pair now splits weighted via
        // ndm::video_pair_connections (video max_conn − 2, audio 2,
        // near-even at small counts). The video stream is the one not
        // carrying the audio track (an audio-only format always
        // reports an acodec); a pair where both streams carry audio
        // (a combined stream plus a separate track) treats the first
        // as primary. A single stream keeps the full count, and
        // 3+-stream sets (exotic) keep the old even split. The split
        // itself is computed inside the attempt loop below, because a
        // stall recovery's re-resolve can change the stream set.

        // 2.3.1 stall recovery (bugs 2+3): a throttled CDN stream
        // (googlevideo-class) decays to ~0 B/s and then sits there
        // — every connection alive, no error, no bytes. The old
        // shape waited for the engine's own 30 s no-progress
        // verdict, then abandoned the engine for the yt-dlp
        // fallback, whose single stream throttled the same way;
        // the entry cycled "Probing connection…" for hours at
        // 12% (the user's recording). The attempt loop instead
        // recovers IN PLACE: cancel the wedged streams (parts and
        // their resume state stay on disk), restart them from the
        // downloaded offset on fresh connections, and — when
        // fresh connections alone do not revive the transfer —
        // re-resolve the stream URLs once and continue from the
        // same offset. The cap counts BARREN attempts only (ones
        // that gained almost nothing): a recovery that moves
        // real bytes is the medicine working, not a failure to
        // ration — a per-connection throttle can need several.
        // Only a job that stops gaining falls back to yt-dlp,
        // as before.
        let mut part_paths: Vec<PathBuf> = Vec::new();
        let mut streams = streams;
        let mut barren = 0u32;
        // WS-A: once a preresolved job has healed, it runs on
        // yt-dlp-resolved streams under this selector — later
        // stall re-resolves use it instead of the capture key
        let mut reresolve_sel: Option<String> = None;
        'attempts: loop {
            // A stop that landed while the previous attempt was
            // being recovered from — above all during the
            // re-resolve below, whose yt-dlp child is not the
            // job's tracked child and so cannot be killed by
            // pause()/cancel() — must not restart any stream:
            // the resolve's result is discarded here, exactly as
            // the pre-loop resolve's is (the gate above the loop).
            // Without this gate the loop restarted the streams
            // and the poll, which only honours "cancelled" and
            // the generation, ran the transfer on against an
            // explicit pause (audit-232 B3a).
            if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
                || self.stop_flag.lock().unwrap().is_some()
            {
                return true;
            }
            // the connection split is recomputed per attempt: a
            // re-resolve can change the stream set under us
            let primary = streams.iter().position(|s| !s.has_audio).unwrap_or(0);
            let stream_conn = |i: usize| -> usize {
                if streams.len() == 2 {
                    let (video_conn, audio_conn) = ndm::video_pair_connections(max_conn);
                    if i == primary {
                        video_conn
                    } else {
                        audio_conn
                    }
                } else if streams.len() > 1 {
                    (max_conn / streams.len()).max(1)
                } else {
                    max_conn
                }
            };
            let mut dls: Vec<Arc<ndm::Download>> = Vec::new();
            let mut start_auth_dead = false;
            part_paths.clear();
            for (i, st) in streams.iter().enumerate() {
                let part = self
                    .dir
                    .join(format!(".{stem}.ndm_stream{i}.{}.part", st.ext));
                let auth = ndm::Auth {
                    cookie: st.cookie.clone(),
                    referer: st.referer.clone(),
                    user_agent: st.user_agent.clone(),
                    authorization: None,
                };
                // WS-D D2 (audit-232 S4): a pair starts each side
                // with the FULL pool spawned but only its pair
                // share active — when one side finishes, the poll
                // raises the survivor to the whole pool live (see
                // poll_engine_attempt), instead of the larger
                // stream finishing on its share while the finished
                // side's connections idle. Non-pair sets keep the
                // plain start (spawn == active, as before).
                let started = if streams.len() == 2 {
                    ndm::start_with_auth_limited(
                        &st.url,
                        &part,
                        max_conn,
                        stream_conn(i),
                        ndm::DEFAULT_BLOCK,
                        auth,
                    )
                } else {
                    ndm::start_with_auth(&st.url, &part, stream_conn(i), ndm::DEFAULT_BLOCK, auth)
                };
                match started {
                    Ok(dl) => {
                        let lim = self.limit();
                        if lim > 0 {
                            dl.set_limit(lim);
                        }
                        part_paths.push(part);
                        dls.push(dl);
                    }
                    Err(e) => {
                        for d in &dls {
                            d.control.cancel();
                        }
                        self.active_downloads.lock().unwrap().clear();
                        // WS-A: a preresolved stream refused at
                        // START with an auth-death error — the
                        // capture died before this attempt got
                        // going. Heal it below like the
                        // mid-transfer death, instead of failing
                        // the job on the spot.
                        if self.preresolved.is_some() && ndm::error_is_auth_dead(&e) {
                            start_auth_dead = true;
                            break;
                        }
                        // KEEP the part files and their resume state: a
                        // stream that failed to START must not destroy the
                        // bytes the other stream already fetched — a retry
                        // resumes them instead of restarting from byte 0
                        // (only a user cancel deletes partials).
                        return false;
                    }
                }
            }

            if start_auth_dead {
                // WS-A heal for a capture that was already dead
                // at start: settle the cancelled partial starts,
                // spend one recovery unit, re-resolve the page
                // URL, and retry the attempt on fresh streams —
                // bounded by the same budget as every recovery
                {
                    let mut p = self.progress.lock().unwrap();
                    if p.status != "paused" {
                        p.status = "resuming".to_string();
                        p.speed_bps = 0.0;
                        p.conns = 0;
                    }
                }
                match self.settle_streams(&dls, my_gen) {
                    StreamSettle::Stopped => return true,
                    StreamSettle::TimedOut => {
                        self.active_downloads.lock().unwrap().clear();
                        return false;
                    }
                    StreamSettle::Done => {}
                }
                if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
                    || self.stop_flag.lock().unwrap().is_some()
                {
                    return true;
                }
                barren += 1;
                if barren > MAX_VIDEO_RECOVERIES {
                    self.active_downloads.lock().unwrap().clear();
                    return false;
                }
                match self.heal_reresolve_preresolved(ytdlp, components_dir) {
                    Some((fresh, heal_sel)) => {
                        streams = fresh;
                        reresolve_sel = Some(heal_sel);
                        continue 'attempts;
                    }
                    None => return false,
                }
            }

            *self.active_downloads.lock().unwrap() = dls.clone();
            // A pause that landed in the start window above (the
            // active set was still empty, so pause() could only
            // set the stop flag) applies to the fresh streams
            // now: pausing them lets the poll park the job, the
            // same end state as a pause mid-transfer. A cancel in
            // the same window needs no help — the poll's first
            // check already honours it.
            if self.stop_flag.lock().unwrap().as_deref() == Some("paused") {
                for d in &dls {
                    d.control.pause();
                }
            }
            let attempt_start_done = self.progress.lock().unwrap().done;
            let outcome = self.poll_engine_attempt(&dls, my_gen, attempt_start_done);
            // WS-A: did any stream's engine see its address die
            // (403/410) during this attempt? Read before the
            // outcome arm cancels anything — the flag lives on
            // the engine handle, not the poll verdict
            let attempt_auth_dead = dls.iter().any(|d| d.progress().auth_dead);
            match outcome {
                EngineAttempt::Finished => break 'attempts,
                EngineAttempt::Stopped => return true,
                EngineAttempt::Errored | EngineAttempt::Stalled => {
                    let done_now = self.progress.lock().unwrap().done;
                    // WS-A accounting: a preresolved auth-death
                    // spends one recovery unit for the attempt
                    // even when the attempt gained real bytes
                    // before dying — the page re-resolve it buys
                    // is the expensive recovery, and letting
                    // gains reset the budget would cycle forever
                    // (heal, gain, die, heal…). The heal counts
                    // against the SAME MAX_VIDEO_RECOVERIES
                    // budget as every other recovery; no second
                    // budget exists.
                    let auth_heal_case = self.preresolved.is_some()
                        && attempt_auth_dead
                        && (outcome == EngineAttempt::Errored
                            || (outcome == EngineAttempt::Stalled && done_now == 0));
                    // An error before ANY byte moved is a start
                    // failure, not a stall: the yt-dlp fallback
                    // stays the fast answer, exactly as before.
                    // C-2 (audit-233) exception: a preresolved
                    // auth death at byte 0 — the start probe's
                    // HEAD passed but every range GET was refused
                    // (403) — is the same dead capture as a
                    // mid-download death and heals through the
                    // recovery flow below instead of ending the
                    // job with the capture-expired message. The
                    // heal case above also covers this shape's
                    // Stalled verdict: the poll's stall window
                    // fires before the engine's own error does,
                    // and at byte 0 with a typed auth death there
                    // are no bytes for fresh connections to
                    // protect — the re-resolve is the only remedy.
                    // (A Stalled attempt WITH bytes keeps the
                    // existing fresh-connections-first behaviour
                    // and self-corrects on its next Errored
                    // attempt, as before.)
                    if outcome == EngineAttempt::Errored && done_now == 0 && !auth_heal_case {
                        self.active_downloads.lock().unwrap().clear();
                        return false;
                    }
                    // barren accounting: an attempt that gained
                    // real bytes resets the count; only nearly
                    // motionless attempts spend it
                    if auth_heal_case {
                        barren += 1;
                    } else if done_now.saturating_sub(attempt_start_done) >= VIDEO_STALL_MIN_ADVANCE * 4 {
                        barren = 0;
                    } else {
                        barren += 1;
                    }
                    if barren > MAX_VIDEO_RECOVERIES {
                        self.active_downloads.lock().unwrap().clear();
                        return false; // Fall back to yt-dlp
                    }
                    // Errored streams point at dead URLs more
                    // than throttled ones: re-resolve on the
                    // error path and once stalls turn barren;
                    // plain stalls first retry the same URLs on
                    // fresh connections (a new connection is what
                    // a per-connection throttle resets on).
                    let reresolve = outcome == EngineAttempt::Errored || barren >= 2;
                    for d in &dls {
                        d.control.cancel();
                    }
                    self.active_downloads.lock().unwrap().clear();
                    // The cancelled streams must fully SETTLE
                    // before the restart reopens the same part
                    // files: the engine's one-writer guard is
                    // released by the supervisor only after the
                    // final state save, and a worker wedged on a
                    // silent socket can take the engine's own
                    // watchdog horizon (~30 s) to die. Restarting
                    // earlier does not fail soft — the new start
                    // is REFUSED ("another download is already
                    // writing") and the whole engine path dies
                    // into the fallback. So wait the horizon
                    // out; "resuming" (set just above the wait)
                    // is the honest status meanwhile — a job that
                    // already HAS bytes is resuming, never
                    // "probing" again (bug 3). A job whose streams
                    // still have not settled by then gives up to
                    // the fallback instead of starting into the
                    // guard.
                    {
                        let mut p = self.progress.lock().unwrap();
                        if p.status != "paused" {
                            p.status = "resuming".to_string();
                            p.speed_bps = 0.0;
                            p.conns = 0;
                        }
                    }
                    match self.settle_streams(&dls, my_gen) {
                        StreamSettle::Stopped => return true,
                        StreamSettle::TimedOut => {
                            self.active_downloads.lock().unwrap().clear();
                            return false; // Fall back to yt-dlp
                        }
                        StreamSettle::Done => {}
                    }
                    if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
                        || self.stop_flag.lock().unwrap().is_some()
                    {
                        return true;
                    }
                    if auth_heal_case {
                        // WS-A (audit-232 S5): the capture's
                        // streams auth-died — retrying them can
                        // only burn the budget on dead URLs.
                        // Re-resolve the PAGE URL instead and
                        // continue from the same part offsets.
                        // Non-auth failures never reach this
                        // branch: they keep the behaviour below
                        // (fresh connections, then the capture's
                        // plain expired-message failure) exactly.
                        match self.heal_reresolve_preresolved(ytdlp, components_dir) {
                            Some((fresh, heal_sel)) => {
                                streams = fresh;
                                reresolve_sel = Some(heal_sel);
                            }
                            None => return false, // spawn reports the capture as expired
                        }
                    } else if reresolve && (self.preresolved.is_none() || reresolve_sel.is_some()) {
                        // a healed job runs on yt-dlp streams:
                        // later stall re-resolves use the heal's
                        // selector, never the capture key
                        let sel_now = reresolve_sel.as_deref().unwrap_or(sel);
                        resolution_cache_evict(&self.resolution_key(sel_now));
                        match self.resolve_direct_streams(ytdlp, components_dir, sel_now) {
                            Some((_, fresh)) => streams = fresh,
                            None => return false, // Fall back to yt-dlp
                        }
                    }
                    // loop: restart every stream from its offset
                }
            }
        }

        self.active_downloads.lock().unwrap().clear();
        {
            let mut p = self.progress.lock().unwrap();
            if p.total > 0 {
                p.done = p.total;
            }
            // A2: every part has arrived — from here to the
            // finalize verdict the job is merging (the ffmpeg run
            // below plus the ensure_audio window); the finalize
            // branches replace this with their own verdict
            p.status = "merging".to_string();
            p.speed_bps = 0.0;
            p.conns = 0;
            // M-4: the merged output path is known NOW, before
            // ffmpeg runs — record it at merge START, so the
            // journal (persisted every ~2 s) carries status
            // "merging" WITH the final_path. If the app quits
            // mid-merge, the orphaned ffmpeg still completes the
            // file, and the recorded path is what lets a later
            // run adopt that completed merge instead of
            // re-downloading every part
            p.final_path = Some(final_path.clone());
            // the finish line's parts sum: the parts' locked total
            *self.downloaded_total.lock().unwrap() = p.done;
        }

        // The finish-line guard's baseline for this merge: the
        // media payload of the parts about to be merged, estimated
        // from the parts themselves (duration × their streams' own
        // bitrates). The parts' byte counts are NOT the baseline —
        // a part's container can carry arbitrary non-media payload
        // that the remux legitimately drops (R3-EXT-01). Unknown
        // when any part reports no bitrate/duration; the guard then
        // skips this job rather than guessing.
        let expected_media = if self.audio {
            None
        } else {
            let mut sum = 0u64;
            let mut known = !part_paths.is_empty();
            for part in &part_paths {
                match estimate_media_payload(part, components_dir) {
                    Some(n) => sum = sum.saturating_add(n),
                    None => {
                        known = false;
                        break;
                    }
                }
            }
            if known && sum > 0 {
                Some(sum)
            } else {
                None
            }
        };

        // Merge / remux into final output file with ffmpeg
        let mut ff_args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
        for part in &part_paths {
            ff_args.push("-i".into());
            ff_args.push(part.display().to_string());
        }
        if self.audio {
            ff_args.push("-vn".into());
            if final_ext == "m4a" {
                ff_args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]);
            } else {
                ff_args.extend(["-c:a".into(), "libmp3lame".into(), "-q:a".into(), "0".into()]);
            }
        } else if part_paths.len() >= 2 {
            ff_args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "1:a:0".into(),
                "-c:v".into(),
                "copy".into(),
            ]);
            // copy when the audio is already AAC, re-encode
            // otherwise (see merge_audio_args, WS-D D3)
            ff_args.extend(merge_audio_args(&part_paths[1], components_dir));
            ff_args.extend(["-movflags".into(), "+faststart".into()]);
        } else if final_ext == "mp4" || final_ext == "m4a" || final_ext == "mov" {
            // Normalize the audio to AAC like the two-stream branch:
            // a verbatim copy can leave Opus-in-MP4 that many players
            // refuse to play. Already-AAC audio is copied, not
            // re-encoded (see merge_audio_args, WS-D D3).
            ff_args.extend(["-c:v".into(), "copy".into()]);
            ff_args.extend(merge_audio_args(&part_paths[0], components_dir));
            ff_args.extend(["-movflags".into(), "+faststart".into()]);
        } else {
            ff_args.extend([
                "-c".into(),
                "copy".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        }
        ff_args.push(final_path.display().to_string());

        // M-4: the stream parts and their state files are deleted
        // ONLY once the finalize below has fully completed (a
        // terminal verdict). They used to be deleted the moment
        // ffmpeg returned — before ensure_audio ran — so quitting
        // anywhere in the merge→finalize window left a complete
        // merged file whose parts were gone, and the next run
        // could only re-download. While the job is anywhere short
        // of a terminal state (paused mid-merge, cancelled,
        // merge failed → fallback), the parts survive and a
        // resume re-uses them.
        let delete_parts = || {
            for part in &part_paths {
                let _ = std::fs::remove_file(part);
                let _ = std::fs::remove_file(ndm::state_path_for(part));
            }
        };

        let merge_ok = tool_cmd(ffmpeg)
            .args(&ff_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && final_path.is_file();

        // A cancel that landed DURING the merge (which can take
        // seconds) must win over the merge result: publishing
        // "finished" here would resurrect a download the user just
        // cancelled and leave the merged file in place.
        if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
            return true; // a newer job owns the progress now
        }
        if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
            if flag == "cancelled" {
                if merge_ok {
                    let _ = std::fs::remove_file(&final_path);
                }
                let mut p = self.progress.lock().unwrap();
                p.status = "cancelled".to_string();
                p.speed_bps = 0.0;
                p.conns = 0;
                return true;
            }
            if flag == "paused" {
                // the user paused while ffmpeg was merging: the
                // entry must end PAUSED — publishing "finished"
                // would fire the completion actions (sound, power)
                // over a download the user deliberately stopped.
                // A good merge is KEPT and recorded, so the resume
                // finalizes it (see the spawn thread) instead of
                // re-downloading; a failed merge leaves nothing
                // usable, but the pause still wins over a fallback.
                if merge_ok {
                    *self.merged_final.lock().unwrap() = Some(final_path.clone());
                    let mut p = self.progress.lock().unwrap();
                    if let Ok(m) = final_path.metadata() {
                        p.total = m.len();
                        p.done = m.len();
                    }
                    p.status = "paused".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(final_path);
                } else {
                    let mut p = self.progress.lock().unwrap();
                    p.status = "paused".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                }
                return true;
            }
        }

        if merge_ok {
            // the bytes this job actually downloaded, captured BEFORE
            // the finalize below can overwrite the total with the
            // merged file's size (F-EXT-04)
            let downloaded = self.progress.lock().unwrap().total;
            let audio_result = self.ensure_audio(&final_path, components_dir);
            // A stop that landed DURING the audio check (which can
            // take seconds when it fetches a separate rendition)
            // wins over the merge result — the stop check above ran
            // BEFORE the check and cannot see it. A cancelled job
            // must never publish "finished" (R2-EXT-05).
            if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
                if flag == "cancelled" {
                    if let Ok(verified) = &audio_result {
                        let _ = std::fs::remove_file(verified);
                    }
                    let _ = std::fs::remove_file(&final_path);
                    let mut p = self.progress.lock().unwrap();
                    p.status = "cancelled".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    return true;
                }
                if flag == "paused" {
                    // same contract as a pause during the merge
                    // itself: the merge is KEPT and recorded, so the
                    // resume finalizes it instead of re-downloading
                    let kept = match &audio_result {
                        Ok(verified) => verified.clone(),
                        Err(_) => final_path.clone(),
                    };
                    *self.merged_final.lock().unwrap() = Some(kept.clone());
                    let mut p = self.progress.lock().unwrap();
                    if let Ok(m) = kept.metadata() {
                        p.total = m.len();
                        p.done = m.len();
                    }
                    p.status = "paused".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(kept);
                    return true;
                }
            }
            match audio_result {
                Ok(verified) => {
                    let flen = verified.metadata().map(|m| m.len()).unwrap_or(0);
                    // judged against the selected streams' media
                    // payload (expected_media), never the downloaded
                    // total (R3-EXT-01); audio jobs are exempt: an
                    // extracted audio track is legitimately a small
                    // fraction of the downloaded container (R2-EXT-02)
                    let shrink = if flen > 0 && !self.audio {
                        drastic_shrink(expected_media, downloaded, flen)
                    } else {
                        None
                    };
                    // W-2: a merge that passes the media-payload
                    // guard can still contradict what the user was
                    // shown at add time (a different stream than
                    // the one probed) — that is an error too, never
                    // a clean success
                    let verdict = match shrink {
                        Some(msg) => Some(msg),
                        None => self.finish_contradiction(&verified, flen, components_dir),
                    };
                    let finished_now = verdict.is_none();
                    let mut p = self.progress.lock().unwrap();
                    if let Some(msg) = verdict {
                        // keep the true downloaded total; a
                        // drastically smaller deliverable is a
                        // visible error, not a silent success
                        p.status = "error".to_string();
                        p.error = Some(msg);
                        p.speed_bps = 0.0;
                        p.conns = 0;
                        p.final_path = Some(verified);
                    } else {
                        if flen > 0 {
                            p.total = flen;
                            p.done = flen;
                        }
                        p.status = "finished".to_string();
                        p.speed_bps = 0.0;
                        p.conns = 0;
                        p.final_path = Some(verified);
                    }
                    drop(p);
                    // the finalize has fully completed (finished
                    // or a terminal verdict): the parts' work is
                    // done, they go now — and not a moment before
                    // (M-4)
                    delete_parts();
                    if finished_now {
                        // a finished job leaves no working files
                        // behind — including a previous attempt's
                        // yt-dlp partials (R4 side observation)
                        sweep_job_temps(&self.dir, &stem);
                    }
                    true
                }
                Err(msg) => {
                    // The bytes arrived but the file must not be
                    // presented as a success (e.g. it has no sound).
                    // Terminal: do NOT fall back to a second download.
                    let mut p = self.progress.lock().unwrap();
                    p.status = "error".to_string();
                    p.error = Some(msg);
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(final_path);
                    drop(p);
                    delete_parts();
                    true
                }
            }
        } else {
            false
        }
    }

    /// Poll one engine attempt's streams until they all finish,
    /// a stream errors, the job is stopped — or the transfer
    /// stalls (2.3.1 bugs 2+3): the combined advance over a
    /// VIDEO_STALL_WINDOW stays under VIDEO_STALL_MIN_ADVANCE.
    /// A pure zero-advance test is not enough: a throttled
    /// googlevideo connection DRIBBLES (the field recording sat
    /// at ~0.8 KB/s), and any dribble would reset a zero-test
    /// forever while the ETA reads hours. The floor (~2.7 KB/s)
    /// sits far below any healthy stream and far above the
    /// throttle's drip. The aggregate progress bookkeeping is
    /// the pre-2.3.1 poll loop's, unchanged; only the exits are
    /// named now, so the caller (run_parallel_engine's attempt
    /// loop) can recover a stalled attempt in place instead of
    /// dropping to the yt-dlp fallback at the first wedge.
    fn poll_engine_attempt(
        &self,
        dls: &[Arc<ndm::Download>],
        my_gen: u64,
        resume_from: u64,
    ) -> EngineAttempt {
        let mut mark_done = 0u64;
        let mut mark_at = Instant::now();
        // WS-D D2: pair cross-stealing fires at most once per attempt
        let mut donated = false;
        loop {
            if self.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                for d in dls {
                    d.control.cancel();
                }
                return EngineAttempt::Stopped;
            }
            if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
                if flag == "cancelled" {
                    for d in dls {
                        d.control.cancel();
                    }
                    // the part files and their resume state are
                    // KEPT: plain list removal preserves partials
                    // on BOTH engines (parity with the yt-dlp path),
                    // and deleting them is exclusive to the discard
                    // flow in main.rs, which sweeps .ndm_stream*
                    // when the user explicitly discards
                    let mut p = self.progress.lock().unwrap();
                    p.status = "cancelled".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    return EngineAttempt::Stopped;
                }
            }

            let mut total_sum = 0u64;
            let mut done_sum = 0u64;
            let mut speed_sum = 0.0f64;
            let mut conns_sum = 0usize;
            let mut all_finished = true;
            let mut any_error = false;
            let mut any_paused = false;
            // A1: the total may lock only when EVERY part has
            // reported a probed total — a part that has not probed
            // yet reports 0 and vetoes the lock
            let mut all_totals_known = !dls.is_empty();
            let mut statuses: Vec<ndm::Status> = Vec::with_capacity(dls.len());

            for d in dls {
                let pr = d.progress();
                statuses.push(pr.status);
                if pr.total == 0 {
                    all_totals_known = false;
                }
                total_sum = total_sum.saturating_add(pr.total);
                done_sum = done_sum.saturating_add(pr.done_bytes);
                if pr.status == ndm::Status::Downloading {
                    speed_sum += pr.speed_bps;
                    conns_sum = conns_sum.saturating_add(pr.conns);
                }
                match pr.status {
                    ndm::Status::Finished => {}
                    ndm::Status::Paused => {
                        all_finished = false;
                        any_paused = true;
                    }
                    ndm::Status::Error | ndm::Status::Cancelled => {
                        all_finished = false;
                        any_error = true;
                    }
                    ndm::Status::Downloading => {
                        all_finished = false;
                    }
                }
            }

            // (the stall window is judged at the bottom of the
            // loop from this mark — see there)

            {
                let mut p = self.progress.lock().unwrap();
                // A1: once every part has probed, the total is the
                // parts' sum — set ONCE and locked. Before the lock
                // the total keeps its hint value instead of jumping
                // with each part that happens to report early; after
                // it, per-part sum drift can never move it again.
                if !p.total_locked && all_totals_known && total_sum > 0 {
                    p.total = total_sum;
                    p.total_locked = true;
                }
                if done_sum != p.done {
                    p.last_progress_at = Instant::now();
                }
                p.done = done_sum;
                p.speed_bps = if any_paused { 0.0 } else { speed_sum };
                p.conns = if any_paused || all_finished {
                    0
                } else {
                    conns_sum.max(1)
                };
                p.status = if any_paused {
                    "paused".to_string()
                } else if resume_from > 0 && done_sum <= resume_from && p.status == "resuming" {
                    // a recovery attempt stays "resuming" until
                    // NEW bytes land past the offset it resumed
                    // from — flipping to "downloading" on the
                    // resumed total alone would lie about data
                    // moving again (bug 3's honest status)
                    "resuming".to_string()
                } else if done_sum > 0 || p.total_locked {
                    // bytes are moving — or every stream has
                    // reported in (probed), which ends "starting"
                    // even before the first byte lands
                    "downloading".to_string()
                } else {
                    "starting".to_string()
                };
            }

            // WS-D D2 (audit-232 S4): pair cross-stealing. The
            // moment exactly one side of a two-stream pair has
            // finished and the other is still downloading, the
            // finished side's share of the pool is idle by
            // definition — raise the survivor to the full
            // connection count so it inherits those connections
            // live (its parked workers ungate within a tick).
            // An errored pair neither donates nor receives (the
            // error path below settles the attempt), and a paused
            // pair stays parked exactly as it was.
            if !donated && dls.len() == 2 && !any_error && !any_paused {
                let survivor = match (statuses[0], statuses[1]) {
                    (ndm::Status::Finished, ndm::Status::Downloading) => Some(&dls[1]),
                    (ndm::Status::Downloading, ndm::Status::Finished) => Some(&dls[0]),
                    _ => None,
                };
                if let Some(d) = survivor {
                    d.set_connection_limit(ndm::global_connections().clamp(1, 32));
                    // the survivor's tail blocks are now the
                    // job's finish line and its newly inherited
                    // workers are dedicated helpers — relax its
                    // tail byte-split gate accordingly (WS-D D2)
                    d.mark_pair_survivor();
                    donated = true;
                }
            }

            if any_error {
                for d in dls {
                    d.control.cancel();
                }
                // KEEP part files + resume state (see above): the
                // yt-dlp fallback uses different file names, so the
                // engine partials cost nothing to keep and a later
                // retry resumes instead of re-downloading gigabytes.
                return EngineAttempt::Errored;
            }

            if all_finished {
                return EngineAttempt::Finished;
            }

            // The stall verdict comes last: a paused job is
            // waiting on the user, not the network, and a
            // finished one has already returned above. The mark
            // rolls forward one window at a time; a window whose
            // advance stayed under the floor is a stall.
            if mark_at.elapsed() >= VIDEO_STALL_WINDOW {
                if !any_paused && done_sum.saturating_sub(mark_done) < VIDEO_STALL_MIN_ADVANCE {
                    return EngineAttempt::Stalled;
                }
                mark_done = done_sum;
                mark_at = Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    }

    /// The W-2 contradiction check for one produced file: compares
    /// it against this job's add-time expectations (see
    /// `contradiction_verdict`). Skips itself entirely when nothing
    /// was probed at add time.
    ///
    /// M-15: the SIZE half no longer trusts `expect_size` alone —
    /// that figure can itself be a poisoned journal total (written
    /// by the pre-fix fragment accounting and restored into the
    /// job), which branded byte-perfect merges as errors. It is
    /// judged by `completion_size_error` against the trustworthy
    /// baselines: the bytes this run actually downloaded
    /// (downloaded_total) and the probe's format-level size
    /// (format_size). The DURATION half is unchanged — a delivered
    /// file running a plainly different length is still an error,
    /// however its size adds up.
    fn finish_contradiction(
        &self,
        produced: &Path,
        produced_len: u64,
        components_dir: &Path,
    ) -> Option<String> {
        let format_size = *self.format_size.lock().unwrap();
        let parts_sum = *self.downloaded_total.lock().unwrap();
        if self.expect_size == 0
            && self.expect_duration <= 0.0
            && format_size.is_none()
            && parts_sum == 0
        {
            return None;
        }
        if self.audio {
            // audio jobs: size is never judged (R2-EXT-02), only
            // duration — exactly the old verdict's audio behaviour
            let produced_duration = if self.expect_duration > 0.0 {
                probe_duration_secs(produced, components_dir)
            } else {
                None
            };
            return contradiction_verdict(
                self.expect_size,
                self.expect_duration,
                produced_len,
                produced_duration,
                true,
            );
        }
        let produced_duration = if self.expect_duration > 0.0 {
            probe_duration_secs(produced, components_dir)
        } else {
            None
        };
        if let Some(msg) = duration_contradiction(self.expect_duration, produced_duration) {
            return Some(msg);
        }
        // sanity for the estimate-waiver: the file probes as the
        // media it should be (video present, sound present when
        // the source offered it) and its duration does not
        // contradict the page's (checked just above; an
        // unprobeable duration does not block the waiver — the
        // stream check is the byte-validity evidence)
        let duration_ok = self.expect_duration < 1.0
            || produced_duration
                .map(|d| d >= 0.5 && d >= self.expect_duration * 0.5 && d <= self.expect_duration * 2.0)
                .unwrap_or(true);
        let streams_ok = probe_streams(produced, components_dir)
            .map(|(v, a)| {
                v && (a || !*self.expect_audio.lock().unwrap())
            })
            .unwrap_or(false);
        completion_size_error(
            self.expect_size,
            format_size,
            *self.format_size_exact.lock().unwrap(),
            parts_sum,
            produced_len,
            streams_ok && duration_ok,
        )
    }

    /// The silent-video guard. A video download is only "finished" when
    /// the file on disk actually contains the sound the source offered:
    /// verify the merged file's streams, and when the sound lives in a
    /// separate HLS rendition, download it and mux it in. Returns the
    /// final path (possibly the muxed replacement) or a user-readable
    /// error — a silent file must never pass as a success.
    fn ensure_audio(
        &self,
        final_path: &Path,
        components_dir: &Path,
    ) -> Result<PathBuf, String> {
        if self.audio {
            return Ok(final_path.to_path_buf());
        }
        let expect = *self.expect_audio.lock().unwrap();
        let pair = self.pair_audio_url.lock().unwrap().clone();
        let (has_video, has_audio) = match probe_streams(final_path, components_dir) {
            Some(pair) => pair,
            None => {
                if expect || pair.is_some() {
                    return Err("the download completed but its sound could not be verified (the stream check failed) — the file may be silent; please retry".to_string());
                }
                return Ok(final_path.to_path_buf());
            }
        };
        if !has_video {
            return Err("the downloaded file has no playable video stream — the source appears to be broken".to_string());
        }
        if has_audio {
            return Ok(final_path.to_path_buf());
        }
        // Video is here, sound is missing.
        if !expect && pair.is_none() {
            return Ok(final_path.to_path_buf()); // the source itself is silent
        }
        let pair_url = pair.ok_or_else(|| {
            "this video was expected to have sound, but no audio stream could be found for it — the file would be silent".to_string()
        })?;
        let ffmpeg = find_tool("ffmpeg", components_dir)
            .ok_or("ffmpeg is not available, so the separate audio track cannot be joined")?;
        let ytdlp = find_tool("yt-dlp", components_dir)
            .ok_or("yt-dlp is not available, so the separate audio track cannot be fetched")?;

        // 1. Fetch the audio rendition as its own small download.
        let stem = final_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "video".to_string());
        let audio_tmpl = self.dir.join(format!(".{stem}.ndm_audio.%(ext)s"));
        // Clear any stale audio left by an earlier failed attempt:
        // the fetch uses --no-overwrites with this fixed template and
        // the result scan below ignores pre-existing names, so a
        // leftover would strand the retry with "could not be
        // downloaded" even though a usable file is sitting there.
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            let stale_prefix = format!(".{stem}.ndm_audio.");
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&stale_prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let before = dir_snapshot(&self.dir);
        let mut args = base_args_for_url(&pair_url, components_dir, self.cookies.as_deref());
        if !self.login.is_empty() {
            args.push("--username".into());
            args.push(self.login.clone());
            args.push("--password".into());
            args.push(self.password.clone());
        }
        args.push("-f".into());
        args.push("bestaudio/best".into());
        args.push("--ffmpeg-location".into());
        args.push(
            ffmpeg
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        args.push("--no-overwrites".into());
        args.push("-o".into());
        args.push(audio_tmpl.display().to_string());
        args.push("--".into());
        args.push(pair_url.clone());
        let fetched = tool_cmd(&ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let audio_file = if fetched {
            // whatever new file the rendition download produced
            let prefix = format!(".{stem}.ndm_audio.");
            let mut found: Option<PathBuf> = None;
            if let Ok(rd) = std::fs::read_dir(&self.dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with(&prefix) && !name.ends_with(".part") {
                        if !before.contains_key(&name) {
                            found = Some(e.path());
                            break;
                        }
                    }
                }
            }
            found
        } else {
            None
        };
        let audio_file = audio_file.ok_or_else(|| {
            "the separate audio track could not be downloaded, so the video would be silent — please retry".to_string()
        })?;

        // 2. Mux video + audio into a temp file and verify it.
        let mux_path = self.dir.join(format!(".{stem}.ndm_mux.mp4"));
        let mux_ok = tool_cmd(&ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-i",
            ])
            .arg(final_path)
            .arg("-i")
            .arg(&audio_file)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
            ])
            .arg(&mux_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && mux_path.is_file();
        let _ = std::fs::remove_file(&audio_file);
        if !mux_ok {
            let _ = std::fs::remove_file(&mux_path);
            return Err("joining the separate audio track into the video failed — the file would be silent; please retry".to_string());
        }
        match probe_streams(&mux_path, components_dir) {
            Some((true, true)) => {}
            _ => {
                let _ = std::fs::remove_file(&mux_path);
                return Err("the joined file still has no sound — refusing to call it finished".to_string());
            }
        }

        // 3. Replace the silent file with the muxed one. One rename
        // over the destination is atomic where the platform allows
        // it; deleting first would leave a crash window in which NO
        // file exists at the final path (only the hidden mux temp,
        // which nothing would ever pick up again).
        #[cfg(windows)]
        let _ = std::fs::remove_file(final_path);
        std::fs::rename(&mux_path, final_path).map_err(|e| {
            format!("the sound was joined but the file could not be saved in place: {e}")
        })?;
        Ok(final_path.to_path_buf())
    }

    /// Finalize a merged file that already exists on disk — this
    /// job's own kept merge (a pause landed during the merge) or
    /// an adopted completed final from a previous session (M-4).
    /// Verifies the sound, runs the finish-line guards and
    /// publishes the verdict. Returns true when this file settles
    /// the job's outcome (finished, error, or a stop that landed
    /// during the check). With `publish_error == false` (an
    /// adopted candidate — a file this run did not produce), a
    /// guard rejection instead returns false and publishes
    /// nothing: the caller falls through to a normal download,
    /// because a wrong candidate must not become this entry's
    /// error and the download the user asked for should still run.
    fn finalize_kept_merge(
        self: &Arc<Self>,
        merged: &Path,
        components_dir: &Path,
        publish_error: bool,
    ) -> bool {
        // the recorded total here IS the kept merge's own size
        // (set when the pause landed), so it is no baseline at
        // all; the guard judges the file against the selected
        // streams' media payload learned when the producing run
        // resolved them — unknown (e.g. the merge came from a run
        // whose resolution reported no bitrates) means the guard
        // skips this finalize (R3-EXT-01). For the finish line's
        // parts sum, an on-disk merge stands in as its own parts:
        // it is the whole download, already assembled
        let downloaded = self.progress.lock().unwrap().total;
        if let Ok(m) = merged.metadata() {
            *self.downloaded_total.lock().unwrap() = m.len();
        }
        let expected_media = *self.expected_media.lock().unwrap();
        // the audio check below is part of the finalize window:
        // report it as "merging", not "starting"
        self.progress.lock().unwrap().status = "merging".to_string();
        let audio_result = self.ensure_audio(merged, components_dir);
        // a stop that landed DURING the audio check wins over the
        // kept merge's verdict, exactly as in the fresh-merge
        // finalize (R2-EXT-05)
        if let Some(flag) = self.stop_flag.lock().unwrap().clone() {
            if flag == "cancelled" {
                // the kept file stays on disk (it predates this
                // run); only the status is the job's to publish
                let mut p = self.progress.lock().unwrap();
                p.status = "cancelled".to_string();
                p.speed_bps = 0.0;
                p.conns = 0;
                return true;
            }
            if flag == "paused" {
                *self.merged_final.lock().unwrap() = Some(merged.to_path_buf());
                let mut p = self.progress.lock().unwrap();
                p.status = "paused".to_string();
                p.speed_bps = 0.0;
                p.conns = 0;
                p.final_path = Some(merged.to_path_buf());
                return true;
            }
        }
        let mut published_finish = false;
        match audio_result {
            Ok(verified) => {
                let flen = verified.metadata().map(|m| m.len()).unwrap_or(0);
                // audio jobs exempt (R2-EXT-02), as in the
                // fresh-merge finalize
                let shrink = if flen > 0 && !self.audio {
                    drastic_shrink(expected_media, downloaded, flen)
                } else {
                    None
                };
                // W-2: same add-time contradiction check as the
                // fresh-merge finalize
                let verdict = match shrink {
                    Some(msg) => Some(msg),
                    None => self.finish_contradiction(&verified, flen, components_dir),
                };
                if let Some(msg) = verdict {
                    if !publish_error {
                        return false;
                    }
                    let mut p = self.progress.lock().unwrap();
                    p.status = "error".to_string();
                    p.error = Some(msg);
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(verified);
                } else {
                    let mut p = self.progress.lock().unwrap();
                    if flen > 0 {
                        p.total = flen;
                        p.done = flen;
                    }
                    p.status = "finished".to_string();
                    p.speed_bps = 0.0;
                    p.conns = 0;
                    p.final_path = Some(verified);
                    published_finish = true;
                }
            }
            Err(msg) => {
                if !publish_error {
                    return false;
                }
                let mut p = self.progress.lock().unwrap();
                p.status = "error".to_string();
                p.error = Some(msg);
                p.speed_bps = 0.0;
                p.conns = 0;
                p.final_path = Some(merged.to_path_buf());
            }
        }
        if published_finish {
            // a finished job leaves no working files behind,
            // whichever engine produced them (R4 side
            // observation). Sweep by the MERGED file's stem: for
            // an adopted merge that is the stem the producing run
            // worked under, which can differ from this job's
            // (uniquified) chosen stem
            if let Some(stem) = merged
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
            {
                if !stem.is_empty() {
                    sweep_job_temps(&self.dir, &stem);
                }
            }
        }
        true
    }

    fn spawn(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
        // A captured job invokes yt-dlp only for the WS-A
        // auth-death re-resolve of its page — never for its
        // initial streams and never as the download fallback —
        // so a missing yt-dlp must not refuse it: an empty path
        // stands in, and a heal that needs it simply fails soft
        // into the capture-expired error.
        let ytdlp = match find_tool("yt-dlp", components_dir) {
            Some(p) => p,
            None if self.preresolved.is_some() => PathBuf::new(),
            None => {
                return Err(
                    "yt-dlp not found — the video component is not installed yet".to_string(),
                )
            }
        };
        let ffmpeg = find_tool("ffmpeg", components_dir).ok_or(
            "ffmpeg is not available on this system — it joins the video and audio into one playable file. Install ffmpeg with your system package manager (Debian/Ubuntu: sudo apt install ffmpeg), or use an NDM build that bundles it, then try again".to_string(),
        )?;
        let sel = if self.selector.trim().is_empty() {
            ndm::video_probe::best_selector()
        } else {
            self.selector.clone()
        };

        let _spawn_guard = self.spawn_lock.lock().unwrap();
        {
            let mut slot = self.child.lock().unwrap();
            if let Some(c) = slot.as_mut() {
                if matches!(c.try_wait(), Ok(None)) {
                    return Err("this video is already downloading".to_string());
                }
            }
        }
        *self.stop_flag.lock().unwrap() = None;
        let my_gen = self
            .gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        {
            let mut p = self.progress.lock().unwrap();
            // A2: a fresh attempt starts as "starting" — it stays
            // that way until bytes actually move (the engine poll
            // loop / the yt-dlp progress handler promote it to
            // "downloading"), so the resolve-and-connect window no
            // longer masquerades as a running download
            p.status = "starting".to_string();
            // a new attempt re-earns its total lock: the parts
            // re-probe, and until they have all reported, `total`
            // keeps the previous attempt's value as the hint
            p.total_locked = false;
            // a (re)start supersedes the previous attempt's
            // failure: the stored error clears NOW, so progress
            // payloads never carry a stale error alongside a live
            // "downloading" state (the browser panel showed the old
            // error text through whole retry cycles)
            p.error = None;
            p.conns = ndm::global_connections().clamp(1, 32);
            p.last_progress_at = Instant::now();
        }
        // claimed synchronously, before the thread spawns, so a
        // payload sampled in the gap already sees the live run (A3)
        self.alive
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let job = self.clone();
        let comp_dir = components_dir.to_path_buf();
        std::thread::spawn(move || {
            // clears `alive` on every exit path of this thread (A3)
            let _alive_guard = AliveGuard(job.clone());
            // 0. A previous run merged the file but was stopped
            // before it could be finalized: verify and publish
            // that merge instead of downloading everything again.
            // Two sources: this job's own kept merge (in memory —
            // a pause landed during the merge), and a completed
            // final a previous SESSION left on disk (M-4: the app
            // quit during the merge, the orphaned ffmpeg finished
            // the file, and no finalize ever ran). The second
            // arrives via `recorded_final` (the journal's recorded
            // path, or a complete file found at the expected final
            // path when this job was built) and is adopted only
            // when its size is consistent with the expectation —
            // and even then only if the finalize guards accept
            // it; a rejected candidate falls through to a normal
            // download, untouched.
            let kept_merge = job.merged_final.lock().unwrap().take();
            if let Some(merged) = kept_merge {
                if merged.is_file() {
                    job.finalize_kept_merge(&merged, &comp_dir, true);
                    return;
                }
                // the kept file vanished meanwhile: fall through
                // and download normally
            }
            if let Some(recorded) = job.recorded_final.clone() {
                let size_ok = std::fs::metadata(&recorded)
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| adoptable_final(m.len(), job.expect_size))
                    .unwrap_or(false);
                if size_ok && job.finalize_kept_merge(&recorded, &comp_dir, false) {
                    return;
                }
                // not adoptable, or the guards rejected it:
                // download normally (the fresh attempt starts at
                // "starting" again, as after any fallback)
                job.progress.lock().unwrap().status = "starting".to_string();
            }
            // 1. Try parallel multi-connection Rust engine download first
            if job.run_parallel_engine(&ytdlp, &ffmpeg, &comp_dir, &sel, my_gen) {
                return;
            }
            if job.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen
                || job.stop_flag.lock().unwrap().is_some()
            {
                return;
            }
            if job.preresolved.is_some() {
                // A captured job has NO yt-dlp fallback: its
                // selector names a capture group, and running
                // yt-dlp with it would only manufacture a raw
                // format error. The engine refused the streams —
                // in practice their signed links have died — so
                // the entry fails with the plain-language message
                // the design promises for exactly this case. The
                // part files stay on disk (the engine path kept
                // them), so a fresh capture can still resume.
                let mut p = job.progress.lock().unwrap();
                p.status = "error".to_string();
                p.error = Some(crate::capture::EXPIRED_MESSAGE.to_string());
                p.speed_bps = 0.0;
                p.conns = 0;
                return;
            }
            // the fallback is a fresh attempt: whatever phase the
            // engine reached, this one starts at "starting" again
            // and earns "downloading" from its first bytes (A2)
            job.progress.lock().unwrap().status = "starting".to_string();

            // 2. Fallback to yt-dlp with concurrent fragments for HLS/DASH streams
            let mut args = base_args_for_url(&job.url, &comp_dir, job.cookies.as_deref());
            if !job.login.is_empty() {
                args.push("--username".into());
                args.push(job.login.clone());
                args.push("--password".into());
                args.push(job.password.clone());
            }
            args.push("-f".into());
            args.push(sel.clone());
            if job.audio {
                args.push("-x".into());
                args.push("--audio-format".into());
                args.push("mp3".into());
                args.push("--audio-quality".into());
                args.push("0".into());
            } else {
                args.push("--merge-output-format".into());
                args.push("mp4".into());
                args.push("--postprocessor-args".into());
                args.push("Merger+ffmpeg_o1:-c:v copy -c:a aac -b:a 192k".into());
            }
            let lim = job.limit();
            if lim > 0 {
                args.push("--limit-rate".into());
                args.push(lim.to_string());
            }
            let frags = ndm::global_connections().clamp(1, 32);
            args.push("--concurrent-fragments".into());
            args.push(frags.to_string());
            args.push("--buffer-size".into());
            args.push("512K".into());
            args.push("--http-chunk-size".into());
            args.push("10M".into());
            args.push("--hls-prefer-native".into());
            args.push("--no-overwrites".into());

            let limit = ndm::global_limit();
            if lim == 0 && limit > 0 {
                // the per-download limit above takes precedence:
                // yt-dlp honours the LAST --limit-rate it is given,
                // so pushing the global one too would silently
                // override this download's own limiter
                args.push("--limit-rate".into());
                args.push(format!("{}K", (limit / 1024).max(1)));
            }
            if let Some(parent) = ffmpeg.parent() {
                args.push("--ffmpeg-location".into());
                args.push(parent.display().to_string());
            }
            args.push("--progress-template".into());
            args.push(
                // M-15: the template also carries the fragment
                // index/count, the report status and the file the
                // report belongs to — the accounting keys every
                // report by (file, fragment) and never sums
                // cumulative counters across reports (see
                // NdmProgTracker)
                "NDMPROG:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.fragment_index)s|%(progress.fragment_count)s|%(progress.status)s|%(progress.filename)s"
                    .into(),
            );
            let stem = job.resolve_or_lock_stem("");
            let out_tmpl = if stem.is_empty() {
                job.dir.join("%(title).150B [%(id)s].%(ext)s")
            } else {
                job.dir.join(format!("{stem}.%(ext)s"))
            };
            args.push("-o".into());
            args.push(out_tmpl.display().to_string());
            args.push("--".into());
            args.push(job.url.clone());

            // snapshot BEFORE the child starts: taken after the
            // spawn, a fast site could finish and write its final
            // file inside the snapshot window, and find_final_file
            // would then skip that file as "pre-existing" and report
            // a finished download as an error
            let before = dir_snapshot(&job.dir);
            let Ok(mut child) = tool_cmd(&ytdlp)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                let mut p = job.progress.lock().unwrap();
                p.status = "error".to_string();
                p.conns = 0;
                p.error = Some("could not start yt-dlp".to_string());
                return;
            };
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            let stderr = child.stderr.take();
            let my_pid = child.id();
            {
                let mut slot = job.child.lock().unwrap();
                if let Some(mut old) = slot.take() {
                    let _ = old.wait();
                }
                *slot = Some(child);
            }

            let err_buf = Arc::new(Mutex::new(String::new()));
            let err_thread = if let Some(mut se) = stderr {
                let eb = err_buf.clone();
                Some(std::thread::spawn(move || {
                    use std::io::Read as _;
                    let mut tail: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        match se.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                tail.extend_from_slice(&chunk[..n]);
                                if tail.len() > 65536 {
                                    let cut = tail.len() - 65536;
                                    tail.drain(..cut);
                                }
                            }
                        }
                    }
                    *eb.lock().unwrap() = String::from_utf8_lossy(&tail).to_string();
                }))
            } else {
                None
            };
            // M-15: per-(file, fragment) accounting for the whole
            // run. The format-level size learned at resolve time
            // (when the resolve ran) outranks every fragment
            // estimate as the run's total
            // V3-a (208B): seed the tracker ONCE, before any
            // progress line is processed — the resolve's format
            // size when it exists, else the job's known total (the
            // picked option's size, `expect_size`). The combined
            // job's progress template carries no explicit total, so
            // without this seed the denominator would have to be
            // invented from downloaded bytes — the ratchet this
            // batch exists to kill. The seeded figure stands for
            // the whole run; the finish line below corrects the
            // reported total from the tracker's parts sum (the
            // per-file exact totals yt-dlp reports as each file
            // finishes), and the Finished outcome then replaces it
            // with the merged file's true length
            let mut tracker = NdmProgTracker::default();
            {
                let fmt = *job.format_size.lock().unwrap();
                let fmt_exact = *job.format_size_exact.lock().unwrap();
                match fmt {
                    Some(_) => tracker.set_format_total(fmt, fmt_exact),
                    None if job.expect_size > 0 => {
                        tracker.set_format_total(Some(job.expect_size), false)
                    }
                    None => {}
                }
            }
            let mut detected_final: Option<PathBuf> = None;

            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(rest) = line.strip_prefix("NDMPROG:") {
                    tracker.apply(parse_ndmprog(rest));
                    let mut p = job.progress.lock().unwrap();
                    p.conns = frags;
                    let new_done = tracker.done();
                    if new_done != p.done {
                        p.last_progress_at = Instant::now();
                    }
                    p.done = new_done;
                    // A2: the first real bytes end "starting"
                    if p.done > 0 && p.status == "starting" {
                        p.status = "downloading".to_string();
                    }
                    // the tracker's total is the honest figure —
                    // per-file exact totals, the format-level size,
                    // or a fragment estimate that is allowed to
                    // come DOWN as it refines (the pre-fix parser
                    // only ever let the total grow, which locked
                    // the early concurrent-fragment estimate spike
                    // in for the whole download). V3-a (208B): the
                    // tracker never returns a downloaded-derived
                    // figure, and a 0 here (total still unknown at
                    // tracker level) leaves the job's seeded total
                    // untouched — the denominator never chases the
                    // numerator
                    let new_total = tracker.total();
                    if new_total > 0 {
                        p.total = new_total;
                    }
                    p.speed_bps = tracker.speed();
                } else {
                    if let Some(fp) = parse_output_path_line(&line) {
                        detected_final = Some(fp.clone());
                        // yt-dlp has named a file it is writing:
                        // the download proper has begun (A2)
                        let mut p = job.progress.lock().unwrap();
                        if p.status == "starting" {
                            p.status = "downloading".to_string();
                        }
                        // M-4: a [Merger] line names the merged
                        // output — record it as the final path the
                        // moment the merge starts, so the journal
                        // carries it through the merge window
                        if line.contains("[Merger]") && p.final_path.is_none() {
                            p.final_path = Some(fp);
                        }
                    }
                    if line.contains("[Merger]") || line.contains("[ExtractAudio]") {
                        // 208B (candidate e): this branch used to
                        // force `done = total` and claim "merging"
                        // on ANY merger line. yt-dlp prints a
                        // [Merger] line per output, and with
                        // several formats a later file can still be
                        // downloading — forcing done = total there
                        // manufactured the recording's false
                        // "Finishing…" window at a pinned 100%.
                        // Now the claim is scoped to what the
                        // tracker can prove: EVERY tracked file has
                        // reported finished. Until then the line is
                        // only a path sighting (handled above) and
                        // the status stays the transfer's own.
                        if tracker.all_finished() {
                            let mut p = job.progress.lock().unwrap();
                            if p.total > 0 {
                                p.done = p.total;
                            }
                            // A2: the merge / audio-extraction
                            // window — including the ensure_audio
                            // check that runs after this loop —
                            // reports as "merging"
                            p.status = "merging".to_string();
                            p.speed_bps = 0.0;
                        }
                    }
                }
            }
            if job.gen.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                return;
            }
            let taken = {
                let mut slot = job.child.lock().unwrap();
                match slot.as_mut() {
                    Some(c) if c.id() == my_pid => slot.take(),
                    _ => None,
                }
            };
            let status = match taken {
                Some(mut c) => c.wait().ok(),
                None => None,
            };
            // The stderr reader publishes its tail only at EOF; wait
            // for it, or the failure reason races us and is lost.
            if let Some(h) = err_thread {
                let _ = h.join();
            }
            let succeeded = status.map(|s| s.success()).unwrap_or(false);
            if succeeded && tracker.parts_sum() > 0 {
                // M-15: the finish line judges the merged file
                // against what this run actually downloaded — the
                // tracker's per-file final sizes — never against a
                // fragment estimate; the reported total settles at
                // the same true figure
                *job.downloaded_total.lock().unwrap() = tracker.parts_sum();
                job.progress.lock().unwrap().total = tracker.parts_sum();
            }
            // A pause/cancel that landed while yt-dlp was finishing
            // wins over the success report: the user stopped this job,
            // so its files stay partial and resumable.
            let stop = job.stop_flag.lock().unwrap().clone();
            // Resolve the outcome BEFORE taking the progress lock: the
            // audio guard shells out to ffmpeg/yt-dlp and can take a
            // few seconds on the pairing path.
            enum Outcome {
                Finished(PathBuf),
                GuardError(PathBuf, String),
                MergeMissing,
                Stopped(String),
                Failed,
            }
            let outcome = if let Some(flag) = stop {
                Outcome::Stopped(flag)
            } else if succeeded {
                let prev_final = job.progress.lock().unwrap().final_path.clone();
                let stem_hint = job
                    .chosen_stem
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default();
                let found = detected_final
                    .filter(|fp| fp.is_file())
                    .or_else(|| prev_final.filter(|fp| fp.is_file()))
                    .or_else(|| find_final_file(&job.dir, &before, job.audio, &stem_hint));
                match found {
                    Some(fp) => match job.ensure_audio(&fp, &comp_dir) {
                        Ok(verified) => {
                            // W-2: the produced file must also
                            // match what was probed at add time —
                            // a plain contradiction is reported
                            // through the guard-error outcome, not
                            // presented as a clean finish
                            let flen = verified.metadata().map(|m| m.len()).unwrap_or(0);
                            match job.finish_contradiction(&verified, flen, &comp_dir) {
                                Some(msg) => Outcome::GuardError(verified, msg),
                                None => Outcome::Finished(verified),
                            }
                        }
                        Err(msg) => Outcome::GuardError(fp, msg),
                    },
                    None => Outcome::MergeMissing,
                }
            } else {
                Outcome::Failed
            };
            // A stop that landed while the outcome was being
            // resolved (the audio check above can take seconds)
            // wins over a "finished" verdict: the stop flag sampled
            // before the resolution is stale by now, and a
            // cancelled job must never publish "finished"
            // (R2-EXT-05). A pause keeps the produced file as a
            // kept merge, so the resume finalizes it instead of
            // re-downloading.
            let outcome = match (job.stop_flag.lock().unwrap().clone(), outcome) {
                (Some(flag), Outcome::Finished(fp)) if flag == "cancelled" => {
                    let _ = std::fs::remove_file(&fp);
                    Outcome::Stopped(flag)
                }
                (Some(flag), Outcome::Finished(fp)) if flag == "paused" => {
                    *job.merged_final.lock().unwrap() = Some(fp);
                    Outcome::Stopped(flag)
                }
                (Some(flag), Outcome::GuardError(_, _)) if flag == "cancelled" => {
                    Outcome::Stopped(flag)
                }
                (_, o) => o,
            };
            let expected_media = *job.expected_media.lock().unwrap();
            let mut p = job.progress.lock().unwrap();
            p.conns = 0;
            let mut published_finish = false;
            match outcome {
                Outcome::Finished(fp) => {
                    let flen = fp.metadata().map(|m| m.len()).unwrap_or(0);
                    // F-EXT-04 / R3-EXT-01: the merged file is
                    // judged against the selected streams' media
                    // payload (learned at resolution time), NOT
                    // against p.total — what yt-dlp downloaded can
                    // far exceed the media inside a padded
                    // container, and a good merge must not be
                    // declared broken. Unknown payload → the guard
                    // skips. On a verdict the true total stays; it
                    // is not rewritten to the output size. Audio
                    // jobs are exempt: an extracted audio track is
                    // legitimately a small fraction of the
                    // downloaded source (R2-EXT-02).
                    let shrink = if flen > 0 && !job.audio {
                        drastic_shrink(expected_media, p.total, flen)
                    } else {
                        None
                    };
                    if let Some(msg) = shrink {
                        p.status = "error".to_string();
                        p.error = Some(msg);
                    } else {
                        p.status = "finished".to_string();
                        if flen > 0 {
                            // completion correction: a seeded /
                            // estimated total is corrected HERE, to
                            // the merged file's true length — never
                            // mid-transfer (208B total rule)
                            p.total = flen;
                            p.done = flen;
                        }
                        published_finish = true;
                    }
                    p.final_path = Some(fp);
                    p.speed_bps = 0.0;
                }
                Outcome::GuardError(fp, msg) => {
                    p.status = "error".to_string();
                    p.error = Some(msg);
                    p.final_path = Some(fp);
                    p.speed_bps = 0.0;
                }
                Outcome::MergeMissing => {
                    p.status = "error".to_string();
                    p.error = Some(
                        "the video and audio downloaded but could not be merged into one playable file — merging needs ffmpeg. Reinstall NDM with apt (ffmpeg is a package dependency), then try again"
                            .to_string(),
                    );
                    p.speed_bps = 0.0;
                }
                Outcome::Stopped(flag) => {
                    p.status = flag;
                    p.speed_bps = 0.0;
                }
                Outcome::Failed => {
                    p.status = "error".to_string();
                    p.speed_bps = 0.0;
                    let eb = err_buf.lock().unwrap();
                    // On a machine that cannot run a JavaScript
                    // runtime, a challenge-gated failure reads in
                    // plain language instead of as a raw yt-dlp
                    // line — checked against the whole stderr
                    // tail, where the JS wording may sit in a
                    // WARNING above the final ERROR.
                    let js_plain = if ndm::video_probe::no_js_runtime_mode() {
                        ndm::video_probe::js_required_error(&eb)
                    } else {
                        None
                    };
                    p.error = js_plain.or_else(|| {
                        eb.lines()
                            .rev()
                            .find(|l| l.contains("ERROR"))
                            .map(|l| l.trim().to_string())
                            .or_else(|| {
                                eb.lines()
                                    .rev()
                                    .find(|l| !l.trim().is_empty())
                                    .map(|l| l.trim().to_string())
                            })
                            .or_else(|| Some("yt-dlp failed".to_string()))
                    });
                }
            }
            drop(p);
            if published_finish {
                // a finished job leaves no working files behind —
                // including a previous attempt's parallel-engine
                // parts (R4 side observation)
                if let Some(stem) = job.chosen_stem.lock().unwrap().clone() {
                    sweep_job_temps(&job.dir, &stem);
                }
            }
        });
        Ok(())
    }

    /// Set this job's own speed limit (bytes/sec, 0 = none): live
    /// engine streams adopt it immediately; the yt-dlp fallback reads
    /// it when (re)spawned.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        self.limit_bps
            .store(bytes_per_sec, std::sync::atomic::Ordering::Relaxed);
        for d in self.active_downloads.lock().unwrap().iter() {
            d.set_limit(bytes_per_sec);
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit_bps.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn pause(&self) {
        let _spawn_guard = self.spawn_lock.lock().unwrap();
        let active = self.active_downloads.lock().unwrap().clone();
        if !active.is_empty() {
            for d in &active {
                d.control.pause();
            }
            let mut p = self.progress.lock().unwrap();
            p.status = "paused".to_string();
            p.speed_bps = 0.0;
            p.conns = 0;
            return;
        }
        *self.stop_flag.lock().unwrap() = Some("paused".to_string());
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        let mut p = self.progress.lock().unwrap();
        // "starting" included (A2): a pause that lands while the
        // job is still resolving has no downloads and no child to
        // observe the stop — without the flip here the entry would
        // sit in "starting" after its thread has gone. "resuming"
        // (2.3.1 stall recovery) is the same shape: the pause can
        // land in the gap between attempts, when no stream is
        // active either. ("merging" is deliberately absent: the
        // merge finalize sees the stop flag itself and publishes
        // the pause with its kept merge.)
        if p.status == "downloading" || p.status == "starting" || p.status == "resuming" {
            p.status = "paused".to_string();
            p.speed_bps = 0.0;
            p.conns = 0;
        }
    }

    pub fn cancel(&self) {
        let _spawn_guard = lock_ok(&self.spawn_lock);
        // A finished job has nothing to cancel — and overwriting
        // its status here would let a stop that lost the finish
        // race flip a completed download to "cancelled", after
        // which a discard could sweep it as if it had never
        // finished (R2-EXT-05, the F-EXT-12 rule).
        if lock_ok(&self.progress).status == "finished" {
            return;
        }
        *lock_ok(&self.stop_flag) = Some("cancelled".to_string());
        for d in lock_ok(&self.active_downloads).iter() {
            d.control.cancel();
        }
        if let Some(c) = lock_ok(&self.child).as_mut() {
            let _ = c.kill();
        }
        let mut p = lock_ok(&self.progress);
        p.status = "cancelled".to_string();
        p.conns = 0;
    }

    /// Is a run of this job in flight right now (its spawn thread
    /// is alive in any phase)? payload_of uses it for the A3 rule:
    /// the restored-session byte hint may stand in for progress
    /// only while NO live job owns the entry's numbers.
    pub fn is_live(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Has any run of this job ever been spawned? payload_of's
    /// restored-session hint is legitimate only before the first
    /// run: once a run has happened, a not-live job reporting
    /// "downloading" is a wedged state, and standing the PREVIOUS
    /// session's byte count in as its progress presents old bytes
    /// as fresh movement (2.0.7 #7).
    pub fn has_run(&self) -> bool {
        self.gen.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// True once nothing of this job is still running: the yt-dlp
    /// child has exited and every engine stream is SETTLED —
    /// terminal with its final resume-state save on disk (a merely
    /// terminal stream can still be between the supervisor's
    /// status settle and its last save, and sweeping in that
    /// window is how the stream journal survived deletes, R4
    /// item 2). The discard flow waits for this before sweeping
    /// the job's files, the way the HTTP path waits for its
    /// engine (F-EXT-08).
    pub fn is_settled(&self) -> bool {
        // #18: the spawn thread itself is part of "running" — an
        // ffmpeg merge executes as a blocking call ON that thread
        // (not as the tracked child), and cancel() flips the
        // status to "cancelled" the moment it is pressed, so a
        // settle that ignored `alive` declared victory ~50 ms
        // into a delete while ffmpeg was still writing the final
        // file (and on Windows the "deleted" file then reappeared
        // when the merge finished recreating it).
        if self.is_live() {
            return false;
        }
        let child_done = lock_ok(&self.child)
            .as_mut()
            .map(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)))
            .unwrap_or(true);
        if !child_done {
            return false;
        }
        lock_ok(&self.active_downloads)
            .iter()
            .all(|d| d.is_settled())
    }

    pub fn resume(self: &Arc<Self>, components_dir: &Path) -> Result<(), String> {
        {
            let active = self.active_downloads.lock().unwrap().clone();
            if !active.is_empty() {
                *self.stop_flag.lock().unwrap() = None;
                for d in &active {
                    d.control.resume();
                }
                // same stale-error rule as spawn(): a resumed job
                // is a new attempt, its old error does not ride
                // along in the progress payloads
                let mut p = self.progress.lock().unwrap();
                p.status = "downloading".to_string();
                p.error = None;
                return Ok(());
            }
        }
        let running = self
            .child
            .lock()
            .unwrap()
            .as_mut()
            .map(|c| c.try_wait().map(|s| s.is_none()).unwrap_or(false))
            .unwrap_or(false);
        if running {
            return Ok(());
        }
        self.spawn(components_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(url: &str, sel: &str) -> ResolutionKey {
        ResolutionKey {
            url: url.to_string(),
            sel: sel.to_string(),
            audio: false,
            login: String::new(),
            password_sig: 0,
            cookies_sig: 0,
            proxy: String::new(),
        }
    }

    fn test_resolution() -> Resolution {
        Resolution {
            title: "t".to_string(),
            streams: vec![DirectStream {
                url: "https://cdn.example/v.mp4".to_string(),
                ext: "mp4".to_string(),
                has_audio: true,
                cookie: None,
                referer: None,
                user_agent: None,
            }],
            expect_audio: true,
            expected_media: Some(1234),
            pair_audio_url: None,
            format_size: Some(5678),
            format_size_exact: true,
        }
    }

    #[test]
    fn resolution_cache_hit_miss_and_key_strictness() {
        // unique URLs per test: the cache is process-global and
        // cargo runs tests on parallel threads
        let now = Instant::now();
        let key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        assert!(resolution_cache_get(&key, now).is_none());
        resolution_cache_store(key, test_resolution(), now);
        let hit = resolution_cache_get(
            &test_key("https://example.com/watch?v=cache1", "bv*+ba"),
            now,
        )
        .expect("same key must hit");
        assert_eq!(hit.streams.len(), 1);
        assert_eq!(hit.expected_media, Some(1234));
        // any key component differing must miss: another video, another
        // selector, the audio variant, another login, other cookies
        assert!(
            resolution_cache_get(&test_key("https://example.com/watch?v=other", "bv*+ba"), now)
                .is_none()
        );
        assert!(
            resolution_cache_get(&test_key("https://example.com/watch?v=cache1", "bv*[height<=720]+ba"), now)
                .is_none()
        );
        let mut audio_key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        audio_key.audio = true;
        assert!(resolution_cache_get(&audio_key, now).is_none());
        let mut login_key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        login_key.login = "someone".to_string();
        assert!(resolution_cache_get(&login_key, now).is_none());
        let mut cookie_key = test_key("https://example.com/watch?v=cache1", "bv*+ba");
        cookie_key.cookies_sig = 42;
        assert!(resolution_cache_get(&cookie_key, now).is_none());
    }

    #[test]
    fn resolution_cache_expires_after_ttl() {
        let key = test_key("https://example.com/watch?v=cache2", "bv*+ba");
        let stored_at = Instant::now();
        resolution_cache_store(key, test_resolution(), stored_at);
        assert!(
            resolution_cache_get(
                &test_key("https://example.com/watch?v=cache2", "bv*+ba"),
                stored_at
            )
            .is_some(),
            "a fresh entry must be served"
        );
        assert!(
            resolution_cache_get(
                &test_key("https://example.com/watch?v=cache2", "bv*+ba"),
                stored_at + RESOLUTION_TTL + std::time::Duration::from_secs(1)
            )
            .is_none(),
            "an entry older than the TTL must never be served"
        );
    }

    fn picker_option(
        selector: &str,
        label: &str,
        audio: bool,
        height: u64,
    ) -> ndm::video_probe::QualityOption {
        ndm::video_probe::QualityOption {
            selector: selector.to_string(),
            label: label.to_string(),
            ext: "MP4".to_string(),
            note: String::new(),
            size_bytes: 0,
            size_approx: false,
            size_text: String::new(),
            audio,
            height,
            source: String::new(),
        }
    }

    /// Change 5 (2.0.4): the picker seeding warms the preselected
    /// row (mirroring the picker's own preselect rules) plus the
    /// best row — never the whole list.
    #[test]
    fn picker_warm_indices_follow_the_picker_preselect() {
        let options = vec![
            picker_option("SEL720", "720p", false, 720),
            picker_option("SEL360", "360p", false, 360),
            picker_option("SELA", "Audio Only (M4A)", true, 0),
        ];
        assert_eq!(picker_warm_indices(&options, "best"), vec![0]);
        assert_eq!(picker_warm_indices(&options, "720"), vec![0]);
        assert_eq!(picker_warm_indices(&options, "360"), vec![1, 0]);
        assert_eq!(picker_warm_indices(&options, "audio"), vec![2, 0]);
        // a preference with no matching row falls back to the best row
        assert_eq!(picker_warm_indices(&options, "2160"), vec![0]);
        assert!(picker_warm_indices(&[], "best").is_empty());
    }

    /// Change 5 (2.0.4): with a counting stub yt-dlp, seeding the
    /// picker runs exactly ONE resolve (the preselected row); the
    /// Download click's key then hits the cache with NO further
    /// yt-dlp run; an unwarmed row stays uncached; and once the
    /// entry ages past the TTL the resolve runs again.
    #[cfg(unix)]
    #[test]
    fn picker_seed_warms_cache_so_the_click_resolves_nothing() {
        let dir =
            std::env::temp_dir().join(format!("ndm-seed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let count_file = dir.join("count.txt");
        let json = "{\"title\":\"Stub Video\",\"duration\":100,\"requested_formats\":[{\"format_id\":\"136\",\"url\":\"https://cdn.example/v720.mp4\",\"ext\":\"mp4\",\"protocol\":\"https\",\"vcodec\":\"avc1.640028\",\"acodec\":\"none\",\"tbr\":1000},{\"format_id\":\"140\",\"url\":\"https://cdn.example/a.m4a\",\"ext\":\"m4a\",\"protocol\":\"https\",\"vcodec\":\"none\",\"acodec\":\"mp4a.40.2\",\"tbr\":128}]}";
        let script = format!(
            "#!/bin/sh\necho x >> \"{}\"\nprintf '%s\\n' '{}'\n",
            count_file.display(),
            json
        );
        let stub = dir.join("yt-dlp");
        std::fs::write(&stub, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runs = || -> usize {
            std::fs::read_to_string(&count_file)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        let url = "https://picker-seed.example/watch?v=seed1";
        let options = vec![
            picker_option("SEL720", "720p", false, 720),
            picker_option("SEL360", "360p", false, 360),
            picker_option("SELA", "Audio Only (M4A)", true, 0),
        ];
        // the key exactly as the Download click's job will build it
        let job_key = |sel: &str| ResolutionKey {
            url: url.to_string(),
            sel: sel.to_string(),
            audio: false,
            login: String::new(),
            password_sig: password_signature(""),
            cookies_sig: cookies_signature(None),
            proxy: ndm::global_proxy(),
        };
        seed_picker_resolutions(url, &options, "best", None, "", "", &dir);
        assert_eq!(runs(), 1, "seeding resolves only the preselected row");
        // the Download click's own key: fresh hit, no second run
        let click_key = job_key("SEL720");
        let hit = resolution_cache_get(&click_key, Instant::now())
            .expect("the click must find the seeded resolution");
        assert_eq!(hit.streams.len(), 2);
        assert_eq!(runs(), 1, "the click itself runs no yt-dlp");
        // an unwarmed row was NOT seeded
        assert!(resolution_cache_get(&job_key("SEL360"), Instant::now()).is_none());
        // after the TTL the entry is dead and a resolve runs again
        assert!(resolution_cache_get(
            &click_key,
            Instant::now() + RESOLUTION_TTL + std::time::Duration::from_secs(1),
        )
        .is_none());
        let probe = resolve_streams_core(url, "SEL720", false, None, "", "", &stub, &dir)
            .expect("re-resolve after expiry must succeed");
        assert!(probe.resolved.is_some());
        assert_eq!(runs(), 2, "an expired entry re-resolves exactly once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_format_probe_recognises_only_the_derivable_case() {
        // a direct media file: the media fields sit on the result
        // object itself, no formats array — exactly one format
        let direct: serde_json::Value = serde_json::json!({
            "id": "clip", "title": "clip",
            "url": "http://127.0.0.1/clip.mp4", "ext": "mp4", "duration": 12,
        });
        assert!(single_format_probe(&direct));
        // a formats array with exactly one real format
        let one: serde_json::Value = serde_json::json!({
            "id": "x", "title": "x",
            "formats": [{ "format_id": "0", "url": "http://h/v.mp4", "ext": "mp4", "protocol": "https" }],
        });
        assert!(single_format_probe(&one));
        // two formats: a selector's choice is no longer knowable
        // without yt-dlp — never derivable
        let two: serde_json::Value = serde_json::json!({
            "id": "x", "title": "x",
            "formats": [
                { "format_id": "0", "url": "http://h/v.mp4", "ext": "mp4", "protocol": "https" },
                { "format_id": "1", "url": "http://h/a.m4a", "ext": "m4a", "protocol": "https" }
            ],
        });
        assert!(!single_format_probe(&two));
        // a single PLAYLIST format is not a direct stream either
        let hls: serde_json::Value = serde_json::json!({
            "id": "x", "title": "x",
            "formats": [{ "format_id": "0", "url": "http://h/v.m3u8", "ext": "mp4", "protocol": "m3u8_native" }],
        });
        assert!(!single_format_probe(&hls));
        // a lone storyboard is not the page's video
        let board: serde_json::Value = serde_json::json!({
            "id": "x", "title": "x",
            "formats": [{ "format_id": "sb0", "url": "http://h/s.jpg", "ext": "mhtml", "protocol": "https" }],
        });
        assert!(!single_format_probe(&board));
    }

    /// WS-B derivation, end to end with a counting stub: the
    /// picker probe runs yt-dlp once; the Download click's
    /// resolve for a single-format page then runs it ZERO more
    /// times — the streams come from the probe's cached dump.
    #[cfg(unix)]
    #[test]
    fn click_resolve_derives_from_the_picker_probe_without_a_second_run() {
        let dir =
            std::env::temp_dir().join(format!("ndm-derive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let count_file = dir.join("count.txt");
        // the generic extractor's shape for a bare .mp4: media
        // fields on the result object, NO formats array — and the
        // same body answers a -j resolve, so a run that DID happen
        // would still succeed (and be counted)
        let json = "{\"id\":\"clip\",\"title\":\"Clip\",\"url\":\"https://cdn.example/clip.mp4\",\"ext\":\"mp4\",\"protocol\":\"https\",\"vcodec\":\"avc1.640028\",\"acodec\":\"mp4a.40.2\",\"tbr\":800,\"duration\":12}";
        let script = format!(
            "#!/bin/sh\necho x >> \"{}\"\nprintf '%s\\n' '{}'\n",
            count_file.display(),
            json
        );
        let stub = dir.join("yt-dlp");
        std::fs::write(&stub, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runs = || -> usize {
            std::fs::read_to_string(&count_file)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        let url = "https://derive.example/direct/clip.mp4";
        // the picker's Fetch Info: one run, and it fills the probe cache
        let info = ndm::video_probe::probe(url, &dir, None).expect("picker probe");
        assert_eq!(info.title, "Clip");
        assert_eq!(runs(), 1);
        // the click's resolve: derived, no second spawn
        let probe = resolve_streams_core(
            url,
            &ndm::video_probe::best_selector(),
            false,
            None,
            "",
            "",
            &stub,
            &dir,
        )
        .expect("the resolve must be derived from the probe dump");
        let (title, streams) = probe.resolved.expect("single format must resolve");
        assert_eq!(title, "Clip");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url, "https://cdn.example/clip.mp4");
        assert!(streams[0].has_audio);
        assert_eq!(runs(), 1, "derivation must not spawn yt-dlp again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WS-B in-flight dedup: two callers resolving the same key
    /// at the same moment share ONE produce run — the second is
    /// served the first's result from the resolution cache.
    #[test]
    fn concurrent_resolves_of_one_key_run_the_resolve_once() {
        use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
        let key = test_key("https://example.com/watch?v=dedup1", "bv*+ba");
        let calls = Arc::new(AtomicUsize::new(0));
        let mk = |calls: Arc<AtomicUsize>| {
            move || {
                calls.fetch_add(1, AOrd::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(300));
                Some(ResolutionProbe {
                    expect_audio: true,
                    pair_audio_url: None,
                    expected_media: None,
                    format_size: None,
                    format_size_exact: false,
                    resolved: Some((
                        "t".to_string(),
                        vec![DirectStream {
                            url: "https://cdn.example/v.mp4".to_string(),
                            ext: "mp4".to_string(),
                            has_audio: true,
                            cookie: None,
                            referer: None,
                            user_agent: None,
                        }],
                    )),
                })
            }
        };
        let (c1, c2) = (calls.clone(), calls.clone());
        let k1 = key.clone();
        let t1 = std::thread::spawn(move || resolve_streams_once(&k1, mk(c1)));
        // make sure thread 1 holds the claim before thread 2 calls
        std::thread::sleep(std::time::Duration::from_millis(60));
        let r2 = resolve_streams_once(&key, mk(c2));
        let r1 = t1.join().unwrap();
        assert_eq!(
            calls.load(AOrd::SeqCst),
            1,
            "two concurrent resolves of one key must share one run"
        );
        assert!(r1.and_then(|p| p.resolved).is_some());
        let (title, streams) = r2
            .and_then(|p| p.resolved)
            .expect("the waiter must be served the shared result");
        assert_eq!(title, "t");
        assert_eq!(streams.len(), 1);
    }

    #[test]
    fn cookies_signature_distinguishes_content() {
        let dir = std::env::temp_dir().join(format!("ndm-cksig-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"session=one").unwrap();
        std::fs::write(&b, b"session=two").unwrap();
        assert_eq!(cookies_signature(Some(&a)), cookies_signature(Some(&a)));
        assert_ne!(cookies_signature(Some(&a)), cookies_signature(Some(&b)));
        assert_ne!(cookies_signature(None), cookies_signature(Some(&a)));
        assert_ne!(
            cookies_signature(Some(&dir.join("missing.txt"))),
            cookies_signature(Some(&a))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cookies_signature_ignores_export_order() {
        // 2.3.1 (bug 1): the picker's warm and the job's resolve
        // snapshot the browser's cookies in two separate exports
        // whose line order is not guaranteed. The signature must
        // be over the cookie SET, or the warmed resolution is
        // invisible to the job and Download re-probes from zero.
        let dir = std::env::temp_dir().join(format!("ndm-cksig2-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let l1 = ".youtube.com\tTRUE\t/\tTRUE\t1893456000\tSID\talpha";
        let l2 = ".youtube.com\tTRUE\t/\tTRUE\t1893456000\tHSID\tbeta";
        let l3 = "#HttpOnly_.google.com\tTRUE\t/\tTRUE\t1893456000\tNID\tgamma";
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        std::fs::write(&a, format!("# Netscape HTTP Cookie File\n{l1}\n{l2}\n{l3}\n")).unwrap();
        std::fs::write(&b, format!("{l3}\n{l1}\n{l2}\n")).unwrap();
        // same set, one value changed → a different login state
        std::fs::write(&c, format!("{l1}\n{l2}\n")).unwrap();
        assert_eq!(
            cookies_signature(Some(&a)),
            cookies_signature(Some(&b)),
            "same cookie set in another order must sign the same"
        );
        assert_ne!(cookies_signature(Some(&a)), cookies_signature(Some(&c)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warmed_resolution_is_visible_to_the_job_key() {
        // 2.3.1 (bug 1), the picker→job bridge end to end at the
        // cache layer: a resolution stored under the key the
        // WARM path computes must be found by the key the JOB
        // path computes when both sides sign the same cookie set
        // exported in different orders.
        let dir = std::env::temp_dir().join(format!("ndm-warmkey-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("warm.txt");
        let b = dir.join("job.txt");
        std::fs::write(&a, ".x.com\tTRUE\t/\tTRUE\t1893456000\tA\t1\n.x.com\tTRUE\t/\tTRUE\t1893456000\tB\t2\n").unwrap();
        std::fs::write(&b, ".x.com\tTRUE\t/\tTRUE\t1893456000\tB\t2\n.x.com\tTRUE\t/\tTRUE\t1893456000\tA\t1\n").unwrap();
        let url = format!("https://example.com/watch?v=warm{}", std::process::id());
        let key_with = |cookies: &Path| ResolutionKey {
            url: url.clone(),
            sel: "bv*+ba".to_string(),
            audio: false,
            login: String::new(),
            password_sig: password_signature(""),
            cookies_sig: cookies_signature(Some(cookies)),
            proxy: ndm::global_proxy(),
        };
        let warm_key = key_with(&a);
        let job_key = key_with(&b);
        assert!(warm_key == job_key, "both paths must compute the same key");
        resolution_cache_store(warm_key, test_resolution(), Instant::now());
        assert!(
            resolution_cache_get(&job_key, Instant::now()).is_some(),
            "the job must find the warmed resolution — no second probe"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One throttle-mimic connection. While the throttle
    /// window is open it serves a small fast budget and then
    /// goes SILENT holding the socket open — the googlevideo
    /// shape from the field recording (connections neither
    /// deliver nor die). After the window, connections serve
    /// at full speed, as a revived route does after the job
    /// re-connects / re-resolves.
    fn throttle_conn(
        mut sock: std::net::TcpStream,
        payload: Arc<Vec<u8>>,
        conns: Arc<std::sync::atomic::AtomicUsize>,
        throttle_until: Instant,
    ) {
        use std::io::{Read, Write};
        conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match sock.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p;
                    }
                    if buf.len() > 64 * 1024 {
                        return;
                    }
                }
                Err(_) => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let total = payload.len() as u64;
        let (start, end, partial) = if let Some(line) =
            head.lines().find(|l| l.to_ascii_lowercase().starts_with("range:"))
        {
            let spec = line.split('=').nth(1).unwrap_or("").trim();
            let mut it = spec.splitn(2, '-');
            let a: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let b: u64 = it
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(total - 1);
            (a, b.min(total - 1), true)
        } else {
            (0, total - 1, false)
        };
        let is_head = head.starts_with("HEAD");
        let len = end - start + 1;
        let headers = if partial {
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
        };
        if sock.write_all(headers.as_bytes()).is_err() || is_head {
            return;
        }
        const BUDGET: u64 = 64 * 1024;
        let body = &payload[start as usize..=end as usize];
        if Instant::now() >= throttle_until {
            let _ = sock.write_all(body);
            return;
        }
        let fast = (len.min(BUDGET)) as usize;
        if sock.write_all(&body[..fast]).is_err() {
            return;
        }
        // the throttle's silence: hold the socket, send nothing
        // more, until the client gives up and closes (which is
        // the recovery working) — the read detects the close
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(120)));
        let mut sink = [0u8; 1024];
        loop {
            match sock.read(&mut sink) {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    }

    #[test]
    fn stall_recovery_resumes_and_completes() {
        // 2.3.1 (bugs 2+3): a preresolved job whose streams all
        // decay into the throttle dribble must recover in place
        // — fresh connections from the downloaded offset, status
        // "resuming", no yt-dlp fallback, no full re-probe — and
        // land byte-exact. Before the fix this shape froze at
        // ~12% and cycled "Probing connection..." for hours.
        // The fixture must be a REAL mp4: the job's finish line
        // remuxes the downloaded stream with ffmpeg (audio is
        // normalized to AAC), so random bytes would fail the
        // merge, not the download. Generated with the ffmpeg the
        // product itself requires; skipped when it is absent.
        let gen_dir = std::env::temp_dir().join(format!("ndm-stallgen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&gen_dir);
        std::fs::create_dir_all(&gen_dir).unwrap();
        let src = gen_dir.join("src.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15",
                "-f", "lavfi", "-i", "sine=frequency=440",
                "-t", "7", "-vf", "noise=alls=70:allf=t",
                "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "6M",
                "-c:a", "aac", "-shortest",
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("ffmpeg unavailable — stall fixture skipped");
            let _ = std::fs::remove_dir_all(&gen_dir);
            return;
        }
        let payload = Arc::new(std::fs::read(&src).unwrap());
        assert!(payload.len() > 4 * 1024 * 1024, "fixture too small: {}", payload.len());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // the throttle window: long enough for the job's first
        // attempt to wedge and its stall verdict to fire (one
        // full window to drain the budgets, one to judge the
        // silence), short enough to keep the test quick
        let throttle_until = Instant::now() + std::time::Duration::from_secs(20);
        {
            let payload = payload.clone();
            let conns = conns.clone();
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    match sock {
                        Ok(s) => {
                            let p = payload.clone();
                            let c = conns.clone();
                            std::thread::spawn(move || throttle_conn(s, p, c, throttle_until));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        let url = format!("http://127.0.0.1:{port}/videoplayback");
        let dir = std::env::temp_dir().join(format!("ndm-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let comp = dir.join("comp");
        std::fs::create_dir_all(&comp).unwrap();

        let job = VideoJob::start_with_preresolved(
            &url,
            &dir,
            "bv",
            false,
            "stall fixture",
            None,
            "",
            "",
            &comp,
            payload.len() as u64,
            0.0,
            None,
            Preresolved {
                title: "stall fixture".to_string(),
                streams: vec![PreresolvedStream {
                    url: url.clone(),
                    ext: "mp4".to_string(),
                    has_audio: true,
                    cookie: None,
                    referer: None,
                    user_agent: None,
                }],
                expect_audio: true,
            },
        )
        .expect("job must start");

        let mut saw_resuming = false;
        let mut last_dbg = String::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(240);
        let final_status = loop {
            let (status, done) = {
                let p = job.progress.lock().unwrap();
                (p.status.clone(), p.done)
            };
            let dbg = format!("{status}@{done}");
            if dbg != last_dbg {
                last_dbg = dbg;
            }
            if status == "resuming" {
                saw_resuming = true;
            }
            if status == "finished" || status == "error" || status == "cancelled" {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "job wedged in {status} at {done} bytes — the stall loop is back"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let err_text = job.progress.lock().unwrap().error.clone();
        assert_eq!(final_status, "finished", "job error: {err_text:?}");
        assert!(saw_resuming, "a recovery must surface as 'resuming'");
        let final_path = job
            .progress
            .lock()
            .unwrap()
            .final_path
            .clone()
            .expect("finished job records its file");
        // The finish line remuxes (audio normalized to AAC), so
        // the final container legitimately differs from the
        // served bytes: what must hold is that every downloaded
        // byte arrived (progress done == served total, asserted
        // by the finish itself) and the output is a real,
        // playable mp4 of the same magnitude.
        let out_len = std::fs::metadata(&final_path).unwrap().len();
        assert!(
            out_len > payload.len() as u64 / 2 && out_len < payload.len() as u64 * 2,
            "remuxed size {out_len} vs served {}",
            payload.len()
        );
        let probe = std::process::Command::new("ffmpeg")
            .args(["-i"])
            .arg(&final_path)
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&probe.stderr);
        assert!(info.contains("Video:"), "final file has no video stream: {info}");
        assert!(info.contains("Audio:"), "final file has no audio stream: {info}");
        assert!(
            conns.load(std::sync::atomic::Ordering::SeqCst) >= 4,
            "recovery must open fresh connections"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&gen_dir);
    }

    /* ---------------- AUDIT-232 (B3): stall-recovery edges ---------------- */

    /// One permanently-throttling connection for the B3 rigs:
    /// serves `budget` bytes of the requested range, then holds
    /// the socket silent — the googlevideo shape, forever.
    fn audit_throttle_conn(
        mut sock: std::net::TcpStream,
        payload: Arc<Vec<u8>>,
        conns: Arc<std::sync::atomic::AtomicUsize>,
        budget: u64,
    ) {
        use std::io::{Read, Write};
        conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match sock.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p;
                    }
                    if buf.len() > 64 * 1024 {
                        return;
                    }
                }
                Err(_) => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let total = payload.len() as u64;
        let (start, end, partial) = if let Some(line) =
            head.lines().find(|l| l.to_ascii_lowercase().starts_with("range:"))
        {
            let spec = line.split('=').nth(1).unwrap_or("").trim();
            let mut it = spec.splitn(2, '-');
            let a: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let b: u64 = it
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(total - 1);
            (a, b.min(total - 1), true)
        } else {
            (0, total - 1, false)
        };
        let is_head = head.starts_with("HEAD");
        let len = end - start + 1;
        let headers = if partial {
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
        };
        if sock.write_all(headers.as_bytes()).is_err() || is_head {
            return;
        }
        let body = &payload[start as usize..=end as usize];
        let fast = (len.min(budget)) as usize;
        if sock.write_all(&body[..fast]).is_err() {
            return;
        }
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(120)));
        let mut sink = [0u8; 1024];
        loop {
            match sock.read(&mut sink) {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    }

    fn audit_payload(gen_dir: &Path, secs: u32, bitrate: &str) -> Option<Arc<Vec<u8>>> {
        let _ = std::fs::remove_dir_all(gen_dir);
        std::fs::create_dir_all(gen_dir).unwrap();
        let src = gen_dir.join("src.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15",
                "-f", "lavfi", "-i", "sine=frequency=440",
                "-t", &secs.to_string(), "-vf", "noise=alls=70:allf=t",
                "-c:v", "libx264", "-preset", "ultrafast", "-b:v", bitrate,
                "-c:a", "aac", "-shortest",
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("ffmpeg unavailable — B3 fixture skipped");
            return None;
        }
        Some(Arc::new(std::fs::read(&src).unwrap()))
    }

    fn audit_server(
        payload: Arc<Vec<u8>>,
        budget: u64,
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let conns = conns.clone();
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    match sock {
                        Ok(s) => {
                            let p = payload.clone();
                            let c = conns.clone();
                            std::thread::spawn(move || audit_throttle_conn(s, p, c, budget));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        (port, conns)
    }

    /// A stub yt-dlp in the components dir: counts its invocations,
    /// answers every resolve with one combined stream pointing at
    /// the throttle server, and (optionally) sleeps before answering
    /// from the SECOND invocation on — the re-resolve window the
    /// pause/cancel tests strike into.
    #[cfg(unix)]
    fn audit_stub_ytdlp(comp: &Path, stream_url: &str, reresolve_sleep_secs: u32) -> PathBuf {
        let count_file = comp.join("ytdlp-count.txt");
        let json = format!(
            "{{\"title\":\"audit fixture\",\"duration\":100,\"requested_formats\":[{{\"format_id\":\"18\",\"url\":\"{stream_url}\",\"ext\":\"mp4\",\"protocol\":\"http\",\"vcodec\":\"avc1.640028\",\"acodec\":\"mp4a.40.2\",\"tbr\":1000}}]}}"
        );
        let script = format!(
            "#!/bin/sh\necho x >> \"{count}\"\nn=$(grep -c '' \"{count}\")\nif [ \"$n\" -ge 2 ]; then sleep {reresolve_sleep_secs}; fi\nprintf '%s\\n' '{json}'\n",
            count = count_file.display()
        );
        let stub = comp.join("yt-dlp");
        std::fs::write(&stub, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        count_file
    }

    #[cfg(unix)]
    fn audit_stub_runs(count_file: &Path) -> usize {
        std::fs::read_to_string(count_file)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[cfg(unix)]
    struct AuditJob {
        job: Arc<VideoJob>,
        dir: PathBuf,
        gen_dir: PathBuf,
        count_file: PathBuf,
        conns: Arc<std::sync::atomic::AtomicUsize>,
        payload_len: usize,
    }

    /// A non-preresolved job (so stall recovery can re-resolve)
    /// against a permanently throttling server.
    #[cfg(unix)]
    fn audit_start_job(tag: &str, budget: u64, reresolve_sleep_secs: u32, secs: u32, bitrate: &str) -> Option<AuditJob> {
        let gen_dir = std::env::temp_dir().join(format!("ndm-b3gen-{tag}-{}", std::process::id()));
        let payload = audit_payload(&gen_dir, secs, bitrate)?;
        let payload_len = payload.len();
        let (port, conns) = audit_server(payload, budget);
        let stream_url = format!("http://127.0.0.1:{port}/videoplayback");
        let dir = std::env::temp_dir().join(format!("ndm-b3-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let comp = dir.join("comp");
        std::fs::create_dir_all(&comp).unwrap();
        let count_file = audit_stub_ytdlp(&comp, &stream_url, reresolve_sleep_secs);
        let page_url = format!("https://audit-b3-{tag}.example/watch?v={port}");
        let job = VideoJob::start(
            &page_url,
            &dir,
            "bv",
            false,
            "audit fixture",
            None,
            "",
            "",
            &comp,
            payload_len as u64,
            0.0,
        )
        .expect("job must start");
        Some(AuditJob { job, dir, gen_dir, count_file, conns, payload_len })
    }

    #[cfg(unix)]
    fn audit_wait_for<F: Fn() -> bool>(deadline_secs: u64, what: &str, f: F) -> bool {
        let deadline = Instant::now() + std::time::Duration::from_secs(deadline_secs);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("timed out waiting for {what}");
        f()
    }

    /// B3(a): a pause that lands DURING stall recovery's re-resolve
    /// must park the job: the resolve's result is discarded, no
    /// stream restarts, the status stays "paused" and the job
    /// thread exits. The re-resolve runs on a bare child inside
    /// resolve_streams_core — it is not the job's tracked child,
    /// so pause() cannot kill it; the attempt loop must honour
    /// the stop flag when the resolve returns.
    #[cfg(unix)]
    #[test]
    fn pause_during_stall_reresolve_parks_the_job() {
        let Some(rig) = audit_start_job("pause", 1024, 8, 7, "6M") else { return };
        let job = &rig.job;
        // attempts 1+2 each gain <=32 KiB (< the 128 KiB reset
        // line), so the second stall spends barren=2 and the loop
        // re-resolves: stub invocation #2 is that re-resolve.
        assert!(
            audit_wait_for(150, "the recovery re-resolve to start", || audit_stub_runs(&rig.count_file) >= 2),
            "recovery never re-resolved (stub runs: {})",
            audit_stub_runs(&rig.count_file)
        );
        job.pause();
        let done_at_pause = job.progress.lock().unwrap().done;
        let conns_at_pause = rig.conns.load(std::sync::atomic::Ordering::SeqCst);
        // the re-resolve returns ~8 s after it started; a loop
        // that ignores the pause restarts its streams then and
        // the job thread lives on in the poll loop.
        assert!(
            audit_wait_for(40, "the job thread to exit after the pause", || job.is_settled()),
            "job thread still live 40 s after pausing mid-re-resolve — the pause was ignored (status: {})",
            job.progress.lock().unwrap().status
        );
        assert_eq!(job.progress.lock().unwrap().status, "paused");
        assert_eq!(
            job.progress.lock().unwrap().done,
            done_at_pause,
            "bytes moved after the pause"
        );
        assert_eq!(
            rig.conns.load(std::sync::atomic::Ordering::SeqCst),
            conns_at_pause,
            "new connections opened after the pause"
        );
        let _ = std::fs::remove_dir_all(&rig.dir);
        let _ = std::fs::remove_dir_all(&rig.gen_dir);
    }

    /// B3(b): a cancel that lands during the re-resolve must stop
    /// the job outright — no fallback run, no final file, no
    /// further connections once the job thread is gone (the delete
    /// path settles on exactly this before sweeping).
    #[cfg(unix)]
    #[test]
    fn cancel_during_stall_reresolve_stops_the_job() {
        let Some(rig) = audit_start_job("cancel", 1024, 8, 7, "6M") else { return };
        let job = &rig.job;
        assert!(
            audit_wait_for(150, "the recovery re-resolve to start", || audit_stub_runs(&rig.count_file) >= 2),
            "recovery never re-resolved (stub runs: {})",
            audit_stub_runs(&rig.count_file)
        );
        job.cancel();
        assert!(
            audit_wait_for(40, "the job thread to exit after the cancel", || job.is_settled()),
            "job thread still live 40 s after cancelling mid-re-resolve"
        );
        assert_eq!(job.progress.lock().unwrap().status, "cancelled");
        assert_eq!(
            audit_stub_runs(&rig.count_file),
            2,
            "a cancelled job must not reach the yt-dlp fallback"
        );
        let conns_settled = rig.conns.load(std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(
            rig.conns.load(std::sync::atomic::Ordering::SeqCst),
            conns_settled,
            "connections opened after the job thread exited — something resurrected it"
        );
        assert!(
            job.progress.lock().unwrap().final_path.is_none(),
            "a cancelled job must not publish a final file"
        );
        let _ = std::fs::remove_dir_all(&rig.dir);
        let _ = std::fs::remove_dir_all(&rig.gen_dir);
    }

    /* ---------------- WS-A: preresolved auth-death heal ---------------- */

    /// One connection for the WS-A rigs: `/s?gen=1` (the captured
    /// stream URL) serves until `die_after` bytes have been served
    /// in total, then answers 403 to everything; `/s?gen=2` (what
    /// a yt-dlp re-resolve of the page returns) always serves.
    #[cfg(unix)]
    fn wsa_stream_conn(
        mut sock: std::net::TcpStream,
        payload: Arc<Vec<u8>>,
        conns: Arc<std::sync::atomic::AtomicUsize>,
        served: Arc<std::sync::atomic::AtomicU64>,
        die_after: u64,
        gen1_head_ok: bool,
    ) {
        use std::io::{Read, Write};
        conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match sock.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p;
                    }
                    if buf.len() > 64 * 1024 {
                        return;
                    }
                }
                Err(_) => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let first = head.lines().next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let gen2 = path.contains("gen=2");
        if !gen2 && served.load(std::sync::atomic::Ordering::Relaxed) >= die_after {
            // C-2 fixture shape: HEAD still answers (the start
            // probe passes) while every range GET is refused
            if gen1_head_ok && method == "HEAD" {
                let total = payload.len();
                let _ = sock.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"wsa\"\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
                return;
            }
            let _ = sock.write_all(
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 7\r\nConnection: close\r\n\r\nexpired",
            );
            return;
        }
        let total = payload.len() as u64;
        let (start, end, partial) = if let Some(line) =
            head.lines().find(|l| l.to_ascii_lowercase().starts_with("range:"))
        {
            let spec = line.split('=').nth(1).unwrap_or("").trim();
            let mut it = spec.splitn(2, '-');
            let a: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let b: u64 = it
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(total - 1);
            (a, b.min(total - 1), true)
        } else {
            (0, total - 1, false)
        };
        let len = end - start + 1;
        let headers = if partial {
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nETag: \"wsa\"\r\nConnection: close\r\n\r\n"
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nETag: \"wsa\"\r\nConnection: close\r\n\r\n"
            )
        };
        if sock.write_all(headers.as_bytes()).is_err() || method == "HEAD" {
            return;
        }
        let body = &payload[start as usize..=end as usize];
        if sock.write_all(body).is_ok() {
            served.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[cfg(unix)]
    fn wsa_server(
        payload: Arc<Vec<u8>>,
        die_after: u64,
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        wsa_server_ex(payload, die_after, false)
    }

    #[cfg(unix)]
    fn wsa_server_ex(
        payload: Arc<Vec<u8>>,
        die_after: u64,
        gen1_head_ok: bool,
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        {
            let conns = conns.clone();
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    match sock {
                        Ok(s) => {
                            let p = payload.clone();
                            let c = conns.clone();
                            let sv = served.clone();
                            std::thread::spawn(move || wsa_stream_conn(s, p, c, sv, die_after, gen1_head_ok));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        (port, conns)
    }

    /// A stub yt-dlp that sleeps on EVERY invocation (the heal
    /// re-resolve is a preresolved job's FIRST resolve, unlike
    /// the B3 stubs which sleep from the second run on).
    #[cfg(unix)]
    fn wsa_stub_ytdlp(comp: &Path, stream_url: &str, sleep_secs: u32) -> PathBuf {
        let count_file = comp.join("ytdlp-count.txt");
        let json = format!(
            "{{\"title\":\"wsa fixture\",\"duration\":100,\"requested_formats\":[{{\"format_id\":\"18\",\"url\":\"{stream_url}\",\"ext\":\"mp4\",\"protocol\":\"http\",\"vcodec\":\"avc1.640028\",\"acodec\":\"mp4a.40.2\",\"tbr\":1000}}]}}"
        );
        let script = format!(
            "#!/bin/sh\necho x >> \"{count}\"\nsleep {sleep_secs}\nprintf '%s\\n' '{json}'\n",
            count = count_file.display()
        );
        let stub = comp.join("yt-dlp");
        std::fs::write(&stub, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        count_file
    }

    #[cfg(unix)]
    struct WsaJob {
        job: Arc<VideoJob>,
        dir: PathBuf,
        gen_dir: PathBuf,
        count_file: PathBuf,
        conns: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// A preresolved (browser-captured) job whose captured
    /// stream dies after `die_after` bytes; the stub yt-dlp's
    /// resolve of the PAGE URL returns the gen=2 stream.
    #[cfg(unix)]
    fn wsa_start_job(tag: &str, die_after: u64, stub_sleep_secs: u32) -> Option<WsaJob> {
        wsa_start_job_ex(tag, die_after, stub_sleep_secs, false)
    }

    #[cfg(unix)]
    fn wsa_start_job_ex(
        tag: &str,
        die_after: u64,
        stub_sleep_secs: u32,
        gen1_head_ok: bool,
    ) -> Option<WsaJob> {
        let gen_dir = std::env::temp_dir().join(format!("ndm-wsagen-{tag}-{}", std::process::id()));
        let payload = audit_payload(&gen_dir, 7, "6M")?;
        let payload_len = payload.len() as u64;
        let (port, conns) = wsa_server_ex(payload, die_after, gen1_head_ok);
        let dead_url = format!("http://127.0.0.1:{port}/s?gen=1");
        let fresh_url = format!("http://127.0.0.1:{port}/s?gen=2");
        let dir = std::env::temp_dir().join(format!("ndm-wsa-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let comp = dir.join("comp");
        std::fs::create_dir_all(&comp).unwrap();
        let count_file = wsa_stub_ytdlp(&comp, &fresh_url, stub_sleep_secs);
        let page_url = format!("https://wsa-{tag}.example/watch?v={port}");
        let job = VideoJob::start_with_preresolved(
            &page_url,
            &dir,
            "ndmcap:wsafixture",
            false,
            "wsa fixture",
            None,
            "",
            "",
            &comp,
            payload_len,
            0.0,
            None,
            Preresolved {
                title: "wsa fixture".to_string(),
                streams: vec![PreresolvedStream {
                    url: dead_url,
                    ext: "mp4".to_string(),
                    has_audio: true,
                    cookie: None,
                    referer: Some(page_url.clone()),
                    user_agent: None,
                }],
                expect_audio: true,
            },
        )
        .expect("job must start");
        Some(WsaJob { job, dir, gen_dir, count_file, conns })
    }

    /// WS-A (audit-232 S5): a preresolved job whose captured
    /// streams auth-die mid-download must fall back to a yt-dlp
    /// re-resolve of the PAGE URL and finish — not burn its
    /// recovery budget retrying the dead capture URLs and fail
    /// with the capture-expired message (the pre-fix shape: the
    /// re-resolve was explicitly skipped for preresolved jobs).
    /// Fixture evidence: local server + stub yt-dlp.
    #[cfg(unix)]
    #[test]
    fn preresolved_auth_death_heals_via_page_reresolve() {
        let Some(rig) = wsa_start_job("heal", 1024 * 1024, 0) else { return };
        let job = &rig.job;
        let terminal = audit_wait_for(240, "the job to reach a terminal status", || {
            matches!(
                job.progress.lock().unwrap().status.as_str(),
                "finished" | "error" | "cancelled"
            )
        });
        assert!(terminal, "job never terminated (status: {})", job.progress.lock().unwrap().status);
        let err_text = job.progress.lock().unwrap().error.clone();
        assert_eq!(
            job.progress.lock().unwrap().status,
            "finished",
            "the auth-dead capture must heal via the page re-resolve; error: {err_text:?}"
        );
        assert!(
            audit_stub_runs(&rig.count_file) >= 1,
            "the heal must re-resolve the page URL through yt-dlp (stub runs: {})",
            audit_stub_runs(&rig.count_file)
        );
        let final_path = job
            .progress
            .lock()
            .unwrap()
            .final_path
            .clone()
            .expect("finished job records its file");
        assert!(final_path.is_file(), "the healed job's file must exist");
        let probe = std::process::Command::new("ffmpeg")
            .args(["-i"])
            .arg(&final_path)
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&probe.stderr);
        assert!(info.contains("Video:"), "final file has no video stream: {info}");
        assert!(info.contains("Audio:"), "final file has no audio stream: {info}");
        let _ = std::fs::remove_dir_all(&rig.dir);
        let _ = std::fs::remove_dir_all(&rig.gen_dir);
    }

    /// C-2 (audit-233): the byte-0 video shape — the captured
    /// stream's HEAD answers 200 (the start probe passes) but
    /// every range GET is refused 403, so the first attempt ends
    /// Errored having moved ZERO bytes. Pre-fix the attempt loop
    /// returned at once on done == 0 and the job ended with the
    /// capture-expired message; post-fix the typed auth death
    /// heals via the page re-resolve and the job finishes.
    #[cfg(unix)]
    #[test]
    fn preresolved_range_refusal_at_byte_zero_heals_via_page_reresolve() {
        let Some(rig) = wsa_start_job_ex("byte0", 0, 0, true) else { return };
        let job = &rig.job;
        let terminal = audit_wait_for(300, "the job to reach a terminal status", || {
            matches!(
                job.progress.lock().unwrap().status.as_str(),
                "finished" | "error" | "cancelled"
            )
        });
        assert!(terminal, "job never terminated (status: {})", job.progress.lock().unwrap().status);
        let err_text = job.progress.lock().unwrap().error.clone();
        assert_eq!(
            job.progress.lock().unwrap().status,
            "finished",
            "the byte-0 range-refused capture must heal via the page re-resolve; error: {err_text:?}"
        );
        assert!(
            audit_stub_runs(&rig.count_file) >= 1,
            "the heal must re-resolve the page URL through yt-dlp (stub runs: {})",
            audit_stub_runs(&rig.count_file)
        );
        let _ = std::fs::remove_dir_all(&rig.dir);
        let _ = std::fs::remove_dir_all(&rig.gen_dir);
    }

    /// WS-A: a pause that lands DURING the heal re-resolve parks
    /// the job — the resolve's fresh streams are discarded, no
    /// stream restarts, the status stays "paused" (the B3-A stop
    /// gate, on the heal path).
    #[cfg(unix)]
    #[test]
    fn pause_during_preresolved_heal_reresolve_parks_the_job() {
        let Some(rig) = wsa_start_job("pause", 1024 * 1024, 8) else { return };
        let job = &rig.job;
        assert!(
            audit_wait_for(150, "the heal re-resolve to start", || audit_stub_runs(&rig.count_file) >= 1),
            "the heal never re-resolved (stub runs: {})",
            audit_stub_runs(&rig.count_file)
        );
        job.pause();
        assert!(
            audit_wait_for(40, "the job thread to exit after the pause", || job.is_settled()),
            "job thread still live 40 s after pausing mid-heal — the pause was ignored (status: {})",
            job.progress.lock().unwrap().status
        );
        assert_eq!(job.progress.lock().unwrap().status, "paused");
        let conns_settled = rig.conns.load(std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(
            rig.conns.load(std::sync::atomic::Ordering::SeqCst),
            conns_settled,
            "connections opened after the paused heal settled — the job resurrected"
        );
        let _ = std::fs::remove_dir_all(&rig.dir);
        let _ = std::fs::remove_dir_all(&rig.gen_dir);
    }

    /// B3(c) under the line: attempts that each gain < 128 KiB
    /// spend the recovery budget — after MAX_VIDEO_RECOVERIES+1
    /// barren attempts the job leaves the engine for the yt-dlp
    /// fallback (here the stub produces no file, so the job ends
    /// in error rather than cycling forever).
    #[cfg(unix)]
    #[test]
    fn barren_recoveries_exhaust_the_budget_and_fall_back() {
        let Some(rig) = audit_start_job("barren", 1024, 0, 7, "6M") else { return };
        let job = &rig.job;
        let terminal = audit_wait_for(300, "the job to reach a terminal status", || {
            matches!(
                job.progress.lock().unwrap().status.as_str(),
                "finished" | "error" | "cancelled"
            )
        });
        assert!(terminal, "barren job never terminated (status: {})", job.progress.lock().unwrap().status);
        assert_eq!(
            job.progress.lock().unwrap().status,
            "error",
            "the fallback stub downloads nothing, so the job must end in error"
        );
        assert_eq!(
            audit_stub_runs(&rig.count_file),
            4,
            "expected 1 initial resolve + 2 recovery re-resolves + 1 fallback run"
        );
        assert!(
            job.progress.lock().unwrap().done < (rig.payload_len / 4) as u64,
            "barren attempts must not have moved much data"
        );
        let _ = std::fs::remove_dir_all(&rig.dir);
        let _ = std::fs::remove_dir_all(&rig.gen_dir);
    }

    /// One connection for the B3(c)-over rig: serves its range at
    /// 32 KiB/s, but only inside the shared fast windows (6 s
    /// every 36 s, the first at once); outside them it holds the
    /// socket silent. A job therefore banks megabytes in the
    /// attempt that straddles a window, then stalls; a later
    /// attempt straddles the next window and banks again — the
    /// real shape of a route that revives in bursts.
    #[cfg(unix)]
    fn audit_window_conn(
        mut sock: std::net::TcpStream,
        payload: Arc<Vec<u8>>,
        conns: Arc<std::sync::atomic::AtomicUsize>,
        t0: Instant,
    ) {
        use std::io::{Read, Write};
        conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match sock.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p;
                    }
                    if buf.len() > 64 * 1024 {
                        return;
                    }
                }
                Err(_) => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let total = payload.len() as u64;
        let (start, end, partial) = if let Some(line) =
            head.lines().find(|l| l.to_ascii_lowercase().starts_with("range:"))
        {
            let spec = line.split('=').nth(1).unwrap_or("").trim();
            let mut it = spec.splitn(2, '-');
            let a: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let b: u64 = it
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(total - 1);
            (a, b.min(total - 1), true)
        } else {
            (0, total - 1, false)
        };
        let is_head = head.starts_with("HEAD");
        let len = end - start + 1;
        let headers = if partial {
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
        };
        if sock.write_all(headers.as_bytes()).is_err() || is_head {
            return;
        }
        let in_window = |t: Instant| {
            // windows start at once (a stream whose first bytes
            // wait half a minute is abandoned by the engine's own
            // deadlines before the recovery loop is even reached)
            // and recur every 36 s, so successive attempts —
            // whose cycle is ~17 s — keep straddling new ones
            t.duration_since(t0).as_secs() % 36 < 6
        };
        let body = &payload[start as usize..=end as usize];
        // probe-sized answers (HEAD above, validator ranges) are
        // never throttled: the engine will not start workers for
        // a stream whose probe starves, so pacing them would
        // stall the rig at zero bytes for a reason that has
        // nothing to do with the recovery loop under test
        if body.len() <= 64 * 1024 {
            let _ = sock.write_all(body);
            return;
        }
        let mut sent = 0usize;
        while sent < body.len() {
            // silent stretch: hold the socket and wait for the
            // next window (if the client gave up meanwhile, the
            // write at the window fails and ends the thread)
            while !in_window(Instant::now()) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            let n = (body.len() - sent).min(32 * 1024);
            if sock.write_all(&body[sent..sent + n]).is_err() {
                return;
            }
            sent += n;
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// B3(c) over the line: attempts that each gain >= 128 KiB
    /// RESET the recovery budget. Against the window server the
    /// job alternates barren attempts (no window: they spend
    /// budget) with productive ones (a window: ~4 MiB banked,
    /// budget reset) — the barren count can therefore never
    /// reach the cap, and the job must still be in the engine,
    /// gigabytes short of finished but millions of bytes in,
    /// long after a purely barren job would have fallen back
    /// (the barren test above terminates in ~65 s).
    #[cfg(unix)]
    #[test]
    fn productive_recoveries_reset_the_budget() {
        let gen_dir = std::env::temp_dir().join(format!("ndm-b3gen-reset-{}", std::process::id()));
        let Some(payload) = audit_payload(&gen_dir, 30, "8M") else { return };
        let payload_len = payload.len();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let t0 = Instant::now();
        {
            let (p, c) = (payload.clone(), conns.clone());
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    match sock {
                        Ok(s) => {
                            let (p, c) = (p.clone(), c.clone());
                            std::thread::spawn(move || audit_window_conn(s, p, c, t0));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        let stream_url = format!("http://127.0.0.1:{port}/videoplayback");
        let dir = std::env::temp_dir().join(format!("ndm-b3-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let comp = dir.join("comp");
        std::fs::create_dir_all(&comp).unwrap();
        let count_file = audit_stub_ytdlp(&comp, &stream_url, 0);
        let page_url = format!("https://audit-b3-reset.example/watch?v={port}");
        let job = VideoJob::start(
            &page_url, &dir, "bv", false, "audit fixture", None, "", "",
            &comp, payload_len as u64, 0.0,
        )
        .expect("job must start");
        // 170 s covers ~11 stall windows and three fast windows:
        // a job that only spent budget would have fallen back at
        // the fourth barren attempt (< ~90 s) and errored on the
        // fileless stub; a resetting job is still cycling with
        // >= 8 MiB banked (two windows' worth).
        let job_start = Instant::now();
        let ok = audit_wait_for(170, "productive windows banked, still cycling", || {
            let p = job.progress.lock().unwrap();
            // 2 MiB is 16x the 128 KiB reset line: reaching it
            // takes several window-straddling attempts at this
            // rig's measured rates (attempt/window alignment,
            // settle time and suite-parallel load eat most of
            // each window)
            p.done >= 2 * 1024 * 1024
                && matches!(p.status.as_str(), "starting" | "downloading" | "resuming")
        });
        let (status, done) = {
            let p = job.progress.lock().unwrap();
            (p.status.clone(), p.done)
        };
        assert!(
            ok,
            "job did not stay productive: status {status} at {done} bytes \
             (payload {payload_len}, stub runs {}, conns {}, error {:?})",
            audit_stub_runs(&count_file),
            conns.load(std::sync::atomic::Ordering::SeqCst),
            job.progress.lock().unwrap().error
        );
        // The actual claim: past the point where four barren
        // attempts would have exhausted the budget and fallen
        // back (~90 s of stall cycles), the job is STILL in the
        // engine — productive attempts kept resetting the count.
        let survived = audit_wait_for(150, "the job to outlive the barren budget", || {
            job_start.elapsed() >= std::time::Duration::from_secs(110)
                && matches!(
                    job.progress.lock().unwrap().status.as_str(),
                    "starting" | "downloading" | "resuming"
                )
        });
        assert!(
            survived,
            "job left the engine by 110 s (status {}) — the budget was spent, not reset",
            job.progress.lock().unwrap().status
        );
        assert!(
            audit_stub_runs(&count_file) <= 5,
            "stub runs {} — the fallback must not have run (resolves + re-resolves only)",
            audit_stub_runs(&count_file)
        );
        job.cancel();
        let _ = audit_wait_for(40, "the job thread to exit after cancel", || job.is_settled());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&gen_dir);
    }

    // ---------- WS-C (2.3.2): the captured browser User-Agent ----------
    // Drives the REAL capture chain — a CapturedSet stored exactly
    // as the drain stores it, capture::preresolved_for, then
    // VideoJob::start_with_preresolved — against a header-recording
    // range server, and returns the User-Agent of every request the
    // engine made (probe and transfer alike). Audit-232 S3 proved
    // cookies and Referer flow down this chain but the UA was
    // always the engine's fixed Linux Chrome/126 constant, even
    // for a Windows/Brave capture.
    fn run_capture_chain_recording_uas(tag: &str, user_agent: Option<&str>) -> Option<Vec<String>> {
        use std::io::Write as _;
        let gen_dir = std::env::temp_dir().join(format!("ndm-uagen-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&gen_dir);
        std::fs::create_dir_all(&gen_dir).unwrap();
        let src = gen_dir.join("src.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10",
                "-f", "lavfi", "-i", "sine=frequency=440",
                "-t", "2", "-c:v", "libx264", "-preset", "ultrafast",
                "-c:a", "aac", "-shortest",
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            // Same contract as audit_payload: a machine without
            // ffmpeg in PATH (the Windows CI runners) skips the
            // fixture instead of failing the suite.
            eprintln!("ffmpeg unavailable — UA chain fixture skipped");
            return None;
        }
        let payload = Arc::new(std::fs::read(&src).unwrap());
        let total = payload.len() as u64;

        // header-recording range server: keeps the UA of every request
        let seen: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        {
            let payload = payload.clone();
            let seen = seen.clone();
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    let Ok(mut s) = sock else { break };
                    let p = payload.clone();
                    let sn = seen.clone();
                    std::thread::spawn(move || {
                        let mut rd = BufReader::new(s.try_clone().unwrap());
                        let mut req_line = String::new();
                        if rd.read_line(&mut req_line).is_err() {
                            return;
                        }
                        let mut hdrs: Vec<(String, String)> = Vec::new();
                        loop {
                            let mut l = String::new();
                            match rd.read_line(&mut l) {
                                Ok(0) => return,
                                Ok(_) => {
                                    let l = l.trim_end().to_string();
                                    if l.is_empty() {
                                        break;
                                    }
                                    if let Some((k, v)) = l.split_once(':') {
                                        hdrs.push((k.trim().to_string(), v.trim().to_string()));
                                    }
                                }
                                Err(_) => return,
                            }
                        }
                        let ua = hdrs
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("User-Agent"))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| "<none>".to_string());
                        sn.lock().unwrap().push(format!("{} ua=[{ua}]", req_line.trim()));
                        let is_head = req_line.starts_with("HEAD");
                        let (start, end, partial) = match hdrs
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("Range"))
                        {
                            Some((_, v)) => {
                                let v = v.trim_start_matches("bytes=");
                                let (a, b) = v.split_once('-').unwrap_or(("0", ""));
                                let a: u64 = a.parse().unwrap_or(0);
                                let b: u64 = if b.is_empty() {
                                    total - 1
                                } else {
                                    b.parse().unwrap_or(total - 1)
                                };
                                (a, b.min(total - 1), true)
                            }
                            None => (0, total - 1, false),
                        };
                        let len = end - start + 1;
                        let head_txt = if partial {
                            format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                        } else {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                        };
                        if s.write_all(head_txt.as_bytes()).is_err() || is_head {
                            return;
                        }
                        let _ = s.write_all(&p[start as usize..=end as usize]);
                    });
                }
            });
        }

        let stream_url = format!("http://127.0.0.1:{port}/videoplayback?sig=abc");
        let page_url = format!("http://127.0.0.1:{port}/watch?v=xyz");
        let dir = std::env::temp_dir().join(format!("ndm-ua-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cap_dir = dir.join("capdata");
        std::fs::create_dir_all(&cap_dir).unwrap();

        // the capture set, exactly as the host validated + stored it
        let set = crate::capture::CapturedSet {
            page_url: page_url.clone(),
            title: "ua fixture".to_string(),
            streams: vec![crate::capture::CapturedStream {
                url: stream_url.clone(),
                mime_type: "video/mp4".to_string(),
                quality_label: Some("720p".to_string()),
                has_audio: true,
                has_video: true,
                user_agent: user_agent.map(|s| s.to_string()),
            }],
            received_unix: crate::capture::now_unix(),
            video_key: String::new(),
        };
        crate::capture::store_set(Some(&cap_dir), set.clone());
        let groups = crate::capture::group_streams(&set.streams);
        assert_eq!(groups.len(), 1);
        let selector = crate::capture::selector_for(&groups[0]);

        let pre = crate::capture::preresolved_for(
            Some(&cap_dir),
            &page_url,
            &selector,
            crate::capture::now_unix(),
        )
        .expect("preresolved lookup must succeed")
        .expect("a preresolved set must be found");
        assert_eq!(pre.streams.len(), 1);
        assert_eq!(
            pre.streams[0].user_agent.as_deref(),
            user_agent,
            "the preresolved stream must carry the capture's UA"
        );

        let comp = dir.join("comp");
        std::fs::create_dir_all(&comp).unwrap();
        let job = VideoJob::start_with_preresolved(
            &page_url,
            &dir,
            &selector,
            false,
            "ua fixture",
            None,
            "",
            "",
            &comp,
            total,
            0.0,
            None,
            pre,
        )
        .expect("job must start");
        let deadline = Instant::now() + std::time::Duration::from_secs(120);
        let final_status = loop {
            let status = job.progress.lock().unwrap().status.clone();
            if status == "finished" || status == "error" || status == "cancelled" {
                break status;
            }
            assert!(Instant::now() < deadline, "UA chain job wedged in {status}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let err_text = job.progress.lock().unwrap().error.clone();
        assert_eq!(final_status, "finished", "job error: {err_text:?}");

        let dump = seen.lock().unwrap().clone();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&gen_dir);
        Some(dump)
    }

    #[test]
    fn captured_browser_user_agent_reaches_every_stream_request() {
        // a Windows/Brave-shaped UA — deliberately NOT the engine's
        // Linux Chrome/126 constant, so a pass cannot be the fallback
        const CAPTURED_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        let Some(dump) = run_capture_chain_recording_uas("withua", Some(CAPTURED_UA)) else {
            return;
        };
        assert!(!dump.is_empty(), "the server must record requests");
        assert!(
            dump.iter().any(|l| l.starts_with("GET")),
            "no GET recorded: {dump:?}"
        );
        for l in &dump {
            assert!(
                l.contains(&format!("ua=[{CAPTURED_UA}]")),
                "request did not carry the captured browser UA: {l}"
            );
        }
    }

    #[test]
    fn capture_without_user_agent_falls_back_to_engine_ua() {
        // an old capture (or a sender that never reported a UA)
        // keeps today's behaviour: the engine's BROWSER_UA constant
        const ENGINE_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
        let Some(dump) = run_capture_chain_recording_uas("noua", None) else {
            return;
        };
        assert!(!dump.is_empty(), "the server must record requests");
        for l in &dump {
            assert!(
                l.contains(&format!("ua=[{ENGINE_UA}]")),
                "request did not fall back to the engine UA: {l}"
            );
        }
    }

    // ---------- WS-D D2: pair cross-stealing ----------
    // A preresolved video+audio pair under a per-connection cap
    // (the audit-232 S4 fixture shape: ~90/10 sizes, 256 KiB/s
    // per connection, 8 connections). Audio finishes at ~4 s
    // while video needs ~12 s on its (max-2) share. The fix:
    // when audio finishes, its connections join the video side —
    // observable on the server as MORE than the video share's
    // worth of concurrent video GETs after the audio's last
    // byte (pre-fix the video side can never exceed 6 concurrent
    // GETs at max_conn 8; post-fix the two parked workers ungate
    // and it reaches 7-8). Both parts must land byte-identical
    // and the merged output must be produced.
    #[test]
    fn audio_codec_parse_reads_first_audio_stream() {
        // WS-D D3: the merge's copy-vs-re-encode decision rests on
        // this parse of `ffmpeg -i` output
        let aac = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.m4a':\n  Stream #0:0[0x1](und): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, stereo, fltp, 128 kb/s (default)";
        assert_eq!(audio_codec_in_ffmpeg_info(aac).as_deref(), Some("aac"));
        let opus = "  Stream #0:0: Video: vp9, yuv420p(tv, bt709), 1280x720, 30 fps\n  Stream #0:1: Audio: opus, 48000 Hz, stereo, fltp";
        assert_eq!(audio_codec_in_ffmpeg_info(opus).as_deref(), Some("opus"));
        let none = "  Stream #0:0: Video: h264 (High), yuv420p, 320x240";
        assert_eq!(audio_codec_in_ffmpeg_info(none), None);
        let mp3 = "  Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 192 kb/s";
        assert_eq!(audio_codec_in_ffmpeg_info(mp3).as_deref(), Some("mp3"));
    }

    #[test]
    fn pair_survivor_inherits_finished_sides_connections() {
        use std::io::{BufRead, BufReader, Write};
        let gen_dir = std::env::temp_dir().join(format!("ndm-d2gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&gen_dir);
        std::fs::create_dir_all(&gen_dir).unwrap();
        let vsrc = gen_dir.join("v.mp4");
        let asrc = gen_dir.join("a.m4a");
        let made_v = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15",
                "-t", "24", "-vf", "noise=alls=70:allf=t",
                "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "6M", "-an",
            ])
            .arg(&vsrc)
            .status().map(|s| s.success()).unwrap_or(false);
        let made_a = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "sine=frequency=440",
                "-t", "128", "-c:a", "aac", "-b:a", "128k",
            ])
            .arg(&asrc)
            .status().map(|s| s.success()).unwrap_or(false);
        if !made_v || !made_a {
            eprintln!("ffmpeg unavailable — D2 fixture skipped");
            let _ = std::fs::remove_dir_all(&gen_dir);
            return;
        }
        let vbody = Arc::new(std::fs::read(&vsrc).unwrap());
        let abody = Arc::new(std::fs::read(&asrc).unwrap());

        // (path, get_start, get_end) for every body-serving GET
        let gets: Arc<std::sync::Mutex<Vec<(String, f64, f64)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let t0 = Instant::now();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        {
            let (vb, ab, gg) = (vbody.clone(), abody.clone(), gets.clone());
            std::thread::spawn(move || {
                for sock in listener.incoming() {
                    let Ok(mut s) = sock else { break };
                    let (vb, ab, gg) = (vb.clone(), ab.clone(), gg.clone());
                    std::thread::spawn(move || {
                        let mut rd = BufReader::new(match s.try_clone() { Ok(c) => c, Err(_) => return });
                        let mut line = String::new();
                        if rd.read_line(&mut line).is_err() { return; }
                        let path = if line.contains("/a") { "audio" } else { "video" }.to_string();
                        let mut range: Option<(u64, u64)> = None;
                        loop {
                            let mut h = String::new();
                            match rd.read_line(&mut h) {
                                Ok(0) => return,
                                Ok(_) if h.trim().is_empty() => break,
                                Ok(_) => {
                                    let hl = h.to_ascii_lowercase();
                                    if let Some(v) = hl.strip_prefix("range:") {
                                        let spec = v.trim().trim_start_matches("bytes=");
                                        let mut it = spec.splitn(2, '-');
                                        let a: u64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                                        let b: u64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(u64::MAX);
                                        range = Some((a, b));
                                    }
                                }
                                Err(_) => return,
                            }
                        }
                        let body: &Arc<Vec<u8>> = if path == "audio" { &ab } else { &vb };
                        let total = body.len() as u64;
                        let is_head = line.starts_with("HEAD");
                        let (a, b) = range.unwrap_or((0, total - 1));
                        let b = b.min(total - 1);
                        let slice = &body[a as usize..=b as usize];
                        let hdr = if range.is_some() {
                            format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len())
                        } else {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len())
                        };
                        if s.write_all(hdr.as_bytes()).is_err() || is_head { return; }
                        let started = t0.elapsed().as_secs_f64();
                        // 256 KiB/s per connection: 32 KiB per 125 ms
                        for piece in slice.chunks(32 * 1024) {
                            if s.write_all(piece).is_err() { break; }
                            std::thread::sleep(std::time::Duration::from_millis(125));
                        }
                        gg.lock().unwrap().push((path, started, t0.elapsed().as_secs_f64()));
                    });
                }
            });
        }

        // the connection setting is process-global and this
        // suite runs tests in parallel threads: save and restore
        // it so a concurrently starting job never inherits this
        // fixture's pool size
        struct RestoreConns(usize);
        impl Drop for RestoreConns {
            fn drop(&mut self) {
                ndm::set_global_connections(self.0);
            }
        }
        let _conns_guard = RestoreConns(ndm::global_connections());
        ndm::set_global_connections(8);
        let dir = std::env::temp_dir().join(format!("ndm-d2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let comp = dir.join("comp");
        std::fs::create_dir_all(&comp).unwrap();
        let vurl = format!("http://127.0.0.1:{port}/v");
        let aurl = format!("http://127.0.0.1:{port}/a");
        let job = VideoJob::start_with_preresolved(
            &format!("http://127.0.0.1:{port}/watch"),
            &dir, "bv", false, "d2 fixture", None, "", "", &comp,
            (vbody.len() + abody.len()) as u64, 0.0, None,
            Preresolved {
                title: "d2 fixture".to_string(),
                streams: vec![
                    PreresolvedStream { url: vurl, ext: "mp4".to_string(), has_audio: false, cookie: None, referer: None, user_agent: None },
                    PreresolvedStream { url: aurl, ext: "m4a".to_string(), has_audio: true, cookie: None, referer: None, user_agent: None },
                ],
                expect_audio: true,
            },
        ).expect("job must start");
        let deadline = Instant::now() + std::time::Duration::from_secs(180);
        // (t, job-wide active connections) sampled while polling —
        // the client-side truth about the pair's pool: after the
        // audio side finishes, its own count drops to 0, so any
        // reading above the video share (6) is the survivor
        // working on inherited connections
        let mut conn_samples: Vec<(f64, usize)> = Vec::new();
        let final_status = loop {
            let (status, conns) = {
                let p = job.progress.lock().unwrap();
                (p.status.clone(), p.conns)
            };
            conn_samples.push((t0.elapsed().as_secs_f64(), conns));
            if status == "finished" || status == "error" || status == "cancelled" { break status; }
            assert!(Instant::now() < deadline, "D2 job wedged in {status}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let err_text = job.progress.lock().unwrap().error.clone();
        let job_secs = t0.elapsed().as_secs_f64();
        assert_eq!(final_status, "finished", "job error: {err_text:?}");

        let gets = gets.lock().unwrap();
        let audio_last = gets.iter().filter(|g| g.0 == "audio").map(|g| g.2).fold(0.0f64, f64::max);
        let video_last = gets.iter().filter(|g| g.0 == "video").map(|g| g.2).fold(0.0f64, f64::max);
        assert!(audio_last > 0.0 && video_last > audio_last,
            "audio must finish before video: audio {audio_last:.2}s video {video_last:.2}s");
        // max concurrent video GETs at any instant after audio's
        // last byte (sweep the video intervals clipped to that era)
        let mut events: Vec<(f64, i64)> = Vec::new();
        for g in gets.iter().filter(|g| g.0 == "video" && g.2 > audio_last) {
            events.push((g.1.max(audio_last), 1));
            events.push((g.2, -1));
        }
        events.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(b.1.cmp(&a.1)));
        let (mut cur, mut peak) = (0i64, 0i64);
        for (_, d) in &events {
            cur += d;
            peak = peak.max(cur);
        }
        // client-side: the job's active-connection count after
        // the audio side's last byte (+ a beat for the poll that
        // fires the donation) must exceed the video share of 6 —
        // pre-fix it can never pass 6 once audio is done
        let max_conns_after = conn_samples.iter()
            .filter(|(t, _)| *t > audio_last + 0.3)
            .map(|(_, c)| *c)
            .max()
            .unwrap_or(0);
        eprintln!("D2 RESULT job={job_secs:.2}s audio_last={audio_last:.2}s video_last={video_last:.2}s server_peak_video={peak} max_job_conns_after_audio={max_conns_after}");
        assert!(max_conns_after >= 7,
            "survivor never inherited the finished side's connections: job conns after audio finished peaked at {max_conns_after} (pair share is 6)");
        // (the server-side concurrency sweep above stays a
        // diagnostic only: its interval overlap also counts
        // handlers still draining after a client-side close, so
        // it can read above the pool size — the client-side count
        // asserted above is the donation proof)
        // the wall-clock gain on this fixture is bounded by
        // block quantization, not by the fluid bound: the video
        // side's remaining work at donation is ~13 one-MiB blocks
        // locked in six 3-block claims, and a block is a ~4 s
        // quantum at the cap — redistributing whole blocks over
        // 8 workers still leaves a last wave that cannot start
        // before the first wave lands. Measured locally: unhelped
        // 12.40 s, helped 12.0–12.1 s — a few percent, inside any
        // loaded CI machine's timing noise, so no tight absolute
        // threshold can carry the claim (a < 12.3 s bound against
        // the locally measured 12.4 s flaked in CI). The donation
        // is therefore proven structurally above, and the guard
        // that remains here is a gross-regression tripwire
        // computed from the fixture's own constants: even the
        // UNHELPED shape cannot take longer than the video bytes
        // at 6 connections × the server's scripted per-connection
        // rate (32 KiB per 125 ms); three times that fluid bound,
        // counted from the audio finish, only trips if the pair
        // machinery itself is broken.
        let fluid_unhelped = vbody.len() as f64 / (6.0 * 262_144.0);
        assert!(video_last < audio_last + 3.0 * fluid_unhelped,
            "video stream finished at {video_last:.2}s — beyond 3x the unhelped fluid bound (audio {audio_last:.2}s + 3 x {fluid_unhelped:.1}s)");
        // the merged output exists and is a real file
        let merged = std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.ends_with(".mp4") && !n.contains(".ndm_stream")
            })
            .expect("merged output must exist");
        assert!(merged.metadata().unwrap().len() > 1024 * 1024, "merged output too small");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&gen_dir);
    }


    #[test]
    fn finish_sweep_removes_both_engines_temps_only() {
        // R4 side observation: a job paused on yt-dlp and finished
        // on the parallel engine left its yt-dlp partial behind —
        // the finish sweep must take BOTH engines' temps for the
        // job's stem, plus the stream journals, and nothing else.
        let dir =
            std::env::temp_dir().join(format!("ndm-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gone = [
            ".big2.ndm_stream0.mp4.part",
            ".big2.ndm_stream0.mp4.part.ndm.json",
            ".big2.ndm_mux.mp4",
            ".big2.ndm_audio.m4a",
            "big2.mp4.part",
            "big2.mp4.ytdl",
            "big2.f140.m4a.part",
        ];
        let kept = [
            "big2.mp4",
            "big2.txt.part",
            "big22.mp4.part",
            "other.mp4.part",
            ".big2.ndm_backup",
        ];
        for n in gone.iter().chain(kept.iter()) {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        sweep_job_temps(&dir, "big2");
        for n in gone {
            assert!(!dir.join(n).exists(), "{n} must be swept");
        }
        for n in kept {
            assert!(dir.join(n).exists(), "{n} must survive");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merged_mp4_reports_both_streams() {
        let info = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'out.mp4':
  Duration: 00:03:12.04, start: 0.000000, bitrate: 2145 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 1920x1080, 1980 kb/s, 30 fps
  Stream #0:1[0x2](eng): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, stereo, fltp, 160 kb/s
At least one output file must be specified";
        assert_eq!(streams_in_ffmpeg_info(info), (true, true));
    }

    #[test]
    fn video_only_file_reports_missing_audio() {
        let info = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'silent.mp4':
  Duration: 00:03:12.04, start: 0.000000, bitrate: 1980 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 1920x1080, 1980 kb/s, 30 fps
At least one output file must be specified";
        assert_eq!(streams_in_ffmpeg_info(info), (true, false));
    }

    #[test]
    fn audio_only_file_reports_no_video() {
        let info = "\
Input #0, mp3, from 'song.mp3':
  Duration: 00:03:12.04, start: 0.025057, bitrate: 192 kb/s
  Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 192 kb/s";
        assert_eq!(streams_in_ffmpeg_info(info), (false, true));
    }

    #[test]
    fn safe_filename_stem_preserves_full_title_without_truncating_at_60_chars() {
        assert_eq!(
            safe_filename_stem("Fixture Player — Test Video"),
            "Fixture Player — Test Video"
        );
        assert_eq!(
            safe_filename_stem("Fixture Player — Test Video.mp4"),
            "Fixture Player — Test Video"
        );
    }

    #[test]
    fn merge_guard_judges_output_against_selected_media_not_download_total() {
        // (a) R3-EXT-01, the padded container: 100,663,296 bytes
        // were downloaded, but the selected streams hold only
        // ≈2.31 MB of media and the merge produced the byte-perfect
        // 2,342,828-byte movie — PASS. (The old baseline, the
        // download total, errored exactly this case three times.)
        assert!(drastic_shrink(Some(2_309_250), 100_663_296, 2_342_828).is_none());
        // (b) the F-EXT-04 class the guard exists for: the selected
        // media really is ~96 MB and the merge produced a 2.34 MB
        // stub instead of the movie — ERROR, honestly reported
        let err = drastic_shrink(Some(96_000_000), 100_663_296, 2_342_828)
            .expect("a stub merge must still error");
        assert!(err.contains("much smaller than what was downloaded"), "{err}");
        // (c) the expected media size is genuinely unknown — the
        // guard skips the job rather than fabricating a verdict,
        // however lopsided the download total looks
        assert!(drastic_shrink(None, 100_663_296, 2_342_828).is_none());
        assert!(drastic_shrink(None, 0, 0).is_none());
        // a zero estimate is no estimate
        assert!(drastic_shrink(Some(0), 100_663_296, 10).is_none());
        // exactly half of the expected media is within tolerance
        assert!(drastic_shrink(Some(1000), 5000, 500).is_none());
        // a normal remux (audio re-encode) shrinks a few percent
        assert!(drastic_shrink(Some(1000), 1000, 970).is_none());
    }

    #[test]
    fn media_payload_estimate_uses_stream_bitrates_not_file_size() {
        // the R3 fixture's real `ffmpeg -i` header: the Duration
        // line's 134217 kb/s is derived from the padded file size,
        // while the streams report their true bitrates — 3010 + 69
        // kb/s over 6.00 s is the ≈2.31 MB payload of the
        // 2,342,828-byte movie inside the 100 MB container
        let info = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'big.mp4':
  Duration: 00:00:06.00, start: 0.000000, bitrate: 134217 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 1280x720 [SAR 1:1 DAR 16:9], 3010 kb/s, 30 fps, 30 tbr, 15360 tbn (default)
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, mono, fltp, 69 kb/s (default)
At least one output file must be specified";
        assert_eq!(media_payload_estimate(info), Some(2_309_250));
    }

    #[test]
    fn media_payload_estimate_refuses_partial_data() {
        // a stream with no reported bitrate → unknown, not a guess
        let no_bitrate = "\
  Duration: 00:00:06.00, start: 0.000000, bitrate: 134217 kb/s
  Stream #0:0: Video: h264, yuv420p, 1280x720, 30 fps";
        assert_eq!(media_payload_estimate(no_bitrate), None);
        // unknown duration → unknown
        let no_duration = "\
  Duration: N/A, bitrate: N/A
  Stream #0:0: Audio: aac, 44100 Hz, mono, 69 kb/s";
        assert_eq!(media_payload_estimate(no_duration), None);
        // no streams at all → unknown
        assert_eq!(
            media_payload_estimate("  Duration: 00:00:06.00, bitrate: 100 kb/s"),
            None
        );
    }

    #[test]
    fn formats_media_bytes_estimates_from_bitrates_only() {
        let v = serde_json::json!({"formats": [
            {"tbr": 800.0},
            {"vbr": null, "abr": 128.0}
        ]});
        let formats: Vec<&serde_json::Value> =
            v["formats"].as_array().unwrap().iter().collect();
        // (800 + 128) kb/s × 100 s = 11,600,000 bytes of media
        assert_eq!(formats_media_bytes(100.0, &formats), Some(11_600_000));
        // a selected format with no bitrate at all → unknown
        let v2 = serde_json::json!({"formats": [{"tbr": 800.0}, {"ext": "mp4"}]});
        let formats2: Vec<&serde_json::Value> =
            v2["formats"].as_array().unwrap().iter().collect();
        assert_eq!(formats_media_bytes(100.0, &formats2), None);
        // unknown duration → unknown
        assert_eq!(formats_media_bytes(0.0, &formats), None);
    }

    #[test]
    fn unique_stem_on_disk_disambiguates_duplicate_titles() {
        let dir = std::env::temp_dir().join(format!("ndm-uniq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s0 = unique_stem_on_disk(&dir, "Fixture Player — Test Video", false);
        assert_eq!(s0, "Fixture Player — Test Video");
        std::fs::write(dir.join(format!("{s0}.mp4")), b"1").unwrap();
        let s1 = unique_stem_on_disk(&dir, "Fixture Player — Test Video", false);
        assert_eq!(s1, "Fixture Player — Test Video (1)");
        std::fs::write(dir.join(format!("{s1}.mp4")), b"2").unwrap();
        let s2 = unique_stem_on_disk(&dir, "Fixture Player — Test Video", false);
        assert_eq!(s2, "Fixture Player — Test Video (2)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_stem_on_disk_ignores_the_other_kinds_files() {
        // R3-EXT-04: the video half of a download-all pair landing
        // first (big2.mp4 — and likewise any other video container)
        // must not rename the audio half: no big2.m4a / big2.mp3
        // exists, so the audio stem stays "big2".
        let dir = std::env::temp_dir().join(format!("ndm-uniq-cross-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big2.mp4"), b"v").unwrap();
        std::fs::write(dir.join("big2.mkv"), b"v").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "big2", true), "big2");
        // Symmetrically, audio files never rename a video stem.
        std::fs::write(dir.join("song.m4a"), b"a").unwrap();
        std::fs::write(dir.join("song.mp3"), b"a").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "song", false), "song");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_stem_on_disk_renames_on_same_kind_collision() {
        // Genuine collision: the entry's OWN final filename already
        // exists, so "(1)" is correct — for either audio container.
        let dir = std::env::temp_dir().join(format!("ndm-uniq-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big2.m4a"), b"a").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "big2", true), "big2 (1)");
        std::fs::write(dir.join("song.mp3"), b"a").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "song", true), "song (1)");
        // …and the video kind against its own container.
        std::fs::write(dir.join("clip.mkv"), b"v").unwrap();
        assert_eq!(unique_stem_on_disk(&dir, "clip", false), "clip (1)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contradiction_verdict_flags_plain_mismatches_only() {
        // the walkthrough's shape: the page's video runs ~40 min,
        // the delivered file runs 6 s — a plain contradiction
        assert!(contradiction_verdict(0, 2400.0, 2_342_828, Some(6.0), false).is_some());
        // same story told by size alone: picked ~100 MB, got 2.3 MB
        assert!(contradiction_verdict(100_663_296, 0.0, 2_342_828, None, false).is_some());
        // a delivery that matches the probe passes clean
        assert!(contradiction_verdict(100_000_000, 2400.0, 98_000_000, Some(2395.0), false).is_none());
        // nothing probed → nothing to contradict (the padded-
        // container fixture: option size unknown, duration unknown)
        assert!(contradiction_verdict(0, 0.0, 2_342_828, Some(6.0), false).is_none());
        // an unprobeable produced duration never fabricates a verdict
        assert!(contradiction_verdict(0, 2400.0, 2_342_828, None, false).is_none());
        // remux/transcode slack is not a contradiction: 60% of the
        // expected size and 95% of the duration are both fine
        assert!(contradiction_verdict(100_000_000, 2400.0, 60_000_000, Some(2280.0), false).is_none());
        // audio jobs: size is never judged (transcodes shrink
        // legitimately), duration still is
        assert!(contradiction_verdict(100_000_000, 2400.0, 3_000_000, Some(2390.0), true).is_none());
        assert!(contradiction_verdict(0, 2400.0, 3_000_000, Some(6.0), true).is_some());
    }

    /// M-15: synthetic NDMPROG reports in the per-fragment shape —
    /// 8 fragments of one file downloading CONCURRENTLY, their
    /// cumulative reports interleaved round-robin. The pre-fix
    /// parser folded these into a base+current sum and reported
    /// many times the real size; the tracker keys by (file,
    /// fragment) and must stay at the real total throughout.
    #[test]
    fn ndmprog_eight_concurrent_fragments_stay_at_real_size() {
        // fragment sizes in bytes (sum 18,000,000)
        let sizes: [u64; 8] = [
            3_000_000, 1_000_000, 2_000_000, 4_000_000, 1_000_000, 2_000_000, 3_000_000,
            2_000_000,
        ];
        let real: u64 = sizes.iter().sum();
        let mut tr = NdmProgTracker::default();
        // three reporting rounds: each fragment reports 1/3, 2/3,
        // then all of its cumulative bytes, interleaved
        for round in 1..=3u64 {
            for (f, sz) in sizes.iter().enumerate() {
                let d = sz * round / 3;
                let line = format!(
                    "{d}|{sz}|NA|500000|{f}|8|downloading|/dl/vid.f137.mp4"
                );
                tr.apply(parse_ndmprog(&line));
                // not-yet-reported fragments are extrapolated
                // from the mean of the known ones, which can
                // transiently overshoot a little (bounded well
                // under 1.5×) — never the old ~37×
                assert!(
                    tr.total() <= real * 3 / 2,
                    "total {} ballooned past the real {real}",
                    tr.total()
                );
                assert!(tr.done() <= real);
                if round == 1 && f == sizes.len() - 1 {
                    // every fragment has reported once: all
                    // fragment totals are known, so the total is
                    // already exact
                    assert_eq!(tr.total(), real);
                }
            }
        }
        for (f, sz) in sizes.iter().enumerate() {
            let line = format!("{sz}|{sz}|NA|NA|{f}|8|finished|/dl/vid.f137.mp4");
            tr.apply(parse_ndmprog(&line));
        }
        assert_eq!(tr.done(), real);
        assert_eq!(tr.total(), real);
        assert_eq!(tr.parts_sum(), real);
    }

    /// M-15 control: the 6-fragment sequential shape that already
    /// worked before the fix must keep working — fragments report
    /// and finish one at a time.
    #[test]
    fn ndmprog_six_fragment_control_stays_exact() {
        let mut tr = NdmProgTracker::default();
        let frag = 1_000_000u64;
        for f in 0..6u64 {
            for d in [250_000, 500_000, 750_000, frag] {
                let line = format!(
                    "{d}|{frag}|NA|800000|{f}|6|downloading|/dl/clip.f0.mp4"
                );
                tr.apply(parse_ndmprog(&line));
                assert!(tr.total() <= 6 * frag, "total overshot: {}", tr.total());
            }
            let line = format!("{frag}|{frag}|NA|NA|{f}|6|finished|/dl/clip.f0.mp4");
            tr.apply(parse_ndmprog(&line));
            assert_eq!(tr.done(), (f + 1) * frag);
        }
        assert_eq!(tr.total(), 6 * frag);
        assert_eq!(tr.parts_sum(), 6 * frag);
    }

    /// M-15, the hunt's live shape: yt-dlp's AGGREGATE fragment
    /// state (per-file cumulative downloaded_bytes, NO per-report
    /// total, a total_bytes_estimate whose formula spikes while
    /// the first of 49 fragments are still in flight), with the
    /// video and audio files' reports interleaved. Real parts:
    /// 150,220,105 + 4,251,008 = 154,471,113 bytes. The old
    /// parser's journal read 5,707,844,548 total / 5,686,229,871
    /// done for this download.
    #[test]
    fn ndmprog_aggregate_fragments_never_inflate() {
        let mut tr = NdmProgTracker::default();
        let real = 154_471_113u64;
        let report = |d: u64, est: u64, idx: u64, count: u64, file: &str| {
            format!("{d}|NA|{est}|3000000|{idx}|{count}|downloading|{file}")
        };
        // the spike: 8 fragments in flight, none finished — the
        // estimate formula divides ~24 MB by (0 + 1) and scales
        // by 49
        tr.apply(parse_ndmprog(&report(
            24_000_000,
            1_176_000_000,
            0,
            49,
            "/dl/manifest.f0.mp4",
        )));
        assert!(
            tr.total() <= real,
            "the early estimate spike must not become the total (got {})",
            tr.total()
        );
        // fragments start finishing; the estimate converges
        tr.apply(parse_ndmprog(&report(
            60_000_000,
            183_000_000,
            12,
            49,
            "/dl/manifest.f0.mp4",
        )));
        tr.apply(parse_ndmprog(&report(
            120_000_000,
            155_000_000,
            30,
            49,
            "/dl/manifest.f0.mp4",
        )));
        // the audio file reports into the same stream
        tr.apply(parse_ndmprog(&report(
            2_000_000,
            4_400_000,
            1,
            3,
            "/dl/manifest.f1.m4a",
        )));
        assert!(tr.total() <= 160_000_000, "total drifted: {}", tr.total());
        assert!(tr.done() <= real);
        // both files finish with exact totals
        tr.apply(parse_ndmprog(
            "150220105|150220105|NA|NA|NA|NA|finished|/dl/manifest.f0.mp4",
        ));
        tr.apply(parse_ndmprog(
            "4251008|4251008|NA|NA|NA|NA|finished|/dl/manifest.f1.m4a",
        ));
        assert_eq!(tr.done(), real);
        assert_eq!(tr.total(), real);
        assert_eq!(tr.parts_sum(), real);
    }

    /// M-15: a format-level size from the probe outranks fragment
    /// estimates for the whole run.
    #[test]
    fn ndmprog_format_total_outranks_estimates() {
        let mut tr = NdmProgTracker::default();
        tr.set_format_total(Some(154_471_113), false);
        tr.apply(parse_ndmprog(
            "24000000|NA|1176000000|3000000|0|49|downloading|/dl/manifest.f0.mp4",
        ));
        assert_eq!(tr.total(), 154_471_113);
        assert_eq!(tr.done(), 24_000_000);
    }

    /// V3-a (208B): with NO trustworthy total anywhere — the
    /// combined job's template lines carry no explicit total and
    /// no qualified estimate — the tracker total stays UNKNOWN
    /// (0) while downloaded climbs. The pre-fix fallback returned
    /// the downloaded count as the total, so the displayed total
    /// ratcheted upward in lockstep with downloaded at a pinned
    /// 100%. A finished report's exact total is the correction
    /// point, and only there.
    #[test]
    fn ndmprog_unknown_total_stays_zero_never_ratchets() {
        let mut tr = NdmProgTracker::default();
        for d in [1_000_000u64, 5_000_000, 12_000_000, 20_000_000] {
            tr.apply(parse_ndmprog(&format!(
                "{d}|NA|NA|3000000|0|49|downloading|/dl/clip.f137.mp4"
            )));
            assert_eq!(tr.done(), d);
            assert_eq!(
                tr.total(),
                0,
                "unknown total must stay 0, not chase downloaded ({d})"
            );
        }
        // the file finishes and reports its exact total: the
        // tracker total corrects to it at completion
        tr.apply(parse_ndmprog(
            "20000000|20000000|NA|NA|NA|NA|finished|/dl/clip.f137.mp4",
        ));
        assert_eq!(tr.total(), 20_000_000);
        assert_eq!(tr.parts_sum(), 20_000_000);
    }

    /// V3-a (208B): the seeded job total (the picked option's
    /// 24,393,000-byte row for the incident video, seeded exactly
    /// as the spawn path seeds `expect_size` when the resolve
    /// learned no format size) stands for the whole run over
    /// interleaved cumulative reports from both formats (video via
    /// 6 connections, audio via 2) whose template lines carry no
    /// explicit total — downloaded converges to it, the total
    /// never moves, and the finished reports' parts sum confirms
    /// it at completion.
    #[test]
    fn ndmprog_seeded_job_total_never_ratchets() {
        let job_total = 24_393_000u64;
        let video = 20_000_000u64;
        let audio = job_total - video;
        let mut tr = NdmProgTracker::default();
        tr.set_format_total(Some(job_total), false);
        for step in 1..=4u64 {
            let dv = video * step / 4;
            let da = audio * step / 4;
            tr.apply(parse_ndmprog(&format!(
                "{dv}|NA|NA|3000000|0|49|downloading|/dl/clip.f137.mp4"
            )));
            tr.apply(parse_ndmprog(&format!(
                "{da}|NA|NA|500000|0|3|downloading|/dl/clip.f140.m4a"
            )));
            assert_eq!(tr.total(), job_total, "total moved at step {step}");
            assert_eq!(tr.done(), dv + da);
            assert!(tr.done() <= tr.total());
        }
        tr.apply(parse_ndmprog(&format!(
            "{video}|{video}|NA|NA|NA|NA|finished|/dl/clip.f137.mp4"
        )));
        tr.apply(parse_ndmprog(&format!(
            "{audio}|{audio}|NA|NA|NA|NA|finished|/dl/clip.f140.m4a"
        )));
        assert_eq!(tr.done(), job_total);
        assert_eq!(tr.parts_sum(), job_total);
        assert_eq!(tr.total(), job_total);
    }

    /// 208B classification guard, the incident shape: aggregate
    /// state-dict reports (downloaded_bytes = FILE-cumulative)
    /// that carry a fragment-sized total_bytes (fragment 0's
    /// content length, 82,047) plus a fragment index. The first
    /// report is ambiguous (40,000 <= 82,047 fits both readings),
    /// but once the cumulative count passes a fragment's own
    /// total the aggregate reading is PROVEN — the tracker must
    /// reclassify, discard the snapshot slots, and count bytes
    /// truthfully from then on. And no fragment-bearing report's
    /// total may ever stand as the file's total.
    #[test]
    fn ndmprog_aggregate_dict_with_fragment_totals_reclassifies() {
        let mut tr = NdmProgTracker::default();
        let f = "/dl/clip.f137.mp4";
        tr.apply(parse_ndmprog(&format!("40000|82047|NA|3000000|0|NA|downloading|{f}")));
        // ambiguous so far: one slot, no runaway yet
        assert_eq!(tr.done(), 40_000);
        tr.apply(parse_ndmprog(&format!("82047|82047|NA|3000000|0|NA|downloading|{f}")));
        assert_eq!(tr.done(), 82_047);
        // the cumulative count passes fragment 1's own size:
        // aggregate is proven, the snapshot sum must die here
        tr.apply(parse_ndmprog(&format!("2000000|300000|NA|3000000|1|49|downloading|{f}")));
        assert_eq!(tr.done(), 2_000_000, "done must be the true cumulative count");
        tr.apply(parse_ndmprog(&format!("9000000|280000|NA|3000000|3|49|downloading|{f}")));
        assert_eq!(tr.done(), 9_000_000);
        assert!(
            tr.total() != 82_047,
            "a fragment's size must never stand as the file's total"
        );
        // the file's own finished report: exact total at last
        tr.apply(parse_ndmprog(&format!(
            "20000000|20000000|NA|NA|NA|NA|finished|{f}"
        )));
        assert_eq!(tr.done(), 20_000_000);
        assert_eq!(tr.total(), 20_000_000);
        assert_eq!(tr.parts_sum(), 20_000_000);
    }

    /// 208B classification guard, first-report proof: a deciding
    /// report whose downloaded already exceeds its fragment-sized
    /// total is Aggregate from the very first line — no slot is
    /// ever created for it.
    #[test]
    fn ndmprog_first_report_downloaded_beyond_fragment_total_is_aggregate() {
        let mut tr = NdmProgTracker::default();
        tr.apply(parse_ndmprog(
            "5000000|300000|NA|3000000|7|49|downloading|/dl/clip.f137.mp4",
        ));
        assert_eq!(tr.done(), 5_000_000);
        assert_eq!(tr.total(), 0, "no trustworthy total exists yet");
        tr.apply(parse_ndmprog(
            "9000000|280000|NA|3000000|9|49|downloading|/dl/clip.f137.mp4",
        ));
        assert_eq!(tr.done(), 9_000_000, "snapshots must never be summed");
    }

    /// Legacy 4-field NDMPROG lines (the pre-M-15 template) still
    /// parse into a sane single-file accounting.
    #[test]
    fn ndmprog_legacy_lines_still_parse() {
        let r = parse_ndmprog("500|NA|NA|NA");
        assert_eq!(r.downloaded, Some(500));
        let mut tr = NdmProgTracker::default();
        tr.apply(parse_ndmprog("500|1000|NA|2000"));
        tr.apply(parse_ndmprog("1000|1000|NA|NA|NA|NA|finished|"));
        assert_eq!(tr.done(), 1000);
        assert_eq!(tr.total(), 1000);
        assert_eq!(tr.parts_sum(), 1000);
    }

    /// An NA-filename report lands in the "" bucket; in Fragment
    /// mode that bucket cannot prove itself finished, so it must
    /// not veto all_finished once a named file has finished —
    /// but a run with ONLY the "" bucket keeps the plain rule.
    #[test]
    fn ndmprog_empty_key_bucket_does_not_veto_named_finish() {
        let mut tr = NdmProgTracker::default();
        // "" bucket, Fragment mode, unfinished (1 of 6 fragments in)
        tr.apply(parse_ndmprog("500|1000|NA|3000000|0|6|downloading|NA"));
        tr.apply(parse_ndmprog(
            "2000|2000|NA|NA|NA|NA|finished|/dl/clip.mp4",
        ));
        assert!(tr.all_finished());
        // sole "" bucket, still unfinished: not finished
        let mut anon = NdmProgTracker::default();
        anon.apply(parse_ndmprog("500|1000|NA|3000000|0|6|downloading|NA"));
        assert!(!anon.all_finished());
    }

    /// M-15 finish line: the W5-4 case end to end — the merged
    /// file is byte-valid, the add-time expectation is a poisoned
    /// 5.7 GB journal figure, and the run's own parts sum proves
    /// the file is exactly what was downloaded. No error.
    #[test]
    fn completion_size_error_trusts_parts_over_poisoned_expectation() {
        // W5-4 verbatim numbers
        assert!(completion_size_error(
            5_707_844_548,
            None,
            false,
            154_471_113,
            155_201_613,
            true,
        )
        .is_none());
        // a parts mismatch ALONE is not an error either (R3: a
        // padded container legitimately remuxes down to a small
        // fraction of its bytes) — the stub-merge guard is
        // drastic_shrink against the media payload, upstream
        assert!(completion_size_error(0, None, false, 154_471_113, 2_342_828, true)
            .is_none());
        // an EXACT format size is a trustworthy expectation
        assert!(completion_size_error(0, Some(154_471_113), true, 0, 155_201_613, true)
            .is_none());
        assert!(completion_size_error(0, Some(154_471_113), true, 0, 2_342_828, true)
            .is_some());
        // an ESTIMATE-only format size never brands a sane file…
        assert!(completion_size_error(
            5_707_844_548,
            Some(160_000_000),
            false,
            0,
            155_201_613,
            true,
        )
        .is_none());
        // …but without stream/duration sanity the legacy rule
        // still applies to it
        assert!(completion_size_error(
            5_707_844_548,
            Some(160_000_000),
            false,
            0,
            155_201_613,
            false,
        )
        .is_some());
        // the legacy rule is unchanged when nothing better exists
        assert!(completion_size_error(100_663_296, None, false, 0, 2_342_828, true)
            .is_some());
        assert!(completion_size_error(100_663_296, None, false, 0, 98_000_000, true)
            .is_none());
        assert!(completion_size_error(0, None, false, 0, 0, true).is_none());
    }

    /// M-4: the adoption gate for a completed final found on disk.
    #[test]
    fn adoptable_final_requires_a_consistent_size() {
        // the hunt's kept merge: 155,201,613-byte file against the
        // journal's 154,471,113-byte download total (container
        // overhead makes the merge slightly LARGER than its parts)
        assert!(adoptable_final(155_201_613, 154_471_113));
        assert!(adoptable_final(154_471_113, 154_471_113));
        // an unknown expectation adopts nothing
        assert!(!adoptable_final(155_201_613, 0));
        assert!(!adoptable_final(0, 154_471_113));
        // a plainly different file is not adopted on size trust
        assert!(!adoptable_final(10_000_000, 154_471_113));
        assert!(!adoptable_final(400_000_000, 154_471_113));
        // a poisoned (inflated) expectation adopts nothing either —
        // the job falls back to downloading, the safe direction
        assert!(!adoptable_final(155_201_613, 5_707_844_548));
    }

    #[test]
    fn formats_filesize_prefers_exact_then_approx() {
        let v = serde_json::json!({"formats": [
            {"filesize": 150_220_105},
            {"filesize": 4_251_008}
        ]});
        let formats: Vec<&serde_json::Value> =
            v["formats"].as_array().unwrap().iter().collect();
        assert_eq!(formats_filesize(&formats), (Some(154_471_113), true));
        // one exact + one approx → the sum is an estimate
        let v2 = serde_json::json!({"formats": [
            {"filesize": 150_220_105},
            {"filesize_approx": 4_300_000}
        ]});
        let formats2: Vec<&serde_json::Value> =
            v2["formats"].as_array().unwrap().iter().collect();
        assert_eq!(formats_filesize(&formats2), (Some(154_520_105), false));
        // a format with neither → unknown, never a partial sum
        let v3 = serde_json::json!({"formats": [
            {"filesize": 150_220_105},
            {"ext": "m4a"}
        ]});
        let formats3: Vec<&serde_json::Value> =
            v3["formats"].as_array().unwrap().iter().collect();
        assert_eq!(formats_filesize(&formats3), (None, false));
        assert_eq!(formats_filesize(&[]), (None, false));
    }

    #[test]
    fn direct_format_wins_over_playlist_variants() {
        // the W-2 fixture's shape: a direct <video> file plus a
        // script-planted HLS ladder in one probe
        let v = serde_json::json!({
            "formats": [
                {"format_id": "hls-720", "ext": "mp4", "protocol": "m3u8_native",
                 "url": "http://cdn.example/hls/v720/pl.m3u8", "height": 720, "vcodec": "avc1"},
                {"format_id": "0", "ext": "mp4", "protocol": "http",
                 "url": "http://cdn.example/media/big.mp4", "vcodec": null, "acodec": null},
                {"format_id": "hls-360", "ext": "mp4", "protocol": "m3u8_native",
                 "url": "http://cdn.example/hls/v360/pl.m3u8", "height": 360, "vcodec": "avc1"}
            ]
        });
        let direct = best_direct_format(&v).expect("a direct format exists");
        assert_eq!(direct["url"].as_str(), Some("http://cdn.example/media/big.mp4"));
        assert!(format_is_playlist(&v["formats"][0]));
        assert!(!format_is_playlist(&v["formats"][1]));
        // a list with only playlists yields no direct candidate
        let v2 = serde_json::json!({
            "formats": [
                {"format_id": "hls-720", "ext": "mp4", "protocol": "m3u8_native",
                 "url": "http://cdn.example/hls/v720/pl.m3u8", "height": 720, "vcodec": "avc1"}
            ]
        });
        assert!(best_direct_format(&v2).is_none());
        // an audio-only direct entry is not the page's video
        let v3 = serde_json::json!({
            "formats": [
                {"format_id": "a1", "ext": "mp4", "protocol": "https",
                 "url": "https://cdn.example/audio.m4a", "vcodec": "none", "acodec": "aac"}
            ]
        });
        assert!(best_direct_format(&v3).is_none());
    }

    #[test]
    fn password_signature_separates_passwords() {
        // m-6: the resolution cache key must change when the
        // password does, and stay stable when it doesn't
        assert_eq!(password_signature(""), 0);
        assert_eq!(password_signature("hunter2"), password_signature("hunter2"));
        assert_ne!(password_signature("hunter2"), password_signature("hunter3"));
        assert_ne!(password_signature("hunter2"), password_signature(""));
    }

    #[test]
    fn duration_parses_from_ffmpeg_info() {
        let text = "  Duration: 00:00:06.01, start: 0.000000, bitrate: 134217 kb/s\n";
        assert_eq!(duration_in_ffmpeg_info(text), Some(6.01));
        let text2 = "Duration: 01:02:03.50, start: 0.000000";
        assert_eq!(duration_in_ffmpeg_info(text2), Some(3723.5));
        assert_eq!(duration_in_ffmpeg_info("Duration: N/A"), None);
        assert_eq!(duration_in_ffmpeg_info("no duration here"), None);
    }

    fn bare_job() -> VideoJob {
        VideoJob {
            url: "https://example.com/v".to_string(),
            dir: PathBuf::from("/tmp"),
            selector: String::new(),
            audio: false,
            title: "t".to_string(),
            cookies: None,
            progress: Arc::new(Mutex::new(VideoProgress::default())),
            chosen_stem: Mutex::new(None),
            child: Mutex::new(None),
            active_downloads: Mutex::new(Vec::new()),
            stop_flag: Mutex::new(None),
            spawn_lock: Mutex::new(()),
            gen: std::sync::atomic::AtomicU64::new(0),
            alive: std::sync::atomic::AtomicBool::new(false),
            expect_audio: Mutex::new(false),
            expected_media: Mutex::new(None),
            pair_audio_url: Mutex::new(None),
            limit_bps: std::sync::atomic::AtomicU64::new(0),
            login: String::new(),
            password: String::new(),
            merged_final: Mutex::new(None),
            format_size: Mutex::new(None),
            format_size_exact: Mutex::new(false),
            downloaded_total: Mutex::new(0),
            recorded_final: None,
            preresolved: None,
            expect_size: 0,
            expect_duration: 0.0,
        }
    }

    #[test]
    fn pause_during_probing_and_recovery_parks_the_job() {
        // 2.3.1 (bug 4): Pause must work while the job is still
        // resolving ("starting" — the field recording's Probing
        // state, where the dialog's Pause button was disabled)
        // and in the stall-recovery gap ("resuming", no streams
        // active): the stop flag is set for the resolve/runner
        // to discard its result, and the status flips at once.
        let job = bare_job();
        job.progress.lock().unwrap().status = "starting".to_string();
        job.pause();
        assert_eq!(job.progress.lock().unwrap().status, "paused");
        assert_eq!(
            job.stop_flag.lock().unwrap().as_deref(),
            Some("paused"),
            "the in-flight resolve must be told to discard"
        );
        let job = bare_job();
        job.progress.lock().unwrap().status = "resuming".to_string();
        job.pause();
        assert_eq!(job.progress.lock().unwrap().status, "paused");
        assert_eq!(job.stop_flag.lock().unwrap().as_deref(), Some("paused"));
    }

    #[test]
    fn is_settled_waits_for_the_spawn_thread_not_just_the_status() {
        // 2.0.7 #18: cancel() flips the status to "cancelled"
        // immediately, but an ffmpeg merge running on the spawn
        // thread is still writing the final file — a job in that
        // shape must NOT report settled, or delete sweeps race
        // the merge (and the "deleted" file reappears on
        // Windows when the merge lands)
        let job = bare_job();
        assert!(job.is_settled(), "an idle job is settled");
        job.alive
            .store(true, std::sync::atomic::Ordering::SeqCst);
        job.progress.lock().unwrap().status = "cancelled".to_string();
        assert!(
            !job.is_settled(),
            "cancelled status + live spawn thread (merge) is not settled"
        );
        job.alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(job.is_settled(), "thread gone: settled");
    }
}

