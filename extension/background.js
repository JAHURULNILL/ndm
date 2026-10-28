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
  'zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'tgz', 'tbz2', 'txz',
  'exe', 'msi', 'deb', 'rpm', 'dmg', 'iso', 'img', 'apk', 'bin', 'appimage', 'flatpakref',
  'pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'epub', 'mobi',
  'mp3', 'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac', 'aac', 'ogg', 'm4a', 'opus', 'wav',
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
/* Media stream sniffer (HLS .m3u8 / DASH .mpd / direct video streams) */
/* ------------------------------------------------------------------ */

const tabMediaStreams = new Map();

function isSubVariantPlaylist(url) {
  return /(chunklist|variant|rendition|audio[_-]|video[_-]|segment|frag)/i.test(url);
}

function recordTabStream(tabId, url) {
  if (tabId == null || tabId < 0 || !url) return;
  if (!/^https?:\/\//i.test(url)) return;
  // Ignore YouTube/Facebook raw segment requests because yt-dlp handles page URLs directly
  if (/googlevideo\.com\/videoplayback|fbcdn\.net.*bytestart=/i.test(url)) return;
  const prev = tabMediaStreams.get(tabId);
  if (prev && /\.m3u8(\?|$)/i.test(prev) && !isSubVariantPlaylist(prev) && isSubVariantPlaylist(url)) {
    return;
  }
  tabMediaStreams.set(tabId, url);
  if (tabMediaStreams.size > 300) {
    tabMediaStreams.delete(tabMediaStreams.keys().next().value);
  }
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

function setHostStatus(status) {
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
          resolve(false);
          return;
        }
        const ok = !!(response && response.ok);
        setHostStatus(ok ? 'ok' : 'fail');
        if (!ok) console.warn('NDM host rejected the download:', response);
        resolve(ok);
      });
    } catch (e) {
      setHostStatus('host-missing');
      console.warn('NDM send failed:', e);
      resolve(false);
    }
  });
  const timeout = new Promise(r => setTimeout(() => r(false), 25000));
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
          setHostStatus('ok');
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
    setTimeout(() => r({ ok: false, error: 'Timed out while reading video info.' }), 65000)
  );
  return Promise.race([attempt, timeout]);
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
    const cookies = await captureCookies(info.linkUrl, pageUrl);
    sendToNdm({ kind, url: info.linkUrl, pageUrl, title, cookies });
  }

  if (info.menuItemId === MENU_VIDEO) {
    const src = info.srcUrl || '';
    const url = src && !src.startsWith('blob:') ? src : pageUrl;
    sendToNdm({ kind: 'video', url, pageUrl, title, cookies: await captureCookies(url, pageUrl) });
  }

  if (info.menuItemId === MENU_PAGE_VIDEO) {
    const sniffed = tab && tab.id != null ? tabMediaStreams.get(tab.id) : '';
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
    sendResponse({ streamUrl: (tabId != null && tabMediaStreams.get(tabId)) || '' });
    return false;
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
    const kind =
      message.kind === 'video_direct'
        ? 'video_direct'
        : message.kind === 'video'
        ? 'video'
        : 'file';
    (async () => {
      const cookies = await captureCookies(targetUrl, pageUrl);
      const payload = {
        kind,
        url: targetUrl,
        pageUrl,
        title,
        cookies,
      };
      if (kind === 'video_direct') {
        payload.selector = message.selector || '';
        payload.audio = !!message.audio;
        payload.dir = message.dir || '~/Downloads';
        payload.later = !!message.later;
        payload.total = Number(message.total) || 0;
      }
      const ok = await sendToNdm(payload);
      sendResponse({ ok });
    })();
    return true;
  }
});

/* ------------------------------------------------------------------ */
/* Browser download interception                                       */
/* ------------------------------------------------------------------ */

const suggestedNames = new Map();
if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener(item => {
    if (item && item.id != null && item.filename) {
      suggestedNames.set(item.id, item.filename);
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
    const mimeHit =
      !!item.mime &&
      (item.mime.startsWith('application/') ||
        item.mime.startsWith('video/') ||
        item.mime.startsWith('audio/')) &&
      item.mime !== 'application/xhtml+xml' &&
      item.mime !== 'application/json';
    const looksLikeFile =
      isDownloadableFile(url) ||
      isDownloadableFile(item.filename || '') ||
      mimeHit;
    if (!looksLikeFile) return;

    await chrome.downloads.pause(item.id).catch(() => {});
    await new Promise(r => setTimeout(r, 250));
    let suggested = suggestedNames.get(item.id) || item.filename || '';
    suggestedNames.delete(item.id);
    try {
      const [fresh] = await chrome.downloads.search({ id: item.id });
      if (
        fresh &&
        fresh.filename &&
        !/^[0-9a-f-]{36}$/i.test(
          fresh.filename.split(/[\\/]/).pop().replace(/\.[^.]*$/, '')
        )
      ) {
        suggested = fresh.filename;
      }
    } catch (e) {}
    const baseName = suggested.split(/[\\/]/).pop() || '';
    const accepted = await sendToNdm({
      kind: 'file',
      url,
      pageUrl: item.referrer || '',
      title: baseName,
      cookies: await captureCookies(url, item.referrer || ''),
    });
    if (accepted) {
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
