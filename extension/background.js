'use strict';
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

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message) return;
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
