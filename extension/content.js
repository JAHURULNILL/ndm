'use strict';
/* Nill Download Manager: content script.
 *
 * Shows the IDM-style "⬇ Download with NDM" floating button at the top-right
 * corner of any downloadable video player. Clicking it opens an IDM-style
 * Video Download dialog RIGHT INSIDE THE BROWSER (inside a closed Shadow DOM)
 * displaying:
 *   - Video Thumbnail, Duration, and Channel/Uploader
 *   - Editable Title / File Name
 *   - Quality & Format Selection List with exact/estimated MB sizes
 *   - Save to Folder + Download Now / Download Later buttons
 */

(() => {
  // The background can inject this script as a fallback AND the
  // manifest can run it in the same frame: two instances would mean
  // duplicate listeners, two pills/modals and two speculative sends
  // per click. Run at most once per frame (page-hook.js already
  // guards itself the same way).
  if (window.__ndmContentLoaded) return;
  window.__ndmContentLoaded = true;

  // Streaming services whose videos are DRM-protected: never show a panel.
  const DRM_HOSTS = [
    'netflix.com',
    'primevideo.com',
    'disneyplus.com',
    'hotstar.com',
    'open.spotify.com',
  ];

  const host = location.hostname.toLowerCase();
  if (DRM_HOSTS.some(d => host === d || host.endsWith('.' + d))) return;

  let panelHost = null;   // shadow-DOM host element
  let shadowRoot = null;  // closed shadow root
  let cardEl = null;      // the floating pill element
  let modalEl = null;     // the in-browser IDM quality modal backdrop
  let flyCardEl = null;   // the video flyout card (inside modalEl while open)
  let flyPollTimer = null; // 1s progress poll for the flyout's progress view
  let anchor = null;      // the <video> the pill is attached to
  let dismissed = false;  // user closed the pill on this page
  let modalOpen = false;  // whether the in-browser modal is currently open
  let activeProbeToken = 0;
  // The open modal's own dismiss logic. Every close path —
  // backdrop, pill ×, SPA navigation, modal replacement — must go
  // through it, never the raw teardown alone, so the modal can
  // settle its speculative state ( timers, polls, an in-flight
  // start) as it closes. Dismiss semantics are IDM's: closing a
  // dialog NEVER discards a download that has started — only the
  // file dialog's labelled Cancel button does that (R3-EXT-02).
  let activeModalDismiss = null;

  function dismissActiveModal() {
    if (activeModalDismiss) {
      activeModalDismiss();
    } else {
      closeBrowserModal();
    }
  }

  function isKnownVideoSite() {
    if (
      (host === 'youtube.com' || host.endsWith('.youtube.com')) &&
      (location.pathname.startsWith('/watch') ||
        location.pathname.startsWith('/shorts/') ||
        location.pathname.startsWith('/live/') ||
        location.pathname.startsWith('/embed/'))
    ) {
      return true;
    }
    if (host === 'youtu.be') return true;
    if (host === 'facebook.com' || host.endsWith('.facebook.com') || host === 'fb.watch') {
      return document.querySelector('video') !== null ||
        location.pathname.includes('/videos/') ||
        location.pathname.startsWith('/watch') ||
        location.pathname.startsWith('/reel/');
    }
    if (
      host === 'instagram.com' || host.endsWith('.instagram.com') ||
      host === 'tiktok.com' || host.endsWith('.tiktok.com') ||
      host === 'vimeo.com' || host.endsWith('.vimeo.com') ||
      host === 'dailymotion.com' || host.endsWith('.dailymotion.com') ||
      host === 'x.com' ||
      host.endsWith('.x.com') ||
      host === 'twitter.com' ||
      host.endsWith('.twitter.com') ||
      host === 'twitch.tv' || host.endsWith('.twitch.tv') ||
      host === 'reddit.com' || host.endsWith('.reddit.com') ||
      host === 'bilibili.com' || host.endsWith('.bilibili.com')
    ) {
      return document.querySelector('video') !== null;
    }
    return false;
  }

  function hasPlayableVideoElement() {
    for (const v of document.querySelectorAll('video')) {
      const r = v.getBoundingClientRect();
      if (r.width < 180 || r.height < 100) continue;
      const src = v.currentSrc || v.src || '';
      if (src && !src.startsWith('blob:')) return true;
      const s = v.querySelector('source[src]');
      if (s && !(s.getAttribute('src') || '').startsWith('blob:')) return true;
      // Visible substantial video player that has loaded metadata or is playing
      if (r.width >= 240 && r.height >= 135 && (v.readyState >= 1 || !v.paused || v.duration > 3)) {
        return true;
      }
    }
    return false;
  }

  /* IDM's panel also appears on audio-only pages (with audio
     formats): an <audio> player with a real source counts as media
     worth offering the panel for. */
  function hasPlayableAudioElement() {
    for (const a of document.querySelectorAll('audio')) {
      const r = a.getBoundingClientRect();
      if (r.width < 180) continue;
      const src = a.currentSrc || a.src || '';
      if (src && !src.startsWith('blob:')) return true;
      const srcEl = a.querySelector('source[src]');
      if (srcEl && !(srcEl.getAttribute('src') || '').startsWith('blob:')) return true;
      if (a.readyState >= 1 || !a.paused || a.duration > 3) return true;
    }
    return false;
  }

  function videoDetected() {
    return isKnownVideoSite() || hasPlayableVideoElement() || hasPlayableAudioElement();
  }

  function pickAnchor() {
    let best = null;
    let bestArea = 0;
    for (const v of document.querySelectorAll('video')) {
      const r = v.getBoundingClientRect();
      if (r.width < 160 || r.height < 90) continue;
      const visW = Math.max(0, Math.min(r.right, innerWidth) - Math.max(r.left, 0));
      const visH = Math.max(0, Math.min(r.bottom, innerHeight) - Math.max(r.top, 0));
      const area = visW * visH;
      if (area > bestArea) {
        best = v;
        bestArea = area;
      }
    }
    if (best) return best;
    // audio-only page: anchor the pill to the widest visible player
    for (const a of document.querySelectorAll('audio')) {
      const r = a.getBoundingClientRect();
      if (r.width < 160) continue;
      const visW = Math.max(0, Math.min(r.right, innerWidth) - Math.max(r.left, 0));
      const visH = Math.max(0, Math.min(r.bottom, innerHeight) - Math.max(r.top, 0));
      const area = visW * visH;
      if (area > bestArea) {
        best = a;
        bestArea = area;
      }
    }
    return best;
  }

  /* Resolve the exact permalink for the video player the user clicked on,
     especially when browsing a feed (Facebook Feed/Watch, Instagram, TikTok, X)
     or a generic site with a direct <video> stream. */
  async function resolveVideoUrl(videoEl) {
    const href = location.href;
    // 1. If already on a dedicated video permalink, use it directly
    if (
      /youtube\.com\/(watch|shorts\/|live\/|embed\/)/i.test(href) ||
      /youtu\.be\//i.test(href) ||
      /facebook\.com\/.*(\/videos\/|\/reel\/|\/share\/(r|v)\/|watch\/?\?v=)/i.test(href) ||
      /instagram\.com\/(reel|p|tv)\//i.test(href) ||
      /tiktok\.com\/@[^/]+\/video\//i.test(href)
    ) {
      return href;
    }

    // 2. The element's OWN playable source outranks any neighbouring
    //    link: the ancestor walk below used to run first and return
    //    a "watch on YouTube" credit link out of the article subtree
    //    for a plain embedded file (EX-m3).
    if (videoEl) {
      const direct = videoEl.currentSrc || videoEl.src || '';
      if (/^https?:\/\//i.test(direct)) return direct;
      const sourceEl = videoEl.querySelector('source[src]');
      const sUrl = sourceEl ? sourceEl.getAttribute('src') || '' : '';
      if (/^https?:\/\//i.test(sUrl)) {
        try {
          return new URL(sUrl, href).href;
        } catch (e) {}
      }
    }

    // 3. Walk up from the element to find a post/reel/video
    //    permalink — scoped to the element's own post container:
    //    the old unbounded walk climbed into neighbouring posts'
    //    subtrees and could return THEIR permalink.
    if (videoEl) {
      const container = videoEl.closest
        ? videoEl.closest('article, [role="article"], [data-testid*="post" i]')
        : null;
      let cur = videoEl;
      for (let depth = 0; depth < 16 && cur && cur !== document.body; depth++, cur = cur.parentElement) {
        const links = cur.querySelectorAll ? cur.querySelectorAll('a[href]') : [];
        for (const a of links) {
          const h = a.href || '';
          if (
            /facebook\.com\/.*(\/reel\/\d+|\/videos\/\d+|watch\/?\?v=\d+|\/share\/(r|v)\/)/i.test(h) ||
            /instagram\.com\/(reel|p|tv)\/[^/?#]+/i.test(h) ||
            /tiktok\.com\/@[^/]+\/video\/\d+/i.test(h) ||
            /(twitter|x)\.com\/[^/]+\/status\/\d+/i.test(h) ||
            /youtube\.com\/(watch\?v=|shorts\/)/i.test(h)
          ) {
            return h.split('&__cft__')[0];
          }
        }
        if (container ? cur === container : depth >= 3) break;
      }
    }

    // 4. For generic sites: if the background worker sniffed an
    //    .m3u8 / .mpd / .mp4 stream for this page, use it. The
    //    request carries the page's canonical video key so the
    //    background can prefer THIS page's streams over the tab's
    //    older ones (EX-m2).
    if (!isKnownVideoSite() && videoEl) {
      try {
        const mediaResp = await chrome.runtime.sendMessage({
          action: 'ndm-tab-media',
          pageUrl: href,
          key: canonicalVideoKey(href),
        });
        if (mediaResp && mediaResp.streamUrl) return mediaResp.streamUrl;
      } catch (e) {}
    }

    return href;
  }

  function guessFallbackThumbnail(targetUrl, videoEl) {
    if (videoEl && videoEl.poster && /^https?:|^data:/i.test(videoEl.poster)) {
      return videoEl.poster;
    }
    try {
      const u = new URL(targetUrl);
      let ytId = u.searchParams.get('v');
      if (!ytId && u.pathname.startsWith('/shorts/')) {
        ytId = u.pathname.split('/')[2];
      }
      if (ytId && /^[\w-]{11}$/.test(ytId)) {
        return `https://i.ytimg.com/vi/${ytId}/hqdefault.jpg`;
      }
    } catch (e) {}
    const og = document.querySelector('meta[property="og:image"]');
    if (og && og.content) return og.content;
    return '';
  }

  function fmtDuration(sec) {
    const s = Math.max(0, Math.round(Number(sec) || 0));
    if (!s) return '';
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const r = s % 60;
    const pad = n => String(n).padStart(2, '0');
    return h > 0 ? `${h}:${pad(m)}:${pad(r)}` : `${m}:${pad(r)}`;
  }

  function fmtBytes(n) {
    const b = Number(n) || 0;
    if (b <= 0) return '';
    const units = ['B', 'KB', 'MB', 'GB'];
    let v = b;
    let i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(i >= 2 ? 1 : 0)} ${units[i]}`;
  }

  /* IDM's own number formats (progress dialog): sizes always
     carry 3 decimals with the unit picked by magnitude, the
     rate is "<v> KB/sec" under 1 MB/s and "<v> MB/sec" above,
     and the time left reads "57 min 5 sec" / "1 hour(s) 18 min"
     / "0 sec". Used only by the flyout's progress view, which
     mirrors IDM's dialog element for element. */
  function fmtIdmSize(n) {
    const b = Number(n) || 0;
    const units = ['KB', 'MB', 'GB', 'TB'];
    let v = b / 1024;
    let i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(3)} ${units[i]}`;
  }

  function fmtIdmRate(bps) {
    const r = Number(bps) || 0;
    if (r >= 1024 * 1024) return `${(r / 1048576).toFixed(3)} MB/sec`;
    return `${(r / 1024).toFixed(3)} KB/sec`;
  }

  function fmtIdmEta(secs) {
    const s = Math.max(0, Math.round(Number(secs) || 0));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const r = s % 60;
    if (h > 0) return `${h} hour(s) ${m} min`;
    if (m > 0) return `${m} min ${r} sec`;
    return `${r} sec`;
  }

  function cleanPageTitle() {
    return (document.title || 'video')
      .replace(/^\(\d+\)\s*/, '')
      .replace(/\s*[-|•]\s*(YouTube|Facebook|Instagram|TikTok|Vimeo|Dailymotion|X|Twitch).*$/i, '')
      .trim() || 'video';
  }

  /* Ensure panelHost is attached inside fullscreenElement when the player enters
     HTML5 Fullscreen so the NDM button and modal stay visible on top of the video. */
  function syncHostParent() {
    if (!panelHost) return;
    const targetParent = document.fullscreenElement || document.documentElement;
    if (targetParent && panelHost.parentNode !== targetParent) {
      targetParent.appendChild(panelHost);
    }
  }

  function place() {
    if (!panelHost || !cardEl) return;
    syncHostParent();
    if (modalOpen) {
      panelHost.style.display = 'block';
    }
    if (!anchor || !anchor.isConnected) anchor = pickAnchor();
    if (!anchor) {
      if (!modalOpen) panelHost.style.display = 'none';
      cardEl.style.display = 'none';
      return;
    }
    const r = anchor.getBoundingClientRect();
    if (r.width < 160 || r.bottom < 48 || r.top > innerHeight - 24) {
      if (!modalOpen) panelHost.style.display = 'none';
      cardEl.style.display = 'none';
      return;
    }
    panelHost.style.display = 'block';
    cardEl.style.display = 'flex';
    const w = cardEl.offsetWidth || 196;
    cardEl.style.left = Math.max(4, r.right - w - 12) + 'px';
    let pillTop = r.top + 12;
    // Small native widgets (a standard <audio> player is ~40 px
    // tall and nearly all controls): parking the pill ON the box
    // buries the player's own controls under it (F-EXT-16) —
    // float just above the widget, or just below it when there
    // is no room above.
    if ((anchor && anchor.tagName === 'AUDIO') || r.height <= 56) {
      const ph = cardEl.offsetHeight || 34;
      pillTop = r.top - ph - 6 >= 4 ? r.top - ph - 6 : r.bottom + 6;
    }
    cardEl.style.top = Math.max(4, pillTop) + 'px';
    // The video flyout rides the same anchor — keep it glued just
    // under the pill while the page scrolls / the player resizes.
    if (flyCardEl && flyCardEl.isConnected) {
      flyCardEl.style.top = Math.max(4, r.top + 52) + 'px';
      flyCardEl.style.right =
        Math.max(4, window.innerWidth - r.right + 12) + 'px';
      clampFlyIntoView(flyCardEl);
    }
  }

  /* Keep a flyout card fully inside the viewport (F-EXT-09):
     anchoring to a left-edge player used to extend the card LEFT
     past the screen edge (its `right` offset was positive, just
     large), and a bottom-edge player could push it off the
     bottom. Measures the card's ACTUAL rendered rect after
     positioning and pulls it back with an 8 px margin on every
     side (R3-EXT-09): the old style-parsing version engaged on
     audio pages too, but landed the card at a 4 px inset where
     its border and shadow still read as clipped against the
     screen edge — a rect check with a real margin is the honest
     guarantee, whatever width the card rendered at. */
  function clampFlyIntoView(el) {
    if (!el || !el.isConnected) return;
    const M = 8;
    const r = el.getBoundingClientRect();
    if (r.width <= 0) return;
    if (r.left < M) {
      const right = parseFloat(el.style.right);
      if (!isNaN(right)) {
        el.style.right = Math.max(M, right + (r.left - M)) + 'px';
      } else {
        el.style.right = Math.max(M, window.innerWidth - r.width - M) + 'px';
      }
    } else if (r.right > window.innerWidth - M) {
      const right = parseFloat(el.style.right);
      if (!isNaN(right)) {
        // too far right: a LARGER right offset moves the card left
        el.style.right =
          Math.max(M, right + (r.right - (window.innerWidth - M))) + 'px';
      }
    }
    const r2 = el.getBoundingClientRect();
    if (r2.height > 0 && r2.bottom > window.innerHeight - M) {
      const top = parseFloat(el.style.top);
      if (!isNaN(top)) {
        el.style.top =
          Math.max(M, top - (r2.bottom - (window.innerHeight - M))) + 'px';
      }
    }
    if (r2.top < M) {
      const top = parseFloat(el.style.top);
      if (!isNaN(top)) el.style.top = Math.max(M, top + (M - r2.top)) + 'px';
    }
  }

  const localProbeCache = new Map(); // canonicalUrl -> info
  const inFlightProbes = new Map();  // canonicalUrl -> Promise<res>
  /* Entries this page started from the video flyout, keyed
     `${canonicalUrl}|${selector}|${audio?'a':'v'}` ->
     { id, name }. The flyout is rebuilt on every open, so without
     this it reopens as a plain quality list with no sign that one
     of its rows is already downloading (R3-EXT-08): renderList
     consults the registry to badge the live row and route its
     click back to that entry's progress view. */
  const flyEntryRegistry = new Map();
  let lastPreProbedUrl = '';
  let onInstantInfoCallback = null;

  function canonicalVideoKey(urlStr) {
    try {
      const u = new URL(urlStr || location.href);
      if (u.hostname.includes('youtube.com')) {
        const v = u.searchParams.get('v');
        if (v) return `https://www.youtube.com/watch?v=${v}`;
        if (u.pathname.startsWith('/shorts/')) {
          const id = u.pathname.split('/shorts/')[1]?.split(/[/?#]/)[0];
          if (id) return `https://www.youtube.com/shorts/${id}`;
        }
      }
      if (u.hostname === 'youtu.be') {
        const id = u.pathname.slice(1).split(/[/?#]/)[0];
        if (id) return `https://www.youtube.com/watch?v=${id}`;
      }
      return u.href;
    } catch (e) {
      return urlStr || location.href;
    }
  }

  function getYouTubeVideoId(urlStr) {
    try {
      const u = new URL(urlStr || location.href);
      if (u.hostname.includes('youtube.com')) {
        if (u.pathname.startsWith('/shorts/')) {
          return u.pathname.split('/shorts/')[1]?.split(/[/?#]/)[0] || '';
        }
        return u.searchParams.get('v') || '';
      }
      if (u.hostname === 'youtu.be') {
        return u.pathname.slice(1).split(/[/?#]/)[0] || '';
      }
    } catch (e) {}
    return '';
  }

  window.addEventListener('message', e => {
    if (e.source !== window || !e.data || e.data.source !== 'ndm-page-hook') return;
    if (e.data.type === 'ndm-player-info' && e.data.info) {
      // Validate origin: only trust YouTube pages
      const host = location.hostname.toLowerCase();
      if (!host.endsWith('youtube.com') && host !== 'youtube.com') return;

      const expectedId = getYouTubeVideoId(location.href);
      const info = e.data.info;
      if (!expectedId || info.videoId !== expectedId) return;

      // Validate options format to prevent DOM event injection
      if (!Array.isArray(info.options) || !info.options.length) return;
      const validOptions = [];
      const SAFE_SELECTOR_RE = /^[a-zA-Z0-9_\-*+/[\]=<>. ]+$/;
      for (const opt of info.options) {
        if (!opt || typeof opt !== 'object') continue;
        if (typeof opt.selector !== 'string' || !SAFE_SELECTOR_RE.test(opt.selector)) continue;
        if (typeof opt.ext !== 'string' || !/^[a-zA-Z0-9]{2,5}$/.test(opt.ext)) continue;
        validOptions.push(opt);
      }
      if (!validOptions.length) return;
      info.options = validOptions;

      if (info.url) localProbeCache.set(canonicalVideoKey(info.url), info);
      if (e.data.pageUrl) localProbeCache.set(canonicalVideoKey(e.data.pageUrl), info);
      localProbeCache.set(canonicalVideoKey(location.href), info);
      if (typeof onInstantInfoCallback === 'function') {
        onInstantInfoCallback(info);
      }
    }
  });

  function requestMainWorldPlayerInfo() {
    try {
      window.postMessage({ source: 'ndm-content', type: 'ndm-request-player-info' }, '*');
    } catch (e) {}
  }

  function fetchOrReuseProbe(targetUrl, warm) {
    const key = canonicalVideoKey(targetUrl);
    if (localProbeCache.has(key)) {
      return Promise.resolve({ ok: true, info: localProbeCache.get(key) });
    }
    if (inFlightProbes.has(key)) {
      return inFlightProbes.get(key);
    }
    const p = chrome.runtime
      .sendMessage({
        action: 'ndm-probe',
        url: key,
        pageUrl: location.href,
        // a "warm" probe only pre-fetches formats for a pill the
        // user may click: it must not carry the user's session
        // cookies for a download they never asked about
        warm: !!warm,
      })
      .then(res => {
        inFlightProbes.delete(key);
        if (res && res.ok && res.info && (res.info.options || res.info.qualities || []).length) {
          localProbeCache.set(key, res.info);
        }
        return res;
      })
      .catch(err => {
        inFlightProbes.delete(key);
        throw err;
      });
    inFlightProbes.set(key, p);
    return p;
  }

  async function backgroundPreProbe() {
    requestMainWorldPlayerInfo();
    const curAnchor = anchor || pickAnchor();
    if (!curAnchor && !isKnownVideoSite()) return;
    // Feed pages hold many videos: firing a real (cookie-bearing)
    // probe for whichever one is momentarily dominant while the
    // user scrolls is exactly the passive-surveillance pattern IDM
    // never does — it resolves formats when its panel is clicked.
    // Warm only a page's main video, and warm probes carry no
    // session cookies (see fetchOrReuseProbe).
    if (document.querySelectorAll('video, audio').length > 3) return;
    const targetUrl = await resolveVideoUrl(curAnchor);
    const key = canonicalVideoKey(targetUrl);
    if (!key || key === lastPreProbedUrl || localProbeCache.has(key)) return;
    lastPreProbedUrl = key;
    // Wait briefly for MAIN-world page-hook on YouTube; if not populated, warm cache via ndm-host
    setTimeout(() => {
      if (!localProbeCache.has(key)) {
        fetchOrReuseProbe(key, true).catch(() => {});
      }
    }, 120);
  }

  function showBrowserToast(title, qualityLabel, sizeLabel, later, badgeText) {
    if (!shadowRoot) return;
    const old = shadowRoot.querySelector('.ndm-toast');
    if (old) old.remove();
    const toast = document.createElement('div');
    toast.className = 'ndm-toast';
    const badge = badgeText || (later ? '⏸ Queued in NDM' : '⬇ Downloading in NDM');
    const sub = [qualityLabel, sizeLabel].filter(Boolean).join(' • ');
    toast.innerHTML = `
      <div class="ndm-toast-icon">${badgeText ? '✓' : (later ? '⏸' : '⬇')}</div>
      <div class="ndm-toast-body">
        <div class="ndm-toast-head">${escapeHtml(badge)}</div>
        <div class="ndm-toast-title">${escapeHtml(title)}</div>
        ${sub ? `<div class="ndm-toast-sub">${escapeHtml(sub)}</div>` : ''}
      </div>
      <button class="ndm-toast-open" type="button">Show in NDM</button>
    `;
    for (const evt of ['mousedown', 'pointerdown', 'mouseup', 'click']) {
      toast.addEventListener(evt, e => e.stopPropagation());
    }
    toast.querySelector('.ndm-toast-open').addEventListener('click', () => {
      chrome.runtime.sendMessage({ action: 'ndm-show-app' }).catch(() => {});
      toast.remove();
    });
    shadowRoot.appendChild(toast);
    setTimeout(() => {
      if (toast.isConnected) toast.remove();
    }, 4500);
  }

  function closeBrowserModal() {
    activeProbeToken++;
    onInstantInfoCallback = null;
    modalOpen = false;
    activeModalDismiss = null;
    // The flyout's progress poll dies with the card — the download
    // itself is never touched by closing the panel.
    if (flyPollTimer) {
      clearInterval(flyPollTimer);
      flyPollTimer = null;
    }
    flyCardEl = null;
    if (modalEl) {
      modalEl.remove();
      modalEl = null;
    }
    place();
  }

  async function openBrowserModal(pillBtn, pillContainer, srcUrl, onShellOpen) {
    if (!shadowRoot) return;
    dismissActiveModal();
    modalOpen = true;
    const myToken = ++activeProbeToken;

    // EX-m4: a caller that names the exact stream (right-click on a
    // <video>) anchors the panel to THAT element, not the largest.
    let srcAnchor = null;
    if (srcUrl) {
      for (const v of document.querySelectorAll('video')) {
        if (v.currentSrc === srcUrl || v.src === srcUrl) { srcAnchor = v; break; }
        const sEl = v.querySelector('source[src]');
        if (sEl && sEl.src === srcUrl) { srcAnchor = v; break; }
      }
      if (srcAnchor) anchor = srcAnchor;
    }

    requestMainWorldPlayerInfo();
    const currentAnchor = srcAnchor || anchor || pickAnchor();
    const rawTargetUrl = await resolveVideoUrl(currentAnchor);
    const targetUrl = canonicalVideoKey(rawTargetUrl);
    const fallbackTitle = cleanPageTitle();

    let savedDir = '~/Downloads';
    try {
      const st = await chrome.storage.local.get({ ndmSaveDir: '~/Downloads' });
      if (st && st.ndmSaveDir) savedDir = st.ndmSaveDir;
    } catch (e) {}

    // A close (SPA navigation, pill removal, a replacement modal)
    // may have landed while the URL was being resolved: building
    // the card now would leave a zombie panel nobody can dismiss.
    if (myToken !== activeProbeToken || !shadowRoot || !panelHost) return;

    /* ── IDM panel behaviour ────────────────────────────────────
       A compact flyout anchored at the player — NOT a centered
       dialog. Header row "Download all", a media header (video
       thumbnail, title, duration line), then one row per quality
       ("1. <title>, MP4 file, quality 1080p • size"). NOTHING is
       sent to NDM until the user clicks a row (that format) or
       "Download all" (every format). After a click, THIS card
       becomes the progress view — IDM's download dialog itself:
       "<percent>% <filename>" in the header, the URL line and
       the Status / File size / Downloaded / Transfer rate /
       Time left rows, the green bar, Pause / Cancel. Closing
       the card (×, Esc, outside click) NEVER touches the
       download. */
    const backdrop = document.createElement('div');
    backdrop.className = 'ndm-fly-backdrop';
    for (const evt of ['mousedown', 'pointerdown', 'mouseup', 'dblclick']) {
      backdrop.addEventListener(evt, e => e.stopPropagation());
    }
    backdrop.addEventListener('click', e => {
      e.stopPropagation();
      if (e.target === backdrop) closeFly();
    });

    // Audio-only pages get honest wording (F-EXT-15): the panel
    // is reading an audio player, not a video.
    const loadingText =
      currentAnchor && currentAnchor.tagName === 'AUDIO'
        ? 'Reading the audio…'
        : 'Reading the video…';

    const card = document.createElement('div');
    card.className = 'ndm-fly';
    card.innerHTML = `
      <div class="ndm-fly-head">
        <button class="ndm-fly-all" type="button">Download all</button>
        <span class="ndm-fly-title" style="display:none"></span>
        <button class="ndm-fly-x" type="button" title="Close">×</button>
      </div>
      <div class="ndm-fly-body">
        <div class="ndm-fly-loading"><div class="ndm-spinner"></div><div>${loadingText}</div></div>
      </div>
      <div class="ndm-fly-foot" style="display:none">
        <button class="ndm-fly-app" type="button">Open in NDM App</button>
      </div>
    `;

    backdrop.appendChild(card);
    shadowRoot.appendChild(backdrop);
    modalEl = backdrop;
    flyCardEl = card;
    syncHostParent();
    panelHost.style.display = 'block';
    positionFly();
    // EX-m5 (kept): focus the backdrop so Esc works without a
    // first click inside the card.
    backdrop.tabIndex = -1;
    backdrop.focus();
    // EX-m12 (kept): the shell is open — callers waiting on this
    // open (popup) are answered NOW, not after the probe settles.
    if (typeof onShellOpen === 'function') onShellOpen();

    const bodyEl = card.querySelector('.ndm-fly-body');
    const footEl = card.querySelector('.ndm-fly-foot');
    const btnAll = card.querySelector('.ndm-fly-all');
    const btnX = card.querySelector('.ndm-fly-x');
    const btnAppLink = card.querySelector('.ndm-fly-app');
    const titleEl = card.querySelector('.ndm-fly-title');

    /* The card header is the "Download all" button in list view
       and the entry's "<percent>% <filename>" title in the
       progress views — IDM's progress-dialog title bar verbatim
       ("94% mult.flv": percent, one space, file name, F-EXT-05). */
    function showFlyTitle(text) {
      btnAll.style.display = 'none';
      titleEl.style.display = '';
      titleEl.textContent = text;
      titleEl.title = text;
    }
    function showFlyListHead() {
      titleEl.style.display = 'none';
      btnAll.style.display = '';
    }

    let probedTitle = fallbackTitle;
    let probedThumb = '';
    let probedDuration = 0;
    let probedUploader = '';
    let qualities = [];
    let rendered = false;
    let closed = false;
    let startBusy = false;
    let probeSettled = false; // rows (or the failure fallback) are on screen
    let wantAll = false;      // "Download all" clicked while still probing
    let warmSentFor = '';     // target URL the warm resolve was fired for

    // Used only when probing produced no format list at all (the
    // same two automatic choices the old dialog fell back to).
    const DEFAULT_QUALITIES = [
      {
        id: 'best',
        label: 'Best available quality (Auto)',
        selector: 'bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b',
        audio: false,
        ext: 'MP4',
        size_bytes: null,
        height: 1080,
      },
      {
        id: 'audio',
        // No format claim: probing failed, so the container the
        // audio job will land in is unknown from here (the app
        // saves the chosen stream as-is — M4A, MP3, …). The label
        // renders verbatim on the row, and '' is this file's "no
        // honest ext word" marker (see audioRowExt).
        label: 'Audio only',
        selector: 'ba/b',
        audio: true,
        ext: '',
        size_bytes: null,
        height: 0,
      },
    ];

    function positionFly() {
      const el = currentAnchor && currentAnchor.isConnected
        ? currentAnchor
        : anchor;
      if (!el) return;
      const r = el.getBoundingClientRect();
      card.style.top = Math.max(4, r.top + 52) + 'px';
      card.style.right = Math.max(4, window.innerWidth - r.right + 12) + 'px';
      clampFlyIntoView(card);
    }

    function stopPoll() {
      if (flyPollTimer) {
        clearInterval(flyPollTimer);
        flyPollTimer = null;
      }
    }

    function closeFly() {
      if (closed) return;
      closed = true;
      stopPoll();
      closeBrowserModal();
    }
    activeModalDismiss = closeFly;

    btnX.addEventListener('click', closeFly);
    backdrop.addEventListener('keydown', e => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        e.preventDefault();
        closeFly();
      }
    });

    // "Open in NDM App" — the fallback link, shown only when
    // probing failed: hand the page to the app's own video dialog
    // (kind 'video'), exactly like the old dialog's app button but
    // without the speculative entry it used to manage.
    btnAppLink.addEventListener('click', async () => {
      btnAppLink.disabled = true;
      try {
        const resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'video',
          url: targetUrl,
          pageUrl: location.href,
          title: probedTitle || fallbackTitle,
        });
        if (resp && resp.ok) {
          closeFly();
        } else {
          btnAppLink.disabled = false;
          showNote('Could not reach NDM desktop app. Please make sure NDM is installed.');
        }
      } catch (e) {
        btnAppLink.disabled = false;
        showNote('Could not reach NDM desktop app.');
      }
    });

    function showNote(msg) {
      let note = bodyEl.querySelector('.ndm-fly-note');
      if (!note) {
        note = document.createElement('div');
        note.className = 'ndm-fly-note';
        bodyEl.insertBefore(note, bodyEl.firstChild);
      }
      note.textContent = msg;
    }

    function qualityText(q) {
      if (q.id === 'best') return 'Best available';
      if (q.audio) return q.label || 'Audio';
      if (Number(q.height) > 0) return `${q.height}p`;
      return q.label || 'Best';
    }

    /* The extension the audio job will actually produce. An
       audio download saves the chosen stream in its OWN container
       — the app does not convert anything to MP3 — so the probe
       option's "MP3" label must never become the displayed file
       name (the card said big.mp3 while big.m4a landed on disk).
       Truth available on this side, in order: a direct audio
       file's own extension (the file is saved as-is); the format
       the option's selector pins first ([ext=m4a] — the stream
       the job prefers); otherwise no extension at all rather than
       an invented one. The progress payload carries no file name,
       so this derivation is all the card has. */
    function audioOutputExt(q) {
      try {
        const u = new URL(targetUrl);
        const tail = (u.pathname.split('/').pop() || '').toLowerCase();
        const dot = tail.lastIndexOf('.');
        if (dot > 0) {
          const e = tail.slice(dot + 1);
          if (['mp3', 'm4a', 'm4b', 'aac', 'flac', 'wav', 'ogg', 'opus', 'wma'].includes(e)) {
            return e;
          }
        }
      } catch (e) {}
      const pin = String((q && q.selector) || '').match(/\[ext=([a-z0-9]+)\]/i);
      if (pin) return pin[1].toLowerCase();
      return '';
    }

    /* Container word for an audio row in the list: the same
       derivation as the entry name, except the probe's explicit
       HLS label stays — an HLS rendition is not a file container
       (F-EXT-02). '' means "no honest word" and the caller falls
       back to plain "Audio". */
    function audioRowExt(q) {
      if (String((q && q.ext) || '').toLowerCase() === 'audio (hls)') {
        return q.ext;
      }
      return audioOutputExt(q);
    }

    function fileNameFor(q) {
      const title = probedTitle || fallbackTitle;
      if (q.audio) {
        const ext = audioOutputExt(q);
        return ext ? `${title}.${ext}` : title;
      }
      const ext = (q.ext || 'mp4').toLowerCase();
      return `${title}.${ext}`;
    }

    /* Registry key for the entry a row starts (R3-EXT-08). */
    function regKeyFor(q) {
      return `${targetUrl}|${(q && q.selector) || ''}|${q && q.audio ? 'a' : 'v'}`;
    }

    /* Map a raw probe/engine error to plain language for the
       flyout banner (R3-EXT-05). The raw text is yt-dlp / tool
       stderr: PyInstaller extraction crashes, tracebacks and
       library paths leaked verbatim into the banner. Only three
       honest shapes survive: an HTTP status the user can act on,
       a transport failure (where "check your connection" is the
       truth), and a short single-line message with no internals
       in it. Everything else collapses to '' and the caller uses
       its plain fallback sentence. */
    function plainErrorText(raw) {
      const t = String(raw || '').trim();
      if (!t) return '';
      const http = t.match(/HTTP Error (\d{3})(?::?\s*([A-Za-z][A-Za-z ]*))?/i) ||
        t.match(/\b(404 Not Found|403 Forbidden|410 Gone|429 Too Many Requests|5\d\d [A-Za-z ]+)\b/);
      if (http) {
        return http[2] ? `HTTP ${http[1]} ${http[2].trim()}` : `HTTP ${http[1]}`;
      }
      if (/unsupported url/i.test(t)) return 'This page is not a downloadable video.';
      if (
        /timed? ?out|name (resolution|or service)|failed to resolve|temporary failure in name|connection (refused|reset|aborted)|network is unreachable|no route to host|eof occurred|curl:\s*\(\d+\)/i.test(t)
      ) {
        return 'Could not reach the site — check your connection.';
      }
      const firstLine = t.split('\n')[0].replace(/^ERROR:\s*/i, '').trim();
      if (
        firstLine.length > 0 &&
        firstLine.length <= 90 &&
        !/traceback|pyinstaller|\.so\b|\.py\b|site-packages|\\|\/usr\/|\/tmp\/|appdata/i.test(firstLine)
      ) {
        return firstLine;
      }
      return '';
    }

    /* ── Sending ────────────────────────────────────────────────
       The ONLY two send sites of this panel: a row click (one
       format) and "Download all" (every format). No send happens
       on open, on probe completion, or on selection changes. */
    async function startQuality(q, force) {
      try {
        const resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'video_direct',
          url: targetUrl,
          pageUrl: location.href,
          title: probedTitle || fallbackTitle,
          selector: q.selector,
          audio: !!q.audio,
          dir: savedDir,
          later: false,
          total: Number(q.size_bytes) || 0,
          force: !!force,
        });
        if (resp && resp.ok && resp.id && !resp.dupe) return { id: resp.id };
        if (resp && resp.dupe) return { duplicate: true, id: resp.id || null };
        if (
          resp &&
          typeof resp.error === 'string' &&
          resp.error.startsWith('duplicate:')
        ) {
          // The app names the existing entry ("duplicate:<id>"):
          // keep the id — the flyout polls that entry before
          // choosing which state to show, so an ERRORED entry is
          // presented as an error, not as a plain duplicate
          // (R2-EXT-04).
          const dupId = resp.error.slice('duplicate:'.length).trim();
          return { duplicate: true, id: dupId || resp.id || null };
        }
        // A send timeout is not a refusal: the request may still
        // land in the app — but with no entry id there is nothing
        // to poll, so the card degrades to the static state.
        if (resp && resp.unknown) return { unknown: true };
        return {
          error:
            (resp && resp.error) ||
            'NDM could not be started. Make sure the NDM desktop app is installed.',
        };
      } catch (e) {
        return { error: 'Could not reach NDM desktop app.' };
      }
    }

    async function onRowClick(q) {
      if (startBusy || closed) return;
      startBusy = true;
      card.classList.add('busy');
      btnAll.disabled = true;
      const res = await startQuality(q, false);
      if (closed) return;
      card.classList.remove('busy');
      if (res.id && !res.duplicate) {
        flyEntryRegistry.set(regKeyFor(q), { id: res.id, name: fileNameFor(q) });
        showProgress({ id: res.id, name: fileNameFor(q), regKey: regKeyFor(q) });
      } else if (res.unknown) {
        showProgress({ id: null, name: fileNameFor(q) });
      } else if (res.duplicate) {
        showDuplicate(q, res.id);
      } else {
        startBusy = false;
        btnAll.disabled = false;
        showNote(res.error);
      }
    }

    async function onDownloadAll() {
      if (startBusy || closed) return;
      startBusy = true;
      card.classList.add('busy');
      btnAll.disabled = true;
      const list = qualities.length ? qualities : DEFAULT_QUALITIES;
      const started = [];
      let skipped = 0;
      for (const q of list) {
        const res = await startQuality(q, false);
        if (closed) return;
        if (res.id) {
          flyEntryRegistry.set(regKeyFor(q), { id: res.id, name: fileNameFor(q) });
          started.push({
            id: res.id,
            regKey: regKeyFor(q),
            name: `${probedTitle || fallbackTitle} — ${qualityText(q)}`,
          });
        } else {
          skipped++;
        }
      }
      card.classList.remove('busy');
      if (started.length) {
        showMultiProgress(
          started,
          skipped
            ? `${skipped} format${skipped > 1 ? 's' : ''} skipped (already in the list or refused).`
            : ''
        );
      } else {
        startBusy = false;
        btnAll.disabled = false;
        showNote('Nothing could be started — the formats may already be in your download list.');
      }
    }
    btnAll.addEventListener('click', () => {
      if (!probeSettled) {
        // Clicked while the probe is still running: REGISTER the
        // intent instead of swallowing it (F-EXT-14) — it fires
        // the moment the rows land, and the button says so.
        if (!wantAll) {
          wantAll = true;
          btnAll.textContent = 'Download all — waiting…';
        }
        return;
      }
      onDownloadAll();
    });

    /* The existing entry is in ERROR: the error state wins over
       the duplicate state (R2-EXT-04). The user sees what actually
       went wrong and gets a Retry that resumes THAT entry (the
       card then follows it live), instead of an "already in your
       list" note whose only exits were a forced second copy or
       leaving for the app. */
    function showErroredExisting(q, id, r) {
      startBusy = false;
      btnAll.disabled = false;
      showFlyTitle(fileNameFor(q));
      const msg =
        (r && typeof r.error === 'string' && r.error) ||
        'The download stopped with an error in NDM.';
      bodyEl.innerHTML = `
        <div class="ndm-fly-prog">
          <div class="ndm-fly-purl"></div>
          <div class="ndm-fly-prow"><span class="ndm-fly-plab">Status</span><span class="ndm-fly-pval">Error</span></div>
          <div class="ndm-fly-pmsg">${escapeHtml(msg)}</div>
          <div class="ndm-fly-btns">
            <button class="ndm-pbtn ndm-pbtn-def ndm-fly-retry" type="button">Retry</button>
            <button class="ndm-pbtn ndm-fly-showe" type="button">Show in NDM</button>
          </div>
        </div>
      `;
      const urlElE = bodyEl.querySelector('.ndm-fly-purl');
      urlElE.textContent = targetUrl;
      urlElE.title = targetUrl;
      bodyEl.querySelector('.ndm-fly-showe').addEventListener('click', () => {
        chrome.runtime.sendMessage({ action: 'ndm-show-app' }).catch(() => {});
      });
      bodyEl.querySelector('.ndm-fly-retry').addEventListener('click', async ev => {
        const btn = ev.currentTarget;
        btn.disabled = true;
        try {
          await chrome.runtime.sendMessage({
            action: 'ndm-resume-entry',
            entry: id,
          });
        } catch (e) {}
        if (closed) return;
        // Follow the entry live: the poll renders whatever the
        // truth now is — downloading, paused, finished, or the
        // error again if the retry did not take.
        flyEntryRegistry.set(regKeyFor(q), { id, name: fileNameFor(q) });
        showProgress({ id, name: fileNameFor(q), regKey: regKeyFor(q) });
      });
    }

    async function showDuplicate(q, existingId) {
      startBusy = false;
      // State selection (R2-EXT-04): when the app named the
      // existing entry, ask what state it is in BEFORE presenting
      // the duplicate state — an errored entry must surface as an
      // error with a retry, never as a plain "already in list".
      // Any other state (finished, downloading, paused) — or no
      // answer at all — keeps the duplicate state below.
      if (existingId) {
        const r = await pollOnce(existingId);
        if (closed) return;
        if (r && String(r.status || '') === 'error') {
          showErroredExisting(q, existingId, r);
          return;
        }
      }
      showFlyListHead();
      btnAll.disabled = false;
      bodyEl.innerHTML = `
        <div class="ndm-fly-note">This video is already in your download list.</div>
        <div class="ndm-fly-dupe">
          <button class="ndm-btn ndm-btn-secondary ndm-fly-again" type="button">Download again</button>
          <button class="ndm-btn ndm-btn-secondary ndm-fly-showd" type="button">Show in NDM</button>
        </div>
      `;
      bodyEl.querySelector('.ndm-fly-showd').addEventListener('click', () => {
        chrome.runtime.sendMessage({ action: 'ndm-show-app' }).catch(() => {});
      });
      bodyEl.querySelector('.ndm-fly-again').addEventListener('click', async ev => {
        const btn = ev.currentTarget;
        btn.disabled = true;
        const res = await startQuality(q, true);
        if (closed) return;
        if (res.id) {
          flyEntryRegistry.set(regKeyFor(q), { id: res.id, name: fileNameFor(q) });
          showProgress({ id: res.id, name: fileNameFor(q), regKey: regKeyFor(q) });
        } else if (res.unknown) {
          showProgress({ id: null, name: fileNameFor(q) });
        } else {
          btn.disabled = false;
          showNote(res.error || 'NDM refused the second copy.');
        }
      });
    }

    /* ── Progress view ──────────────────────────────────────────
       IDM's download dialog, element for element (SPEC:
       ndm-research/idm-dialog-ref/SPEC.md): the card header is
       the title bar "<percent>% <filename>"; the body is the
       "Download status" panel — URL line, then Status / File
       size / Downloaded / Transfer rate / Time left / Resume
       capability rows in IDM's own number formats — over the
       classic green bar, with IDM's details toggle and "Show
       in NDM" on the left and Pause↔Resume and Cancel on the
       right; the toggle expands the per-connection chart +
       table (SPEC §1.7). The tab strip carries exactly the two
       tabs this card can drive for real — Download status and
       Speed Limiter (the limiter tab drives the app's own
       per-entry set-limit command); IDM's remaining tabs are
       per-file controls with no honest flyout equivalent, so
       they stay unrendered rather than decorative. The poll carries
       the mirror's fields — {status, done_bytes, total,
       rate_bps, eta_secs, error} plus {resume_capability,
       conns, conns_detail, blocks} from an app new enough to
       mirror them (an older app's silence just hides the row
       and the toggle). Polls the entry every 1 s through the
       background ({action:'ndm-entry-progress', id} → the
       host's {type:'progress', id}). ANY non-ok answer (a
       background/host without the channel) degrades the card
       to a static "Downloading in NDM…" state — never
       invented numbers. */
    async function pollOnce(id) {
      try {
        const r = await chrome.runtime.sendMessage({
          action: 'ndm-entry-progress',
          id,
        });
        return r && r.ok ? r : null;
      } catch (e) {
        return null;
      }
    }

    function showProgress(entry) {
      stopPoll();
      btnAll.disabled = true;
      showFlyTitle(entry.name);

      /* ── IDM details section (SPEC §1.7) ────────────────────
         Behind the button row's left-slot toggle: the centred
         caption, the segment chart drawn from the poll's block
         map, and the N. / Downloaded / Info table from its
         per-connection bytes. Everything renders from the
         mirror's real fields only; a card talking to an older
         app (which mirrors neither field) never shows the
         toggle. The helpers live here because the live view and
         the settled Complete view share them — the panel keeps
         its details across the finish line, toggle state and
         all. */
      let detailsOpen = false;

      /* IDM's Resume capability value is a bare Yes/No; the
         app's payload carries the same answer with its reason
         attached ("Yes (Range supported)") — the row shows
         IDM's word for the app's answer. */
      function idmResumeWord(raw) {
        const s = String(raw || '').trim();
        if (/^yes\b/i.test(s)) return 'Yes';
        if (/^no\b/i.test(s)) return 'No';
        return s;
      }

      function detailsHtml() {
        return `
          <div class="ndm-fly-details" style="display:none">
            <div class="ndm-fly-dcap">Start positions and download progress by connections</div>
            <div class="ndm-fly-dchart" style="display:none"></div>
            <div class="ndm-fly-dtwrap" style="display:none">
              <table class="ndm-fly-dtable">
                <thead><tr><th class="ndm-dc-n">N.</th><th class="ndm-dc-d">Downloaded</th><th class="ndm-dc-i">Info</th></tr></thead>
                <tbody></tbody>
              </table>
            </div>
          </div>
        `;
      }

      function wireDetails(root) {
        const els = {
          toggle: root.querySelector('.ndm-fly-dtoggle'),
          wrap: root.querySelector('.ndm-fly-details'),
          chart: root.querySelector('.ndm-fly-dchart'),
          twrap: root.querySelector('.ndm-fly-dtwrap'),
          tbody: root.querySelector('.ndm-fly-dtable tbody'),
          chartKey: null,
        };
        if (els.toggle) {
          els.toggle.addEventListener('click', () => {
            detailsOpen = !detailsOpen;
            paintDetails(els);
          });
        }
        return els;
      }

      /* IDM's per-connection Info vocabulary, from the poll's
         own numbers only: a connection inside the live count
         that has bytes is receiving; inside the count without
         bytes it has not started receiving; anything else (a
         finished session's history, a paused entry) states
         nothing. The row filter is the app dialog's own: a row
         exists once its connection has bytes, or while it is
         one of the live connections. */
      function detailsRows() {
        if (!last) return [];
        const det = Array.isArray(last.conns_detail) ? last.conns_detail : [];
        const conns = Number(last.conns) || 0;
        const live = String(last.status || '') === 'downloading';
        const rows = [];
        det.forEach((b, i) => {
          const bytes = Number(b) || 0;
          if (!(bytes > 0 || (live && i < conns))) return;
          rows.push({
            n: i + 1,
            bytes,
            info:
              live && i < conns
                ? bytes > 0
                  ? 'Receiving data...'
                  : 'Connecting...'
                : '',
          });
        });
        return rows;
      }

      function paintDetails(els) {
        if (!els || !els.toggle || !els.wrap) return;
        if (!last) {
          els.toggle.style.display = 'none';
          els.wrap.style.display = 'none';
          return;
        }
        const blocks = typeof last.blocks === 'string' ? last.blocks : '';
        const rows = detailsRows();
        const has = blocks.length > 0 || rows.length > 0;
        els.toggle.style.display = has ? '' : 'none';
        const open = has && detailsOpen;
        els.wrap.style.display = open ? '' : 'none';
        els.toggle.textContent = open ? '<< Hide details' : 'Show details >>';
        if (!open) return;
        if (blocks.length > 0) {
          els.chart.style.display = '';
          if (els.chartKey !== blocks) {
            els.chartKey = blocks;
            els.chart.innerHTML = blocks
              .split('')
              .map(c => `<i class="${c === '1' ? 'b1' : c === '2' ? 'b2' : 'b0'}"></i>`)
              .join('');
          }
        } else {
          els.chart.style.display = 'none';
        }
        if (rows.length > 0) {
          els.twrap.style.display = '';
          els.tbody.innerHTML = rows
            .map(
              r =>
                `<tr><td class="ndm-dc-n">${r.n}</td><td class="ndm-dc-d">${fmtIdmSize(r.bytes)}</td><td class="ndm-dc-i">${r.info}</td></tr>`
            )
            .join('');
        } else {
          els.twrap.style.display = 'none';
        }
      }

      bodyEl.innerHTML = `
        <div class="ndm-fly-prog">
          <div class="ndm-fly-tabs" style="display:none">
            <button class="ndm-fly-tab ndm-fly-tab-status ndm-fly-tab-on" type="button">Download status</button>
            <button class="ndm-fly-tab ndm-fly-tab-limit" type="button">Speed Limiter</button>
          </div>
          <div class="ndm-fly-pane ndm-fly-pane-status">
          <div class="ndm-fly-purl"></div>
          <div class="ndm-fly-prow"><span class="ndm-fly-plab">Status</span><span class="ndm-fly-pval ndm-fly-pstatusv">Downloading in NDM…</span></div>
          <div class="ndm-fly-prow ndm-fly-rfsize" style="display:none"><span class="ndm-fly-plab">File size</span><span class="ndm-fly-pval ndm-fly-pfsize"></span></div>
          <div class="ndm-fly-prow ndm-fly-rdown" style="display:none"><span class="ndm-fly-plab">Downloaded</span><span class="ndm-fly-pval ndm-fly-pdown"></span></div>
          <div class="ndm-fly-prow ndm-fly-rrate" style="display:none"><span class="ndm-fly-plab">Transfer rate</span><span class="ndm-fly-pval ndm-fly-prate"></span></div>
          <div class="ndm-fly-prow ndm-fly-rtime" style="display:none"><span class="ndm-fly-plab">Time left</span><span class="ndm-fly-pval ndm-fly-ptime"></span></div>
          <div class="ndm-fly-prow ndm-fly-rresume" style="display:none"><span class="ndm-fly-plab">Resume capability</span><span class="ndm-fly-pval ndm-fly-presume"></span></div>
          </div>
          <div class="ndm-fly-pane ndm-fly-pane-limit" style="display:none">
            <div class="ndm-fly-prow"><span class="ndm-fly-plab">Current speed</span><span class="ndm-fly-pval ndm-fly-lcur"></span></div>
            <label class="ndm-fly-lrow ndm-fly-luserow"><input type="checkbox" class="ndm-fly-luse"> Use Speed Limiter</label>
            <div class="ndm-fly-lrow">
              <span>Maximum download speed:</span>
              <input type="text" class="ndm-fly-lval" inputmode="decimal" autocomplete="off" spellcheck="false" disabled>
              <select class="ndm-fly-lunit" disabled>
                <option value="1024">KB/sec</option>
                <option value="1048576">MB/sec</option>
              </select>
            </div>
            <div class="ndm-fly-lnote"></div>
          </div>
          <div class="ndm-fly-bar"><div class="ndm-fly-bar-fill"></div></div>
          <div class="ndm-fly-btns">
            <button class="ndm-pbtn ndm-fly-dtoggle" type="button" style="display:none">Show details &gt;&gt;</button>
            <button class="ndm-pbtn ndm-fly-show" type="button" style="display:none">Show in NDM</button>
            <span class="ndm-fly-btnsp"></span>
            <button class="ndm-pbtn ndm-fly-pause" type="button">Pause</button>
            <button class="ndm-pbtn ndm-fly-resume" type="button" style="display:none">Resume</button>
            <button class="ndm-pbtn ndm-fly-cancel" type="button">Cancel</button>
          </div>
          ${detailsHtml()}
        </div>
      `;
      const urlEl = bodyEl.querySelector('.ndm-fly-purl');
      urlEl.textContent = targetUrl;
      urlEl.title = targetUrl;
      const statusEl = bodyEl.querySelector('.ndm-fly-pstatusv');
      const fsizeEl = bodyEl.querySelector('.ndm-fly-pfsize');
      const downEl = bodyEl.querySelector('.ndm-fly-pdown');
      const rateEl = bodyEl.querySelector('.ndm-fly-prate');
      const timeEl = bodyEl.querySelector('.ndm-fly-ptime');
      const resumeEl = bodyEl.querySelector('.ndm-fly-presume');
      const rowsEl = {
        fsize: bodyEl.querySelector('.ndm-fly-rfsize'),
        down: bodyEl.querySelector('.ndm-fly-rdown'),
        rate: bodyEl.querySelector('.ndm-fly-rrate'),
        time: bodyEl.querySelector('.ndm-fly-rtime'),
        resume: bodyEl.querySelector('.ndm-fly-rresume'),
      };
      const progEls = wireDetails(bodyEl);
      const fillEl = bodyEl.querySelector('.ndm-fly-bar-fill');
      const btnPause = bodyEl.querySelector('.ndm-fly-pause');
      const btnResume = bodyEl.querySelector('.ndm-fly-resume');
      const btnCancel = bodyEl.querySelector('.ndm-fly-cancel');
      const btnShow = bodyEl.querySelector('.ndm-fly-show');
      /* ── IDM tab strip (SPEC §1.2) ─────────────────────────
         "Download status" | "Speed Limiter" above the panel
         body; the bar, the button row and the details section
         sit below both panes, exactly as in IDM's dialog, so
         switching tabs never disturbs the poll or the download.
         The strip appears only once the progress mirror has
         reported the entry's speed_limit — a card talking to an
         older app (which mirrors no such field) keeps the
         single-panel view it has always had. */
      const tabsEl = bodyEl.querySelector('.ndm-fly-tabs');
      const tabBtnStatus = bodyEl.querySelector('.ndm-fly-tab-status');
      const tabBtnLimit = bodyEl.querySelector('.ndm-fly-tab-limit');
      const paneStatus = bodyEl.querySelector('.ndm-fly-pane-status');
      const paneLimit = bodyEl.querySelector('.ndm-fly-pane-limit');
      const lUse = bodyEl.querySelector('.ndm-fly-luse');
      const lVal = bodyEl.querySelector('.ndm-fly-lval');
      const lUnit = bodyEl.querySelector('.ndm-fly-lunit');
      const lCur = bodyEl.querySelector('.ndm-fly-lcur');
      const lNote = bodyEl.querySelector('.ndm-fly-lnote');
      let activeTab = 'status';
      let limitFocus = false;
      let prevUnit = 1024;
      let limitBusy = false;
      // m-10: the exact bytes/sec the display was last synced
      // from. The field shows only 2 decimals, so re-applying an
      // untouched panel would otherwise SEND the rounded readback
      // (1,048,575 B/s → "1024" KB/sec → 1,048,576 B/s).
      let limExactBps = 0;

      function switchTab(name) {
        activeTab = name;
        tabBtnStatus.classList.toggle('ndm-fly-tab-on', name === 'status');
        tabBtnLimit.classList.toggle('ndm-fly-tab-on', name === 'limit');
        paneStatus.style.display = name === 'status' ? '' : 'none';
        paneLimit.style.display = name === 'limit' ? '' : 'none';
      }
      tabBtnStatus.addEventListener('click', () => switchTab('status'));
      tabBtnLimit.addEventListener('click', () => switchTab('limit'));
      paneLimit.addEventListener('focusin', () => {
        limitFocus = true;
      });
      paneLimit.addEventListener('focusout', () => {
        limitFocus = false;
      });

      function fmtLimVal(x) {
        return String(Math.round(x * 100) / 100);
      }

      /* Send the panel's current limiter state to the app
         through the bridge (background → host → the app's own
         set_entry_limit: entry field + live engine/job handles
         + persist). Unchecked = clear (null). A rejection is
         reported in the panel, never silently swallowed — and
         the next poll re-syncs the controls from the mirror, so
         the panel always settles on the app's truth. */
      async function applyLimiter() {
        if (!entry.id || limitBusy) return;
        let bps = null;
        if (lUse.checked) {
          const v = parseFloat(String(lVal.value).replace(',', '.'));
          const mult = Number(lUnit.value) || 1024;
          if (!isFinite(v) || v <= 0) {
            lNote.textContent = 'Enter a maximum speed above zero.';
            return;
          }
          bps = Math.max(1024, Math.round(v * mult));
          // m-10: a readback within half the display quantum
          // (0.005 of the current unit) of the synced value is
          // the untouched panel, not an edit — send the exact
          // synced bytes/sec. A deliberate edit larger than the
          // quantum still goes through as typed.
          if (limExactBps > 0 && Math.abs(bps - limExactBps) <= mult / 200 + 1) {
            bps = limExactBps;
          }
        }
        if (bps !== null) limExactBps = bps;
        limitBusy = true;
        try {
          const r = await chrome.runtime.sendMessage({
            action: 'ndm-set-speed-limit',
            entry: entry.id,
            limit_bps: bps,
          });
          lNote.textContent =
            r && r.ok ? '' : (r && r.error) || 'NDM rejected the speed limit.';
        } catch (e) {
          lNote.textContent = 'Could not reach the NDM app.';
        }
        limitBusy = false;
        tick();
      }
      lUse.addEventListener('change', () => {
        const on = lUse.checked;
        lVal.disabled = !on;
        lUnit.disabled = !on;
        if (on && !(parseFloat(String(lVal.value).replace(',', '.')) > 0)) {
          lVal.value = '500';
          lUnit.value = '1024';
          prevUnit = 1024;
        }
        applyLimiter();
      });
      lVal.addEventListener('change', () => {
        if (lUse.checked) applyLimiter();
      });
      lUnit.addEventListener('change', () => {
        // Keep the effective limit constant across the unit
        // switch (re-express the same bytes/sec in the new unit),
        // then apply it like any other edit.
        const newMult = Number(lUnit.value) || 1024;
        const v = parseFloat(String(lVal.value).replace(',', '.'));
        if (isFinite(v) && v > 0) {
          lVal.value = fmtLimVal((v * prevUnit) / newMult);
        }
        prevUnit = newMult;
        if (lUse.checked) applyLimiter();
      });

      /* Reflect the mirror's truth in the limiter panel: the
         strip's existence, the live rate on the tab's own speed
         line (IDM shows it there), and the controls' pre-fill
         from the entry's current limit. While the user is
         editing inside the panel the poll must not fight them —
         their change events apply, and sync resumes on blur. */
      function syncLimiter() {
        const has = !!last && typeof last.speed_limit === 'number';
        tabsEl.style.display = has ? '' : 'none';
        if (!has) {
          if (activeTab !== 'status') switchTab('status');
          return;
        }
        lCur.textContent = fmtIdmRate(last.rate_bps);
        if (limitFocus) return;
        const lim = Number(last.speed_limit) || 0;
        limExactBps = lim;
        lUse.checked = lim > 0;
        lVal.disabled = lim <= 0;
        lUnit.disabled = lim <= 0;
        if (lim > 0) {
          const mb = lim >= 1048576;
          prevUnit = mb ? 1048576 : 1024;
          lUnit.value = String(prevUnit);
          lVal.value = fmtLimVal(lim / prevUnit);
        } else if (!lVal.value) {
          lVal.value = '500';
        }
      }
      /* IDM renders the live Status value as blue underlined
         link-style text; settled states (Paused, Cancelled, the
         no-data fallback) read as plain dialog text. */
      function setStatusLive(on) {
        statusEl.classList.toggle('ndm-fly-pstatus', !!on);
      }

      let last = null;       // last good progress reply
      let uiPaused = false;  // Pause pressed here (poll may lag/never answer)
      let terminal = false;

      /* Keep the reopen-badge registry truthful (R3-EXT-08): a
         cancelled or errored entry is no longer "live" for its
         list row — the row goes back to being a plain starter. */
      function syncRegistry(st) {
        if (!entry.regKey) return;
        if (st === 'cancelled' || st === 'error') {
          flyEntryRegistry.delete(entry.regKey);
        }
      }

      function render() {
        if (!last && !uiPaused) {
          statusEl.textContent = 'Downloading in NDM…';
          setStatusLive(false);
          rowsEl.fsize.style.display = 'none';
          rowsEl.down.style.display = 'none';
          rowsEl.rate.style.display = 'none';
          rowsEl.time.style.display = 'none';
          rowsEl.resume.style.display = 'none';
          paintDetails(progEls);
          syncLimiter();
          btnPause.style.display = '';
          btnResume.style.display = 'none';
          btnShow.style.display = 'none';
          return;
        }
        if (!last && uiPaused) {
          statusEl.textContent = 'Paused';
          setStatusLive(false);
          rowsEl.fsize.style.display = 'none';
          rowsEl.down.style.display = 'none';
          rowsEl.rate.style.display = 'none';
          rowsEl.time.style.display = 'none';
          rowsEl.resume.style.display = 'none';
          paintDetails(progEls);
          syncLimiter();
          btnPause.style.display = 'none';
          btnResume.style.display = '';
          btnShow.style.display = '';
          return;
        }
        const total = Number(last.total) || 0;
        const done = Number(last.done_bytes) || 0;
        const pct =
          total > 0 ? Math.min(100, Math.floor((done / total) * 100)) : null;
        const st = String(last.status || '');
        // Every byte is in but the entry has not settled to
        // 'finished': the app is merging / finalising. There is
        // nothing left to pause or cancel — those buttons must
        // not stay live in this window.
        const finalizing = st !== 'paused' && total > 0 && done >= total;
        // The "<percent>% <filename>" line belongs to the card's
        // title bar only (IDM's progress-dialog title); the body
        // carries the labelled status rows instead. Printing the
        // same line in both places was pure duplication.
        showFlyTitle(pct != null ? `${pct}% ${entry.name}` : entry.name);
        statusEl.textContent =
          st === 'paused'
            ? 'Paused'
            : finalizing
              ? 'Finishing...'
              : st === 'queued'
                ? 'Queued'
                : 'Receiving data...';
        setStatusLive(st !== 'paused');
        fillEl.style.width = pct != null ? pct + '%' : '0%';
        // File size / Downloaded (SPEC §1.3): a row appears only
        // when its number is actually known — never invented.
        rowsEl.fsize.style.display = total > 0 ? '' : 'none';
        if (total > 0) fsizeEl.textContent = fmtIdmSize(total);
        rowsEl.down.style.display = total > 0 || done > 0 ? '' : 'none';
        if (total > 0) {
          downEl.textContent =
            `${fmtIdmSize(done)} ( ${((done / total) * 100).toFixed(2)} %)`;
        } else if (done > 0) {
          downEl.textContent = fmtIdmSize(done);
        }
        // Transfer rate / Time left describe a live transfer
        // only: while paused or finalising the rows leave with
        // the numbers they would repeat. A stalled transfer
        // honestly reads 0.000 KB/sec (the app zeroes stale
        // rates itself, F-15).
        const active = st !== 'paused' && !finalizing;
        rowsEl.rate.style.display = active ? '' : 'none';
        if (active) rateEl.textContent = fmtIdmRate(last.rate_bps);
        const eta = Number(last.eta_secs) || 0;
        rowsEl.time.style.display = active && eta > 0 ? '' : 'none';
        if (active && eta > 0) timeEl.textContent = fmtIdmEta(eta);
        // Resume capability is a property of the download, not
        // of the live transfer: the row stays in every state the
        // poll has spoken for, as long as the app mirrored an
        // answer (an older app's silence hides the row).
        const resumeWord = idmResumeWord(last.resume_capability);
        rowsEl.resume.style.display = resumeWord ? '' : 'none';
        if (resumeWord) resumeEl.textContent = resumeWord;
        paintDetails(progEls);
        if (finalizing) {
          btnPause.style.display = 'none';
          btnResume.style.display = 'none';
          btnCancel.style.display = 'none';
          btnShow.style.display = '';
        } else {
          btnPause.style.display = st === 'paused' ? 'none' : '';
          btnResume.style.display = st === 'paused' ? '' : 'none';
          btnCancel.style.display = '';
          btnShow.style.display = st === 'paused' ? '' : 'none';
        }
        syncLimiter();
      }

      function renderFinished() {
        terminal = true;
        stopPoll();
        showFlyTitle(`100% ${entry.name}`);
        // IDM closes its dialog at 100%; the card lives in the
        // page, so it settles into the same panel carrying IDM's
        // own finished word — "Complete", its main-list Status —
        // and the final numbers from the last poll, when the poll
        // ever gave any.
        bodyEl.innerHTML = `
          <div class="ndm-fly-prog">
            <div class="ndm-fly-purl"></div>
            <div class="ndm-fly-prow"><span class="ndm-fly-plab">Status</span><span class="ndm-fly-pval">Complete</span></div>
            <div class="ndm-fly-prow ndm-fly-rfsize2" style="display:none"><span class="ndm-fly-plab">File size</span><span class="ndm-fly-pval ndm-fly-pfsize2"></span></div>
            <div class="ndm-fly-prow ndm-fly-rdown2" style="display:none"><span class="ndm-fly-plab">Downloaded</span><span class="ndm-fly-pval ndm-fly-pdown2"></span></div>
            <div class="ndm-fly-prow ndm-fly-rresume2" style="display:none"><span class="ndm-fly-plab">Resume capability</span><span class="ndm-fly-pval ndm-fly-presume2"></span></div>
            <div class="ndm-fly-bar"><div class="ndm-fly-bar-fill" style="width:100%"></div></div>
            <div class="ndm-fly-btns">
              <button class="ndm-pbtn ndm-fly-dtoggle" type="button" style="display:none">Show details &gt;&gt;</button>
              <button class="ndm-pbtn ndm-fly-show2" type="button">Show in NDM</button>
              <span class="ndm-fly-btnsp"></span>
            </div>
            ${detailsHtml()}
          </div>
        `;
        const urlEl2 = bodyEl.querySelector('.ndm-fly-purl');
        urlEl2.textContent = targetUrl;
        urlEl2.title = targetUrl;
        const total = last ? Number(last.total) || 0 : 0;
        const done = last ? Number(last.done_bytes) || 0 : 0;
        if (total > 0) {
          bodyEl.querySelector('.ndm-fly-rfsize2').style.display = '';
          bodyEl.querySelector('.ndm-fly-pfsize2').textContent =
            fmtIdmSize(total);
          bodyEl.querySelector('.ndm-fly-rdown2').style.display = '';
          bodyEl.querySelector('.ndm-fly-pdown2').textContent =
            `${fmtIdmSize(done)} ( ${((done / total) * 100).toFixed(2)} %)`;
        }
        // The static rows of the panel survive the finish line:
        // Resume capability was true of the transfer all along,
        // and the details section keeps the final per-connection
        // record (full block map, session byte counts) behind
        // the same toggle, in the state the user left it.
        const resumeWord2 = last ? idmResumeWord(last.resume_capability) : '';
        if (resumeWord2) {
          bodyEl.querySelector('.ndm-fly-rresume2').style.display = '';
          bodyEl.querySelector('.ndm-fly-presume2').textContent = resumeWord2;
        }
        paintDetails(wireDetails(bodyEl));
        bodyEl.querySelector('.ndm-fly-show2').addEventListener('click', () => {
          chrome.runtime.sendMessage({ action: 'ndm-show-app' }).catch(() => {});
        });
      }

      function renderFailed(st, r) {
        terminal = true;
        stopPoll();
        syncRegistry(st);
        showFlyTitle(entry.name);
        const msg =
          (r && typeof r.error === 'string' && r.error) ||
          (st === 'cancelled'
            ? 'The download was cancelled in NDM.'
            : 'The download stopped with an error in NDM.');
        bodyEl.innerHTML = `
          <div class="ndm-fly-prog">
            <div class="ndm-fly-purl"></div>
            <div class="ndm-fly-prow"><span class="ndm-fly-plab">Status</span><span class="ndm-fly-pval">${st === 'cancelled' ? 'Cancelled' : 'Error'}</span></div>
            <div class="ndm-fly-pmsg">${escapeHtml(msg)}</div>
            <div class="ndm-fly-btns">
              <button class="ndm-pbtn ndm-fly-show3" type="button">Show in NDM</button>
            </div>
          </div>
        `;
        const urlEl3 = bodyEl.querySelector('.ndm-fly-purl');
        urlEl3.textContent = targetUrl;
        urlEl3.title = targetUrl;
        bodyEl.querySelector('.ndm-fly-show3').addEventListener('click', () => {
          chrome.runtime.sendMessage({ action: 'ndm-show-app' }).catch(() => {});
        });
        if (st === 'error' && entry.id) {
          // An errored entry may leave the error state on its own
          // (the app retries it): the error text shown here must
          // not go stale. Keep a slow watch — while the entry is
          // still errored, refresh the message if it changed; the
          // moment it is downloading / paused / finished again,
          // rebuild the live view so the error is gone from the
          // card entirely. (The card only ever renders an error
          // text that belongs to the entry's CURRENT error state.)
          const statsEl2 = bodyEl.querySelector('.ndm-fly-pmsg');
          flyPollTimer = setInterval(async () => {
            if (closed) {
              stopPoll();
              return;
            }
            const r2 = await pollOnce(entry.id);
            if (closed || !r2) return;
            const st2 = String(r2.status || '');
            if (st2 === 'error') {
              const msg2 =
                (typeof r2.error === 'string' && r2.error) ||
                'The download stopped with an error in NDM.';
              if (statsEl2 && statsEl2.textContent !== msg2) {
                statsEl2.textContent = msg2;
              }
              return;
            }
            stopPoll();
            if (st2 === 'cancelled') {
              renderFailed('cancelled', r2);
              return;
            }
            showProgress(entry);
          }, 2000);
        }
      }

      btnPause.addEventListener('click', async () => {
        if (!entry.id || terminal) return;
        btnPause.disabled = true;
        uiPaused = true;
        render();
        try {
          await chrome.runtime.sendMessage({
            action: 'ndm-pause-entry',
            entry: entry.id,
          });
        } catch (e) {}
        btnPause.disabled = false;
        tick();
      });
      btnResume.addEventListener('click', async () => {
        if (!entry.id || terminal) return;
        btnResume.disabled = true;
        uiPaused = false;
        render();
        try {
          await chrome.runtime.sendMessage({
            action: 'ndm-resume-entry',
            entry: entry.id,
          });
        } catch (e) {}
        btnResume.disabled = false;
        tick();
      });
      btnCancel.addEventListener('click', async () => {
        if (!entry.id || terminal) return;
        btnCancel.disabled = true;
        btnPause.disabled = true;
        try {
          await chrome.runtime.sendMessage({
            action: 'ndm-cancel-entry',
            entry: entry.id,
          });
        } catch (e) {}
        if (closed) return;
        terminal = true;
        stopPoll();
        syncRegistry('cancelled');
        statusEl.textContent = 'Cancelled';
        setStatusLive(false);
        rowsEl.rate.style.display = 'none';
        rowsEl.time.style.display = 'none';
        btnPause.style.display = 'none';
        btnResume.style.display = 'none';
        btnCancel.style.display = 'none';
        btnShow.style.display = '';
        // The details section freezes with the entry: its Info
        // column must stop claiming connections are receiving
        // the moment the entry is cancelled.
        if (last) {
          last = Object.assign({}, last, { status: 'cancelled' });
          paintDetails(progEls);
        }
      });
      btnShow.addEventListener('click', () => {
        chrome.runtime.sendMessage({ action: 'ndm-show-app' }).catch(() => {});
      });

      if (!entry.id) {
        // The send may have landed but returned no id (timeout):
        // nothing exists to poll or control — static state only.
        btnPause.style.display = 'none';
        btnCancel.style.display = 'none';
        btnShow.style.display = '';
        render();
        return;
      }

      const tick = async () => {
        if (closed || terminal) return;
        const r = await pollOnce(entry.id);
        if (closed || terminal) return;
        if (r) {
          last = r;
          const st = String(r.status || '');
          if (st === 'finished') {
            renderFinished();
            return;
          }
          if (st === 'error' || st === 'cancelled') {
            renderFailed(st, r);
            return;
          }
          if (st !== 'paused') uiPaused = false;
          render();
        } else if (!last) {
          render();
        }
      };
      tick();
      flyPollTimer = setInterval(tick, 1000);
    }

    function showMultiProgress(entries, note) {
      stopPoll();
      btnAll.disabled = true;
      showFlyTitle(`${entries.length} downloads`);
      bodyEl.innerHTML =
        (note ? `<div class="ndm-fly-note">${escapeHtml(note)}</div>` : '') +
        '<div class="ndm-fly-multi"></div>';
      const wrap = bodyEl.querySelector('.ndm-fly-multi');
      const rows = entries.map(en => {
        const row = document.createElement('div');
        row.className = 'ndm-fly-mrow';
        const name = document.createElement('span');
        name.className = 'ndm-fly-mname';
        name.textContent = en.name;
        name.title = en.name;
        const pct = document.createElement('span');
        pct.className = 'ndm-fly-mpct';
        pct.textContent = '…';
        // Per-entry controls (R3-EXT-03): the rows used to be
        // display-only — pause/resume/cancel for one of the pair
        // meant leaving for the app. Each row now drives ITS entry
        // through the same message paths the single-progress card
        // uses (ndm-pause-entry / ndm-resume-entry /
        // ndm-cancel-entry).
        const btns = document.createElement('span');
        btns.className = 'ndm-fly-mbtns';
        const btnPause = document.createElement('button');
        btnPause.type = 'button';
        btnPause.className = 'ndm-fly-mbtn';
        btnPause.textContent = 'Pause';
        const btnCancel = document.createElement('button');
        btnCancel.type = 'button';
        btnCancel.className = 'ndm-fly-mbtn';
        btnCancel.textContent = 'Cancel';
        btns.appendChild(btnPause);
        btns.appendChild(btnCancel);
        // The row speaks the progress dialog's language (SPEC
        // §4.6 — IDM itself has one dialog per download, no
        // combined view to copy): "<percent>% <name>" as the
        // title line, the classic green bar under it, and the
        // Downloaded line / state word beneath the bar.
        const mbar = document.createElement('div');
        mbar.className = 'ndm-fly-mbar';
        const mfill = document.createElement('div');
        mfill.className = 'ndm-fly-mbar-fill';
        mbar.appendChild(mfill);
        row.appendChild(name);
        row.appendChild(pct);
        row.appendChild(btns);
        row.appendChild(mbar);
        wrap.appendChild(row);
        const rw = {
          en, pctEl: pct, nameEl: name, fillEl: mfill, btnPause, btnCancel, btns,
          done: false, touched: false,
          lastTotal: 0, lastDone: 0, lastSt: '',
          busy: false, // a control message is in flight
        };
        btnPause.addEventListener('click', async () => {
          if (rw.done || rw.busy) return;
          rw.busy = true;
          const pausing = rw.lastSt !== 'paused';
          // optimistic: the poll confirms within a second
          rw.lastSt = pausing ? 'paused' : 'downloading';
          applyRowState(rw);
          try {
            await chrome.runtime.sendMessage({
              action: pausing ? 'ndm-pause-entry' : 'ndm-resume-entry',
              entry: en.id,
            });
          } catch (e) {}
          rw.busy = false;
        });
        btnCancel.addEventListener('click', async () => {
          if (rw.done || rw.busy) return;
          rw.busy = true;
          btnPause.disabled = true;
          btnCancel.disabled = true;
          try {
            await chrome.runtime.sendMessage({
              action: 'ndm-cancel-entry',
              entry: en.id,
            });
          } catch (e) {}
          rw.busy = false;
          if (closed) return;
          rw.done = true;
          rw.lastSt = 'cancelled';
          if (en.regKey) flyEntryRegistry.delete(en.regKey);
          applyRowState(rw);
        });
        return rw;
      });

      /* One place renders a row from its last known state, shared
         by the poll and the control buttons. */
      function applyRowState(rw) {
        const st = rw.lastSt;
        const pct =
          rw.lastTotal > 0
            ? Math.min(100, Math.floor((rw.lastDone / rw.lastTotal) * 100))
            : null;
        rw.fillEl.style.width = pct != null ? pct + '%' : '0%';
        rw.nameEl.textContent =
          pct != null ? `${pct}% ${rw.en.name}` : rw.en.name;
        if (st === 'finished') {
          rw.nameEl.textContent = `100% ${rw.en.name}`;
          rw.pctEl.textContent = 'Complete';
          rw.btns.style.display = 'none';
        } else if (st === 'cancelled') {
          rw.pctEl.textContent = 'Cancelled';
          rw.btns.style.display = 'none';
        } else if (st === 'error') {
          // NOT terminal for the row: an errored entry can be
          // restarted (by the app or the user), and the row must
          // then show its live state again instead of a stale
          // "Failed" forever. No pause/cancel applies to an
          // errored entry, so the controls hide while it lasts.
          rw.pctEl.textContent = 'Failed';
          rw.btns.style.display = 'none';
        } else if (st === 'paused') {
          rw.pctEl.textContent = 'Paused';
          rw.btns.style.display = '';
          rw.btnPause.textContent = 'Resume';
          rw.btnPause.disabled = rw.busy;
          rw.btnCancel.disabled = rw.busy;
        } else {
          rw.btns.style.display = '';
          rw.btnPause.textContent = 'Pause';
          rw.btnPause.disabled = rw.busy;
          rw.btnCancel.disabled = rw.busy;
          if (rw.lastTotal > 0) {
            rw.pctEl.textContent =
              `${fmtIdmSize(rw.lastDone)} ( ${((rw.lastDone / rw.lastTotal) * 100).toFixed(2)} %)`;
          } else if (rw.lastDone > 0) {
            rw.pctEl.textContent = fmtIdmSize(rw.lastDone);
          } else if (!rw.touched) {
            rw.pctEl.textContent = '…';
          } else {
            rw.pctEl.textContent = '';
          }
        }
      }

      const tick = async () => {
        if (closed) return;
        await Promise.all(
          rows.map(async rw => {
            if (rw.done) return;
            const r = await pollOnce(rw.en.id);
            // a control click may have settled the row (cancel)
            // while this poll was in flight — its state wins
            if (closed || rw.done) return;
            if (!r) {
              // no progress channel: say so once, never invent a %
              if (!rw.touched) rw.pctEl.textContent = 'in NDM';
              return;
            }
            rw.touched = true;
            rw.lastTotal = Number(r.total) || 0;
            rw.lastDone = Number(r.done_bytes) || 0;
            rw.lastSt = String(r.status || '');
            if (rw.lastSt === 'finished' || rw.lastSt === 'cancelled') {
              rw.done = true;
              if (rw.lastSt === 'cancelled' && rw.en.regKey) {
                flyEntryRegistry.delete(rw.en.regKey);
              }
            }
            applyRowState(rw);
          })
        );
        if (!closed && rows.every(rw => rw.done)) {
          titleEl.textContent = `100% ${entries.length} downloads`;
          stopPoll();
        } else if (!closed) {
          const tot = rows.reduce((s, rw) => s + (rw.lastTotal || 0), 0);
          const don = rows.reduce((s, rw) => s + (rw.lastDone || 0), 0);
          if (tot > 0) {
            titleEl.textContent =
              `${Math.min(100, Math.floor((don / tot) * 100))}% ${entries.length} downloads`;
          }
        }
      };
      tick();
      flyPollTimer = setInterval(tick, 1000);
    }

    /* ── List view ──────────────────────────────────────────── */
    function renderList() {
      const list = qualities.length ? qualities : DEFAULT_QUALITIES;
      bodyEl.innerHTML = '';
      // Media header (restored): the video's thumbnail, title and
      // a details line (duration • uploader) above the quality
      // rows — the card header the panel originally had, at the
      // original metrics (104×59 thumb, 8px radius, 13px title
      // clamped to two lines, 11.5px details line). The thumb
      // comes from the probe when it sent one, else the page's
      // own poster / YouTube / og:image art. A thumb that fails
      // to load hides itself — never a broken-image box.
      const media = document.createElement('div');
      media.className = 'ndm-fly-media';
      const thumbSrc =
        probedThumb || guessFallbackThumbnail(targetUrl, currentAnchor);
      if (thumbSrc) {
        const img = document.createElement('img');
        img.className = 'ndm-fly-thumb';
        img.alt = '';
        img.src = thumbSrc;
        img.addEventListener('error', () => {
          img.style.display = 'none';
        });
        media.appendChild(img);
      }
      const meta = document.createElement('div');
      meta.className = 'ndm-fly-meta';
      const mTitle = document.createElement('div');
      mTitle.className = 'ndm-fly-mtitle';
      mTitle.textContent = probedTitle;
      mTitle.title = probedTitle;
      const mSub = document.createElement('div');
      mSub.className = 'ndm-fly-msub';
      mSub.textContent =
        [fmtDuration(probedDuration), probedUploader]
          .filter(Boolean)
          .join(' • ') || 'Nill Download Manager';
      meta.appendChild(mTitle);
      meta.appendChild(mSub);
      media.appendChild(meta);
      bodyEl.appendChild(media);
      list.forEach((q, i) => {
        const row = document.createElement('div');
        row.className = 'ndm-fly-row';
        // Two lines (F-EXT-01): line 1 is the numbered title and
        // may ellipsize; line 2 NEVER truncates — the quality is
        // the very thing the user is choosing.
        const t = document.createElement('div');
        t.className = 'ndm-fly-row-t';
        t.textContent = `${i + 1}. ${probedTitle}`;
        const sub = document.createElement('div');
        sub.className = 'ndm-fly-row-q';
        if (q.audio && q.label) {
          // An audio row shows the probe's full honest label
          // verbatim — "Audio Only (M4A)" (N-09): composing
          // "<EXT> file, quality Audio Only" around it dropped
          // the format half of the label the probe supplied.
          sub.textContent = q.label;
        } else {
          // Video rows: "<EXT> file, quality <label>". The
          // container label is whatever the probe reported
          // (F-EXT-02): when the probe gives no container, an
          // audio row says "Audio", never an invented "MP3".
          const ext = String(
            q.container ||
              (q.audio ? audioRowExt(q) : q.ext) ||
              (q.audio ? 'Audio' : 'MP4')
          ).toUpperCase();
          // The row carries its size again (restored): the
          // producer's own formatted size when it sent one, else
          // the byte count formatted here. Unknown size stays
          // absent — never an invented figure.
          const size =
            String(q.size_text || '').trim() ||
            (Number(q.size_bytes) > 0 ? fmtBytes(q.size_bytes) : '');
          sub.textContent =
            `${ext} file, quality ${qualityText(q)}` +
            (size ? ` • ${size}` : '');
        }
        row.appendChild(t);
        row.appendChild(sub);
        row.title = `${t.textContent}, ${sub.textContent}`;
        // markLiveRow may claim the click (the row's entry is
        // already live): it installs row._ndmLiveClick, checked
        // here so there is exactly one click path per row.
        row.addEventListener('click', () => {
          if (row._ndmLiveClick) row._ndmLiveClick();
          else onRowClick(q);
        });
        bodyEl.appendChild(row);
        markLiveRow(q, row, sub);
      });
      btnAll.disabled = false;
    }

    /* R3-EXT-08: if this page already started this exact format,
       the reopened list must SAY so — a state badge on the row —
       and the row's click must reopen that entry's progress view
       instead of starting a second copy. The registry is written
       by onRowClick / onDownloadAll; one poll settles the badge.
       States that are over (error / cancelled) clear the registry
       and leave the row a plain starter again — the backend
       re-add semantics for those states live outside this card. */
    function markLiveRow(q, row, sub) {
      const key = regKeyFor(q);
      const reg = flyEntryRegistry.get(key);
      if (!reg || !reg.id) return;
      const badge = document.createElement('span');
      badge.className = 'ndm-fly-state';
      badge.textContent = ' · …';
      sub.appendChild(badge);
      pollOnce(reg.id).then(r => {
        if (closed) return;
        if (!r) {
          badge.remove();
          return;
        }
        const st = String(r.status || '');
        if (st === 'downloading' || st === 'queued') {
          const total = Number(r.total) || 0;
          const done = Number(r.done_bytes) || 0;
          badge.textContent =
            total > 0
              ? ` · Downloading ${Math.min(100, Math.floor((done / total) * 100))}%`
              : ' · Downloading…';
          row._ndmLiveClick = () =>
            showProgress({ id: reg.id, name: reg.name, regKey: key });
        } else if (st === 'paused') {
          badge.textContent = ' · Paused';
          row._ndmLiveClick = () =>
            showProgress({ id: reg.id, name: reg.name, regKey: key });
        } else if (st === 'finished') {
          badge.textContent = ' · Downloaded';
          row._ndmLiveClick = () => showDuplicate(q, reg.id);
        } else {
          flyEntryRegistry.delete(key);
          badge.remove();
        }
      });
    }

    function applyInfo(info) {
      if (!info) return;
      rendered = true;
      if (info.title) probedTitle = info.title;
      if (info.thumbnail) probedThumb = info.thumbnail;
      if (Number(info.duration) > 0) probedDuration = Number(info.duration);
      if (info.uploader) probedUploader = info.uploader;
      qualities = info.options || info.qualities || [];
      warmResolve();
      renderList();
      settleProbe();
    }

    /* Warm resolve (start-latency fix, FINDINGS-START-LATENCY
       §5): both info paths — page-hook instant info and the
       fetched probe — converge in applyInfo, so this is the one
       point where the flyout's real options become known. Fire
       a fire-and-forget 'ndm-warm' ONCE per target URL so the
       app pre-resolves these selectors' streams in the
       background; the click that follows then starts from the
       cache (~0.78 s to first byte instead of ~3.8 s cold).
       The reply and any error are deliberately ignored, and
       nothing here can delay or block the list: no options with
       selectors, no send. */
    function warmResolve() {
      if (warmSentFor === targetUrl) return;
      const items = qualities
        .filter(o => o && typeof o.selector === 'string' && o.selector)
        .slice(0, 4)
        .map(o => ({
          selector: o.selector, // the same string the row's click passes
          audio: !!o.audio,     // the same flag its click sends
        }));
      if (!items.length) return;
      warmSentFor = targetUrl;
      try {
        chrome.runtime.sendMessage(
          {
            action: 'ndm-warm',
            url: targetUrl,
            pageUrl: location.href,
            items,
          },
          () => void chrome.runtime.lastError
        );
      } catch (e) {}
    }

    /* The probe has produced a list (real formats or the failure
       fallback): a "Download all" click that arrived while
       probing was registered, not swallowed (F-EXT-14) — the
       button returns to its plain label and the intent fires. */
    function settleProbe() {
      probeSettled = true;
      btnAll.textContent = 'Download all';
      if (wantAll && !closed) {
        wantAll = false;
        onDownloadAll();
      }
    }

    /* No detected media: a plain statement, no rows, no probe
       error, and no "Open in NDM App" hand-off — the page itself
       is not a video and must never be offered as one
       (R2-EXT-08). */
    function showNoMedia() {
      rendered = true;
      probeSettled = true;
      btnAll.style.display = 'none';
      bodyEl.innerHTML = `
        <div class="ndm-fly-note">No video found on this page.</div>
      `;
    }

    function probeFailed(errText) {
      rendered = true;
      // Belt and braces (R2-EXT-08): if the probe target IS this
      // page's own document and the page has no detected media,
      // the document must not be offered as a video row and the
      // raw probe error must not be shown — whatever path led
      // here, the answer is the plain no-media state.
      if (
        !currentAnchor &&
        !videoDetected() &&
        canonicalVideoKey(targetUrl) === canonicalVideoKey(location.href)
      ) {
        showNoMedia();
        return;
      }
      qualities = [];
      // The automatic choices stay available (as before), the
      // error is a note above them, and the app fallback link —
      // shown ONLY on probe failure — appears in the footer.
      // The note is PLAIN language only (R3-EXT-05): the raw
      // probe error is tool stderr and must never reach the user
      // verbatim.
      renderList();
      settleProbe();
      const plain = plainErrorText(errText);
      showNote(
        plain
          ? `${plain} — automatic quality is still available below.`
          : 'Could not read exact formats — automatic quality is still available below.'
      );
      footEl.style.display = 'flex';
    }

    // 1. Instant cache (0ms on YouTube or pre-probed pages)
    const cached =
      localProbeCache.get(targetUrl) ||
      localProbeCache.get(canonicalVideoKey(location.href));
    if (cached && (cached.options || cached.qualities || []).length) {
      applyInfo(cached);
      return;
    }

    /* R2-EXT-08 gate: with NO detected media there is nothing to
       offer, and the page must not be probed as if it were a
       video — that probe failed with a raw yt-dlp error and the
       failure fallback then offered the HTML document itself as
       an "MP4 Best available" row. Detected media here means: a
       player element to anchor to, a known video page, or a
       stream the background sniffed on this page. (Cached probe
       info was already ruled out above.) */
    let mediaKnown = !!currentAnchor || videoDetected();
    if (!mediaKnown) {
      try {
        const mediaResp = await chrome.runtime.sendMessage({
          action: 'ndm-tab-media',
          pageUrl: location.href,
          key: canonicalVideoKey(location.href),
        });
        if (mediaResp && mediaResp.streamUrl) mediaKnown = true;
      } catch (e) {}
      if (myToken !== activeProbeToken) return;
      if (!mediaKnown) {
        showNoMedia();
        return;
      }
    }

    // 2. MAIN-world page-hook info, if it lands within milliseconds
    onInstantInfoCallback = info => {
      if (myToken !== activeProbeToken || rendered) return;
      applyInfo(info);
    };

    // 3. Otherwise await the in-flight or a fresh native probe
    try {
      const res = await fetchOrReuseProbe(targetUrl);
      if (myToken !== activeProbeToken || rendered) return;
      if (res && res.ok && res.info) {
        applyInfo(res.info);
      } else {
        probeFailed((res && res.error) || '');
      }
    } catch (e) {
      if (myToken !== activeProbeToken || rendered) return;
      probeFailed('');
    }
  }
  function defaultFileNameFromUrl(url) {
    try {
      const u = new URL(url);
      const tail = decodeURIComponent(u.pathname.split('/').pop() || '').trim();
      if (tail && tail !== '/') return tail;
    } catch (e) {}
    return 'download.bin';
  }

  function categoryForName(name) {
    const ext = (name.split('.').pop() || '').toLowerCase();
    if (['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'tgz', 'tbz2', 'txz'].includes(ext)) return 'Compressed';
    if (['exe', 'msi', 'msix', 'msixbundle', 'deb', 'rpm', 'dmg', 'pkg', 'appimage', 'apk', 'bin', 'run', 'cab', 'jar', 'war', 'vmdk', 'qcow2', 'vdi', 'iso', 'img', 'flatpakref'].includes(ext)) return 'Programs';
    if (['mp4', 'mkv', 'webm', 'avi', 'mov', 'flv', 'wmv', 'm4v', 'ts'].includes(ext)) return 'Video';
    if (['mp3', 'm4a', 'm4b', 'flac', 'wav', 'ogg', 'opus', 'aac', 'wma'].includes(ext)) return 'Music';
    if (['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'txt', 'md', 'csv', 'rtf', 'epub', 'mobi'].includes(ext)) return 'Documents';
    return 'General';
  }

  /* ---------------------------------------------------------------- */
  /* In-Browser IDM Download File Info Modal (.iso, .zip, .exe, etc.) */
  /* ---------------------------------------------------------------- */

  async function openBrowserFileModal(opts) {
    if (!panelHost || !panelHost.isConnected || !shadowRoot) {
      buildPanel();
    }
    dismissActiveModal();
    modalOpen = true;
    const myToken = ++activeProbeToken;
    const targetUrl = (opts && opts.url) || '';
    const pageReferrer = (opts && opts.referrer) || location.href;
    let initialName = (opts && opts.filename) ? opts.filename.trim() : '';
    if (!initialName) initialName = defaultFileNameFromUrl(targetUrl);
    let currentBytes = Number(opts && opts.totalBytes) || 0;
    let currentCat = categoryForName(initialName);

    let savedDir = '~/Downloads';
    try {
      const st = await chrome.storage.local.get({ ndmSaveDir: '~/Downloads' });
      if (st && st.ndmSaveDir) savedDir = st.ndmSaveDir;
    } catch (e) {}

    const backdrop = document.createElement('div');
    backdrop.className = 'ndm-modal-backdrop';
    for (const evt of ['mousedown', 'pointerdown', 'mouseup', 'click', 'dblclick', 'keydown', 'keyup']) {
      backdrop.addEventListener(evt, e => e.stopPropagation());
    }

    const dialog = document.createElement('div');
    dialog.className = 'ndm-modal';
    dialog.innerHTML = `
      <div class="ndm-m-head">
        <div class="ndm-m-brand">
          <span class="ndm-m-logo">⬇</span>
          <span>Download File Info — NDM</span>
        </div>
        <button class="ndm-m-x" type="button" title="Close">×</button>
      </div>
      <div class="ndm-m-body">
        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">URL</label>
          <input class="ndm-m-input ndm-f-url" type="text" readonly />
        </div>
        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">Category</label>
          <div><span class="ndm-q-badge video ndm-f-cat">${escapeHtml(currentCat)}</span></div>
        </div>
        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">Save to folder</label>
          <input class="ndm-m-input ndm-f-dir" type="text" placeholder="~/Downloads" />
        </div>
        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">File name</label>
          <input class="ndm-m-input ndm-f-name" type="text" spellcheck="false" />
        </div>
        <div class="ndm-f-summary">
          <span class="ndm-f-size">${currentBytes > 0 ? escapeHtml(fmtBytes(currentBytes)) : 'Checking size…'}</span>
          <span class="ndm-f-resume">Checking…</span>
        </div>
        <div class="ndm-m-status" style="display:none"></div>
      </div>
      <div class="ndm-m-foot">
        <button class="ndm-btn ndm-btn-ghost ndm-btn-app" type="button" title="Open this download in the NDM desktop app window">Open in NDM App</button>
        <div class="ndm-m-foot-right">
          <button class="ndm-btn ndm-btn-secondary ndm-btn-cancel" type="button">Cancel</button>
          <button class="ndm-btn ndm-btn-secondary ndm-btn-later" type="button">Download Later</button>
          <button class="ndm-btn ndm-btn-primary ndm-btn-start" type="button">⬇ Download Now</button>
        </div>
      </div>
    `;

    backdrop.appendChild(dialog);
    shadowRoot.appendChild(backdrop);
    modalEl = backdrop;
    syncHostParent();
    panelHost.style.display = 'block';
    // EX-m5: focus the backdrop so Esc/Enter work without a first
    // click inside the dialog (its keydown listener lives here).
    backdrop.tabIndex = -1;
    backdrop.focus();

    const urlIn = dialog.querySelector('.ndm-f-url');
    const catEl = dialog.querySelector('.ndm-f-cat');
    const sizeEl = dialog.querySelector('.ndm-f-size');
    const resumeEl = dialog.querySelector('.ndm-f-resume');
    const nameIn = dialog.querySelector('.ndm-f-name');
    const dirIn = dialog.querySelector('.ndm-f-dir');
    const statusEl = dialog.querySelector('.ndm-m-status');
    const btnClose = dialog.querySelector('.ndm-m-x');
    const btnCancel = dialog.querySelector('.ndm-btn-cancel');
    const btnApp = dialog.querySelector('.ndm-btn-app');
    const btnLater = dialog.querySelector('.ndm-btn-later');
    const btnStart = dialog.querySelector('.ndm-btn-start');

    urlIn.value = targetUrl;
    nameIn.value = initialName;
    dirIn.value = savedDir;

    let userEditedName = false;
    nameIn.addEventListener('input', () => {
      userEditedName = true;
      catEl.textContent = categoryForName(nameIn.value.trim());
    });

    backdrop.addEventListener('keydown', e => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        e.preventDefault();
        dismissDialog();
      } else if (e.key === 'Enter' && !btnStart.disabled) {
        e.preventDefault();
        btnStart.click();
      }
    });

    // Dismiss gestures (×, Esc, backdrop click) are IDM dismissals:
    // they close the dialog and KEEP a started download running
    // (dismissDialog). Only the labelled Cancel button destroys —
    // it alone calls dismissAsCancel (R3-EXT-02).
    btnClose.addEventListener('click', () => dismissDialog());
    btnCancel.addEventListener('click', () => dismissAsCancel());
    activeModalDismiss = dismissDialog;
    backdrop.addEventListener('click', e => {
      if (e.target === backdrop) dismissDialog();
    });

    btnApp.addEventListener('click', async () => {
      if (spec.inflight) return;
      if (spec.id) {
        // the download is already running in the app (it started
        // while this dialog was open): just show the app
        dismissAsKeep();
        try {
          await chrome.runtime.sendMessage({ action: 'ndm-show-app' });
        } catch (e) {}
        return;
      }
      btnApp.disabled = true;
      btnApp.textContent = 'Opening NDM…';
      try {
        const resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'file',
          url: targetUrl,
          pageUrl: pageReferrer,
          title: nameIn.value.trim() || initialName,
        });
        if (resp && resp.ok) {
          closeBrowserModal();
        } else {
          btnApp.disabled = false;
          btnApp.textContent = 'Open in NDM App';
        }
      } catch (e) {
        btnApp.disabled = false;
        btnApp.textContent = 'Open in NDM App';
      }
    });

    async function triggerFileDownload(later) {
      spec.started = true; // the classic path replaces the speculative one
      const chosenDir = dirIn.value.trim() || '~/Downloads';
      const chosenName = nameIn.value.trim() || initialName;
      const szLabel = currentBytes > 0 ? fmtBytes(currentBytes) : '';
      try {
        chrome.storage.local.set({ ndmSaveDir: chosenDir }).catch(() => {});
      } catch (e) {}

      btnStart.disabled = true;
      btnLater.disabled = true;
      btnStart.textContent = later ? 'Queueing in NDM…' : 'Starting in NDM…';

      let ok = false;
      let resp = null;
      try {
        resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'file_direct',
          url: targetUrl,
          pageUrl: pageReferrer,
          title: chosenName,
          dir: chosenDir,
          later: !!later,
          total: currentBytes,
        });
        ok = !!(resp && resp.ok);
      } catch (e) {
        ok = false;
      }

      if (ok) {
        closeBrowserModal();
        if (resp && resp.dupe) {
          // IDM asks about duplicates; the silent path just points
          // at the entry that is already downloading
          showBrowserToast(chosenName, 'This file is already in the download list', '', false, '✓ Already in NDM');
        } else {
          showBrowserToast(chosenName, catEl.textContent || 'File', szLabel, later);
        }
      } else {
        statusEl.style.display = 'block';
        statusEl.className = 'ndm-m-status err';
        statusEl.textContent = (resp && resp.error) || 'NDM could not be started. Make sure the NDM desktop app is installed.';
        btnStart.disabled = false;
        btnLater.disabled = false;
        btnStart.textContent = '⬇ Download Now';
      }
    }

    /* ── IDM's File Info behavior: the download starts while this
       dialog is open. Start = keep it downloading, Download Later =
       pause it inside the app, Cancel = discard it completely (the
       click is consumed; the browser never takes the file). If the
       speculative start fails, the buttons fall back to the classic
       send-on-click behavior. ── */
    const spec = {
      started: false, // a start (speculative or classic) was kicked off
      inflight: false, // the speculative send has not answered yet
      id: null, // the app entry this dialog is bound to
      owned: false, // this dialog created the entry
      dupe: false, // the entry already existed (not ours to discard)
      failed: false, // speculative start failed → classic buttons
      pending: null, // 'keep' | 'later' | 'cancel' clicked mid-flight
      timer: null, // the 4 s probe-hang fallback start
      closed: false,
      pollTimer: null, // 1 s entry-progress poll (F-EXT-12)
      entryStatus: '', // last status the app reported for the entry
      finished: false, // entry reached 'finished' — closing keeps it
      lastPct: -1,
    };

    function specNote(text) {
      statusEl.style.display = 'block';
      statusEl.className = 'ndm-m-status';
      statusEl.textContent = text;
    }

    async function specControl(action) {
      if (!spec.id) return;
      try {
        await chrome.runtime.sendMessage({ action, entry: spec.id });
      } catch (e) {}
    }

    function stopSpecPoll() {
      if (spec.pollTimer) {
        clearInterval(spec.pollTimer);
        spec.pollTimer = null;
      }
    }

    /* The video flyout polls its entry — the file dialog must
       reflect the same truth (F-EXT-12). A 29-byte file is
       finished long before a human can read this dialog, and a
       dialog still saying "Downloading…" invites an Esc that
       used to discard the FINISHED entry from the list. */
    function startSpecPoll() {
      if (!spec.id || spec.pollTimer) return;
      // Set while the note shows an error text: if the entry
      // leaves the error state (the app retries it), the next
      // live status must overwrite the note — a stale error left
      // on screen after the entry recovered would be a lie.
      let errorShown = false;
      const tickSpec = async () => {
        if (spec.closed || myToken !== activeProbeToken || !backdrop.isConnected) {
          stopSpecPoll();
          return;
        }
        let r = null;
        try {
          r = await chrome.runtime.sendMessage({
            action: 'ndm-entry-progress',
            id: spec.id,
          });
        } catch (e) {}
        if (!r || !r.ok || spec.closed) return;
        const st = String(r.status || '');
        spec.entryStatus = st;
        if (st === 'finished') {
          spec.finished = true;
          stopSpecPoll();
          specNote('✓ Download complete');
          btnStart.disabled = false;
          btnStart.textContent = 'Close';
          // Exactly ONE Close button once finished (R3-EXT-06):
          // Cancel and Download Later have no meaning for a
          // completed entry — hide them instead of relabelling
          // Cancel into a second "Close".
          btnLater.style.display = 'none';
          btnCancel.style.display = 'none';
        } else if (st === 'downloading' || st === 'queued') {
          if (errorShown) {
            // Back from an error: force the note to refresh even
            // if the percentage has not moved.
            errorShown = false;
            spec.lastPct = -1;
          }
          const total = Number(r.total) || 0;
          const done = Number(r.done_bytes) || 0;
          if (total > 0) {
            const pct = Math.min(100, Math.floor((done / total) * 100));
            if (pct !== spec.lastPct) {
              spec.lastPct = pct;
              specNote(`⬇ Downloading in NDM… ${pct}%`);
            }
          } else if (spec.lastPct !== -2) {
            spec.lastPct = -2;
            specNote('⬇ Downloading in NDM…');
          }
        } else if (st === 'paused') {
          errorShown = false;
          specNote('⏸ Paused in NDM');
        } else if (st === 'error') {
          // Do NOT stop polling: if the entry is restarted and
          // leaves the error state, the branches above replace
          // this text with the live status.
          errorShown = true;
          specNote(
            typeof r.error === 'string' && r.error
              ? r.error
              : 'The download stopped with an error in NDM.'
          );
        } else if (st === 'cancelled') {
          stopSpecPoll();
          specNote('The download was cancelled in NDM.');
        }
      };
      tickSpec();
      spec.pollTimer = setInterval(tickSpec, 1000);
    }

    function dismissAsKeep() {
      spec.closed = true;
      stopSpecPoll();
      closeBrowserModal();
      showBrowserToast(
        nameIn.value.trim() || initialName,
        catEl.textContent || 'File',
        currentBytes > 0 ? fmtBytes(currentBytes) : '',
        false
      );
    }

    async function dismissAsLater() {
      spec.closed = true;
      stopSpecPoll();
      closeBrowserModal();
      if (spec.owned) await specControl('ndm-pause-entry');
      showBrowserToast(
        nameIn.value.trim() || initialName,
        catEl.textContent || 'File',
        currentBytes > 0 ? fmtBytes(currentBytes) : '',
        true
      );
    }

    function dismissAsCancel() {
      if (spec.closed) return;
      // A FINISHED entry is never discarded by closing this
      // dialog (F-EXT-12): Esc/× after completion is a plain
      // close that KEEPS the entry — the discard path applies
      // only while the download is unfinished (and the backend
      // now refuses to discard finished entries too).
      if (spec.finished) {
        spec.closed = true;
        if (spec.timer) clearTimeout(spec.timer);
        stopSpecPoll();
        closeBrowserModal();
        return;
      }
      spec.closed = true;
      if (spec.timer) clearTimeout(spec.timer);
      stopSpecPoll();
      if (spec.owned && spec.id) {
        specControl('ndm-cancel-entry');
      } else if (spec.inflight) {
        spec.pending = 'cancel';
      }
      closeBrowserModal();
    }

    /* The dismiss gestures (Esc / × / backdrop click) and the
       modal-teardown paths (activeModalDismiss), per R3-EXT-02:
       IDM's progress dialog never kills the download when it is
       merely closed. If this dialog's download has started, a
       dismiss is exactly the "✓ Keep downloading" button — the
       dialog closes, the entry keeps downloading in the app, and
       the toast says so. If the start is still in flight, the
       pending decision becomes 'keep' so the landing start is let
       to download. Before anything has started (or when the entry
       is not ours), a dismiss is a plain close — and the pending
       start timer is cleared so nothing starts behind the user's
       back. Destruction lives ONLY on the Cancel button above. */
    function dismissDialog() {
      if (spec.closed) return;
      if (spec.owned && spec.id) {
        dismissAsKeep();
        return;
      }
      if (spec.inflight) {
        spec.pending = 'keep';
      }
      spec.closed = true;
      if (spec.timer) clearTimeout(spec.timer);
      stopSpecPoll();
      closeBrowserModal();
    }

    async function specStart() {
      if (spec.started || spec.closed || myToken !== activeProbeToken) return;
      spec.started = true;
      spec.inflight = true;
      // lock the fields the entry is about to be created from: on a
      // cold app start the ack can take seconds, and edits typed in
      // that window used to be silently discarded (the entry kept
      // the pre-edit name/folder while the dialog showed the edits)
      nameIn.disabled = true;
      dirIn.disabled = true;
      btnApp.disabled = true;
      const chosenDir = dirIn.value.trim() || '~/Downloads';
      const chosenName = nameIn.value.trim() || initialName;
      let resp = null;
      try {
        resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'file_direct',
          url: targetUrl,
          pageUrl: pageReferrer,
          title: chosenName,
          dir: chosenDir,
          later: false,
          total: currentBytes,
        });
      } catch (e) {}
      spec.inflight = false;
      btnApp.disabled = false;
      const gone = spec.closed || myToken !== activeProbeToken;
      if (resp && resp.ok && resp.id) {
        spec.id = resp.id;
        spec.dupe = !!resp.dupe;
        spec.owned = !resp.dupe;
        if (gone) {
          // the user decided while the start was in flight
          if (spec.pending === 'cancel' && spec.owned) {
            specControl('ndm-cancel-entry');
          } else if (spec.pending === 'later' && spec.owned) {
            specControl('ndm-pause-entry');
          }
          return;
        }
        if (spec.pending === 'cancel') {
          dismissAsCancel();
          return;
        }
        if (spec.pending === 'later') {
          dismissAsLater();
          return;
        }
        if (spec.pending === 'keep') {
          dismissAsKeep();
          return;
        }
        startSpecPoll();
        if (spec.dupe) {
          specNote('Already in your NDM download list — downloading there.');
          btnStart.textContent = 'OK';
          btnLater.disabled = true;
        } else {
          specNote('⬇ Downloading in NDM…');
          btnStart.textContent = '✓ Keep downloading';
          // honest label: the file is already downloading, so
          // "Later" now pauses it in the app (IDM's Later never
          // starts the file, but this dialog's download has started)
          btnLater.textContent = '⏸ Pause in NDM';
        }
      } else if (!gone) {
        spec.failed = true;
        // the speculative start failed → classic send-on-click:
        // the fields must be editable again
        nameIn.disabled = false;
        dirIn.disabled = false;
      }
    }

    btnStart.addEventListener('click', () => {
      if (spec.finished) {
        // the button reads "Close" now: a plain close that
        // keeps the finished entry (F-EXT-12)
        spec.closed = true;
        stopSpecPoll();
        closeBrowserModal();
      } else if (spec.dupe) {
        spec.closed = true;
        closeBrowserModal();
      } else if (spec.owned) {
        dismissAsKeep();
      } else if (spec.inflight) {
        spec.pending = 'keep';
        btnStart.disabled = true;
        btnLater.disabled = true;
        btnStart.textContent = 'Starting in NDM…';
      } else {
        triggerFileDownload(false);
      }
    });
    btnLater.addEventListener('click', () => {
      if (spec.dupe) {
        spec.closed = true;
        closeBrowserModal();
      } else if (spec.owned) {
        dismissAsLater();
      } else if (spec.inflight) {
        spec.pending = 'later';
        btnStart.disabled = true;
        btnLater.disabled = true;
        btnLater.textContent = 'Queueing in NDM…';
      } else {
        triggerFileDownload(true);
      }
    });

    // if the probe hangs, start anyway with what the page gave us —
    // but never after this dialog is gone: the timer used to outlive
    // raw closes and start a "ghost" download seconds later
    spec.timer = setTimeout(() => {
      if (spec.closed || myToken !== activeProbeToken || !backdrop.isConnected) return;
      specStart();
    }, 4000);

    // Probe exact file size, resumability & Content-Disposition filename in background
    try {
      const res = await chrome.runtime.sendMessage({
        action: 'ndm-probe-file',
        url: targetUrl,
        pageUrl: pageReferrer,
      });
      if (myToken !== activeProbeToken) return;
      if (res && res.ok) {
        if (res.total && res.total > 0) {
          currentBytes = res.total;
          sizeEl.textContent = fmtBytes(res.total);
        } else {
          sizeEl.textContent = 'Stream / Unknown size';
        }
        if (res.suggested_name && !userEditedName && !spec.started) {
          // !spec.started, not !spec.id: once a speculative send is
          // in flight the entry is being created with the name the
          // dialog already had — a late probe must not rename the
          // field under it (screen and disk would disagree)
          nameIn.value = res.suggested_name;
          catEl.textContent = res.category || categoryForName(res.suggested_name);
        }
        resumeEl.textContent = res.resumable !== false
          ? 'Resumable'
          : 'Single-stream download';
      } else {
        resumeEl.textContent = '—';
        if (currentBytes <= 0) sizeEl.textContent = 'Ready to download';
      }
    } catch (e) {
      if (myToken === activeProbeToken) {
        resumeEl.textContent = '—';
        if (currentBytes <= 0) sizeEl.textContent = 'Ready to download';
      }
    }
    // the probe has settled (or failed): start the download now,
    // while the dialog stays open — IDM's File Info behavior
    specStart();
  }

  function escapeHtml(s) {
    return String(s || '')
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function buildPanel() {
    if (panelHost) panelHost.remove();
    panelHost = document.createElement('div');
    panelHost.id = 'ndm-panel-host';
    panelHost.style.cssText =
      'position:fixed;inset:0;pointer-events:none;z-index:2147483647;';
    shadowRoot = panelHost.attachShadow({ mode: 'closed' });

    const style = document.createElement('style');
    style.textContent = `
      * { box-sizing: border-box; }
      .pill {
        position: absolute; pointer-events: auto;
        display: flex; align-items: center; justify-content: center;
        min-width: 184px; gap: 7px;
        background: linear-gradient(180deg, rgba(16, 28, 48, .88), rgba(10, 18, 32, .92));
        color: #fff;
        -webkit-backdrop-filter: blur(10px) saturate(1.4);
        backdrop-filter: blur(10px) saturate(1.4);
        border: 1px solid rgba(110, 190, 255, .38);
        border-radius: 999px; padding: 4px 12px 4px 4px;
        box-shadow: 0 8px 24px rgba(2, 10, 24, .45);
        font-family: "Segoe UI", system-ui, -apple-system, Roboto, Arial, sans-serif;
        font-size: 13px; font-weight: 600; white-space: nowrap;
        opacity: 0; transition: opacity .18s ease, transform .15s ease, background .15s ease;
      }
      .pill.on { opacity: 1; }
      .pill:hover {
        background: linear-gradient(180deg, rgba(20, 38, 66, .95), rgba(12, 24, 44, .98));
        border-color: rgba(130, 205, 255, .65);
        transform: translateY(-1px);
      }
      .chip {
        width: 24px; height: 24px; border-radius: 50%; flex: none;
        display: flex; align-items: center; justify-content: center;
        background: linear-gradient(180deg, #55c1ff, #1f6fd0);
        color: #fff; font-size: 13px; line-height: 1;
        box-shadow: inset 0 1px 0 rgba(255,255,255,.5);
      }
      .pill.sent { background: rgba(18, 106, 52, .92); border-color: rgba(100, 230, 140, .55); }
      .pill.error { background: rgba(156, 30, 30, .92); border-color: rgba(255, 130, 130, .55); }
      .label {
        cursor: pointer; background: none; border: none; color: inherit;
        font: inherit; padding: 0; display: flex; align-items: center; gap: 5px;
      }
      .caret {
        font-size: 10px; opacity: .8; margin-left: 2px;
      }
      .close {
        background: none; border: none; color: rgba(255,255,255,.72);
        cursor: pointer; font-size: 15px; line-height: 1; padding: 0 0 0 4px;
      }
      .close:hover { color: #fff; }

      /* In-Browser IDM Video & File Modal */
      .ndm-modal-backdrop {
        position: fixed; inset: 0; pointer-events: auto;
        background: rgba(8, 15, 28, .62);
        -webkit-backdrop-filter: blur(4px);
        backdrop-filter: blur(4px);
        display: flex; align-items: center; justify-content: center;
        padding: 16px;
        font-family: "Segoe UI", system-ui, -apple-system, Roboto, Arial, sans-serif;
        color: #162334;
        animation: ndmFadeIn .15s ease-out;
      }
      @keyframes ndmFadeIn {
        from { opacity: 0; transform: scale(.98); }
        to { opacity: 1; transform: scale(1); }
      }
      .ndm-modal {
        width: 540px; max-width: 100%;
        background: #f5f8fc;
        border: 1px solid #8ea8c6;
        border-radius: 10px;
        box-shadow: 0 22px 60px rgba(0, 0, 0, .48), 0 0 0 1px rgba(255,255,255,.6) inset;
        overflow: hidden;
        display: flex; flex-direction: column;
      }
      .ndm-m-head {
        display: flex; align-items: center; justify-content: space-between;
        padding: 10px 14px;
        background: linear-gradient(180deg, #e7f0fa 0%, #cfe0f3 100%);
        border-bottom: 1px solid #9eb5d0;
        font-size: 14px; font-weight: 700; color: #122842;
      }
      .ndm-m-brand {
        display: flex; align-items: center; gap: 8px;
      }
      .ndm-m-logo {
        width: 22px; height: 22px; border-radius: 6px;
        background: linear-gradient(180deg, #4db8ff, #1b66c9);
        color: #fff; display: inline-flex; align-items: center; justify-content: center;
        font-size: 12px; box-shadow: inset 0 1px 0 rgba(255,255,255,.45);
      }
      .ndm-m-x {
        width: 26px; height: 26px; border-radius: 5px;
        border: 1px solid transparent; background: transparent;
        color: #344b66; font-size: 18px; line-height: 1; cursor: pointer;
      }
      .ndm-m-x:hover {
        background: #e81123; color: #fff;
      }
      .ndm-m-body {
        padding: 14px;
        display: flex; flex-direction: column; gap: 12px;
      }
      .ndm-m-preview {
        display: flex; gap: 12px; align-items: center;
        background: #fff; border: 1px solid #c5d4e6;
        border-radius: 8px; padding: 10px;
      }
      .ndm-m-thumb-wrap {
        position: relative; width: 136px; height: 78px; flex: none;
        border-radius: 6px; overflow: hidden;
        background: linear-gradient(135deg, #1b2b42, #0f1826);
        border: 1px solid #b0c2d8;
        display: flex; align-items: center; justify-content: center;
      }
      .ndm-m-thumb {
        width: 100%; height: 100%; object-fit: cover; display: block;
      }
      .ndm-m-thumb-ph {
        font-size: 26px; opacity: .7;
      }
      .ndm-m-dur {
        position: absolute; right: 5px; bottom: 5px;
        background: rgba(0, 0, 0, .82); color: #fff;
        font-size: 11px; font-weight: 700; padding: 1px 5px;
        border-radius: 4px; font-variant-numeric: tabular-nums;
      }
      .ndm-m-meta {
        flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 5px;
      }
      .ndm-m-lbl {
        font-size: 11px; font-weight: 700; color: #465d78;
        text-transform: uppercase; letter-spacing: .3px;
      }
      .ndm-m-input {
        width: 100%; height: 32px; padding: 0 9px;
        border: 1px solid #9eb4ce; border-radius: 5px;
        background: #fff; color: #122338;
        font: 600 13px "Segoe UI", system-ui, sans-serif;
        outline: none;
      }
      .ndm-m-input:focus {
        border-color: #1f6fd0;
        box-shadow: 0 0 0 2px rgba(31, 111, 208, .2);
      }
      .ndm-m-sub {
        font-size: 11.5px; color: #566e8a;
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ndm-f-summary {
        display: flex; align-items: center; gap: 10px;
        background: #fff; border: 1px solid #c5d4e6;
        border-radius: 7px; padding: 8px 11px;
        font-size: 12.5px;
      }
      .ndm-f-size {
        font-weight: 700; color: #0d47a1;
        font-variant-numeric: tabular-nums;
      }
      .ndm-f-resume {
        margin-left: auto; font-size: 11.5px; font-weight: 600; color: #1b6e39;
      }
      .ndm-m-qhead {
        display: flex; align-items: center; justify-content: space-between;
        font-size: 11.5px; font-weight: 700; color: #3d5570;
        text-transform: uppercase; letter-spacing: .3px;
        padding: 0 2px;
      }
      .ndm-m-qlist {
        background: #fff; border: 1px solid #b8cae0;
        border-radius: 7px; max-height: 225px; overflow-y: auto;
        divide-y: 1px solid #edf2f8;
      }
      .ndm-m-loading {
        display: flex; align-items: center; gap: 12px;
        padding: 24px 16px; color: #2b4460;
      }
      .ndm-spinner {
        width: 22px; height: 22px; border-radius: 50%; flex: none;
        border: 2.5px solid #cfe0f5; border-top-color: #1b66c9;
        animation: ndmSpin .75s linear infinite;
      }
      @keyframes ndmSpin {
        to { transform: rotate(360deg); }
      }
      .ndm-m-load-title {
        font-size: 13px; font-weight: 600; color: #162b44;
      }
      .ndm-m-load-sub {
        font-size: 11.5px; color: #5c7490; margin-top: 2px;
      }
      .ndm-q-row {
        display: flex; align-items: center; justify-content: space-between;
        padding: 8px 11px; cursor: pointer;
        border-bottom: 1px solid #eef3f9;
        transition: background .1s ease;
      }
      .ndm-q-row:last-child { border-bottom: none; }
      .ndm-q-row:hover { background: #f0f6ff; }
      .ndm-q-row.sel {
        background: linear-gradient(180deg, #dcebff 0%, #c8e0ff 100%);
        color: #0b2545;
      }
      .ndm-q-left {
        display: flex; align-items: center; gap: 8px; min-width: 0;
      }
      .ndm-q-radio {
        width: 15px; height: 15px; border-radius: 50%; flex: none;
        border: 2px solid #7f97b2; background: #fff;
        position: relative;
      }
      .ndm-q-row.sel .ndm-q-radio {
        border-color: #155fc0;
      }
      .ndm-q-row.sel .ndm-q-radio::after {
        content: ""; position: absolute; inset: 2px;
        border-radius: 50%; background: #155fc0;
      }
      .ndm-q-badge {
        font-size: 10.5px; font-weight: 800; padding: 2px 6px;
        border-radius: 4px; letter-spacing: .3px; flex: none;
      }
      .ndm-q-badge.video {
        background: #e1effe; color: #1456b0; border: 1px solid #b5d3f8;
      }
      .ndm-q-badge.audio {
        background: #fef3c7; color: #92400e; border: 1px solid #fde68a;
      }
      .ndm-q-label {
        font-size: 13px; font-weight: 600; color: #13263c;
      }
      .ndm-q-note {
        font-size: 11px; color: #5b728c;
      }
      .ndm-q-right {
        flex: none; margin-left: 8px;
      }
      .ndm-q-size {
        display: inline-block; min-width: 68px; text-align: right;
        font-size: 12.5px; font-weight: 700; color: #47607c;
        font-variant-numeric: tabular-nums;
        background: #eef3f9; padding: 2px 8px; border-radius: 999px;
      }
      .ndm-q-size.exact {
        background: #e3f2fd; color: #0d47a1; border: 1px solid #bbdefb;
      }
      .ndm-q-row.sel .ndm-q-size {
        background: #155fc0; color: #fff; border-color: #155fc0;
      }
      .ndm-m-folder-row {
        display: flex; flex-direction: column; gap: 4px;
      }
      .ndm-m-status {
        font-size: 12px; padding: 7px 10px; border-radius: 6px;
      }
      .ndm-m-status.err {
        background: #fff1f2; border: 1px solid #fecdd3; color: #9f1239;
      }
      .ndm-m-foot {
        display: flex; align-items: center; justify-content: space-between;
        padding: 10px 14px;
        background: #e9eff7; border-top: 1px solid #b8cae0;
        gap: 8px;
      }
      .ndm-m-foot-right {
        display: flex; align-items: center; gap: 7px;
      }
      .ndm-btn {
        height: 32px; padding: 0 12px; border-radius: 5px;
        font: 600 12.5px "Segoe UI", system-ui, sans-serif;
        cursor: pointer; transition: filter .12s ease, opacity .12s ease;
        white-space: nowrap;
      }
      .ndm-btn:disabled {
        opacity: .55; cursor: not-allowed;
      }
      .ndm-btn-primary {
        background: linear-gradient(180deg, #2b84ea 0%, #155fc0 100%);
        border: 1px solid #0e4a9a; color: #fff;
        box-shadow: inset 0 1px 0 rgba(255,255,255,.35);
      }
      .ndm-btn-primary:hover:not(:disabled) {
        filter: brightness(1.07);
      }
      .ndm-btn-secondary {
        background: linear-gradient(180deg, #ffffff 0%, #e6eef7 100%);
        border: 1px solid #96aec9; color: #1c314a;
      }
      .ndm-btn-secondary:hover:not(:disabled) {
        background: #f5f9ff;
      }
      .ndm-btn-ghost {
        background: transparent; border: 1px solid #9fb6d0;
        color: #294566;
      }
      .ndm-btn-ghost:hover:not(:disabled) {
        background: rgba(255,255,255,.65);
      }

      /* ── IDM-style video flyout ─────────────────────────────
         A compact card anchored at the player (the pill's corner),
         NOT a centered dialog: a "Download all" header row, a
         media header (thumbnail + title + duration), then
         numbered rows carrying their sizes; after a click the
         same card is the progress view. The backdrop is transparent — it only
         catches outside clicks / Esc, the page stays visible. */
      .ndm-fly-backdrop {
        position: fixed; inset: 0; z-index: 2147483646;
        background: transparent;
        pointer-events: auto;
        font-family: "Segoe UI", system-ui, -apple-system, sans-serif;
        outline: none;
      }
      .ndm-fly {
        position: fixed;
        width: 348px; max-width: calc(100vw - 16px);
        max-height: 70vh; overflow-y: auto;
        background: #ffffff; color: #17293f;
        border: 1px solid #b9c9da; border-radius: 8px;
        box-shadow: 0 10px 32px rgba(10, 25, 45, .28);
        font-size: 13px; line-height: 1.45;
      }
      /* The progress view IS IDM's download dialog, so while it
         is on board the card takes the dialog's own width:
         526 px, pixel-measured from the reference screenshots
         (ndm-research/idm-dialog-ref 01/02 — the dialog fills
         01's frame edge to edge; 02's bar + panel corroborate).
         At the flyout's 348 px the same elements rendered at
         two-thirds of the dialog's scale and read cramped. The
         list view has no IDM original and keeps 348 px; the
         100vw cap below still protects small viewports. */
      .ndm-fly:has(.ndm-fly-prog) { width: 526px; }
      .ndm-fly-head {
        display: flex; align-items: center; justify-content: space-between;
        padding: 7px 8px 7px 12px;
        border-bottom: 1px solid #dbe5f0;
        background: #f2f6fb;
        border-radius: 8px 8px 0 0;
      }
      .ndm-fly-all {
        background: none; border: none; padding: 2px 4px;
        font: inherit; font-weight: 600; color: #14532d;
        cursor: pointer; text-align: left;
      }
      .ndm-fly-all:hover:not(:disabled) { text-decoration: underline; }
      .ndm-fly-all:disabled { color: #8aa0b8; cursor: default; }
      .ndm-fly-title {
        flex: 1; min-width: 0; margin-right: 8px;
        font-weight: 600; color: #17293f;
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ndm-fly-x {
        background: none; border: none; cursor: pointer;
        font-size: 17px; line-height: 1; color: #5b7189;
        width: 24px; height: 24px; border-radius: 5px; padding: 0;
      }
      .ndm-fly-x:hover { background: #e3ecf6; color: #17293f; }
      .ndm-fly-body { padding: 6px 0; }
      .ndm-fly.busy .ndm-fly-body { opacity: .55; pointer-events: none; }
      .ndm-fly-loading {
        display: flex; align-items: center; gap: 9px;
        padding: 8px 12px; color: #48637f;
      }
      /* Media header (restored): thumbnail + title + details
         line at the top of the list view, at the original
         card's metrics — 104×59 thumb with an 8px radius,
         13px two-line title, 11.5px muted details line. */
      .ndm-fly-media {
        display: flex; gap: 10px; align-items: flex-start;
        padding: 4px 12px 9px;
        border-bottom: 1px solid #e6eef7;
      }
      .ndm-fly-thumb {
        width: 104px; height: 59px; object-fit: cover;
        border-radius: 8px; flex: none; background: #e6eef7;
      }
      .ndm-fly-meta { flex: 1; min-width: 0; }
      .ndm-fly-mtitle {
        font-size: 13px; font-weight: 600; line-height: 1.35;
        display: -webkit-box; -webkit-line-clamp: 2;
        -webkit-box-orient: vertical; overflow: hidden;
      }
      .ndm-fly-msub {
        font-size: 11.5px; color: #48637f; margin-top: 3px;
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ndm-fly-row {
        padding: 5px 12px; cursor: pointer;
      }
      .ndm-fly-row-t {
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ndm-fly-row-q {
        white-space: nowrap; color: #48637f; font-size: 12px; margin-top: 1px;
      }
      .ndm-fly-row:hover { background: #eaf2fb; }
      .ndm-fly-note {
        padding: 6px 12px; color: #8a5a12;
        background: #fdf6e3; border-bottom: 1px solid #f0e2b6;
        font-size: 12px;
      }
      .ndm-fly-foot {
        display: flex; justify-content: flex-end;
        padding: 6px 12px 9px;
        border-top: 1px solid #dbe5f0;
      }
      .ndm-fly-app {
        background: none; border: none; padding: 2px 4px;
        font: inherit; font-size: 12px; color: #14532d;
        cursor: pointer;
      }
      .ndm-fly-app:hover:not(:disabled) { text-decoration: underline; }
      .ndm-fly-app:disabled { color: #8aa0b8; cursor: default; }
      .ndm-fly-dupe {
        display: flex; gap: 8px; padding: 8px 12px 10px;
      }
      /* ── Progress view = IDM's download dialog ──────────────
         The classic tab strip — "Download status" | "Speed
         Limiter" — over the panels (SPEC: ndm-research/
         idm-dialog-ref/SPEC.md §1.2); the Speed Limiter pane
         drives the entry's real per-file limit through the
         bridge. The "Download status" panel, element for
         element: URL line, then
         Status / File size / Downloaded / Transfer rate /
         Time left / Resume capability label rows in Tahoma, the
         classic green bar with an inset border, and flat classic
         buttons — Pause/Resume + Cancel right, IDM's own details
         toggle in the left slot ("Show in NDM", the card's real
         open-in-app action, sits beside it). The expanded
         details section below the button row — caption, segment
         chart, N. / Downloaded / Info connection table — renders
         from the progress mirror's blocks / conns_detail fields;
         against an older app that mirrors neither, the toggle
         simply never appears. */
      .ndm-fly-prog {
        padding: 9px 12px 11px;
        font-family: Tahoma, "Segoe UI", sans-serif;
        font-size: 11px; color: #000;
      }
      /* IDM's text block sits deeper than its bar: labels at
         ≈26 px from the dialog's edge, the value column at
         ≈129 px (pixel-measured, refs 01/02), while the bar and
         button row keep the ≈12 px container inset. The status
         pane carries that extra indent; with the 104 px label
         column below, values land on the dialog's 129 px. */
      .ndm-fly-pane-status { padding-left: 12px; }
      .ndm-fly-purl {
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
        margin-bottom: 6px;
      }
      .ndm-fly-prow { display: flex; line-height: 1.55; }
      .ndm-fly-plab { flex: 0 0 104px; white-space: nowrap; }
      .ndm-fly-pval {
        flex: 1; min-width: 0;
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ndm-fly-pstatus { color: #0000ee; text-decoration: underline; }
      .ndm-fly-pmsg { margin-top: 6px; line-height: 1.45; overflow-wrap: break-word; }
      /* IDM's tab strip (SPEC §1.2): classic raised tabs above
         the panel body — "Download status" | "Speed Limiter".
         The bar, button row and details section stay below both
         panes, as in the dialog. The strip renders only when the
         progress mirror reports the entry's speed_limit. */
      .ndm-fly-tabs {
        display: flex; gap: 2px; padding: 0 2px;
        border-bottom: 1px solid #7f7f7f; margin-bottom: 8px;
      }
      .ndm-fly-tab {
        background: #e4e4e4; border: 1px solid #7f7f7f; border-bottom: none;
        border-radius: 0; box-shadow: none; cursor: pointer;
        padding: 3px 12px; margin-bottom: -1px;
        font: 400 11px Tahoma, "Segoe UI", sans-serif; color: #000;
      }
      .ndm-fly-tab-on { background: #fff; }
      /* The Speed Limiter pane (reference 04): the live speed
         line, the "Use Speed Limiter" checkbox, and the
         "Maximum download speed:" value + unit, in the dialog's
         own plain controls. The note line carries rejections —
         the panel never claims a limit the app did not take. */
      .ndm-fly-lrow { display: flex; align-items: center; gap: 6px; margin-top: 8px; }
      .ndm-fly-luserow { cursor: pointer; }
      .ndm-fly-luse { margin: 0 2px 0 0; }
      .ndm-fly-lval {
        width: 62px; height: 21px; padding: 0 4px;
        border: 1px solid #7f7f7f; border-radius: 0;
        background: #fff; color: #000;
        font: 400 11px Tahoma, "Segoe UI", sans-serif;
      }
      .ndm-fly-lval:disabled { background: #f0f0f0; color: #838383; }
      .ndm-fly-lunit {
        height: 23px; border: 1px solid #7f7f7f; border-radius: 0;
        background: #fff; color: #000;
        font: 400 11px Tahoma, "Segoe UI", sans-serif;
      }
      .ndm-fly-lunit:disabled { color: #838383; }
      .ndm-fly-lnote { color: #b00020; margin-top: 5px; min-height: 13px; }
      .ndm-fly-bar {
        height: 14px; border: 1px solid #7f7f7f;
        background: #e6e6e6; overflow: hidden; margin-top: 8px;
      }
      .ndm-fly-bar-fill {
        height: 100%; width: 0%;
        background: #00b050;
        transition: width .3s ease;
      }
      .ndm-fly-btns { display: flex; align-items: center; gap: 8px; margin-top: 10px; padding-left: 10px; }
      .ndm-fly-btnsp { flex: 1; }
      .ndm-pbtn {
        height: 23px; min-width: 90px; padding: 0 10px;
        background: #f0f0f0; border: 1px solid #7f7f7f; border-radius: 0;
        color: #000; font: 400 11px Tahoma, "Segoe UI", sans-serif;
        cursor: pointer; white-space: nowrap; box-shadow: none;
      }
      .ndm-pbtn:hover:not(:disabled) { background: #e5f1fb; border-color: #0078d7; }
      .ndm-pbtn:disabled { color: #838383; border-color: #c0c0c0; cursor: default; }
      .ndm-pbtn-def { font-weight: 700; }
      /* IDM's details section (SPEC §1.7): centred caption, the
         segment chart (one cell per block of the poll's block
         map — filled = downloaded, pale = in flight, white =
         pending), then the connections table in a bordered,
         scrolling box with a classic header row. */
      .ndm-fly-details { margin-top: 10px; }
      .ndm-fly-dcap { text-align: center; margin-bottom: 5px; }
      .ndm-fly-dchart {
        display: flex; height: 14px; border: 1px solid #7f7f7f;
        background: #fff; overflow: hidden; margin-bottom: 6px;
      }
      .ndm-fly-dchart i { flex: 1 1 0; min-width: 0; }
      .ndm-fly-dchart i.b1 { background: #8fb0dc; }
      .ndm-fly-dchart i.b2 { background: #d7e5f5; }
      .ndm-fly-dtwrap {
        max-height: 118px; overflow-y: auto;
        border: 1px solid #7f7f7f; background: #fff;
      }
      .ndm-fly-dtable { width: 100%; border-collapse: collapse; }
      .ndm-fly-dtable th, .ndm-fly-dtable td {
        text-align: left; padding: 2px 6px; font-weight: 400;
        white-space: nowrap;
      }
      .ndm-fly-dtable thead th {
        background: #f0f0f0; border-bottom: 1px solid #7f7f7f;
        position: sticky; top: 0;
      }
      .ndm-fly-dtable .ndm-dc-n { width: 30px; }
      .ndm-fly-dtable .ndm-dc-d { width: 108px; }
      .ndm-fly-multi { padding: 2px 0; }
      .ndm-fly-mrow {
        display: flex; flex-wrap: wrap; align-items: center;
        column-gap: 10px; row-gap: 3px;
        padding: 6px 12px 7px;
        font-family: Tahoma, "Segoe UI", sans-serif;
        font-size: 11.5px; color: #000;
      }
      .ndm-fly-mname {
        order: 1; flex: 1; min-width: 0;
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ndm-fly-mbtns { order: 2; display: flex; gap: 6px; flex: none; }
      .ndm-fly-mbar {
        order: 3; flex: 0 0 100%; height: 10px;
        border: 1px solid #7f7f7f; background: #e6e6e6; overflow: hidden;
      }
      .ndm-fly-mbar-fill {
        height: 100%; width: 0%;
        background: #00b050; transition: width .3s ease;
      }
      .ndm-fly-mpct { order: 4; flex: 0 0 100%; color: #000; min-height: 15px; }
      .ndm-fly-mbtn {
        background: #f0f0f0;
        border: 1px solid #7f7f7f; color: #000;
        border-radius: 0; height: 23px; padding: 0 10px;
        font: inherit; font-size: 11px; cursor: pointer;
      }
      .ndm-fly-mbtn:hover:not(:disabled) { background: #e5f1fb; border-color: #0078d7; }
      .ndm-fly-mbtn:disabled { color: #838383; border-color: #c0c0c0; cursor: default; }
      .ndm-fly-state { color: #1d7a3a; font-weight: 600; }

      /* Non-intrusive Bottom-Right Background Download Toast */
      .ndm-toast {
        position: fixed; right: 18px; bottom: 18px;
        pointer-events: auto;
        display: flex; align-items: center; gap: 10px;
        max-width: 390px;
        background: linear-gradient(180deg, rgba(16, 28, 48, .95), rgba(10, 18, 32, .98));
        color: #fff;
        border: 1px solid rgba(100, 210, 150, .5);
        border-radius: 10px;
        padding: 10px 12px;
        box-shadow: 0 14px 36px rgba(0, 0, 0, .45);
        font-family: "Segoe UI", system-ui, -apple-system, Roboto, Arial, sans-serif;
        opacity: 0; transform: translateY(10px);
        transition: opacity .18s ease, transform .18s ease;
      }
      .ndm-toast.on {
        opacity: 1; transform: translateY(0);
      }
      .ndm-toast-icon {
        width: 28px; height: 28px; border-radius: 50%; flex: none;
        background: linear-gradient(180deg, #34d399, #059669);
        color: #fff; font-weight: 800; font-size: 14px;
        display: flex; align-items: center; justify-content: center;
      }
      .ndm-toast-body {
        flex: 1; min-width: 0;
      }
      .ndm-toast-title {
        font-size: 12.5px; font-weight: 700; color: #ecfdf5;
      }
      .ndm-toast-sub {
        font-size: 11.5px; color: #cbd5e1;
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
        margin-top: 2px;
      }
      .ndm-toast-open {
        background: rgba(255, 255, 255, .14);
        border: 1px solid rgba(255, 255, 255, .28);
        color: #fff; border-radius: 5px;
        padding: 4px 8px; font-size: 11.5px; font-weight: 600;
        cursor: pointer; white-space: nowrap;
      }
      .ndm-toast-open:hover {
        background: rgba(255, 255, 255, .24);
      }
      .ndm-toast-x {
        background: none; border: none; color: rgba(255, 255, 255, .65);
        font-size: 16px; cursor: pointer; padding: 0 2px; line-height: 1;
      }
      .ndm-toast-x:hover { color: #fff; }
    `;

    const pill = document.createElement('div');
    pill.className = 'pill';
    for (const evt of ['mousedown', 'pointerdown', 'mouseup', 'dblclick']) {
      pill.addEventListener(evt, e => e.stopPropagation());
    }

    const chip = document.createElement('span');
    chip.className = 'chip';
    chip.textContent = '⬇';

    const btn = document.createElement('button');
    btn.className = 'label';
    btn.innerHTML = '<span>Download with NDM</span><span class="caret">▾</span>';
    btn.title = 'Choose video quality & download with NDM';
    btn.addEventListener('click', e => {
      e.stopPropagation();
      openBrowserModal(btn, pill);
    });

    const close = document.createElement('button');
    close.className = 'close';
    close.textContent = '×';
    close.title = 'Hide button on this page';
    close.addEventListener('click', e => {
      e.stopPropagation();
      dismissed = true;
      removePanel();
    });

    pill.appendChild(chip);
    pill.appendChild(btn);
    pill.appendChild(close);
    shadowRoot.appendChild(style);
    shadowRoot.appendChild(pill);
    cardEl = pill;
    (document.fullscreenElement || document.documentElement).appendChild(panelHost);
    place();
    requestAnimationFrame(() => pill.classList.add('on'));
  }

  function removePanel() {
    dismissActiveModal();
    if (panelHost) {
      panelHost.remove();
      panelHost = null;
      shadowRoot = null;
      cardEl = null;
      anchor = null;
    }
  }

  let lastMediaState = null;
  function reportMediaState() {
    // The background mirrors the page context menu's "Download
    // this page's video" item to whether this page HAS media
    // (F-EXT-15): report only when the answer changes.
    const has = videoDetected();
    if (has === lastMediaState) return;
    lastMediaState = has;
    try {
      chrome.runtime
        .sendMessage({ action: 'ndm-media-state', hasMedia: has })
        .catch(() => {});
    } catch (e) {}
  }

  function refresh() {
    reportMediaState();
    if (dismissed) return;
    const hasVid = videoDetected();
    if (hasVid || modalOpen) {
      if (!panelHost || !panelHost.isConnected) buildPanel();
      if (hasVid) {
        anchor = pickAnchor();
        place();
        backgroundPreProbe();
      } else if (cardEl) {
        cardEl.style.display = 'none';
      }
    } else {
      removePanel();
    }
  }

  /* ---------------------------------------------------------------- */
  /* Synchronous <a href="..."> click interceptor for .iso / files    */
  /* Prevents the browser's native Save-As / download bar from ever   */
  /* appearing and opens NDM's in-browser file dialog at 0ms!         */
  /* ---------------------------------------------------------------- */

  // ONE canonical extension list for the whole extension — identical
  // to FILE_EXTENSIONS in background.js (IDM likewise has a single
  // File Types list). The two layers must never drift apart again:
  // a bare .mp4/.webm link click is only ever seen by THIS layer
  // (the browser would otherwise just play the file in a tab).
  const INTERCEPT_EXTS = new Set([
    'zip', 'rar', '7z', 'tar', 'gz', 'gzip', 'bz2', 'xz', 'zst', 'tgz', 'tbz2', 'txz', 'cab', 'ace', 'arj', 'lzh', 'sit', 'sitx',
    'exe', 'msi', 'msix', 'msixbundle', 'deb', 'rpm', 'dmg', 'pkg', 'iso', 'img', 'apk', 'bin', 'run', 'appimage', 'flatpakref', 'msu',
    'pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'pps', 'rtf', 'epub', 'mobi', 'tif', 'tiff',
    'mp3', 'mp4', 'mkv', 'avi', 'mov', 'webm', 'flac', 'aac', 'ogg', 'ogv', 'm4a', 'm4b', 'opus', 'wav', 'wma', 'flv', 'wmv', '3gp', 'm4v', 'ts',
    'mpg', 'mpeg', 'mpa', 'mpe', 'asf', 'rm', 'rmvb', 'aif',
    'torrent', 'jar', 'war', 'whl', 'vmdk', 'qcow2', 'vdi',
  ]);

  let ndmInterceptActive = true;
  // IDM parity, same as the background layer: while the native host
  // is KNOWN to be missing the module stays inert — a click must
  // behave as if the extension were not there at all. "Known" has a
  // shelf life: a status older than ~60 s is re-probed, not trusted
  // forever (one transient crash used to disable interception
  // indefinitely).
  let ndmHostKnownMissing = false;
  let ndmHostStatusStr = '';
  let ndmHostStatusAt = 0;
  let hostRepollAsked = false;
  function evalHostMissing() {
    const fresh = Date.now() - ndmHostStatusAt < 60000;
    ndmHostKnownMissing = ndmHostStatusAt > 0 && fresh && ndmHostStatusStr === 'host-missing';
    if (ndmHostStatusStr === 'host-missing' && !fresh && !hostRepollAsked) {
      hostRepollAsked = true;
      try {
        chrome.runtime.sendMessage({ action: 'ndm-host-ping' }).catch(() => {});
      } catch (e) {}
    }
  }
  // IDM's keys (Options → General → Keys): hold Alt+Shift while
  // clicking to PREVENT taking over one link; hold Insert while
  // clicking to FORCE the download. Per-site exception list and
  // per-tab off remain the other escape hatches.
  let ndmForceHeld = false;
  let ndmSiteExcepted = false;
  let ndmTabDisabled = false;
  // Alt+Shift state is TRACKED from key events (like Insert) and
  // also read off the click event itself — either source alone
  // proved unreliable in the wild (F-EXT-13). The tracked state
  // is also what the background's downloads layer asks about: a
  // download the browser itself started (Shift+click opens a new
  // window) never passes through the click handler below, yet
  // the gesture still belongs to this page.
  let ndmPreventHeld = false;
  let lastPreventClickAt = 0;
  function syncPreventHeld(e) {
    ndmPreventHeld = !!(e.altKey && e.shiftKey);
  }
  function notePreventClick(url) {
    lastPreventClickAt = Date.now();
    try {
      chrome.runtime
        .sendMessage({ action: 'ndm-note-prevent', url: url || '' })
        .catch(() => {});
    } catch (e) {}
  }
  addEventListener('keydown', e => {
    if (e.key === 'Insert') ndmForceHeld = true;
    syncPreventHeld(e);
  }, true);
  addEventListener('keyup', e => {
    if (e.key === 'Insert') ndmForceHeld = false;
    syncPreventHeld(e);
  }, true);
  addEventListener('blur', () => {
    ndmForceHeld = false;
    ndmPreventHeld = false;
  });
  try {
    chrome.storage.local.get({
      ndmIntercept: true,
      ndmHostStatus: '',
      ndmHostStatusAt: 0,
      ndmSiteExceptions: [],
    }).then(v => {
      ndmInterceptActive = v.ndmIntercept !== false;
      ndmHostStatusStr = v.ndmHostStatus || '';
      ndmHostStatusAt = Number(v.ndmHostStatusAt) || 0;
      evalHostMissing();
      const list = Array.isArray(v.ndmSiteExceptions) ? v.ndmSiteExceptions : [];
      ndmSiteExcepted = list.includes(host);
    }).catch(() => {});
    chrome.runtime.sendMessage({ action: 'ndm-tab-state' }).then(r => {
      if (r) ndmTabDisabled = !!r.disabled;
    }).catch(() => {});
    chrome.storage.onChanged.addListener((changes, area) => {
      // ndmTabOff lives in SESSION storage (EX-m6); everything else
      // is local. Session changes only ever carry that one key.
      if (area !== 'local' && area !== 'session') return;
      if (changes.ndmIntercept) {
        ndmInterceptActive = changes.ndmIntercept.newValue !== false;
      }
      if (changes.ndmHostStatus) {
        ndmHostStatusStr = changes.ndmHostStatus.newValue || '';
        evalHostMissing();
      }
      if (changes.ndmHostStatusAt) {
        ndmHostStatusAt = Number(changes.ndmHostStatusAt.newValue) || 0;
        evalHostMissing();
      }
      if (changes.ndmSiteExceptions) {
        const list = Array.isArray(changes.ndmSiteExceptions.newValue)
          ? changes.ndmSiteExceptions.newValue
          : [];
        ndmSiteExcepted = list.includes(host);
      }
      if (changes.ndmTabOff) {
        chrome.runtime.sendMessage({ action: 'ndm-tab-state' }).then(r => {
          if (r) ndmTabDisabled = !!r.disabled;
        }).catch(() => {});
      }
    });
  } catch (e) {}

  function isDirectDownloadLink(a) {
    if (!a || !a.href) return false;
    if (!/^https?:\/\//i.test(a.href)) return false;
    try {
      const u = new URL(a.href);
      const cleanPath = u.pathname.toLowerCase();
      const dot = cleanPath.lastIndexOf('.');
      const ext = dot >= 0 ? cleanPath.slice(dot + 1) : '';
      if (INTERCEPT_EXTS.has(ext)) return true;
      // Query-string filenames (/download?file=setup.exe): the
      // background layer catches these through filenameFromUrl —
      // the click layer must recognise the same links, or the two
      // layers disagree about the one canonical list (EX-m9).
      for (const [k, v] of u.searchParams.entries()) {
        if (/file|name|attachment|download/i.test(k) && v.includes('.')) {
          const clean = v.split(/[?#]/)[0].toLowerCase();
          const qDot = clean.lastIndexOf('.');
          if (qDot >= 0 && INTERCEPT_EXTS.has(clean.slice(qDot + 1))) return true;
        }
      }
      if (a.hasAttribute('download')) {
        const dlAttr = (a.getAttribute('download') || '').toLowerCase();
        const dlDot = dlAttr.lastIndexOf('.');
        const dlExt = dlDot >= 0 ? dlAttr.slice(dlDot + 1) : '';
        if (INTERCEPT_EXTS.has(dlExt)) return true;
      }
    } catch (e) {}
    return false;
  }

  document.addEventListener(
    'click',
    e => {
      // sub-frame clicks are not intercepted here: the dialog would
      // open inside a possibly tiny iframe (the background-driven
      // path already refuses non-top frames). The browser-download
      // layer still catches the file if it becomes a download.
      if (window.top !== window.self) return;
      if (!ndmInterceptActive) return;
      if (ndmHostKnownMissing) return;
      if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey) return;
      // IDM keys: Alt+Shift held PREVENTS the takeover of this
      // one link. The gesture is RECORDED before bailing — the
      // browser is about to start its own download, and the
      // background's downloads layer must be able to learn that
      // this click was a prevent (via the note above and its
      // 'ndm-prevent-state' query), or it would simply grab the
      // file itself and re-open this dialog (F-EXT-13). Any
      // other Alt/Shift click keeps the browser's own semantics
      // (new window/tab) as before.
      if ((e.altKey && e.shiftKey) || ndmPreventHeld) {
        const a =
          e.target && e.target.closest ? e.target.closest('a[href]') : null;
        notePreventClick(a ? a.href : '');
        return;
      }
      if (e.altKey || e.shiftKey) return;
      if (ndmTabDisabled) return;
      const target = e.target && e.target.closest ? e.target.closest('a[href]') : null;
      if (!target || !/^https?:\/\//i.test(target.href)) return;
      // Insert held FORCES the download: it bypasses the per-site
      // exception and the downloadable-type check.
      const forced = ndmForceHeld;
      if (!forced && ndmSiteExcepted) return;
      if (!forced && !isDirectDownloadLink(target)) return;
      e.preventDefault();
      e.stopPropagation();
      const dlName = (target.getAttribute('download') || '').trim();
      openBrowserFileModal({
        url: target.href,
        filename: dlName,
        totalBytes: 0,
        referrer: location.href,
      });
    },
    true
  );

  // Listen for requests from extension popup or background script
  chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (!msg) return;
    if (msg.action === 'ndm-prevent-state') {
      // The background's downloads layer asks before taking over
      // a browser-started download (F-EXT-13): was Alt+Shift held
      // for the click that produced it — held right now, or a
      // prevent click within the last few seconds?
      sendResponse({
        held: ndmPreventHeld,
        recent: Date.now() - lastPreventClickAt < 5000,
        at: lastPreventClickAt,
      });
      return false;
    }
    if (msg.action === 'ndm-query-media') {
      // The popup asks the page itself whether it has media
      // before offering its video button (R2-EXT-08): the page is
      // the freshest source — the background's per-tab record can
      // lag a just-loaded player.
      sendResponse({
        hasMedia: videoDetected() || !!pickAnchor(),
      });
      return false;
    }
    if (msg.action === 'ndm-open-video-modal') {
      // Top frame only (EX-M1): a broadcast open used to build a
      // panel + speculative download in EVERY frame, iframes
      // included. (Callers also target frameId 0 now; this guard
      // is the content side of the same contract.)
      if (window.self !== window.top) {
        return false;
      }
      dismissed = false;
      if (!panelHost || !panelHost.isConnected) buildPanel();
      const btn = cardEl ? cardEl.querySelector('.label') : null;
      // Answer as soon as the dialog SHELL is open (EX-m12): the
      // caller (popup) must not hang until the probe settles. If
      // the shell never appears, the settled promise answers with
      // the real outcome instead — the old instant ok:true left
      // callers with no fallback when the open failed.
      let answered = false;
      const answer = ok => {
        if (answered) return;
        answered = true;
        try { sendResponse({ ok }); } catch (e) {}
      };
      openBrowserModal(btn, cardEl, msg.srcUrl || '', () => answer(true))
        .then(() => answer(!!(modalOpen && modalEl)))
        .catch(() => answer(false));
      return true;
    }
    if (msg.action === 'ndm-open-file-modal') {
      if (window.self !== window.top) {
        return false;
      }
      dismissed = false;
      openBrowserFileModal({
        url: msg.url || '',
        filename: msg.filename || '',
        totalBytes: msg.totalBytes || 0,
        referrer: msg.referrer || location.href,
      });
      sendResponse({ ok: true });
      return false;
    }
  });

  refresh();
  let lastUrl = location.href;
  setInterval(() => {
    if (location.href !== lastUrl) {
      lastUrl = location.href;
      dismissed = false;
      dismissActiveModal();
      // SPA navigation: tell the background to drop this tab's
      // sniffed streams — they belong to the video just left, and
      // the next dialog must never be handed them (EX-m2).
      try {
        chrome.runtime.sendMessage({ action: 'ndm-spa-navigated', url: lastUrl }).catch(() => {});
      } catch (e) {}
      refresh();
    } else {
      place();
    }
  }, 600);
  addEventListener('scroll', place, { passive: true, capture: true });
  addEventListener('resize', place);
  document.addEventListener('fullscreenchange', place);

  let refreshPending = false;
  function scheduleRefresh() {
    if (refreshPending) return;
    refreshPending = true;
    setTimeout(() => {
      refreshPending = false;
      refresh();
    }, 250);
  }
  const observer = new MutationObserver(scheduleRefresh);
  observer.observe(document.documentElement, { childList: true, subtree: true });
})();
