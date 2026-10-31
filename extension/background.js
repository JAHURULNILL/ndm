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
  /(^|\.)facebook\.com\/.*(\/videos\/|\/watch|\/reel\/|\/share\/(r|v)\/|watch\/?\?v=)/,
  /(^|\.)fb\.watch\//,
  /(^|\.)vimeo\.com\/\d+/,
  /(^|\.)dailymotion\.com\/video\//,
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
      const r = await fetch(u, { credentials: 'include' });
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

if (chrome.webRequest && chrome.webRequest.onBeforeRequest) {
  chrome.webRequest.onBeforeRequest.addListener(
    details => {
      if (details.tabId < 0) return;
      if (details.type === 'main_frame') {
        tabMediaStreams.delete(details.tabId);
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
    },
    { urls: ['<all_urls>'] }
  );
}

chrome.tabs.onRemoved.addListener(tabId => {
  tabMediaStreams.delete(tabId);
  tabPageUrls.delete(tabId);
  tabHasMedia.delete(tabId);
});

/* ------------------------------------------------------------------ */
/* Alt+Shift "prevent" gestures (F-EXT-13)                             */
/* ------------------------------------------------------------------ */

/* The click layer notes every Alt+Shift click (URL + time). A
   browser download that starts within a few seconds for a noted
   URL is the browser keeping a file the user explicitly gave
   it — the downloads layer must not grab it after the fact. */
const recentPrevents = new Map(); // url -> timestamp
const PREVENT_WINDOW_MS = 5000;

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

/* ------------------------------------------------------------------ */
/* Per-tab media presence → context-menu visibility (F-EXT-15)         */
/* ------------------------------------------------------------------ */

/* "Download this page's video with NDM" must not be offered on
   pages with no media. Content scripts report their page's
   media state (and a sniffed stream counts too); Chrome's menu
   visibility is global, so the item mirrors the ACTIVE tab. */
const tabHasMedia = new Map(); // tabId -> bool

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
   copy downloading alongside the timed-out first one. */
const inflightSends = new Map();

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
  const msg = { type: 'download', ...message };
  let answered = false;
  const promise = new Promise(resolve => {
    const finish = v => {
      // the in-flight entry always clears when the native call
      // settles — even after a timeout already answered the caller
      inflightSends.delete(key);
      if (answered) return;
      answered = true;
      resolve(v);
    };
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          console.warn('NDM host error:', text);
          finish({
            ok: false,
            error: missing
              ? 'NDM native host is not registered. Open the NDM desktop app once to register it.'
              : text || 'Could not reach NDM native host.',
          });
          return;
        }
        const ok = !!(response && response.ok);
        setHostStatus(ok ? 'ok' : 'fail');
        if (!ok) console.warn('NDM host rejected the download:', response);
        finish({
          ok,
          // the app's entry id and duplicate flag ride along: the
          // in-page dialogs act on them (keep / pause / discard the
          // entry that started while the dialog was open)
          id: response && response.id,
          dupe: !!(response && response.dupe),
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
      // unknown (no host-status penalty, no automatic re-send —
      // the shared in-flight entry keeps later callers joined to
      // this request until it truly settles).
      if (!answered) {
        answered = true;
        resolve({ ok: false, unknown: true, error: 'NDM is still starting — the request was not re-sent.' });
      }
    }, 25000);
  });
  if (message.url) inflightSends.set(key, promise);
  return promise;
}

/* Pause or discard one existing entry (the browser dialog's Later /
   Cancel now that downloads start while the dialog is open).
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
          resolve({ ok: false, error: text || 'Could not reach NDM native host.' });
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
              : text || 'Could not reach NDM native host.',
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
      chrome.runtime.sendNativeMessage(HOST_NAME, { type: 'ping' }, response => {
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

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const pageUrl = (tab && tab.url) || info.pageUrl || '';
  const title = (tab && tab.title) || '';

  if (info.menuItemId === MENU_LINK && info.linkUrl) {
    const kind = isVideoPageUrl(info.linkUrl) ? 'video' : 'file';
    if (tab && tab.id != null) {
      if (kind === 'video') {
        // a video-page link gets the same in-page dialog as the
        // other menu entries (it used to skip straight to the app)
        try {
          const res = await chrome.tabs.sendMessage(
            tab.id,
            { action: 'ndm-open-video-modal' },
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
    sendToNdm({ kind, url: info.linkUrl, pageUrl, title, cookies });
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
    sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url, pageUrl) });
  }

  if (info.menuItemId === MENU_ALL_LINKS && pageUrl) {
    // IDM's second context-menu verb: hand the page to the app's
    // Grabber, which opens on this page's links.
    sendToNdm({ kind: 'grab', url: pageUrl, pageUrl, title });
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
    sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url, pageUrl) });
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
      for (const k of [...masterPickCache.keys()]) {
        if (k.startsWith(tabId + '|')) masterPickCache.delete(k);
      }
      if (message.url) tabPageUrls.set(tabId, message.url);
    }
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-note-prevent') {
    // An Alt+Shift click in some page: remember it so the
    // downloads layer stays out of the browser download it is
    // about to produce (F-EXT-13).
    notePrevent(message.url || '');
    sendResponse({ ok: true });
    return false;
  }

  if (message.action === 'ndm-media-state') {
    const tabId = sender && sender.tab ? sender.tab.id : null;
    if (tabId != null) {
      tabHasMedia.set(tabId, !!message.hasMedia);
      applyPageVideoMenuVisibility();
    }
    sendResponse({ ok: true });
    return false;
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
      };
      if (kind === 'video_direct' || kind === 'file_direct') {
        payload.selector = message.selector || '';
        payload.audio = !!message.audio;
        payload.dir = message.dir || '~/Downloads';
        payload.later = !!message.later;
        payload.total = Number(message.total) || 0;
        payload.force = !!message.force;
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
          files: ['content.js'],
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

/* The tab that started a download: matched by referrer, with
   the active tab as fallback (the same targeting the dialog
   placement below uses). */
async function findReferrerTab(referrer) {
  try {
    const tabs = await chrome.tabs.query({});
    if (referrer) {
      const t =
        tabs.find(t => t.url === referrer) ||
        tabs.find(t => {
          try {
            return t.url && new URL(t.url).origin === new URL(referrer).origin;
          } catch (e) {
            return false;
          }
        });
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

/* Was this download produced by an Alt+Shift "prevent" click
   (F-EXT-13)? Two witnesses, either is enough: the click layer's
   timestamped note for this URL, or the source tab's content
   script reporting the gesture (held now / clicked seconds
   ago). The query is timeout-bounded and fails open — no answer
   means today's behaviour, never a stuck download. */
async function preventGestureForItem(item) {
  if (!item) return false;
  if (
    recentPreventFor(item.finalUrl || '') ||
    recentPreventFor(item.url || '')
  ) {
    return true;
  }
  const tab = await findReferrerTab(item.referrer || '');
  if (!tab || tab.id == null) return false;
  try {
    const res = await Promise.race([
      chrome.tabs.sendMessage(
        tab.id,
        { action: 'ndm-prevent-state' },
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
      message: `"${name || 'Download'}" was left in the browser's download list — resume it there, or open the NDM app and try again.`,
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

  // Alt+Shift "prevent" (F-EXT-13): the user explicitly gave
  // this file to the browser — this layer stays out completely,
  // exactly like the click layer does.
  if (await preventGestureForItem(item)) return;

  // Take the download over: cancel the browser's copy, but do NOT
  // erase it yet. Erasing happens only once NDM has CONFIRMED it
  // owns the download (the in-page dialog opened, or a send acked
  // ok). The old flow erased first and asked later — when every
  // fallback then failed, the download, its partial file and its
  // shelf record were destroyed with no error shown anywhere.
  try {
    chrome.downloads.cancel(item.id, () => {});
  } catch (e) {}

  const url = item.finalUrl || item.url || '';
  const rawName = (suggestedFilename || item.filename || '').split(/[\\/]/).pop() || '';
  const cleanName = /^[0-9a-f-]{36}(\.tmp|\.crdownload)?$/i.test(rawName) ? '' : rawName;
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
    });
    if (opened) {
      eraseBrowserItem(item.id);
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
    eraseBrowserItem(item.id);
    return;
  }
  if (sent && sent.unknown) {
    // the request may still land in the app — say so, and leave the
    // browser copy listed so the user can resume it if it doesn't
    notifyTakeoverProblem(cleanName);
    return;
  }
  // IDM parity: a refused direct start is NEVER handed back to the
  // browser. Escalate to the app itself — the entry lands in the
  // app's list/dialog where the user decides.
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
    eraseBrowserItem(item.id);
    return;
  }
  notifyTakeoverProblem(cleanName);
}

if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    // An Alt+Shift prevent click seconds ago already decided
    // this file belongs to the browser (F-EXT-13) — don't even
    // steer its filename. (The async gate in
    // handleInterceptedDownload re-checks with the tab query.)
    if (recentPreventFor(item.finalUrl || '') || recentPreventFor(item.url || '')) {
      return;
    }
    if (shouldInterceptItem(item)) {
      const suggested = (item && item.filename) || '';
      // Steer the filename so no Save-As dialog flashes — but only
      // when the LAST KNOWN settings positively say interception is
      // on and the host is present. On stale/cold defaults the
      // rename used to land even when no takeover followed, leaving
      // the browser's copy named ndm-intercept.tmp. The takeover
      // itself (cancel first, erase only after NDM confirms) runs
      // in the async handler above and re-checks the real gates.
      if (settingsLoaded && ndmInterceptEnabled && ndmHostStatusStr === 'ok') {
        try {
          suggest({ filename: suggested || 'ndm-intercept.tmp', conflictAction: 'uniquify' });
        } catch (e) {}
      }
      handleInterceptedDownload(item, suggested);
    }
  });
}

chrome.downloads.onCreated.addListener(item => {
  handleInterceptedDownload(item, item.filename || '');
});
