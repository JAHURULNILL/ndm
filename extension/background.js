'use strict';
/* NDM — Nill Download Manager: background service worker.
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
const FILE_EXTENSIONS = [
  'zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'tgz', 'tbz2', 'txz', 'cab',
  'exe', 'msi', 'msix', 'msixbundle', 'deb', 'rpm', 'dmg', 'pkg', 'iso', 'img', 'apk', 'bin', 'run', 'appimage', 'flatpakref',
  'pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'csv', 'txt', 'rtf', 'epub', 'mobi',
  'mp3', 'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac', 'aac', 'ogg', 'm4a', 'm4b', 'opus', 'wav', 'wma', 'flv', 'wmv', '3gp', 'm4v', 'ts',
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
  let list = tabMediaStreams.get(tabId) || [];
  list = list.filter(e => e.url !== url && Date.now() - e.ts < STREAM_TTL_MS);
  // A master-looking manifest stays ahead of sub-variant playlists:
  // the newest response is very often just one variant of the show.
  if (isSubVariantPlaylist(url)) {
    list.push({ url, ts: Date.now() });
  } else {
    list.unshift({ url, ts: Date.now() });
  }
  tabMediaStreams.set(tabId, list.slice(0, STREAMS_PER_TAB));
  if (tabMediaStreams.size > 300) {
    tabMediaStreams.delete(tabMediaStreams.keys().next().value);
  }
}

function tabStreamList(tabId) {
  const list = tabMediaStreams.get(tabId) || [];
  return list
    .filter(e => Date.now() - e.ts < STREAM_TTL_MS)
    .map(e => e.url);
}

/* Pick the stream URL that actually describes the whole show: fetch
   the candidate manifests and prefer the one whose CONTENT is a
   master playlist (#EXT-X-STREAM-INF) — a variant URL handed to the
   probe collapses the quality ladder to that single variant. */
const masterPickCache = new Map();
async function pickMasterStream(tabId) {
  const list = tabStreamList(tabId);
  if (!list.length) return '';
  const manifests = list.filter(u => /\.m3u8(\?|$)/i.test(u.split('#')[0]));
  if (!manifests.length) return list[0];
  const cacheKey = tabId + '|' + manifests[0];
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
        return;
      }
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

/* ------------------------------------------------------------------ */
/* Native messaging                                                    */
/* ------------------------------------------------------------------ */

/* IDM parity: IDM never hands a download back to the browser, and its
   integration module is simply inert when IDM itself is not there.
   Mirror that: while the native host is KNOWN to be missing, the
   extension does not intercept at all and the browser keeps its own
   download — exactly as if the extension were absent. */
let ndmHostMissing = false;
chrome.storage.local
  .get({ ndmHostStatus: '' })
  .then(v => {
    ndmHostMissing = v.ndmHostStatus === 'host-missing';
  })
  .catch(() => {});

function setHostStatus(status) {
  ndmHostMissing = status === 'host-missing';
  chrome.storage.local.set({
    ndmHostStatus: status,
    ndmHostStatusAt: Date.now(),
  }).catch(() => {});
}

function sendToNdm(message) {
  const msg = { type: 'download', ...message };
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, msg, response => {
        if (chrome.runtime.lastError) {
          const text = chrome.runtime.lastError.message || '';
          const missing = /not found|No such native/i.test(text);
          setHostStatus(missing ? 'host-missing' : 'fail');
          console.warn('NDM host error:', text);
          resolve({
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
        resolve({
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
      setHostStatus('host-missing');
      console.warn('NDM send failed:', e);
      resolve({ ok: false, error: String(e) });
    }
  });
  const timeout = new Promise(r =>
    setTimeout(() => {
      setHostStatus('fail');
      r({ ok: false, error: 'Timed out waiting for NDM app.' });
    }, 25000)
  );
  return Promise.race([attempt, timeout]);
}

/* Pause or discard one existing entry (the browser dialog's Later /
   Cancel now that downloads start while the dialog is open). */
function sendControl(type, entry) {
  const attempt = new Promise(resolve => {
    try {
      chrome.runtime.sendNativeMessage(HOST_NAME, { type, entry }, response => {
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
          setHostStatus(response ? 'fail' : 'host-missing');
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
          resolve({ ok: false, error: chrome.runtime.lastError.message || 'Host error' });
          return;
        }
        resolve(response || { ok: false });
      });
    } catch (e) {
      resolve({ ok: false, error: String(e) });
    }
  });
  const timeout = new Promise(r => setTimeout(() => r({ ok: false, error: 'timeout' }), 15000));
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
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const pageUrl = (tab && tab.url) || info.pageUrl || '';
  const title = (tab && tab.title) || '';

  if (info.menuItemId === MENU_LINK && info.linkUrl) {
    const kind = isVideoPageUrl(info.linkUrl) ? 'video' : 'file';
    if (kind === 'file' && tab && tab.id != null) {
      const opened = await openFileModalInTab(tab.id, {
        url: info.linkUrl,
        filename: '',
        totalBytes: 0,
        referrer: pageUrl,
      });
      if (opened) return;
    }
    const cookies = await captureCookies(info.linkUrl, pageUrl);
    sendToNdm({ kind, url: info.linkUrl, pageUrl, title, cookies });
  }

  if (info.menuItemId === MENU_VIDEO) {
    const src = info.srcUrl || '';
    const url = src && !src.startsWith('blob:') ? src : pageUrl;
    if (tab && tab.id != null) {
      try {
        const res = await chrome.tabs.sendMessage(tab.id, { action: 'ndm-open-video-modal' });
        if (res && res.ok) return;
      } catch (e) {}
    }
    sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url, pageUrl) });
  }

  if (info.menuItemId === MENU_PAGE_VIDEO) {
    if (tab && tab.id != null) {
      try {
        const res = await chrome.tabs.sendMessage(tab.id, { action: 'ndm-open-video-modal' });
        if (res && res.ok) return;
      } catch (e) {}
    }
    const sniffed = tab && tab.id != null ? await pickMasterStream(tab.id) : '';
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
    pickMasterStream(tabId)
      .then(u => sendResponse({ streamUrl: u || '' }))
      .catch(() => sendResponse({ streamUrl: '' }));
    return true;
  }

  if (message.action === 'ndm-host-ping') {
    pingHostAndRecord().then(r => sendResponse(r));
    return true;
  }

  if (message.action === 'ndm-pause-entry' || message.action === 'ndm-cancel-entry') {
    const type = message.action === 'ndm-pause-entry' ? 'pause_entry' : 'cancel_entry';
    sendControl(type, String(message.entry || '')).then(r => sendResponse(r));
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
      const cookies = await captureCookies(targetUrl, pageUrl);
      const res = await probeWithNdm(targetUrl, cookies);
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

let ndmInterceptEnabled = true;
chrome.storage.local.get({ ndmIntercept: true }).then(v => {
  ndmInterceptEnabled = v.ndmIntercept !== false;
}).catch(() => {});
chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'local' && changes.ndmIntercept) {
    ndmInterceptEnabled = changes.ndmIntercept.newValue !== false;
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

function shouldInterceptItem(item) {
  if (!ndmInterceptEnabled || !item) return false;
  // Host known missing → stay inert like IDM's module without IDM.
  if (ndmHostMissing) return false;
  const url = item.finalUrl || item.url || '';
  if (!/^https?:\/\//i.test(url)) return false;
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

async function handleInterceptedDownload(item, suggestedFilename) {
  if (!item || interceptedIds.has(item.id)) return;
  interceptedIds.add(item.id);
  if (interceptedIds.size > 500) {
    interceptedIds.delete(interceptedIds.values().next().value);
  }

  // Cancel and erase the native browser download IMMEDIATELY (0ms delay)
  // so the browser's download shelf / Save-As dialog never appears!
  try {
    chrome.downloads.cancel(item.id, () => {
      chrome.downloads.erase({ id: item.id }, () => {});
    });
  } catch (e) {}

  const url = item.finalUrl || item.url || '';
  const rawName = (suggestedFilename || item.filename || '').split(/[\\/]/).pop() || '';
  const cleanName = /^[0-9a-f-]{36}(\.tmp|\.crdownload)?$/i.test(rawName) ? '' : rawName;
  const totalBytes = item.totalBytes && item.totalBytes > 0 ? item.totalBytes : 0;
  const referrer = item.referrer || '';

  // Try to open the IDM Download File Info popup right inside the active browser tab!
  try {
    let tabs = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (!tabs || !tabs.length) {
      tabs = await chrome.tabs.query({ active: true, currentWindow: true });
    }
    if (!tabs || !tabs.length) {
      tabs = await chrome.tabs.query({ active: true });
    }
    const activeTab = tabs && tabs[0];
    if (activeTab && activeTab.id != null) {
      const opened = await openFileModalInTab(activeTab.id, {
        url,
        filename: cleanName,
        totalBytes,
        referrer: referrer || activeTab.url || '',
      });
      if (opened) return;
    }
  } catch (e) {}

  // Fallback if the active tab cannot host content scripts (e.g. blank new tab or restricted page):
  // start directly in NDM background without forcing the desktop window open or stealing focus!
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
  if (!sent || !sent.ok) {
    // IDM parity: a refused direct start is NEVER handed back to the
    // browser (IDM never returns downloads). Escalate to the app
    // itself instead — the entry lands in the app's list/dialog where
    // the user decides, the way IDM's own dialog is always available.
    await sendToNdm({
      kind: 'file',
      url,
      pageUrl: referrer,
      title: cleanName,
      cookies,
      dir: '~/Downloads',
      later: false,
      total: totalBytes,
    }).catch(() => null);
  }
}

if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    if (shouldInterceptItem(item)) {
      // 1. Cancel and erase IMMEDIATELY BEFORE calling suggest() so Chrome never displays the Save As dialog or shelf!
      try {
        chrome.downloads.cancel(item.id, () => {
          chrome.downloads.erase({ id: item.id }, () => {});
        });
      } catch (e) {}
      // 2. Call suggest to satisfy Chrome's API requirement
      const suggested = (item && item.filename) || '';
      try {
        suggest({ filename: suggested || 'ndm-intercept.tmp', conflictAction: 'uniquify' });
      } catch (e) {}
      // 3. Delegate to NDM in-page modal / direct start
      handleInterceptedDownload(item, suggested);
      return;
    }
  });
}

chrome.downloads.onCreated.addListener(item => {
  if (shouldInterceptItem(item)) {
    try {
      chrome.downloads.cancel(item.id, () => {
        chrome.downloads.erase({ id: item.id }, () => {});
      });
    } catch (e) {}
    handleInterceptedDownload(item, item.filename || '');
  }
});
