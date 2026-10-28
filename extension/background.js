'use strict';
/* Pure sniffing helpers (classification + HLS/DASH parsers). Loaded
 * defensively: where importScripts is unavailable the sniff actions
 * simply report "nothing found" and the probe path carries on. */
try { importScripts('sniff.js'); } catch (e) { /* probe-only mode */ }

/* NDM — Nill Download Manager: background service worker.
 *
 * Sends download requests to the desktop app through the Native Messaging
 * host "com.ndm.host". Every message is one JSON object:
 *   { type: "download", kind: "video" | "file", url, pageUrl, title }
 * The host appends each message to a queue file that the NDM app reads.
 */

const HOST_NAME = 'com.ndm.host';

/* ------------------------------------------------------------------ */
/* URL classification                                                  */
/* ------------------------------------------------------------------ */

const VIDEO_HOST_PATTERNS = [
  /(^|\.)youtube\.com\/(watch|shorts\/)/,
  /(^|\.)youtu\.be\//,
  /(^|\.)facebook\.com\/.*(\/videos\/|\/watch|\/reel\/|watch\/?\?v=)/,
  /(^|\.)vimeo\.com\/\d+/,
  /(^|\.)dailymotion\.com\/video\//,
  /(^|\.)instagram\.com\/(reel|p)\//,
  /(^|\.)tiktok\.com\/@[^/]+\/video\//,
];

function isVideoPageUrl(url) {
  if (!url) return false;
  // match host + path only: testing the raw URL lets a lookalike
  // substring anywhere (redirect parameters, query text) impersonate
  // a video host — content.js already classifies this way
  let s;
  try {
    const u = new URL(url);
    s = u.hostname + u.pathname;
  } catch (e) {
    return false;
  }
  return VIDEO_HOST_PATTERNS.some(re => re.test(s));
}

// Known downloadable file extensions for browser-download interception.
const FILE_EXTENSIONS = [
  'zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz',
  'exe', 'msi', 'deb', 'rpm', 'dmg', 'iso', 'img', 'apk',
  'pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx',
  'mp3', 'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac',
  'torrent',
];

function extensionOf(urlOrName) {
  if (!urlOrName) return '';
  const clean = urlOrName.split(/[?#]/)[0];
  const dot = clean.lastIndexOf('.');
  if (dot < 0) return '';
  return clean.slice(dot + 1).toLowerCase();
}

function isDownloadableFile(urlOrName) {
  return FILE_EXTENSIONS.includes(extensionOf(urlOrName));
}

/* ------------------------------------------------------------------ */
/* Native messaging                                                    */
/* ------------------------------------------------------------------ */

function setHostStatus(status) {
  chrome.storage.local.set({
    ndmHostStatus: status,
    ndmHostStatusAt: Date.now(),
  }).catch(() => { /* storage may be unavailable in odd states */ });
}

/* Resolves { ok, picked }: ok = the host accepted the hand-off into
   the queue; picked = the APP confirmed it surfaced the entry (the
   host waits for the app's ack before replying to a video send).
   An error reply from the host (bad kind, unwritable queue) is a
   failure, not a success. Older hosts omit `picked` — treat that as
   picked so an app-only update never degrades the pill to amber. */
function sendToNdm(message) {
  const msg = { type: 'download', ...message };
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          // "Specified native messaging host not found" (Chrome) /
          // "No such native application" (Firefox)
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          console.warn('NDM host error:', text);
          resolve({ ok: false, picked: false });
          return;
        }
        const ok = !!(response && response.ok);
        setHostStatus(ok ? 'ok' : 'fail');
        if (!ok) console.warn('NDM host rejected the download:', response);
        resolve({ ok, picked: ok && response.picked !== false });
      });
    } catch (e) {
      setHostStatus('host-missing');
      console.warn('NDM send failed:', e);
      resolve({ ok: false, picked: false });
    }
  });
  // a bridge that accepts the connection but never answers would
  // otherwise hang the panel button / interception verdict forever.
  // The window is generous on purpose: the host writes the queue
  // BEFORE it replies, so a too-short timeout can declare failure
  // for a hand-off that actually landed — for an intercepted file
  // that means the browser resumes its copy while NDM also queues
  // one (a duplicate). 25 s covers a cold app launch comfortably.
  const timeout = new Promise(r => setTimeout(() => r({ ok: false, picked: false }), 25000));
  return Promise.race([attempt, timeout]);
}

/* ------------------------------------------------------------------ */
/* Context menus                                                       */
/* ------------------------------------------------------------------ */

const MENU_LINK = 'ndm-link';
const MENU_VIDEO = 'ndm-video';
const MENU_PAGE_VIDEO = 'ndm-page-video';

chrome.runtime.onInstalled.addListener(() => {
  // Recreate from scratch so an extension update never hits duplicate ids.
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
    documentUrlPatterns: [
      '*://*.youtube.com/*',
      '*://*.facebook.com/*',
      '*://*.instagram.com/*',
      '*://*.tiktok.com/*',
      '*://vimeo.com/*',
      '*://*.dailymotion.com/*',
    ],
  });
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const pageUrl = (tab && tab.url) || info.pageUrl || '';
  const title = (tab && tab.title) || '';

  if (info.menuItemId === MENU_LINK && info.linkUrl) {
    // A link that points at a video page goes to the video engine;
    // everything else is a plain file download.
    const kind = isVideoPageUrl(info.linkUrl) ? 'video' : 'file';
    const cookies = await captureCookies(info.linkUrl);
    sendToNdm({ kind, url: info.linkUrl, pageUrl, title, cookies });
  }

  if (info.menuItemId === MENU_VIDEO) {
    // <video> sources are often blob: URLs that only the page understands;
    // in that case the page URL is what NDM needs.
    const src = info.srcUrl || '';
    const url = src && !src.startsWith('blob:') ? src : pageUrl;
    sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url) });
  }

  if (info.menuItemId === MENU_PAGE_VIDEO) {
    sendToNdm({ kind: 'video', url: pageUrl, pageUrl, title, cookies: await captureCookies(pageUrl) });
  }
});

/* The browser is already signed in to the site the user is watching;
   NDM must not ask for a separate login or a settings detour. On an
   explicit click we capture the cookies THIS browser would send to
   the video's URL and pass them along, so the app can fetch
   login-gated videos (private Facebook posts, …) with the user's own
   session. Captured only on a user gesture, only for the target URL,
   and sent only to the local NDM app. Returns '' when there are none. */
async function captureCookies(url) {
  try {
    const list = await chrome.cookies.getAll({ url });
    if (!list || !list.length) return '';
    const lines = ['# Netscape HTTP Cookie File'];
    for (const c of list) {
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
    return lines.join('\n');
  } catch (e) {
    return '';
  }
}

/* ------------------------------------------------------------------ */
/* Messages from the content script and the popup                      */
/* ------------------------------------------------------------------ */

/* The in-page panel's round trip: unlike sendToNdm, this resolves
   the host's FULL reply, because the app answers through it (the
   probe result rides back in `result`). The timeout is generous:
   a cold app launch plus a real page probe takes seconds, and a
   cut-off reply would strand the panel on its spinner. */
function sendNativeFull(message, timeoutMs) {
  const msg = { type: 'download', ...message };
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) { resolve(null); return; }
        resolve(response || null);
      });
    } catch (e) { resolve(null); }
  });
  const timeout = new Promise(r => setTimeout(() => r(null), timeoutMs));
  return Promise.race([attempt, timeout]);
}

/* ---- media sniffing (the IDM trick, generalized) ----
 * Watch every tab's responses; remember anything that classifies
 * as a stream manifest or a plain media file. The panel asks for
 * the tab's best candidate when it opens, so media the browser has
 * ALREADY fetched can be offered instantly — no fresh extraction.
 * Registry is memory-only and per tab; a fresh navigation wipes it
 * (the streams belong to the page that fetched them). */
const sniffByTab = new Map(); // tabId -> Map<key, entry>
const SNIFF_TTL_MS = 15 * 60 * 1000;

function sniffNote(tabId, entry) {
  let tab = sniffByTab.get(tabId);
  if (!tab) { tab = new Map(); sniffByTab.set(tabId, tab); }
  tab.set(entry.kind + '|' + entry.url, entry);
  // keep only the newest dozen — a long streaming session would
  // otherwise grow this without bound
  if (tab.size > 12) {
    const sorted = [...tab.entries()].sort((a, b) => a[1].ts - b[1].ts);
    for (const [k] of sorted.slice(0, tab.size - 12)) tab.delete(k);
  }
}

function sniffBest(tabId) {
  const tab = sniffByTab.get(tabId);
  if (!tab) return { found: false };
  const now = Date.now();
  const fresh = [...tab.values()].filter(e => now - e.ts < SNIFF_TTL_MS);
  if (!fresh.length) return { found: false };
  const manifests = fresh
    .filter(e => e.kind === 'hls' || e.kind === 'dash')
    .sort((a, b) => b.ts - a.ts);
  if (manifests.length) return { found: true, ...manifests[0] };
  const files = fresh.filter(e => e.kind === 'file');
  if (!files.length) return { found: false };
  // a playable file the browser committed to: prefer a known, real
  // size; otherwise the newest sighting
  const sized = files
    .filter(e => e.contentLength && e.contentLength >= 200 * 1024)
    .sort((a, b) => b.contentLength - a.contentLength);
  if (sized.length) return { found: true, ...sized[0] };
  files.sort((a, b) => b.ts - a.ts);
  return { found: true, ...files[0] };
}

if (typeof NDM_SNIFF !== 'undefined' && chrome.webRequest) {
  const headerOf = (headers, name) => {
    const h = (headers || []).find(x => x.name.toLowerCase() === name);
    return h ? h.value : '';
  };
  chrome.webRequest.onHeadersReceived.addListener(details => {
    if (details.tabId < 0) return;
    const kind = NDM_SNIFF.classify(
      details.url, headerOf(details.responseHeaders, 'content-type'));
    if (!kind) return;
    const len = parseInt(headerOf(details.responseHeaders, 'content-length') || '', 10);
    sniffNote(details.tabId, {
      kind,
      url: details.url,
      contentLength: Number.isFinite(len) ? len : null,
      ts: Date.now(),
    });
  }, { urls: ['<all_urls>'] }, ['responseHeaders']);
  chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
    if (changeInfo.status === 'loading' && changeInfo.url) {
      sniffByTab.delete(tabId);
    }
  });
  chrome.tabs.onRemoved.addListener(tabId => sniffByTab.delete(tabId));
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message) return;
  if (message.action === 'ndm-sniff') {
    const tabId = sender.tab && sender.tab.id;
    sendResponse(typeof tabId === 'number' ? sniffBest(tabId) : { found: false });
    return;
  }
  if (message.action === 'ndm-manifest') {
    // fetch + parse a sniffed manifest HERE (the worker's fetch
    // carries the browser's cookies for that origin, which is what
    // makes login-gated streams readable) and hand back the ladder.
    // A player fetches the MASTER first and one variant (media)
    // playlist right after, so the newest sniffed manifest is
    // usually the WRONG one: walk the tab's manifests until one
    // parses as a real master (it carries the variant ladder);
    // only when none does, fall back to the first parse.
    (async () => {
      try {
        if (typeof NDM_SNIFF === 'undefined') { sendResponse({ ok: false }); return; }
        const urls = [message.url || ''];
        const tabId = sender.tab && sender.tab.id;
        if (typeof tabId === 'number' && sniffByTab.has(tabId)) {
          const rest = [...sniffByTab.get(tabId).values()]
            .filter(e => (e.kind === 'hls' || e.kind === 'dash') && e.url !== urls[0])
            .sort((a, b) => b.ts - a.ts)
            .map(e => e.url);
          urls.push(...rest);
        }
        let firstParsed = null;
        for (const url of urls.slice(0, 4)) {
          if (!url) continue;
          const resp = await fetch(url, { credentials: 'include' }).catch(() => null);
          if (!resp || !resp.ok) continue;
          const text = await resp.text();
          const isDash = /\.mpd($|\?)/i.test(url)
            || (resp.headers.get('content-type') || '').includes('dash+xml');
          const parsed = isDash
            ? NDM_SNIFF.parseMpd(text, url)
            : NDM_SNIFF.parseHls(text, url);
          // the panel starts the download from the URL that
          // actually held this ladder, which may not be the pick
          parsed.url = url;
          if (!firstParsed) firstParsed = parsed;
          if (parsed.master) { sendResponse({ ok: true, ...parsed }); return; }
        }
        if (firstParsed) { sendResponse({ ok: true, ...firstParsed }); return; }
        sendResponse({ ok: false });
      } catch (e) {
        sendResponse({ ok: false });
      }
    })();
    return true; // async sendResponse
  }
  if (message.action === 'ndm-panel-probe' || message.action === 'ndm-panel-start') {
    const targetUrl = message.url || (sender.tab && sender.tab.url) || '';
    (async () => {
      const cookies = await captureCookies(targetUrl);
      const base = {
        url: targetUrl,
        pageUrl: message.pageUrl || targetUrl,
        title: message.title || '',
        cookies,
      };
      const resp = message.action === 'ndm-panel-probe'
        ? await sendNativeFull({ kind: 'probe', ...base }, 50000)
        : await sendNativeFull({
          kind: 'start',
          ...base,
          selector: message.selector || '',
          audio: !!message.audio,
          hasAudio: !!message.hasAudio,
          // sniffed direct starts: the stream the browser already
          // fetched, instead of a page to re-resolve
          ...(message.mode ? {
            mode: message.mode,
            directUrl: message.directUrl || '',
            height: message.height || 0,
            audioUrl: message.audioUrl || '',
            fileName: message.fileName || '',
          } : {}),
        }, 25000);
      sendResponse(resp || { ok: false, error: 'NDM is not reachable' });
    })();
    return true; // async sendResponse
  }
  if (message.action !== 'ndm-download') return;

  const pageUrl = message.pageUrl
    || (sender.tab && sender.tab.url)
    || '';
  const title = message.title
    || (sender.tab && sender.tab.title)
    || '';
  // keep the channel open and answer with the host's real verdict:
  // the panel/popup must never claim "sent" for a failed hand-off
  const kind = message.kind === 'video' ? 'video' : 'file';
  const targetUrl = message.url || pageUrl;
  (async () => {
    const cookies = await captureCookies(targetUrl);
    const res = await sendToNdm({ kind, url: targetUrl, pageUrl, title, cookies });
    sendResponse({ ok: res.ok, picked: res.picked });
  })();
  return true;
});

/* ------------------------------------------------------------------ */
/* Browser download interception                                       */
/* ------------------------------------------------------------------ */

/* The suggested file name (from Content-Disposition) is decided in
   its own event, which can land before or after onCreated — keep the
   latest suggestion per download so the hand-off can use it. */
const suggestedNames = new Map();
if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener(item => {
    if (item && item.id != null && item.filename) {
      suggestedNames.set(item.id, item.filename);
      // entries are consumed by the onCreated hand-off; this sweep
      // only guards against suggestions nothing ever picks up
      if (suggestedNames.size > 200) {
        suggestedNames.delete(suggestedNames.keys().next().value);
      }
    }
  });
}

chrome.downloads.onCreated.addListener(async item => {
  try {
    const { ndmIntercept } = await chrome.storage.local.get({
      ndmIntercept: true,
    });
    if (ndmIntercept === false) return;

    const url = item.finalUrl || item.url || '';
    if (!/^https?:\/\//i.test(url)) return;
    // catch by the address or suggested name's extension, and by type:
    // plenty of download buttons point at extension-less endpoints
    // (download.php?id=…, /getfile/…) that only reveal themselves as
    // files through their Content-Type / Content-Disposition — the
    // browser has already made that call when it starts a download
    const mimeHit = !!item.mime
      && item.mime.startsWith('application/')
      && item.mime !== 'application/xhtml+xml';
    const looksLikeFile = isDownloadableFile(url)
      || isDownloadableFile(item.filename || '')
      || mimeHit;
    if (!looksLikeFile) return;

    // Hand the download to NDM — but only give up the browser's copy once
    // the host has confirmed it accepted the request. Until then the
    // browser download is paused; if NDM rejects it, resume the browser
    // download so the file is never lost.
    await chrome.downloads.pause(item.id).catch(() => {});
    // the suggested file name (Content-Disposition) is not always on
    // the item yet at onCreated time — give the browser a beat, then
    // re-read the item so NDM's dialog can show the real name
    await new Promise(r => setTimeout(r, 350));
    let suggested = suggestedNames.get(item.id) || item.filename || '';
    suggestedNames.delete(item.id);
    try {
      const [fresh] = await chrome.downloads.search({ id: item.id });
      // a real browser path (…/report.pdf) beats no name; Playwright's
      // GUID artifact paths are not names — the app ignores non-names
      if (fresh && fresh.filename && !/^[0-9a-f-]{36}$/i.test(fresh.filename.split(/[\\/]/).pop().replace(/\.[^.]*$/, ''))) {
        suggested = fresh.filename;
      }
    } catch (e) { /* keep the suggestion */ }
    // only the bare file name travels: the full browser path would
    // leak the user's folder layout into the queue file for no gain
    const baseName = suggested.split(/[\\/]/).pop() || '';
    const accepted = (await sendToNdm({
      kind: 'file', url, pageUrl: item.referrer || '', title: baseName,
      cookies: await captureCookies(url),
    })).ok;
    if (accepted) {
      // IDM removes the browser's own copy after a takeover: cancel,
      // delete any bytes it already fetched, then drop the record —
      // the file must exist exactly once, in NDM
      await chrome.downloads.cancel(item.id).catch(() => {});
      await chrome.downloads.removeFile(item.id).catch(() => {});
      await chrome.downloads.erase({ id: item.id }).catch(() => {});
    } else {
      await chrome.downloads.resume(item.id).catch(() => {});
    }
  } catch (e) {
    console.warn('NDM interception failed:', e);
  }
});
