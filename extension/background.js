'use strict';
/* Nill Download Manager: background service worker.
 *
 * Communicates with the desktop app through the Native Messaging
 * host "com.ndm.host":
 *   1. { type: "probe", url, cookies } -> returns { ok: true, info } with
 *      thumbnail, title, duration, and quality options + MB sizes for the
 *      in-browser IDM video popup.
 *   2. { type: "download", kind: "video_direct" | "video" | "file", ... }
 *      -> queues the download and launches/raises the NDM desktop app.
 */

const HOST_NAME = 'com.ndm.host';

/* Map a raw native-messaging failure to the words the user sees.
   Chrome's own text for a host that launched but whose channel
   broke ("Error when communicating with the native messaging
   host") names no cause and no next step; the one action that
   heals the common cases is opening the app (it re-registers the
   bridge and starts the current host). Anything else passes
   through verbatim — inventing friendly text for errors we do
   not understand would hide real faults. */
function friendlyHostError(text) {
  if (/Error when communicating/i.test(text || '')) {
    return "NDM's browser bridge stopped unexpectedly. Open the NDM app once, then try again.";
  }
  return text || 'Could not reach NDM native host.';
}

/* The shared Facebook module (fb-classify.js): the classifier and
   the pure per-video grouping decisions the recorder below keys
   its captures by. Loaded once here; if it somehow fails, FBC
   stays null and the grouping fallback errs toward NOT offering
   (unknown key) on Facebook rather than pooling a feed's videos. */
let FBC = null;
try {
  importScripts('fb-classify.js');
  FBC = typeof NdmFbClassify !== 'undefined' ? NdmFbClassify : null;
} catch (e) {}
// Firefox runs the background as an event page, not a service
// worker: importScripts does not exist there, and the Firefox
// manifest preloads fb-classify.js as a background script
// instead — take the classifier from that shared global.
if (!FBC && typeof NdmFbClassify !== 'undefined') FBC = NdmFbClassify;

/* Streaming services whose videos are DRM-protected — the same list
   content.js uses to suppress its panel. The background layer must
   honour it too: no stream recording and no page-video action on
   these hosts. */
const DRM_HOSTS = [
  'netflix.com',
  'primevideo.com',
  'disneyplus.com',
  'hotstar.com',
  'open.spotify.com',
];

function isDrmUrl(url) {
  if (!url) return false;
  let h;
  try {
    h = new URL(url).hostname.toLowerCase();
  } catch (e) {
    return false;
  }
  return DRM_HOSTS.some(d => h === d || h.endsWith('.' + d));
}

/* ------------------------------------------------------------------ */
/* URL classification                                                  */
/* ------------------------------------------------------------------ */

const VIDEO_HOST_PATTERNS = [
  /(^|\.)youtube\.com\/(watch|shorts\/|live\/|embed\/)/,
  /(^|\.)youtu\.be\//,
  /(^|\.)facebook\.com\/.*(videos\/|watch|reel\/|share\/(r|v)\/|watch\/?\?v=)/,
  /(^|\.)fb\.watch\//,
  /(^|\.)vimeo\.com\/\d+/,
  // Embedded-player shapes (F4): player.vimeo.com/video/<id> and
  // dailymotion.com/embed/video/<id> carry the id behind a path
  // segment, so the plain patterns above never named them — an
  // iframe pill relay from those frames arrived targetless.
  /(^|\.)vimeo\.com\/video\/\d+/,
  /(^|\.)dailymotion\.com\/video\//,
  /(^|\.)dailymotion\.com\/embed\/video\//,
  /(^|\.)instagram\.com\/(reel|p|tv)\//,
  /(^|\.)tiktok\.com\/(@[^/]+\/video\/|t\/)/,
  /(^|\.)(twitter|x)\.com\/[^/]+\/status\/\d+/,
  /(^|\.)twitch\.tv\/(videos\/\d+|[^/]+\/clip\/)/,
  /(^|\.)reddit\.com\/r\/[^/]+\/comments\//,
  /(^|\.)bilibili\.com\/video\//,
];

function isVideoPageUrl(url) {
  if (!url) return false;
  let s;
  try {
    const u = new URL(url);
    s = u.hostname + u.pathname + u.search;
  } catch (e) {
    return false;
  }
  return VIDEO_HOST_PATTERNS.some(re => re.test(s));
}

// Known downloadable file extensions for browser-download interception.
// IDM's default File Types list is the base set (its MPG/MPEG/ASF/RM…
// were missing here, so plain .mpg links were never caught); NDM's
// own additions (deb, apk, torrent, disk images…) stay. TXT/CSV are
// deliberately NOT intercepted — IDM leaves them to the browser.
const FILE_EXTENSIONS = [
  'zip', 'rar', '7z', 'tar', 'gz', 'gzip', 'bz2', 'xz', 'zst', 'tgz', 'tbz2', 'txz', 'cab', 'ace', 'arj', 'lzh', 'sit', 'sitx',
  'exe', 'msi', 'msix', 'msixbundle', 'deb', 'rpm', 'dmg', 'pkg', 'iso', 'img', 'apk', 'bin', 'run', 'appimage', 'flatpakref', 'msu',
  'pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'pps', 'rtf', 'epub', 'mobi', 'tif', 'tiff',
  'mp3', 'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac', 'aac', 'ogg', 'ogv', 'm4a', 'm4b', 'opus', 'wav', 'wma', 'flv', 'wmv', '3gp', 'm4v', 'ts',
  'mpg', 'mpeg', 'mpa', 'mpe', 'asf', 'rm', 'rmvb', 'aif',
  'torrent', 'jar', 'war', 'whl', 'vmdk', 'qcow2', 'vdi',
];

function extensionOf(urlOrName) {
  if (!urlOrName) return '';
  const clean = urlOrName.split(/[?#]/)[0];
  const dot = clean.lastIndexOf('.');
  if (dot < 0) return '';
  return clean.slice(dot + 1).toLowerCase();
}

function filenameFromUrl(urlStr) {
  try {
    const u = new URL(urlStr);
    for (const [k, v] of u.searchParams.entries()) {
      if (/file|name|attachment|download/i.test(k) && v.includes('.')) {
        return v;
      }
    }
  } catch (e) {}
  return '';
}

function isDownloadableFile(urlOrName) {
  if (!urlOrName) return false;
  return FILE_EXTENSIONS.includes(extensionOf(urlOrName)) ||
    FILE_EXTENSIONS.includes(extensionOf(filenameFromUrl(urlOrName)));
}

/* ------------------------------------------------------------------ */
/* Media stream sniffer (HLS .m3u8 / DASH .mpd / direct video streams) */
/* ------------------------------------------------------------------ */

const tabMediaStreams = new Map();
/* Last main-frame URL per tab: a stream is tagged with the page it
   was seen on, so one tab's registry can tell two videos apart
   (two players on one page, or SPA navigation between videos). */
const tabPageUrls = new Map();
/* W2-1: rehydrate tabPageUrls on service-worker startup. MV3
   workers are killed after ~30 s idle and may restart with
   tabs already open; without this, media requests from those
   tabs arrive while the map is empty and their page is
   UNKNOWN — FB CDN captures then stay unkeyed by design (see
   streamGroupKey) instead of being grouped, and the DRM gate
   sees an empty page. Seeding from chrome.tabs closes most of
   that window; only fill tabs the map does not already know,
   so a live navigation is never overwritten by a stale seed. */
try {
  const seedTabs = tabs => {
    for (const t of tabs || []) {
      if (t && t.id != null && t.url && !tabPageUrls.has(t.id)) {
        tabPageUrls.set(t.id, t.url);
      }
    }
  };
  const q = chrome.tabs.query({}, seedTabs);
  if (q && typeof q.catch === 'function') q.catch(() => {});
} catch (e) {}
const STREAM_TTL_MS = 15 * 60 * 1000;
const STREAMS_PER_TAB = 12;

function isSubVariantPlaylist(url) {
  return /(chunklist|variant|rendition|audio[_-]|video[_-]|segment|frag)/i.test(url);
}

function recordTabStream(tabId, url) {
  if (tabId == null || tabId < 0 || !url) return;
  if (!/^https?:\/\//i.test(url)) return;
  // Ignore YouTube/Facebook raw segment requests because yt-dlp handles page URLs directly
  if (/googlevideo\.com\/videoplayback|fbcdn\.net.*bytestart=/i.test(url)) return;
  const page = tabPageUrls.get(tabId) || '';
  let list = tabMediaStreams.get(tabId) || [];
  list = list.filter(e => e.url !== url && Date.now() - e.ts < STREAM_TTL_MS);
  // A master-looking manifest stays ahead of sub-variant playlists:
  // the newest response is very often just one variant of the show.
  if (isSubVariantPlaylist(url)) {
    list.push({ url, page, ts: Date.now() });
  } else {
    list.unshift({ url, page, ts: Date.now() });
  }
  tabMediaStreams.set(tabId, list.slice(0, STREAMS_PER_TAB));
  if (tabMediaStreams.size > 300) {
    tabMediaStreams.delete(tabMediaStreams.keys().next().value);
  }
  // A sniffed stream is media worth offering the panel for, even
  // before the content script's own report lands (F-EXT-15).
  if (!tabHasMedia.get(tabId)) {
    tabHasMedia.set(tabId, true);
    applyPageVideoMenuVisibility();
  }
}

function tabStreamList(tabId, preferPage, preferKey) {
  const list = (tabMediaStreams.get(tabId) || [])
    .filter(e => Date.now() - e.ts < STREAM_TTL_MS);
  // Streams seen on the page the caller is asking about come first:
  // a tab's older streams (previous SPA video, another player) must
  // not outrank the current page's own.
  const pref = [];
  const rest = [];
  for (const e of list) {
    if (preferPage && (e.page === preferPage || e.page === preferKey)) pref.push(e.url);
    else rest.push(e.url);
  }
  return pref.concat(rest);
}

/* Pick the stream URL that actually describes the whole show: fetch
   the candidate manifests and prefer the one whose CONTENT is a
   master playlist (#EXT-X-STREAM-INF) — a variant URL handed to the
   probe collapses the quality ladder to that single variant. */
const masterPickCache = new Map();
async function pickMasterStream(tabId, preferPage, preferKey) {
  const list = tabStreamList(tabId, preferPage, preferKey);
  if (!list.length) return '';
  const manifests = list.filter(u => /\.m3u8(\?|$)/i.test(u.split('#')[0]));
  if (!manifests.length) return list[0];
  const cacheKey = tabId + '|' + (preferPage || '') + '|' + manifests[0];
  const cached = masterPickCache.get(cacheKey);
  if (cached && Date.now() - cached.ts < 60 * 1000) return cached.url;
  let fallback = '';
  for (const u of manifests.slice(0, 4)) {
    try {
      // Referer-gated CDNs 403 manifest fetches that arrive with
      // no page context, and the quality ladder then silently
      // collapses to manifests[0]. The Referer header itself is
      // a forbidden fetch header, so the page's URL rides
      // through the referrer option instead (full URL — this is
      // the user's own page making the request).
      const page = tabPageUrls.get(tabId) || '';
      const r = await fetch(u, {
        credentials: 'include',
        referrer: page || undefined,
        referrerPolicy: 'unsafe-url',
      });
      if (!r.ok) continue;
      const text = await r.text();
      if (!fallback) fallback = u;
      if (text.includes('#EXT-X-STREAM-INF')) {
        masterPickCache.set(cacheKey, { url: u, ts: Date.now() });
        return u;
      }
    } catch (e) {}
  }
  const chosen = fallback || manifests[0];
  masterPickCache.set(cacheKey, { url: chosen, ts: Date.now() });
  return chosen;
}

/* ------------------------------------------------------------------ */
/* Browser-capture door (2.3.0): passively record the media streams  */
/* the browser itself plays, per tab, and hand them to the app ONLY  */
/* when the user clicks the capture action. The yt-dlp sniffer above */
/* deliberately skips googlevideo (yt-dlp resolves those pages       */
/* itself); this registry exists for the opposite case — pages the  */
/* browser can play but yt-dlp cannot fully read (above all          */
/* pre-Windows-10, where no JS runtime can run). Nothing is sent     */
/* anywhere from here on its own: recording is memory-only.          */
/* ------------------------------------------------------------------ */

const tabCaptured = new Map(); // tabId -> { page, streams: Map<url, stream> }
const CAPTURE_TTL_MS = 30 * 60 * 1000;
const CAPTURE_PER_TAB = 24;

/* YouTube format ids (itag) → [quality label, kind], kind being
   'av' (progressive: audio+video), 'v' (video only) or 'a' (audio
   only). Unknown itags fall back to the stream's own mime param. */
const CAPTURE_ITAGS = {
  18: ['360p', 'av'], 22: ['720p', 'av'], 37: ['1080p', 'av'],
  59: ['480p', 'av'], 78: ['270p', 'av'],
  133: ['240p', 'v'], 134: ['360p', 'v'], 135: ['480p', 'v'],
  136: ['720p', 'v'], 137: ['1080p', 'v'], 160: ['144p', 'v'],
  264: ['1440p', 'v'], 266: ['2160p', 'v'],
  242: ['240p', 'v'], 243: ['360p', 'v'], 244: ['480p', 'v'],
  247: ['720p', 'v'], 248: ['1080p', 'v'], 278: ['144p', 'v'],
  394: ['144p', 'v'], 395: ['240p', 'v'], 396: ['360p', 'v'],
  397: ['480p', 'v'], 398: ['720p', 'v'], 399: ['1080p', 'v'],
  400: ['1440p', 'v'], 401: ['2160p', 'v'],
  139: ['', 'a'], 140: ['', 'a'], 141: ['', 'a'],
  171: ['', 'a'], 172: ['', 'a'],
  249: ['', 'a'], 250: ['', 'a'], 251: ['', 'a'],
};

const CAPTURE_EXT_MIME = {
  mp4: 'video/mp4', webm: 'video/webm', mov: 'video/quicktime',
  m4v: 'video/mp4', mkv: 'video/x-matroska',
  m4a: 'audio/mp4', mp3: 'audio/mpeg', ogg: 'audio/ogg',
  opus: 'audio/ogg', aac: 'audio/aac', flac: 'audio/flac', wav: 'audio/wav',
};

/* One stream URL names ONE stream: the player re-requests it per
   chunk with per-request range parameters that are not part of
   the signed identity — strip exactly those, so the chunks of a
   stream dedupe to a single entry (and the URL the app downloads
   carries no stale chunk window). */
function canonicalStreamUrl(raw) {
  try {
    const u = new URL(raw);
    for (const p of ['range', 'rn', 'rbuf']) u.searchParams.delete(p);
    return u.toString();
  } catch (e) {
    return raw;
  }
}

/* The per-video grouping key for one recorded stream (2.3.2
   WS-B, audit-232 §2 prerequisite): on Facebook one tab page
   holds MANY videos, so a stream may only ever be offered under
   ITS video's key — the page's permalink id, or the video token
   in the stream's own fbcdn file name. No derivable key → ''
   (unknown): the stream is recorded but NEVER offered or
   grouped, so it cannot pair with another video's streams under
   a shared feed identity. Off Facebook the standing key is
   'page' — one page, one video, exactly today's behaviour.
   The derivation is fb-classify's streamGroupKey (unit-tested
   in extension-tests/); the inline branch only covers the
   module failing to load, and errs the same safe way. */
function capturedGroupKeyFor(pageUrl, streamUrl) {
  if (FBC) return FBC.streamGroupKey(pageUrl, streamUrl);
  let h = '';
  try {
    h = new URL(pageUrl || '').hostname.toLowerCase();
  } catch (e) {}
  if (h === 'facebook.com' || h.endsWith('.facebook.com') || h === 'fb.watch') return '';
  // W2-1 mirror of the classifier rule: unknown page + an FB
  // CDN stream is UNKNOWN, never the made-up 'page' identity.
  if (!h) {
    let sh = '';
    try {
      sh = new URL(streamUrl || '').hostname.toLowerCase();
    } catch (e) {}
    if (sh.endsWith('fbcdn.net') || sh.includes('videodelivery')) return '';
  }
  return 'page';
}

function recordCapturedStream(tabId, rawUrl, reqType) {
  if (tabId == null || tabId < 0 || !rawUrl) return;
  if (!/^https?:\/\//i.test(rawUrl)) return;
  const page = tabPageUrls.get(tabId) || '';
  // DRM pages: never record (the same rule as every other layer).
  if (isDrmUrl(page)) return;
  let parsed;
  try {
    parsed = new URL(rawUrl);
  } catch (e) {
    return;
  }
  let stream = null;
  const host = parsed.hostname.toLowerCase();
  if (
    (host === 'googlevideo.com' || host.endsWith('.googlevideo.com')) &&
    parsed.pathname.includes('/videoplayback')
  ) {
    // A sequenced (SABR) fetch is one segment of a stream that is
    // not directly downloadable as a whole — the honest answer
    // for those videos stays the yt-dlp door, so they are not
    // recorded here.
    if (parsed.searchParams.has('sq')) return;
    const mime = parsed.searchParams.get('mime') || '';
    const itag = Number(parsed.searchParams.get('itag') || 0);
    const known = CAPTURE_ITAGS[itag];
    let hasVideo;
    let hasAudio;
    let label = '';
    if (known) {
      label = known[0];
      hasVideo = known[1] !== 'a';
      hasAudio = known[1] !== 'v';
    } else if (mime.startsWith('audio/')) {
      hasVideo = false;
      hasAudio = true;
    } else if (mime.startsWith('video/')) {
      hasVideo = true;
      hasAudio = false;
    } else {
      return;
    }
    stream = {
      url: canonicalStreamUrl(rawUrl),
      mimeType: mime,
      qualityLabel: label,
      hasAudio,
      hasVideo,
      ts: Date.now(),
    };
  } else if (reqType === 'media') {
    // A media element playing a direct file: the element plays
    // it whole, so it carries its audio (when it has any) with
    // it; an audio-file extension is an audio-only stream.
    const clean = rawUrl.split(/[?#]/)[0].toLowerCase();
    const m = clean.match(/\.(mp4|webm|mov|m4v|mkv|m4a|mp3|ogg|opus|aac|flac|wav)$/);
    if (!m) return;
    const audioOnly = ['m4a', 'mp3', 'ogg', 'opus', 'aac', 'flac', 'wav'].includes(m[1]);
    stream = {
      url: rawUrl,
      mimeType: CAPTURE_EXT_MIME[m[1]] || '',
      qualityLabel: '',
      hasAudio: true,
      hasVideo: !audioOnly,
      ts: Date.now(),
    };
  }
  if (!stream) return;
  stream.groupKey = capturedGroupKeyFor(page, stream.url);
  let rec = tabCaptured.get(tabId);
  if (!rec || rec.page !== page) rec = { page, streams: new Map() };
  for (const [k, v] of rec.streams) {
    if (Date.now() - v.ts > CAPTURE_TTL_MS) rec.streams.delete(k);
  }
  // W2-11: set() on an existing URL must refresh its insertion
  // order (delete+set) — a continuously re-requested live
  // stream is the NEWEST entry, not the eviction candidate.
  rec.streams.delete(stream.url);
  rec.streams.set(stream.url, stream);
  while (rec.streams.size > CAPTURE_PER_TAB) {
    rec.streams.delete(rec.streams.keys().next().value);
  }
  // Same refresh at tab level, plus the tab-count cap its
  // sibling tabMediaStreams already has (300): growth was
  // bounded only by however many tabs stayed open.
  tabCaptured.delete(tabId);
  tabCaptured.set(tabId, rec);
  if (tabCaptured.size > 300) {
    tabCaptured.delete(tabCaptured.keys().next().value);
  }
}

/* The wire shape of one captured stream (no timestamps, no
   grouping key — the key travels at set level, see below). */
function wireCapturedStream(s) {
  return {
    url: s.url,
    mimeType: s.mimeType || '',
    qualityLabel: s.qualityLabel || '',
    hasAudio: !!s.hasAudio,
    hasVideo: !!s.hasVideo,
  };
}

/* The tab's captured streams partitioned into the groups that
   may be OFFERED (2.3.2 WS-B): one group per video key, via
   fb-classify's offerableGroups. Streams with an unknown key
   ('' — a Facebook stream whose video could not be determined)
   are in NO group: under a feed page identity they could pair
   with another video's streams, so they are never offered at
   all. Off Facebook every stream shares the 'page' key and the
   single group is exactly the pre-2.3.2 tab set. */
function capturedGroupsForTab(tabId) {
  const rec = tabCaptured.get(tabId);
  if (!rec) return [];
  const now = Date.now();
  const live = [...rec.streams.values()].filter(s => now - s.ts < CAPTURE_TTL_MS);
  if (!live.length) return [];
  if (FBC) {
    return FBC.offerableGroups(live).map(g => ({
      key: g.key,
      latestTs: g.latestTs,
      streams: g.streams.map(wireCapturedStream),
    }));
  }
  // Module unavailable: only the page group can be vouched for.
  const pageStreams = live.filter(s => s.groupKey === 'page').map(wireCapturedStream);
  return pageStreams.length
    ? [{ key: 'page', latestTs: now, streams: pageStreams }]
    : [];
}

/* The tab's captured set as the popup / send path consumes it:
   offerable streams only (unknown-key streams are invisible
   here by design), wire-shaped. */
function capturedStateForTab(tabId) {
  const rec = tabCaptured.get(tabId);
  if (!rec) return { count: 0, pageUrl: '', streams: [] };
  const streams = capturedGroupsForTab(tabId).flatMap(g => g.streams);
  return { count: streams.length, pageUrl: rec.page || '', streams };
}

/* Send one tab's captured set to the app through the host — the
   popup's capture click lands here. Nothing downloads yet: the
   app stores the set and opens its picker on the page.
   One send names ONE video: when the tab's groups span several
   videos (a Facebook feed), only the most recently played
   video's group goes, carrying its videoKey so the app stores
   it under that video's own identity. */
async function ndmSendCapturedForTab(tabId) {
  const groups = capturedGroupsForTab(tabId);
  if (!groups.length) {
    return { ok: false, error: 'Nothing captured on this tab yet — play the video first.' };
  }
  const group = groups.reduce((a, b) => (b.latestTs > a.latestTs ? b : a));
  let tab = null;
  try {
    tab = await chrome.tabs.get(tabId);
  } catch (e) {}
  const pageUrl = (tab && tab.url) || capturedStateForTab(tabId).pageUrl;
  const title = (tab && tab.title) || '';
  const cookies = await captureCookies(pageUrl, pageUrl);
  const msg = {
    type: 'captured_streams',
    pageUrl,
    title,
    streams: group.streams,
    cookies,
    userAgent: navigator.userAgent || '',
    ext_version: chrome.runtime.getManifest().version,
  };
  if (group.key.startsWith('fb:')) msg.videoKey = group.key;
  return sendCapturedNative(msg);
}

function sendCapturedNative(msg) {
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          resolve({
            ok: false,
            error: missing
              ? 'NDM native host is not registered. Open the NDM desktop app once to register it.'
              : friendlyHostError(text),
          });
          return;
        }
        const ok = !!(response && response.ok);
        setHostStatus(ok ? 'ok' : 'fail');
        resolve({
          ok,
          error: ok ? '' : (response && response.error) || 'NDM rejected the capture.',
        });
      });
    } catch (e) {
      setHostStatus('host-missing');
      resolve({ ok: false, error: String(e) });
    }
  });
  const timeout = new Promise(r =>
    setTimeout(() => r({ ok: false, error: 'Timed out waiting for NDM.' }), 20000)
  );
  return Promise.race([attempt, timeout]);
}

if (chrome.webRequest && chrome.webRequest.onBeforeRequest) {
  chrome.webRequest.onBeforeRequest.addListener(
    details => {
      if (details.tabId < 0) return;
      if (details.type === 'main_frame') {
        tabMediaStreams.delete(details.tabId);
        tabCaptured.delete(details.tabId);
        tabPageUrls.set(details.tabId, details.url || '');
        return;
      }
      // DRM pages: never record their streams (content.js also
      // suppresses the panel there — the layers must agree).
      if (isDrmUrl(tabPageUrls.get(details.tabId) || '')) return;
      const u = details.url || '';
      const clean = u.split(/[?#]/)[0].toLowerCase();
      if (
        clean.endsWith('.m3u8') ||
        clean.endsWith('.mpd') ||
        (details.type === 'media' && /\.(mp4|webm|mkv|mov|m4v|ts)$/i.test(clean))
      ) {
        recordTabStream(details.tabId, u);
      }
      // The capture door records what the sniffer above does
      // not: the actual media streams (googlevideo included).
      recordCapturedStream(details.tabId, u, details.type);
    },
    { urls: ['<all_urls>'] }
  );
}

chrome.tabs.onRemoved.addListener(tabId => {
  tabMediaStreams.delete(tabId);
  tabCaptured.delete(tabId);
  tabPageUrls.delete(tabId);
  tabHasMedia.delete(tabId);
  clearTabFrameMedia(tabId);
});

/* ------------------------------------------------------------------ */
/* Hold-key "prevent" gestures (F-EXT-13)                             */
/* ------------------------------------------------------------------ */

/* The keys themselves are configured in the popup and read by
   the click layer (content.js: ndmForceKey / ndmPreventKey,
   defaults force = Alt, prevent = Shift); this layer only
   keeps the bypass set they feed. The click layer notes every
   prevent-key click (URL + time). A
   browser download that starts for a noted URL is the browser
   keeping a file the user explicitly gave it — the downloads
   layer must not grab it after the fact.
   Window semantics (#23): the witness is bound to ONE URL and
   consumed on first use, so a long window is safe — it applies
   from the click that carried the witness for that URL, and
   countdown/redirect portals can take well over 5 s between
   that click and the download it produces. An unrelated click
   can no longer suppress a real intercept, because a witness
   for URL A never matches a download of URL B. */
const recentPrevents = new Map(); // url -> timestamp
const PREVENT_WINDOW_MS = 60000;

function notePrevent(url) {
  if (!url) return;
  recentPrevents.set(url, Date.now());
  if (recentPrevents.size > 100) {
    const cutoff = Date.now() - PREVENT_WINDOW_MS;
    for (const [u, at] of recentPrevents) {
      if (at < cutoff) recentPrevents.delete(u);
    }
  }
}

function recentPreventFor(url) {
  if (!url) return false;
  const at = recentPrevents.get(url);
  if (!at) return false;
  if (Date.now() - at > PREVENT_WINDOW_MS) {
    recentPrevents.delete(url);
    return false;
  }
  return true;
}

/* The downloads layer's check CONSUMES the witness: one
   prevent click shields exactly one download of its URL. */
function consumePreventFor(url) {
  if (!recentPreventFor(url)) return false;
  recentPrevents.delete(url);
  return true;
}

/* ------------------------------------------------------------------ */
/* Per-tab media presence → context-menu visibility (F-EXT-15)         */
/* ------------------------------------------------------------------ */

/* "Download this page's video with NDM" must not be offered on
   pages with no media. Content scripts report their page's
   media state (and a sniffed stream counts too); Chrome's menu
   visibility is global, so the item mirrors the ACTIVE tab. */
const tabHasMedia = new Map(); // tabId -> bool (effective, see below)

/* Media state is reported per FRAME (content.js runs in all
   frames), so the raw reports are keyed tab+frame (#21): an
   embedded player's frame must not overwrite the top frame's
   report in the per-frame map, which stays keyed for targeting.
   The DERIVED value, however, is an OR across frames (F5): a
   page whose only player lives in an embedded iframe HAS media
   for menu/popup purposes, even though its top frame truthfully
   reports none. The old top-frame authority hid exactly those
   pages. tabHasMedia above carries the derived value. */
const tabFrameMedia = new Map(); // `${tabId}:${frameId}` -> bool

function effectiveTabMedia(tabId) {
  for (const [k, v] of tabFrameMedia) {
    if (k.startsWith(tabId + ':') && v) return true;
  }
  return false;
}

function clearTabFrameMedia(tabId) {
  for (const k of [...tabFrameMedia.keys()]) {
    if (k.startsWith(tabId + ':')) tabFrameMedia.delete(k);
  }
}

function applyPageVideoMenuVisibility() {
  const applyFor = tabs => {
    const t = tabs && tabs[0];
    const has = !!(
      t &&
      t.id != null &&
      (tabHasMedia.get(t.id) || tabStreamList(t.id).length > 0)
    );
    try {
      chrome.contextMenus.update(MENU_PAGE_VIDEO, { visible: has }, () => {
        void chrome.runtime.lastError;
      });
    } catch (e) {}
  };
  try {
    chrome.tabs.query({ active: true, lastFocusedWindow: true }, tabs => {
      if (tabs && tabs.length) applyFor(tabs);
      else chrome.tabs.query({ active: true }, applyFor);
    });
  } catch (e) {}
}

chrome.tabs.onActivated.addListener(() => applyPageVideoMenuVisibility());
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === 'loading') {
    // A fresh page has reported nothing yet: the item hides
    // until its content script reports media (or a stream is
    // sniffed on it).
    tabHasMedia.delete(tabId);
    clearTabFrameMedia(tabId);
    applyPageVideoMenuVisibility();
  }
});

/* Per-tab pause (ndmTabOff) lives in chrome.storage.session, whose
   default access level is TRUSTED_CONTEXTS only: storage change
   events for it are never delivered to content scripts, so the
   popup's "Pause link interception on this tab" toggle silently
   did nothing on already-open pages — it only took effect after a
   reload, when the content script's at-load ndm-tab-state ask read
   the stored value (R2-EXT-06). Opening the area to untrusted
   contexts lets the content script's existing onChanged listener
   actually receive the change and re-ask for its tab state, so the
   toggle applies immediately. The stored data and its semantics
   (a per-tab-id map that dies with the browser session) are
   unchanged. Guarded: older browsers / Firefox may lack the API. */
try {
  if (
    chrome.storage.session &&
    typeof chrome.storage.session.setAccessLevel === 'function'
  ) {
    const p = chrome.storage.session.setAccessLevel({
      accessLevel: 'TRUSTED_AND_UNTRUSTED_CONTEXTS',
    });
    if (p && typeof p.catch === 'function') p.catch(() => {});
  }
} catch (e) {}

/* ------------------------------------------------------------------ */
/* Native messaging                                                    */
/* ------------------------------------------------------------------ */

/* IDM parity: IDM never hands a download back to the browser, and its
   integration module is simply inert when IDM itself is not there.
   Mirror that: while the native host is KNOWN to be missing, the
   extension does not intercept at all and the browser keeps its own
   download — exactly as if the extension were absent. */
let ndmHostMissing = false;
let ndmHostStatusStr = '';
let ndmHostStatusAt = 0;
let ndmInterceptEnabled = true;
let ndmSiteExceptions = new Set();

/* "Host missing" has a shelf life: a status older than ~60 s is not
   trusted (one transient crash used to disable interception
   indefinitely, because the recorded timestamp was never read). A
   stale missing-status triggers a re-probe and stops gating. */
function hostKnownMissing() {
  if (ndmHostStatusStr !== 'host-missing') return false;
  if (Date.now() - ndmHostStatusAt < 60000) return true;
  pingHostAndRecord().catch(() => {});
  return false;
}

let settingsLoaded = false;

/* Both interception gates (enabled flag + host status + site
   exceptions) load through this one promise: events that wake the
   service worker await it instead of acting on default values that
   an async storage read has not corrected yet. */
const settingsReady = (async () => {
  try {
    const v = await chrome.storage.local.get({
      ndmIntercept: true,
      ndmHostStatus: '',
      ndmHostStatusAt: 0,
      ndmSiteExceptions: [],
    });
    ndmInterceptEnabled = v.ndmIntercept !== false;
    ndmHostStatusStr = v.ndmHostStatus || '';
    ndmHostStatusAt = Number(v.ndmHostStatusAt) || 0;
    ndmHostMissing = ndmHostStatusStr === 'host-missing';
    if (Array.isArray(v.ndmSiteExceptions)) {
      ndmSiteExceptions = new Set(v.ndmSiteExceptions);
    }
  } catch (e) {}
  settingsLoaded = true;
})();

function setHostStatus(status) {
  ndmHostStatusStr = status;
  ndmHostStatusAt = Date.now();
  ndmHostMissing = status === 'host-missing';
  chrome.storage.local.set({
    ndmHostStatus: status,
    ndmHostStatusAt: ndmHostStatusAt,
  }).catch(() => {});
}

/* Sends of the same kind+URL share one in-flight request: after a
   timeout the underlying native request can STILL create the entry,
   and a caller that blindly re-sent used to end up with a second
   copy downloading alongside the timed-out first one.
   Each send carries a GENERATION (F6): a native reply that lands
   after the 25 s timeout belongs to a dead generation — it must
   not delete the map entry a RETRY has since installed under the
   same key, and it must not rewrite the shared host status. */
const inflightSends = new Map();
const inflightGens = new Map(); // key -> generation of the live send
let sendGeneration = 0;

function sendToNdm(message) {
  // Direct kinds carry the user's actual choices: a retry with a
  // different quality/folder/force must not join an earlier send
  // that only shares kind+URL.
  const keyParts = [message.kind || '', message.url || ''];
  if (message.kind === 'video_direct' || message.kind === 'file_direct') {
    keyParts.push(
      message.selector || '',
      message.dir || '',
      message.force ? '1' : '0',
      message.later ? '1' : '0'
    );
  }
  const key = keyParts.join('|');
  if (message.url && inflightSends.has(key)) {
    return inflightSends.get(key);
  }
  // Every download submission carries ext_version (E3): the host
  // stamps ext_seen on the download path, not only on ping — a
  // download-only session otherwise never refreshes the stamp
  // and a stale extension stays invisible to the app's notice.
  // A caller-supplied value (same source) wins via the spread.
  const msg = { type: 'download', ext_version: chrome.runtime.getManifest().version, ...message };
  const myGen = ++sendGeneration;
  let answered = false;
  const promise = new Promise(resolve => {
    const finish = v => {
      // Clear the in-flight entry ONLY if it is still THIS send's
      // (F6): after a timeout the entry was already cleared and a
      // retry may have installed its own under the same key — a
      // stale finish must not delete the retry's entry and defeat
      // the dedupe.
      if (inflightGens.get(key) === myGen) {
        inflightGens.delete(key);
        inflightSends.delete(key);
      }
      if (answered) return;
      answered = true;
      resolve(v);
    };
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        // A reply arriving after the timeout already answered the
        // caller is STALE (F6): it settles nothing, deletes only
        // its own generation's entry (finish, above), and must
        // not rewrite the current host status.
        const stale = answered;
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          if (!stale) setHostStatus(missing ? 'host-missing' : 'fail');
          console.warn('NDM host error:', text);
          finish({
            ok: false,
            error: missing
              ? 'NDM native host is not registered. Open the NDM desktop app once to register it.'
              : friendlyHostError(text),
          });
          return;
        }
        const ok = !!(response && response.ok);
        if (!stale) setHostStatus(ok ? 'ok' : 'fail');
        if (!ok) console.warn('NDM host rejected the download:', response);
        if (ok && response.queued === true) {
          // The host's ack wait timed out and it answered with the
          // QUEUE LINE's id — a line in the app's incoming file,
          // not a download entry. Passing it on as an entry id
          // would open a progress card keyed to an id that can
          // never report the real download (release BLOCKER). The
          // honest answer is the same one the 25 s timeout below
          // gives: accepted, outcome unknown — the page renders
          // its untracked state and the entry appears in the app
          // when the app drains its queue. (The warm path's
          // `queued` is a COUNT, not this flag — strict === true
          // keeps it on the normal mapping.)
          // The queue id now rides along AS a queue id (never as
          // an entry id): the card polls the host's result store
          // with it and adopts the real entry id once the app has
          // drained the line (field bug Y-2).
          finish({ ok: true, unknown: true, queueId: (response && response.id) || '', dupe: false, error: '' });
          return;
        }
        finish({
          ok,
          // the app's entry id and duplicate flag ride along: the
          // in-page dialogs act on them (keep / pause / discard the
          // entry that started while the dialog was open)
          id: response && response.id,
          dupe: !!(response && response.dupe),
          // the app opened its own progress dialog for this
          // start: the in-page card steps aside (one surface)
          dialog: !!(response && response.dialog),
          error: ok ? '' : (response && response.error) || 'Download rejected by NDM.',
        });
      });
    } catch (e) {
      setHostStatus('fail');
      console.warn('NDM send failed:', e);
      finish({ ok: false, error: String(e) });
    }
    setTimeout(() => {
      // A timeout is NOT a failure: the app may simply be cold-
      // starting, and the request can still land. Report it as
      // unknown (no host-status penalty, no automatic re-send).
      // The in-flight entry is CLEARED here (#24): keeping it
      // meant every later retry of this download was handed this
      // same dead promise and could never make a fresh native
      // call. A retry therefore starts a new call (a forced
      // file_direct retry never even shares this key); if the
      // original request also lands, the app's own duplicate
      // handling is the backstop.
      if (!answered) {
        answered = true;
        // This send's own entry — the generation check inside
        // finish() applies here too for symmetry, but the timeout
        // IS this generation's settlement, so clear directly.
        if (inflightGens.get(key) === myGen) {
          inflightGens.delete(key);
          inflightSends.delete(key);
        }
        resolve({ ok: false, unknown: true, error: 'NDM is still starting — the request was not re-sent.' });
      }
    }, 25000);
  });
  if (message.url) {
    inflightSends.set(key, promise);
    inflightGens.set(key, myGen);
  }
  return promise;
}

/* Pause, resume or discard one existing app entry (the progress
   card's and flyout's entry controls).
   `extra` merges additional fields into the native message — the
   Speed Limiter control carries its limit_bps this way. */
function sendControl(type, entry, extra) {
  const msg = Object.assign({ type, entry }, extra || {});
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          resolve({ ok: false, error: friendlyHostError(text) });
          return;
        }
        const ok = !!(response && response.ok);
        setHostStatus(ok ? 'ok' : 'fail');
        resolve({ ok, error: ok ? '' : (response && response.error) || 'NDM rejected the request.' });
      });
    } catch (e) {
      setHostStatus('host-missing');
      resolve({ ok: false, error: String(e) });
    }
  });
  const timeout = new Promise(r =>
    setTimeout(() => r({ ok: false, error: 'Timed out waiting for NDM app.' }), 20000)
  );
  return Promise.race([attempt, timeout]);
}

/* One native call whose FULL reply object is handed back — the
   progress channel's reply IS the payload, not just an ok flag.
   Every failure collapses to {ok:false} (and deliberately does NOT
   touch the host status: a 1 s poll against an older host without
   the progress type must not flap the recorded status). */
function sendNativeFull(msg) {
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false });
          return;
        }
        resolve(response && typeof response === 'object' ? response : { ok: false });
      });
    } catch (e) {
      resolve({ ok: false });
    }
  });
  const timeout = new Promise(r => setTimeout(() => r({ ok: false }), 5000));
  return Promise.race([attempt, timeout]);
}

function probeWithNdm(url, cookies) {
  const msg = { type: 'probe', url, cookies };
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          resolve({
            ok: false,
            error: missing
              ? 'NDM native host is not registered. Open the NDM desktop app once to register it.'
              : friendlyHostError(text),
          });
          return;
        }
        if (response && response.ok && response.info) {
          setHostStatus('ok');
          resolve({ ok: true, info: response.info });
        } else {
          // a null response means the host CRASHED once — that is a
          // failure, not proof the host is unregistered; only the
          // actual "No such native application" error earns the
          // sticky host-missing state
          setHostStatus('fail');
          resolve({
            ok: false,
            error: (response && response.error) || 'Could not read video formats from this page.',
          });
        }
      });
    } catch (e) {
      setHostStatus('host-missing');
      resolve({ ok: false, error: String(e) });
    }
  });
  const timeout = new Promise(r =>
    setTimeout(() => {
      setHostStatus('fail');
      r({ ok: false, error: 'Timed out while reading video info.' });
    }, 50000)
  );
  return Promise.race([attempt, timeout]);
}

function probeFileWithNdm(url, pageUrl, cookies) {
  const msg = { type: 'probe_file', url, pageUrl, cookies };
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          resolve({ ok: false, error: text || 'Host error' });
          return;
        }
        // record host health like every other host call (this one
        // used to leave the shared status untouched)
        setHostStatus(response && response.ok ? 'ok' : 'fail');
        resolve(response || { ok: false });
      });
    } catch (e) {
      setHostStatus('fail');
      resolve({ ok: false, error: String(e) });
    }
  });
  const timeout = new Promise(r => setTimeout(() => r({ ok: false, error: 'timeout' }), 12000));
  return Promise.race([attempt, timeout]);
}

function showAppWithNdm() {
  // Bounded like every other host call: an unresponsive host must not
  // leave the "Show in NDM" button spinning forever.
  return new Promise(resolve => {
    let settled = false;
    const done = v => {
      if (!settled) {
        settled = true;
        resolve(v);
      }
    };
    setTimeout(() => done(false), 10000);
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, { type: 'show_app' }, response => {
        done(!!(response && response.ok));
      });
    } catch (e) {
      done(false);
    }
  });
}

/* Ping the host AND record the outcome as the shared host status, so
   the popup's connectivity check also refreshes the interception
   gate (the popup used to ping on its own, leaving the gate stale
   right after a fresh host install). */
function pingHostAndRecord() {
  return new Promise(resolve => {
    let settled = false;
    const done = (ok, status) => {
      if (settled) return;
      settled = true;
      setHostStatus(status);
      resolve({ ok, status });
    };
    setTimeout(() => done(false, 'fail'), 5000);
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, { type: 'ping', ext_version: chrome.runtime.getManifest().version }, response => {
        const err = chrome.runtime.lastError;
        if (err) {
          const missing = /not found|No such native/i.test(err.message || '');
          done(false, missing ? 'host-missing' : 'fail');
        } else {
          done(!!(response && response.ok), response && response.ok ? 'ok' : 'fail');
        }
      });
    } catch (e) {
      done(false, 'host-missing');
    }
  });
}

/* ------------------------------------------------------------------ */
/* Context menus                                                       */
/* ------------------------------------------------------------------ */

const MENU_LINK = 'ndm-link';
const MENU_VIDEO = 'ndm-video';
const MENU_PAGE_VIDEO = 'ndm-page-video';
const MENU_ALL_LINKS = 'ndm-all-links';

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: MENU_LINK,
      title: 'Download with NDM',
      contexts: ['link'],
    });
    chrome.contextMenus.create({
      id: MENU_VIDEO,
      title: 'Download video with NDM',
      contexts: ['video'],
    });
    chrome.contextMenus.create({
      id: MENU_PAGE_VIDEO,
      title: "Download this page's video with NDM",
      contexts: ['page'],
      documentUrlPatterns: ['*://*/*'],
    });
    chrome.contextMenus.create({
      id: MENU_ALL_LINKS,
      title: 'Download all links with NDM',
      contexts: ['page'],
      documentUrlPatterns: ['*://*/*'],
    });
    applyPageVideoMenuVisibility();
  });
});

/* Context-menu hand-offs that fall back to a direct send used
   to discard the sendToNdm result entirely (#26): a refused or
   unreachable send looked exactly like success. Surface the
   outcome the same way the popup / in-page paths do — a
   notification, and only on failure. */
function notifyMenuSendResult(res) {
  try {
    if (res && res.ok) return;
    if (res && res.unknown) {
      chrome.notifications.create({
        type: 'basic',
        iconUrl: 'icon128.png',
        title: 'NDM is still starting',
        message:
          'The request was sent but not confirmed — check the NDM app; the download may appear there shortly.',
      });
      return;
    }
    chrome.notifications.create({
      type: 'basic',
      iconUrl: 'icon128.png',
      title: 'NDM could not start the download',
      message:
        (res && res.error) ||
        'Could not reach the NDM app. Open NDM and try again.',
    });
  } catch (e) {}
}

/* The iframe pill relay's failure surface (F4): a click whose
   picker could not be opened in ANY frame must not vanish
   silently — the #26 rule, applied to the relay path. */
function notifyRelayFailure() {
  try {
    chrome.notifications.create({
      type: 'basic',
      iconUrl: 'icon128.png',
      title: 'NDM could not open the video picker',
      message:
        'The page would not let NDM open its picker for that video. Try the NDM button in the browser toolbar on the page itself.',
    });
  } catch (e) {}
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const pageUrl = (tab && tab.url) || info.pageUrl || '';
  const title = (tab && tab.title) || '';

  if (info.menuItemId === MENU_LINK && info.linkUrl) {
    const kind = isVideoPageUrl(info.linkUrl) ? 'video' : 'file';
    if (tab && tab.id != null) {
      if (kind === 'video') {
        // a video-page link gets the same in-page dialog as the
        // other menu entries (it used to skip straight to the
        // app) — carrying the LINK's URL (#6), so the picker
        // resolves the link's video, never the current page's
        try {
          const res = await chrome.tabs.sendMessage(
            tab.id,
            { action: 'ndm-open-video-modal', linkUrl: info.linkUrl },
            { frameId: 0 }
          );
          if (res && res.ok) return;
        } catch (e) {}
      } else {
        const opened = await openFileModalInTab(tab.id, {
          url: info.linkUrl,
          filename: '',
          totalBytes: 0,
          referrer: pageUrl,
        });
        if (opened) return;
      }
    }
    const cookies = await captureCookies(info.linkUrl, pageUrl);
    const sent = await sendToNdm({ kind, url: info.linkUrl, pageUrl, title, cookies });
    notifyMenuSendResult(sent);
  }

  if (info.menuItemId === MENU_VIDEO) {
    const src = info.srcUrl || '';
    const url = src && !src.startsWith('blob:') ? src : pageUrl;
    if (tab && tab.id != null) {
      try {
        // frameId 0 + the clicked element's own srcUrl: only the top
        // frame opens the dialog, and for the video the user
        // actually right-clicked — not the largest one on the page
        const res = await chrome.tabs.sendMessage(
          tab.id,
          { action: 'ndm-open-video-modal', srcUrl: src },
          { frameId: 0 }
        );
        if (res && res.ok) return;
      } catch (e) {}
    }
    const sent = await sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url, pageUrl) });
    notifyMenuSendResult(sent);
  }

  if (info.menuItemId === MENU_ALL_LINKS && pageUrl) {
    // IDM's second context-menu verb: hand the page to the app's
    // Grabber, which opens on this page's links.
    const sent = await sendToNdm({ kind: 'grab', url: pageUrl, pageUrl, title });
    notifyMenuSendResult(sent);
    return;
  }

  if (info.menuItemId === MENU_PAGE_VIDEO) {
    // DRM pages are excluded by design (see DRM_HOSTS).
    if (isDrmUrl(pageUrl)) return;
    if (tab && tab.id != null) {
      try {
        const res = await chrome.tabs.sendMessage(
          tab.id,
          { action: 'ndm-open-video-modal' },
          { frameId: 0 }
        );
        if (res && res.ok) return;
      } catch (e) {}
    }
    const sniffed = tab && tab.id != null ? await pickMasterStream(tab.id, pageUrl) : '';
    const url = isVideoPageUrl(pageUrl) ? pageUrl : (sniffed || pageUrl);
    const sent = await sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url, pageUrl) });
    notifyMenuSendResult(sent);
  }
});

/* Capture Netscape cookies for both the target URL and the page URL so
   embedded players and CDN-hosted streams carry full authentication. */
async function captureCookies(url, secondaryUrl) {
  try {
    const urls = [url];
    if (secondaryUrl && secondaryUrl !== url) urls.push(secondaryUrl);
    const seen = new Set();
    const lines = ['# Netscape HTTP Cookie File'];
    for (const u of urls) {
      if (!u || !/^https?:\/\//i.test(u)) continue;
      const list = await chrome.cookies.getAll({ url: u }).catch(() => []);
      for (const c of list || []) {
        const key = `${c.domain}\t${c.path}\t${c.name}`;
        if (seen.has(key)) continue;
        seen.add(key);
        lines.push([
          c.domain,
          c.hostOnly ? 'FALSE' : 'TRUE',
          c.path || '/',
          c.secure ? 'TRUE' : 'FALSE',
          c.expirationDate ? Math.floor(c.expirationDate) : 0,
          c.name,
          c.value,
        ].join('\t'));
      }
    }
    return lines.length > 1 ? lines.join('\n') : '';
  } catch (e) {
    return '';
  }
}

/* ------------------------------------------------------------------ */
/* Messages from the content script and the popup                      */
/* ------------------------------------------------------------------ */

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message) return;

  if (message.action === 'ndm-tab-media') {
    const tabId = sender && sender.tab ? sender.tab.id : null;
    if (tabId == null) {
      sendResponse({ streamUrl: '' });
      return false;
    }
    const preferPage = message.pageUrl || (sender.tab && sender.tab.url) || '';
    pickMasterStream(tabId, preferPage, message.key || '')
      .then(u => sendResponse({ streamUrl: u || '' }))
      .catch(() => sendResponse({ streamUrl: '' }));
    return true;
  }

  if (message.action === 'ndm-spa-navigated') {
    // SPA navigation: the tab's sniffed streams belong to the page
    // that was left behind — clear them so the next video's dialog
    // can never be handed the previous video's streams.
    const tabId = sender && sender.tab ? sender.tab.id : null;
    if (tabId != null) {
      tabMediaStreams.delete(tabId);
      tabCaptured.delete(tabId);
      for (const k of [...masterPickCache.keys()]) {
        if (k.startsWith(tabId + '|')) masterPickCache.delete(k);
      }
      if (message.url) tabPageUrls.set(tabId, message.url);
    }
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-captured-state') {
    // The popup asks how much this tab has captured, so it can
    // offer the capture action only when there is something to
    // send (and so the proof harness can read the same state).
    const tabId =
      message.tabId != null
        ? message.tabId
        : sender && sender.tab
          ? sender.tab.id
          : null;
    if (tabId == null) {
      sendResponse({ count: 0 });
      return false;
    }
    sendResponse({ count: capturedStateForTab(tabId).count });
    return false;
  }

  if (message.action === 'ndm-capture-store') {
    // 2.3.1 (item 14, browser-primary): the picker extracted
    // the page's OWN stream data (YouTube's player response)
    // and is about to send its download naming one group of
    // it. Store the set in the app FIRST (storeOnly: no app
    // picker opens — the browser flow owns the screen), so
    // the download's start finds its streams waiting and
    // never resolves through yt-dlp.
    const pageUrl = message.pageUrl || (sender.tab && sender.tab.url) || '';
    const streams = Array.isArray(message.streams) ? message.streams.slice(0, 32) : [];
    if (!/^https?:\/\//.test(pageUrl) || !streams.length) {
      sendResponse({ ok: false, error: 'Nothing to store.' });
      return false;
    }
    (async () => {
      const cookies = await captureCookies(pageUrl, pageUrl);
      const res = await sendCapturedNative({
        type: 'captured_streams',
        pageUrl,
        title: message.title || '',
        streams,
        cookies,
        storeOnly: true,
        userAgent: message.userAgent || '',
        ext_version: chrome.runtime.getManifest().version,
      });
      sendResponse(res);
    })();
    return true;
  }

  if (message.action === 'ndm-send-captured') {
    // The popup's capture click: hand this tab's captured set to
    // the app (which stores it and opens the picker). This is
    // the ONLY way a capture leaves the browser — a user's click,
    // never a background send.
    const tabId =
      message.tabId != null
        ? message.tabId
        : sender && sender.tab
          ? sender.tab.id
          : null;
    if (tabId == null) {
      sendResponse({ ok: false, error: 'No tab to capture from.' });
      return false;
    }
    ndmSendCapturedForTab(tabId)
      .then(r => sendResponse(r))
      .catch(() => sendResponse({ ok: false, error: 'Could not send the capture.' }));
    return true;
  }

  if (message.action === 'ndm-note-prevent') {
    // A prevent-key click in some page: remember it so the
    // downloads layer stays out of the browser download it is
    // about to produce (F-EXT-13).
    notePrevent(message.url || '');
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-media-state') {
    const tabId = sender && sender.tab ? sender.tab.id : null;
    if (tabId != null) {
      // Keyed by tab+frame (#21) for targeting; the derived
      // tab value is an OR across frames (F5), so an iframe-only
      // player still raises the tab's media state. A sniffed
      // stream remains independent evidence.
      const frameId = sender && sender.frameId != null ? sender.frameId : 0;
      tabFrameMedia.set(tabId + ':' + frameId, !!message.hasMedia);
      tabHasMedia.set(
        tabId,
        effectiveTabMedia(tabId) || tabStreamList(tabId).length > 0
      );
      applyPageVideoMenuVisibility();
    }
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-takeover-confirm') {
    // The in-page dialog's start landed in the app: NDM owns
    // the download now, so the paused browser copy is disposed
    // of (#5, F2). Untracked ids (backstop already resumed
    // them) are ignored — a resumed copy is the user's again.
    // confirmTakeover is async now (it settles from persisted
    // state, F1) — fire-and-forget is fine, the response does
    // not depend on it.
    confirmTakeover(Number(message.itemId));
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-takeover-release') {
    // The dialog closed with nothing started (dismiss / Cancel
    // / failed start): hand the paused browser download back.
    releaseTakeover(Number(message.itemId));
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-relay-open-video') {
    // A sub-frame's pill was clicked (#27): the picker prefers
    // the TOP frame, so relay the request there, carrying the
    // CLICKED embed's identity (F4): a frame whose own URL is a
    // video page (an embedded player — YouTube /embed/, Vimeo
    // player.…/video/, Dailymotion /embed/video/) passes that
    // URL as the link to resolve; otherwise the clicked
    // element's src goes along, mirroring MENU_VIDEO, and the
    // top-frame picker probes THAT URL rather than re-deriving
    // a target from its own DOM. A blob/MSE embed on a page the
    // classifier does not know still names its own frame URL —
    // a better probe target than the unrelated top page.
    // Failure is never silent (the #26 rule): if frame 0 has no
    // listener, the picker is offered to the CLICKED frame
    // itself, and if that fails too the user gets a notification.
    const tabId = sender && sender.tab ? sender.tab.id : null;
    const frameId = sender && sender.frameId != null ? sender.frameId : null;
    if (tabId == null) {
      sendResponse({ ok: false });
      return false;
    }
    const frameUrl = (sender && sender.url) || message.pageUrl || '';
    const tabUrl = (sender && sender.tab && sender.tab.url) || '';
    const payload = { action: 'ndm-open-video-modal' };
    if (frameUrl && isVideoPageUrl(frameUrl)) payload.linkUrl = frameUrl;
    else if (message.srcUrl) payload.srcUrl = message.srcUrl;
    else if (frameUrl && /^https?:\/\//i.test(frameUrl) && frameUrl !== tabUrl) {
      payload.linkUrl = frameUrl;
    }
    (async () => {
      let opened = false;
      try {
        const res = await chrome.tabs.sendMessage(tabId, payload, { frameId: 0 });
        opened = !!(res && res.ok);
      } catch (e) {}
      if (!opened && frameId != null && frameId !== 0) {
        try {
          const res = await chrome.tabs.sendMessage(
            tabId,
            { ...payload, relayDirect: true },
            { frameId }
          );
          opened = !!(res && res.ok);
        } catch (e) {}
      }
      if (!opened) notifyRelayFailure();
      sendResponse({ ok: opened });
    })();
    return true;
  }

  if (message.action === 'ndm-query-media-all') {
    // The popup's freshest-source check, done right (F5): ask
    // EVERY frame of the tab whether it has media and OR the
    // answers — asking frame 0 only reported an iframe-only
    // player page as media-less. Frame ids come from the
    // per-frame report map (plus 0, always). The background's
    // own derived record answers first when it already knows.
    const tabId =
      message.tabId != null
        ? message.tabId
        : sender && sender.tab
          ? sender.tab.id
          : null;
    (async () => {
      if (tabId == null) {
        sendResponse({ hasMedia: false });
        return;
      }
      if (tabHasMedia.get(tabId) === true || tabStreamList(tabId).length > 0) {
        sendResponse({ hasMedia: true });
        return;
      }
      const frameIds = new Set([0]);
      for (const k of tabFrameMedia.keys()) {
        if (k.startsWith(tabId + ':')) {
          const fid = Number(k.slice(String(tabId).length + 1));
          if (Number.isFinite(fid)) frameIds.add(fid);
        }
      }
      let has = false;
      await Promise.all(
        [...frameIds].map(async fid => {
          try {
            const r = await chrome.tabs.sendMessage(
              tabId,
              { action: 'ndm-query-media' },
              { frameId: fid }
            );
            if (r && r.hasMedia) has = true;
          } catch (e) {}
        })
      );
      sendResponse({ hasMedia: has });
    })();
    return true;
  }

  if (message.action === 'ndm-host-ping') {
    pingHostAndRecord().then(r => sendResponse(r));
    return true;
  }

  if (
    message.action === 'ndm-pause-entry' ||
    message.action === 'ndm-cancel-entry' ||
    message.action === 'ndm-resume-entry'
  ) {
    const type =
      message.action === 'ndm-pause-entry'
        ? 'pause_entry'
        : message.action === 'ndm-cancel-entry'
          ? 'cancel_entry'
          : 'resume_entry';
    sendControl(type, String(message.entry || '')).then(r => sendResponse(r));
    return true;
  }

  if (message.action === 'ndm-set-speed-limit') {
    // The progress card's Speed Limiter tab: a number is the new
    // per-entry limit in bytes/sec; null clears it. Anything else
    // collapses to a clear rather than reaching the app malformed.
    const raw = message.limit_bps;
    const limit_bps =
      raw == null
        ? null
        : Number.isFinite(Number(raw)) && Number(raw) > 0
          ? Math.floor(Number(raw))
          : null;
    sendControl('set_speed_limit', String(message.entry || ''), {
      limit_bps,
    }).then(r => sendResponse(r));
    return true;
  }

  if (message.action === 'ndm-entry-progress') {
    sendNativeFull({ type: 'progress', id: String(message.id || '') })
      .then(r => sendResponse(r || { ok: false }))
      .catch(() => sendResponse({ ok: false }));
    return true;
  }

  if (message.action === 'ndm-start-result') {
    sendNativeFull({ type: 'result', id: String(message.queueId || '') })
      .then(r => sendResponse(r || { ok: false }))
      .catch(() => sendResponse({ ok: false }));
    return true;
  }

  if (message.action === 'ndm-show-app') {
    showAppWithNdm().then(ok => sendResponse({ ok }));
    return true;
  }

  if (message.action === 'ndm-probe-file') {
    const pageUrl = message.pageUrl || (sender.tab && sender.tab.url) || '';
    const targetUrl = message.url || '';
    (async () => {
      const cookies = await captureCookies(targetUrl, pageUrl);
      const res = await probeFileWithNdm(targetUrl, pageUrl, cookies);
      sendResponse(res);
    })();
    return true;
  }

  if (message.action === 'ndm-probe') {
    const pageUrl = message.pageUrl || (sender.tab && sender.tab.url) || '';
    const targetUrl = message.url || pageUrl;
    (async () => {
      // warm (passive pre-) probes never carry the user's session
      await settingsReady;
      const cookies = message.warm ? '' : await captureCookies(targetUrl, pageUrl);
      const res = await probeWithNdm(targetUrl, cookies);
      sendResponse(res);
    })();
    return true;
  }

  if (message.action === 'ndm-classify-url') {
    // the popup asks instead of carrying its own copy of the video
    // host patterns (the copies had already diverged once). It also
    // asks whether this tab is known to HAVE media — a recognised
    // video-page URL, a content-script media report, or a sniffed
    // stream — so it can withhold the video offer entirely on
    // media-less pages instead of surfacing a raw probe error there
    // (R2-EXT-08).
    const video = isVideoPageUrl(message.url || '');
    const tabId =
      message.tabId != null
        ? message.tabId
        : sender && sender.tab
          ? sender.tab.id
          : null;
    const hasMedia =
      video ||
      (tabId != null &&
        (tabHasMedia.get(tabId) === true || tabStreamList(tabId).length > 0));
    sendResponse({ video, hasMedia });
    return false;
  }

  if (message.action === 'ndm-tab-state') {
    const tabId = sender && sender.tab ? sender.tab.id : null;
    (async () => {
      let disabled = false;
      try {
        // Per-tab pause lives in SESSION storage: tab ids are
        // recycled across restarts, so a persistent map would let
        // an unrelated later tab inherit the pause (EX-m6).
        const store = chrome.storage.session || chrome.storage.local;
        const v = await store.get({ ndmTabOff: {} });
        disabled = !!(v.ndmTabOff && tabId != null && v.ndmTabOff[tabId]);
      } catch (e) {}
      sendResponse({ disabled, tabId });
    })();
    return true;
  }

  if (message.action === 'ndm-warm') {
    // The flyout just showed this video's qualities: ask the app to
    // resolve those selectors in the background so the click that
    // follows starts instantly from its resolution cache instead of
    // paying a full yt-dlp run first. Fire-and-forget — nothing is
    // downloaded and no entry is created.
    const pageUrl = message.pageUrl || (sender.tab && sender.tab.url) || '';
    const targetUrl = message.url || pageUrl;
    (async () => {
      await settingsReady;
      const items = (Array.isArray(message.items) ? message.items : [])
        .filter((i) => i && i.selector)
        .slice(0, 6)
        .map((i) => ({ selector: String(i.selector), audio: !!i.audio }));
      if (!/^https?:\/\//.test(targetUrl) || !items.length) {
        sendResponse({ ok: false });
        return;
      }
      const cookies = await captureCookies(targetUrl, pageUrl);
      const res = await sendToNdm({ type: 'warm', url: targetUrl, pageUrl, cookies, items });
      sendResponse(res);
    })();
    return true;
  }

  if (message.action === 'ndm-download') {
    const pageUrl = message.pageUrl || (sender.tab && sender.tab.url) || '';
    const title = message.title || (sender.tab && sender.tab.title) || '';
    const targetUrl = message.url || pageUrl;
    const validKinds = ['video_direct', 'file_direct', 'video', 'file'];
    const kind = validKinds.includes(message.kind) ? message.kind : 'file';
    (async () => {
      const cookies = await captureCookies(targetUrl, pageUrl);
      const payload = {
        kind,
        url: targetUrl,
        pageUrl,
        title,
        cookies,
        ext_version: chrome.runtime.getManifest().version,
      };
      if (kind === 'video_direct' || kind === 'file_direct') {
        payload.selector = message.selector || '';
        payload.audio = !!message.audio;
        payload.force = !!message.force;
      }
      // 2.3.1 (item 14): the page-data group the picker chose,
      // when its options came from the page itself. The host
      // validates the shape; the app honours it only against
      // its stored set.
      if (kind === 'video_direct' && typeof message.capSelector === 'string') {
        payload.capSelector = message.capSelector;
      }
      // 'file' is the in-page "Download Later" kind (E1): its
      // dir/later/total are the user's actual choices, so they
      // are forwarded for it too, not only for the *_direct kinds.
      if (kind === 'video_direct' || kind === 'file_direct' || kind === 'file') {
        payload.dir = message.dir || '~/Downloads';
        payload.later = !!message.later;
        payload.total = Number(message.total) || 0;
      }
      const res = await sendToNdm(payload);
      sendResponse(res);
    })();
    return true;
  }
});

/* ------------------------------------------------------------------ */
/* Browser download interception (0ms instant cancel + in-browser UI)  */
/* ------------------------------------------------------------------ */

chrome.storage.onChanged.addListener((changes, area) => {
  if (area !== 'local') return;
  if (changes.ndmIntercept) {
    ndmInterceptEnabled = changes.ndmIntercept.newValue !== false;
  }
  if (changes.ndmSiteExceptions) {
    const list = Array.isArray(changes.ndmSiteExceptions.newValue)
      ? changes.ndmSiteExceptions.newValue
      : [];
    ndmSiteExceptions = new Set(list);
  }
});

async function openFileModalInTab(tabId, payload) {
  if (tabId == null || tabId < 0) return false;
  try {
    const res = await chrome.tabs.sendMessage(tabId, {
      action: 'ndm-open-file-modal',
      ...payload,
    }, { frameId: 0 });
    return !!(res && res.ok);
  } catch (e) {
    // Tab might not have content script injected yet (e.g. opened before extension installed/reloaded)
    try {
      if (chrome.scripting && chrome.scripting.executeScript) {
        await chrome.scripting.executeScript({
          target: { tabId },
          files: ['fb-classify.js', 'content.js'],
        });
        const res = await chrome.tabs.sendMessage(tabId, {
          action: 'ndm-open-file-modal',
          ...payload,
        }, { frameId: 0 });
        return !!(res && res.ok);
      }
    } catch (e2) {}
    return false;
  }
}

const interceptedIds = new Set();

function siteExceptedFor(url) {
  try {
    return ndmSiteExceptions.has(new URL(url).hostname.toLowerCase());
  } catch (e) {
    return false;
  }
}

/* "Never intercept on this site" names the site the user was ON —
   the page that started the download (the item's referrer), exactly
   as the popup and the click layer mean it. The file's own host is
   only the fallback for downloads with no referrer. */
function siteExceptedForItem(item) {
  const ref = (item && item.referrer) || '';
  if (ref) {
    try {
      return ndmSiteExceptions.has(new URL(ref).hostname.toLowerCase());
    } catch (e) {}
  }
  return siteExceptedFor((item && (item.finalUrl || item.url)) || '');
}

function shouldInterceptItem(item) {
  if (!ndmInterceptEnabled || !item) return false;
  // Host known missing (freshly verified) → inert, like IDM's module
  if (hostKnownMissing()) return false;
  const url = item.finalUrl || item.url || '';
  if (!/^https?:\/\//i.test(url)) return false;
  if (siteExceptedForItem(item)) return false;
  const mimeHit =
    !!item.mime &&
    (item.mime.startsWith('application/') ||
      item.mime.startsWith('video/') ||
      item.mime.startsWith('audio/') ||
      item.mime === 'binary/octet-stream') &&
    item.mime !== 'application/xhtml+xml' &&
    item.mime !== 'application/json' &&
    item.mime !== 'application/javascript';
  return (
    isDownloadableFile(url) ||
    isDownloadableFile(item.finalUrl || '') ||
    isDownloadableFile(item.filename || '') ||
    mimeHit
  );
}

function eraseBrowserItem(id) {
  try {
    chrome.downloads.erase({ id }, () => {});
  } catch (e) {}
}

/* Read one download item's current state (null when it is gone
   or the query failed). */
function downloadItemState(id) {
  return new Promise(resolve => {
    try {
      chrome.downloads.search({ id }, items => {
        void chrome.runtime.lastError;
        resolve(items && items.length ? items[0] : null);
      });
    } catch (e) {
      resolve(null);
    }
  });
}

/* Pause an item and VERIFY where it landed (F2): the old
   fire-and-forget pause() lost its race against fast downloads,
   and the dialog then opened against an already-completed item.
   Returns the item as observed after the pause (one short
   re-check covers a pause that lands a beat late), or null. */
async function pauseAndVerify(id) {
  try {
    await new Promise(res => {
      try {
        chrome.downloads.pause(id, () => {
          void chrome.runtime.lastError;
          res();
        });
      } catch (e) {
        res();
      }
    });
  } catch (e) {}
  let it = await downloadItemState(id);
  if (it && it.state === 'in_progress' && !it.paused) {
    await new Promise(r => setTimeout(r, 150));
    it = (await downloadItemState(id)) || it;
  }
  return it;
}

/* Dispose of a browser copy NDM has confirmed it owns (F2).
   erase() alone never deletes any file — per the downloads API
   it only clears the history record. The order that actually
   removes the bytes depends on the item's state:
   - COMPLETE item: removeFile() deletes its file (it works
     only on completed items), then erase() clears history.
   - Anything else (paused / in progress): cancel() stops it
     AND deletes the partial .crdownload, then erase() clears
     history. */
async function disposeBrowserItem(id) {
  const it = await downloadItemState(id);
  try {
    if (it && it.state === 'complete') {
      await new Promise(res => {
        try {
          chrome.downloads.removeFile(id, () => {
            void chrome.runtime.lastError;
            res();
          });
        } catch (e) {
          res();
        }
      });
    } else {
      await new Promise(res => {
        try {
          chrome.downloads.cancel(id, () => {
            void chrome.runtime.lastError;
            res();
          });
        } catch (e) {
          res();
        }
      });
    }
  } catch (e) {}
  eraseBrowserItem(id);
}

/* The tab that started a download: matched by referrer, with
   the active tab as fallback (the same targeting the dialog
   placement below uses). */
async function findReferrerTab(referrer) {
  try {
    const tabs = await chrome.tabs.query({});
    if (referrer) {
      const originMatches = t => {
        try {
          return t.url && new URL(t.url).origin === new URL(referrer).origin;
        } catch (e) {
          return false;
        }
      };
      // Origin-only fallback (E6): prefer a tab in the current
      // window — the first origin match across ALL windows could
      // be another window's tab and land the dialog there. Only
      // when the current window has no origin match does the
      // fallback widen to every window.
      let currentWindowId = null;
      try {
        const win = await chrome.windows.getCurrent();
        if (win && win.id != null) currentWindowId = win.id;
      } catch (e) {}
      if (currentWindowId == null) {
        try {
          const win = await chrome.windows.getLastFocused();
          if (win && win.id != null) currentWindowId = win.id;
        } catch (e) {}
      }
      const t =
        tabs.find(t => t.url === referrer) ||
        (currentWindowId != null
          ? tabs.find(t => t.windowId === currentWindowId && originMatches(t))
          : null) ||
        tabs.find(originMatches);
      if (t) return t;
    }
    return (
      tabs.find(t => t.active && t.lastFocusedWindow) ||
      tabs.find(t => t.active) ||
      null
    );
  } catch (e) {
    return null;
  }
}

/* Was this download produced by a hold-key "prevent" click
   (F-EXT-13)? Two witnesses, either is enough: the click layer's
   timestamped note for this URL, or the source tab's content
   script reporting the gesture (held now / clicked seconds
   ago). The query is timeout-bounded and fails open — no answer
   means today's behaviour, never a stuck download. */
async function preventGestureForItem(item) {
  if (!item) return false;
  // The stored witness is URL-keyed and consumed by the match
  // (#23): it vouches for exactly one download of its own URL.
  if (
    consumePreventFor(item.finalUrl || '') ||
    consumePreventFor(item.url || '')
  ) {
    return true;
  }
  const tab = await findReferrerTab(item.referrer || '');
  if (!tab || tab.id == null) return false;
  try {
    const res = await Promise.race([
      chrome.tabs.sendMessage(
        tab.id,
        { action: 'ndm-prevent-state', url: item.finalUrl || item.url || '' },
        { frameId: 0 }
      ),
      new Promise(r => setTimeout(() => r(null), 400)),
    ]);
    return !!(res && (res.held || res.recent));
  } catch (e) {
    return false;
  }
}

function notifyTakeoverProblem(name) {
  try {
    chrome.notifications.create({
      type: 'basic',
      iconUrl: 'icon128.png',
      title: 'NDM could not take this download',
      message: `"${name || 'Download'}" was handed back to the browser and is downloading there — or open the NDM app and try again.`,
    });
  } catch (e) {}
}

/* ── Takeover bookkeeping (#5, F1) ────────────────────────────
   IDM's order, and the user's standing rule: the browser
   download is PAUSED at intercept, disposed of ONLY after the
   app confirms it owns the download, and RESUMED on any
   failure, dismiss or timeout — the user's download exists
   somewhere at all times. The in-page dialog settles its
   hand-off through the ndm-takeover-confirm /
   ndm-takeover-release messages; the direct-send path below
   settles its own; a backstop resumes anything never settled
   (a dialog abandoned open, a content script that died)
   rather than leaving the browser copy paused forever.
   F1: the backstop used to be ONLY an in-memory setTimeout in
   an in-memory Map — an MV3 worker death (idle timeout,
   browser update, crash) killed both, stranding the paused
   copy forever, and confirm/release after the restart were
   silent no-ops against the empty Map. Now every hold is
   PERSISTED (chrome.storage.local) and the backstop is ALSO a
   chrome.alarms one-shot, which survives worker death; every
   worker start rehydrates the Map from storage, re-arms what
   is still pending, and releases at once anything whose
   deadline passed while the worker was dead. Settle-once
   semantics are unchanged: the first settle wins, later
   confirm/release calls for the same item are no-ops. */
const TAKEOVER_HOLD_MS = 10 * 60 * 1000;
const TAKEOVER_STORE_KEY = 'ndmPendingTakeovers';
const TAKEOVER_ALARM_PREFIX = 'ndm-takeover-';
const pendingTakeovers = new Map(); // download id -> { timer, deadline, tabId, dialog }

/* Persist writes are chained and snapshot the Map lazily, so a
   write enqueued before a rehydrate/merge still persists the
   merged state, never a stale partial one. Each step also waits
   for the startup rehydration (takeoversReady, below): a hold
   taken in the same worker wake that is still rehydrating —
   handleInterceptedDownload does not await takeoversReady —
   must not let its snapshot reach storage before the persisted
   holds have merged into the Map, or the write would drop those
   records and their alarms would later settle nothing. (The
   rehydration itself never awaits this chain, so the wait
   cannot deadlock.) */
let takeoverPersistChain = Promise.resolve();
function persistTakeovers() {
  takeoverPersistChain = takeoverPersistChain.then(async () => {
    await takeoversReady;
    const arr = [...pendingTakeovers.entries()].map(([itemId, p]) => ({
      itemId,
      tabId: p.tabId != null ? p.tabId : null,
      deadline: p.deadline,
      dialog: !!p.dialog,
    }));
    try {
      const r = chrome.storage.local.set({ [TAKEOVER_STORE_KEY]: arr });
      if (r && typeof r.catch === 'function') return r.catch(() => {});
    } catch (e) {}
    return undefined;
  });
  return takeoverPersistChain;
}

function armTakeoverBackstop(itemId, entry) {
  const remaining = entry.deadline - Date.now();
  if (entry.timer) clearTimeout(entry.timer);
  entry.timer = setTimeout(() => releaseTakeover(itemId), Math.max(0, remaining));
  try {
    if (chrome.alarms && chrome.alarms.create) {
      chrome.alarms.create(TAKEOVER_ALARM_PREFIX + itemId, { when: entry.deadline });
    }
  } catch (e) {}
}

function holdTakeover(itemId, meta) {
  if (pendingTakeovers.has(itemId)) return;
  const entry = {
    timer: null,
    deadline: Date.now() + TAKEOVER_HOLD_MS,
    tabId: meta && meta.tabId != null ? meta.tabId : null,
    dialog: !!(meta && meta.dialog),
  };
  pendingTakeovers.set(itemId, entry);
  armTakeoverBackstop(itemId, entry);
  persistTakeovers();
}

/* Record that a dialog in a known tab owns this takeover now
   (the hold is armed before the dialog target is known). */
function noteTakeoverDialog(itemId, tabId) {
  const p = pendingTakeovers.get(itemId);
  if (!p) return;
  p.dialog = true;
  if (tabId != null) p.tabId = tabId;
  persistTakeovers();
}

/* Worker startup: rehydrate pending takeovers from storage.
   Entries whose deadline is still ahead get their timer +
   alarm re-armed; an entry whose deadline passed while no
   worker was alive is released promptly — its browser copy has
   already waited too long. Runs once per worker lifetime; the
   settle functions await it, so a confirm/release that wakes a
   fresh worker acts on the persisted state, not an empty Map. */
const takeoversReady = (async () => {
  try {
    const v = await chrome.storage.local.get({ [TAKEOVER_STORE_KEY]: [] });
    const arr = Array.isArray(v[TAKEOVER_STORE_KEY]) ? v[TAKEOVER_STORE_KEY] : [];
    const now = Date.now();
    for (const e of arr) {
      const itemId = Number(e && e.itemId);
      if (!Number.isFinite(itemId) || pendingTakeovers.has(itemId)) continue;
      const entry = {
        timer: null,
        deadline: Number(e && e.deadline) || 0,
        tabId: e && e.tabId != null ? e.tabId : null,
        dialog: !!(e && e.dialog),
      };
      pendingTakeovers.set(itemId, entry);
      if (entry.deadline <= now) {
        setTimeout(() => releaseTakeover(itemId), 0);
      } else {
        armTakeoverBackstop(itemId, entry);
      }
    }
    persistTakeovers();
  } catch (e) {}
})();

if (chrome.alarms && chrome.alarms.onAlarm) {
  chrome.alarms.onAlarm.addListener(alarm => {
    if (!alarm || typeof alarm.name !== 'string') return;
    if (!alarm.name.startsWith(TAKEOVER_ALARM_PREFIX)) return;
    const itemId = Number(alarm.name.slice(TAKEOVER_ALARM_PREFIX.length));
    if (Number.isFinite(itemId)) releaseTakeover(itemId);
  });
}

/* Claim a pending takeover for settlement: removes it from the
   Map, the store and the alarm schedule exactly once. Returns
   the entry, or null when the item is not pending (never held,
   or already settled — the settle-once rule). */
function takePendingTakeover(itemId) {
  const p = pendingTakeovers.get(itemId);
  if (!p) return null;
  if (p.timer) clearTimeout(p.timer);
  pendingTakeovers.delete(itemId);
  try {
    if (chrome.alarms && chrome.alarms.clear) {
      chrome.alarms.clear(TAKEOVER_ALARM_PREFIX + itemId);
    }
  } catch (e) {}
  persistTakeovers();
  return p;
}

async function confirmTakeover(itemId) {
  await takeoversReady;
  const p = takePendingTakeover(itemId);
  if (!p) return;
  await disposeBrowserItem(itemId);
}

async function releaseTakeover(itemId) {
  await takeoversReady;
  const p = takePendingTakeover(itemId);
  if (!p) return;
  try {
    chrome.downloads.resume(itemId, () => {
      void chrome.runtime.lastError;
    });
  } catch (e) {}
}

async function handleInterceptedDownload(item, suggestedFilename) {
  if (!item || interceptedIds.has(item.id)) return;
  interceptedIds.add(item.id);
  if (interceptedIds.size > 500) {
    interceptedIds.delete(interceptedIds.values().next().value);
  }
  // Re-check the gates with freshly loaded settings: the download
  // that woke the service worker must not be taken over on stale
  // defaults (user had interception off, host known missing…).
  await settingsReady;
  if (!shouldInterceptItem(item)) return;

  // Hold-key "prevent" (F-EXT-13): the user explicitly gave
  // this file to the browser — this layer stays out completely,
  // exactly like the click layer does.
  if (await preventGestureForItem(item)) return;

  // Take the download over the IDM way (#5): PAUSE the
  // browser's copy — do not cancel it, do not erase it. The
  // paused copy is the safety net until the app CONFIRMS it
  // owns the download (confirmTakeover erases it then); every
  // failure, dismiss or timeout path RESUMES it instead
  // (releaseTakeover). The previous flow cancelled at once and
  // erased the moment the dialog OPENED — the dialog answers
  // before its own start settles, so a failed start plus a
  // dismiss left the download existing nowhere.
  // F2: pause DETERMINISTICALLY — await it and read the item
  // back. A fast download can complete before a fire-and-forget
  // pause() lands; the browser then already owns the finished
  // file outright, and offering an app download of it would
  // produce a silent double download. In that case there is
  // nothing to take over: no dialog — the browser copy simply
  // stays the user's completed download.
  // E4: hold BEFORE pausing. The persisted record + alarm must
  // already exist while pauseAndVerify runs (incl. its 150 ms
  // re-check), or a worker death in that window strands the
  // paused copy with nothing persisted to resume it.
  holdTakeover(item.id);
  const afterPause = await pauseAndVerify(item.id);
  if (!afterPause || afterPause.state === 'complete') {
    // Already complete (browser owns it outright) or gone:
    // clear the early hold via the existing release path — its
    // resume is a harmless no-op for a complete/gone item and
    // a safety net if the state read itself failed.
    await releaseTakeover(item.id);
    return;
  }

  const url = item.finalUrl || item.url || '';
  const rawName = (suggestedFilename || item.filename || '').split(/[\\/]/).pop() || '';
  const cleanName = /^[0-9a-f-]{36}(\.tmp|\.crdownload)?$/i.test(rawName) ? '' : rawName;
  // The name a failure notification points the user at: the
  // real file's name whenever it can be known (the browser
  // copy may have been steered on disk — never leave the user
  // hunting for an ndm-intercept.tmp when the real name is
  // derivable from the URL).
  let displayName = cleanName || filenameFromUrl(url);
  if (!displayName) {
    try {
      displayName = decodeURIComponent(new URL(url).pathname.split('/').pop() || '');
    } catch (e) {
      displayName = '';
    }
  }
  const totalBytes = item.totalBytes && item.totalBytes > 0 ? item.totalBytes : 0;
  const referrer = item.referrer || '';

  // Open the dialog in the tab that STARTED the download (matched
  // by referrer), not whatever tab happens to be active: a download
  // from a background tab used to pop the dialog over an unrelated
  // page and hand it that page's URL as the referrer — login-gated
  // downloads then failed and the context was simply wrong.
  const targetTab = await findReferrerTab(referrer);
  if (targetTab && targetTab.id != null) {
    const opened = await openFileModalInTab(targetTab.id, {
      url,
      filename: cleanName,
      totalBytes,
      referrer: referrer || targetTab.url || '',
      browserItemId: item.id,
    });
    if (opened) {
      // The dialog owns the hand-off now: IT settles the paused
      // browser copy — confirm when its start lands in the app,
      // release on dismiss / Cancel / failed start. Nothing is
      // erased at open any more (that was the #5 regression).
      noteTakeoverDialog(item.id, targetTab.id);
      return;
    }
  }

  // Fallback if no tab can host the dialog: start directly in NDM.
  const cookies = await captureCookies(url, referrer);
  const sent = await sendToNdm({
    kind: 'file_direct',
    url,
    pageUrl: referrer,
    title: cleanName,
    cookies,
    dir: '~/Downloads',
    later: false,
    total: totalBytes,
  }).catch(() => null);
  if (sent && sent.ok) {
    confirmTakeover(item.id);
    return;
  }
  if (sent && sent.unknown) {
    // The send timed out: resume the browser copy (it exists
    // somewhere again at once) and say the request may ALSO
    // still land in the app, so the user is not surprised by
    // either outcome.
    releaseTakeover(item.id);
    notifyTakeoverProblem(displayName);
    return;
  }
  // IDM parity: a refused direct start is not handed straight
  // back to the browser. Escalate to the app itself first — the
  // entry lands in the app's list/dialog where the user decides.
  // Only when even that fails does the browser copy resume.
  const escalated = await sendToNdm({
    kind: 'file',
    url,
    pageUrl: referrer,
    title: cleanName,
    cookies,
    dir: '~/Downloads',
    later: false,
    total: totalBytes,
  }).catch(() => null);
  if (escalated && escalated.ok) {
    confirmTakeover(item.id);
    return;
  }
  // Even the escalation failed: the download goes back to the
  // browser, resumed and intact — never destroyed.
  releaseTakeover(item.id);
  notifyTakeoverProblem(displayName);
}

if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    // A prevent-key click for this URL already decided
    // this file belongs to the browser (F-EXT-13) — don't even
    // steer its filename. (The async gate in
    // handleInterceptedDownload re-checks with the tab query,
    // and consumes the witness there.)
    if (recentPreventFor(item.finalUrl || '') || recentPreventFor(item.url || '')) {
      return;
    }
    if (shouldInterceptItem(item)) {
      const suggested = (item && item.filename) || '';
      // Steer the filename so no Save-As dialog flashes — but only
      // when the LAST KNOWN settings positively say interception is
      // on and the host is present. On stale/cold defaults the
      // rename used to land even when no takeover followed, leaving
      // the browser's copy named ndm-intercept.tmp. When the
      // browser suggested no name, steer to the REAL file name
      // whenever the URL carries one (query-string hint, then
      // path tail) — a takeover that later fails must never
      // leave the user with a copy named ndm-intercept.tmp when
      // the real name was knowable. The takeover itself (pause
      // first, erase only after NDM confirms) runs in the async
      // handler above and re-checks the real gates.
      if (settingsLoaded && ndmInterceptEnabled && ndmHostStatusStr === 'ok') {
        let steer = suggested;
        if (!steer) {
          steer = filenameFromUrl(item.finalUrl || item.url || '');
          if (!steer) {
            try {
              steer = decodeURIComponent(
                new URL(item.finalUrl || item.url || '').pathname.split('/').pop() || ''
              );
            } catch (e) {
              steer = '';
            }
          }
        }
        try {
          suggest({ filename: steer || 'ndm-intercept.tmp', conflictAction: 'uniquify' });
        } catch (e) {}
      }
      handleInterceptedDownload(item, suggested);
    }
  });
}

chrome.downloads.onCreated.addListener(item => {
  handleInterceptedDownload(item, item.filename || '');
});

/* MV3 worker restarts forget every per-tab media report, while
   content scripts keep their lastMediaState dedupe and never
   re-send — popup and menu stayed desynced until the state
   happened to flip (#21). This file's top level runs on every
   worker start, so ask every tab to re-report now; content
   answers 'ndm-ping' by clearing its dedupe and re-sending. */
try {
  chrome.tabs.query({}, tabs => {
    for (const t of tabs || []) {
      if (t.id == null) continue;
      try {
        chrome.tabs.sendMessage(t.id, { action: 'ndm-ping' }).catch(() => {});
      } catch (e) {}
    }
  });
} catch (e) {}

/* The manifest declares page-hook.js as a MAIN-world content
   script, but the content_scripts 'world' key only exists in
   Chrome 111+. On the browsers a Windows 7/8 machine can run
   (Chrome/Brave 109 and older) the unknown key is ignored and the
   hook lands in the isolated world, where it cannot see the
   page's player globals — the YouTube picker's instant cache
   warm-up silently never happens there (downloads still work via
   the host-probe fallback). Re-inject the hook into the page's
   real MAIN world once a YouTube tab finishes loading. Chrome
   111+ already ran the manifest copy in MAIN, and the hook's own
   __ndmPageHookLoaded guard turns this into a no-op there;
   chrome.scripting with world 'MAIN' exists since Chrome 95, and
   the errors are swallowed for tabs without host permission and
   for browsers without the API (Firefox). */
try {
  chrome.tabs.onUpdated.addListener((tabId, info, tab) => {
    if (!info || info.status !== 'complete') return;
    const url = (tab && tab.url) || '';
    if (!/^https?:\/\/([^/]+\.)?youtube\.com\//.test(url)) return;
    try {
      chrome.scripting
        .executeScript({
          target: { tabId },
          world: 'MAIN',
          files: ['page-hook.js'],
        })
        .catch(() => {});
    } catch (e) {}
  });
} catch (e) {}
