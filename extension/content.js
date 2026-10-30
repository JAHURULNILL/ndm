'use strict';
/* NDM — Nill Download Manager: content script.
 *
 * Shows the IDM-style "⬇ Download with NDM" floating button at the top-right
 * corner of any downloadable video player. Clicking it opens an IDM-style
 * Video Download dialog RIGHT INSIDE THE BROWSER (inside a closed Shadow DOM)
 * displaying:
 *   - Video Thumbnail, Duration, and Channel/Uploader
 *   - Editable Title / File Name
 *   - Quality & Format Selection List with exact/estimated MB sizes
 *   - Save to Folder + Start Download / Download Later buttons
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
  let anchor = null;      // the <video> the pill is attached to
  let dismissed = false;  // user closed the pill on this page
  let modalOpen = false;  // whether the in-browser modal is currently open
  let activeProbeToken = 0;
  // The open modal's own dismiss logic (Cancel semantics: discard or
  // pause the entry that started while the dialog was open). Every
  // close path — backdrop, pill ×, SPA navigation, modal replacement
  // — must go through it; the raw teardown alone would leave a
  // "dismissed" download running invisibly in the app.
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
      host.endsWith('instagram.com') ||
      host.endsWith('tiktok.com') ||
      host.endsWith('vimeo.com') ||
      host.endsWith('dailymotion.com') ||
      host === 'x.com' ||
      host.endsWith('.x.com') ||
      host === 'twitter.com' ||
      host.endsWith('.twitter.com') ||
      host.endsWith('twitch.tv') ||
      host.endsWith('reddit.com') ||
      host.endsWith('bilibili.com')
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

  function videoDetected() {
    return isKnownVideoSite() || hasPlayableVideoElement();
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

    // 2. Walk up from the <video> element to find a post/reel/video permalink
    if (videoEl) {
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
      }
    }

    // 3. For generic sites: if the <video> has a direct http(s) media URL, or
    //    if the background worker sniffed an .m3u8 / .mpd / .mp4 stream, use it
    if (!isKnownVideoSite() && videoEl) {
      const direct = videoEl.currentSrc || videoEl.src || '';
      if (/^https?:\/\//i.test(direct)) return direct;
      const sourceEl = videoEl.querySelector('source[src]');
      const sUrl = sourceEl ? sourceEl.getAttribute('src') || '' : '';
      if (/^https?:\/\//i.test(sUrl)) {
        try {
          return new URL(sUrl, href).href;
        } catch (e) {}
      }
      try {
        const mediaResp = await chrome.runtime.sendMessage({ action: 'ndm-tab-media' });
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
    cardEl.style.top = Math.max(4, r.top + 12) + 'px';
  }

  const localProbeCache = new Map(); // canonicalUrl -> info
  const inFlightProbes = new Map();  // canonicalUrl -> Promise<res>
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
    if (document.querySelectorAll('video').length > 3) return;
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
    if (modalEl) {
      modalEl.remove();
      modalEl = null;
    }
    place();
  }

  async function openBrowserModal(pillBtn, pillContainer) {
    if (!shadowRoot) return;
    dismissActiveModal();
    modalOpen = true;
    const myToken = ++activeProbeToken;

    requestMainWorldPlayerInfo();
    const currentAnchor = anchor || pickAnchor();
    const rawTargetUrl = await resolveVideoUrl(currentAnchor);
    const targetUrl = canonicalVideoKey(rawTargetUrl);
    const fallbackTitle = cleanPageTitle();
    const fallbackThumb = guessFallbackThumbnail(targetUrl, currentAnchor);

    let savedDir = '~/Downloads';
    try {
      const st = await chrome.storage.local.get({ ndmSaveDir: '~/Downloads' });
      if (st && st.ndmSaveDir) savedDir = st.ndmSaveDir;
    } catch (e) {}

    const backdrop = document.createElement('div');
    backdrop.className = 'ndm-modal-backdrop';
    for (const evt of ['mousedown', 'pointerdown', 'mouseup', 'dblclick']) {
      backdrop.addEventListener(evt, e => e.stopPropagation());
    }
    backdrop.addEventListener('click', e => {
      e.stopPropagation();
      // clicking outside the dialog is a Cancel, with full Cancel
      // semantics (the speculative entry is discarded) — the raw
      // teardown used to leave it downloading invisibly
      if (e.target === backdrop) dismissVideoCancel();
    });

    const dialog = document.createElement('div');
    dialog.className = 'ndm-modal';
    dialog.innerHTML = `
      <div class="ndm-m-head">
        <div class="ndm-m-brand">
          <span class="ndm-m-logo">⬇</span>
          <span>Download Video with NDM</span>
        </div>
        <button class="ndm-m-x" type="button" title="Close">×</button>
      </div>
      <div class="ndm-m-body">
        <div class="ndm-m-preview">
          <div class="ndm-m-thumb-wrap">
            <img class="ndm-m-thumb" alt="" style="display:none" />
            <div class="ndm-m-thumb-ph">🎬</div>
            <span class="ndm-m-dur" style="display:none"></span>
          </div>
          <div class="ndm-m-meta">
            <label class="ndm-m-lbl">Video Title / File Name</label>
            <input class="ndm-m-input ndm-m-title" type="text" value="" placeholder="Video title" />
            <div class="ndm-m-sub">
              <span class="ndm-m-uploader"></span>
              <span class="ndm-m-host">${location.hostname}</span>
            </div>
          </div>
        </div>

        <div class="ndm-m-qhead">
          <span>Select Quality &amp; Format</span>
          <span class="ndm-m-qhint">File Size</span>
        </div>
        <div class="ndm-m-qlist">
          <div class="ndm-m-loading">
            <div class="ndm-spinner"></div>
            <div>
              <div class="ndm-m-load-title">Reading video qualities &amp; calculating MB sizes…</div>
              <div class="ndm-m-load-sub">Fetching stream manifest via NDM high-speed engine</div>
            </div>
          </div>
        </div>

        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">Save to folder</label>
          <input class="ndm-m-input ndm-m-dir" type="text" value="" placeholder="~/Downloads" />
        </div>
        <div class="ndm-m-status" style="display:none"></div>
      </div>
      <div class="ndm-m-foot">
        <button class="ndm-btn ndm-btn-ghost ndm-btn-app" type="button" title="Open this video in the NDM desktop app window">Open in NDM App</button>
        <div class="ndm-m-foot-right">
          <button class="ndm-btn ndm-btn-secondary ndm-btn-cancel" type="button">Cancel</button>
          <button class="ndm-btn ndm-btn-secondary ndm-btn-all" type="button" disabled title="Download every listed quality (IDM's Download all)">Download all</button>
          <button class="ndm-btn ndm-btn-secondary ndm-btn-later" type="button" disabled>Download Later</button>
          <button class="ndm-btn ndm-btn-primary ndm-btn-start" type="button" disabled>⬇ Start Download</button>
        </div>
      </div>
    `;

    backdrop.appendChild(dialog);
    shadowRoot.appendChild(backdrop);
    modalEl = backdrop;
    syncHostParent();
    panelHost.style.display = 'block';

    const titleIn = dialog.querySelector('.ndm-m-title');
    const dirIn = dialog.querySelector('.ndm-m-dir');
    const thumbImg = dialog.querySelector('.ndm-m-thumb');
    const thumbPh = dialog.querySelector('.ndm-m-thumb-ph');
    const durBadge = dialog.querySelector('.ndm-m-dur');
    const uploaderEl = dialog.querySelector('.ndm-m-uploader');
    const qListEl = dialog.querySelector('.ndm-m-qlist');
    const statusEl = dialog.querySelector('.ndm-m-status');
    const btnClose = dialog.querySelector('.ndm-m-x');
    const btnCancel = dialog.querySelector('.ndm-btn-cancel');
    const btnApp = dialog.querySelector('.ndm-btn-app');
    const btnLater = dialog.querySelector('.ndm-btn-later');
    const btnAll = dialog.querySelector('.ndm-btn-all');
    const btnStart = dialog.querySelector('.ndm-btn-start');

    titleIn.value = fallbackTitle;
    dirIn.value = savedDir;

    backdrop.addEventListener('keydown', e => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        e.preventDefault();
        dismissVideoCancel();
      } else if (e.key === 'Enter' && !btnStart.disabled) {
        e.preventDefault();
        btnStart.click();
      }
    });

    function showThumbnail(src) {
      if (!src) return;
      thumbImg.onload = () => {
        thumbImg.style.display = 'block';
        thumbPh.style.display = 'none';
      };
      thumbImg.onerror = () => {
        if (src !== fallbackThumb && fallbackThumb) {
          thumbImg.src = fallbackThumb;
        }
      };
      thumbImg.src = src;
    }

    if (fallbackThumb) showThumbnail(fallbackThumb);

    btnClose.addEventListener('click', () => dismissVideoCancel());
    btnCancel.addEventListener('click', () => dismissVideoCancel());
    activeModalDismiss = dismissVideoCancel;

    btnApp.addEventListener('click', async () => {
      if (vspec.inflight) return;
      if (vspec.id) {
        // already downloading in the app (started while this dialog
        // was open): just show the app
        vspec.closed = true;
        closeBrowserModal();
        flashPill(pillBtn, pillContainer, true, 'Opened in NDM ✓');
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
          kind: 'video',
          url: targetUrl,
          pageUrl: location.href,
          title: titleIn.value.trim() || fallbackTitle,
        });
        if (resp && resp.ok) {
          closeBrowserModal();
          flashPill(pillBtn, pillContainer, true, 'Opened in NDM ✓');
        } else {
          showError('Could not reach NDM desktop app. Please make sure NDM is installed.');
          btnApp.disabled = false;
          btnApp.textContent = 'Open in NDM App';
        }
      } catch (e) {
        showError('Could not reach NDM desktop app.');
        btnApp.disabled = false;
        btnApp.textContent = 'Open in NDM App';
      }
    });

    function showError(msg) {
      statusEl.style.display = 'block';
      statusEl.className = 'ndm-m-status err';
      statusEl.textContent = msg;
    }

    let selectedOption = null;
    let renderedAlready = false;

    function applyVideoInfo(info) {
      if (!info) return;
      renderedAlready = true;
      if (info.title && (!titleIn.value || titleIn.value === fallbackTitle)) {
        titleIn.value = info.title;
      }
      if (info.thumbnail) {
        showThumbnail(info.thumbnail);
      }
      if (info.duration) {
        durBadge.textContent = fmtDuration(info.duration);
        durBadge.style.display = 'inline-block';
      }
      if (info.uploader) {
        uploaderEl.textContent = info.uploader + ' · ';
      }
      renderQualities(info.options || info.qualities || []);
    }

    function renderQualities(qualities) {
      qListEl.innerHTML = '';
      if (!qualities || !qualities.length) {
        qualities = [
          {
            id: 'best',
            label: 'Best available quality (Auto)',
            selector: 'bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b',
            audio: false,
            ext: 'MP4',
            size_bytes: null,
            size_text: 'Auto',
            height: 1080,
            note: 'MP4 video + AAC audio',
          },
          {
            id: 'audio',
            label: 'Audio only (MP3)',
            selector: 'ba/b',
            audio: true,
            ext: 'MP3',
            size_bytes: null,
            size_text: 'Audio',
            height: 0,
            note: 'High quality MP3 audio',
          },
        ];
      }

      selectedOption = qualities.find(q => q.height === 1080) ||
        qualities.find(q => q.height === 720) ||
        qualities[0];

      for (const q of qualities) {
        const row = document.createElement('div');
        const isSel = q === selectedOption;
        row.className = 'ndm-q-row' + (isSel ? ' sel' : '');
        const extBadge = (q.ext || (q.audio ? 'MP3' : 'MP4')).toUpperCase();
        const sizeLabel = q.size_text || (q.size_bytes ? fmtBytes(q.size_bytes) : (q.id === 'best' ? 'Auto' : 'Stream'));
        const cleanNote = (q.note || '')
          .split('•')
          .map(p => p.trim())
          .filter(p => p && p.toUpperCase() !== extBadge && !/^\d+(\.\d+)?\s*(B|KB|MB|GB)$/i.test(p))
          .join(' · ');
        const noteText = cleanNote ? `<span class="ndm-q-note">${escapeHtml(cleanNote)}</span>` : '';

        row.innerHTML = `
          <div class="ndm-q-left">
            <span class="ndm-q-radio"></span>
            <span class="ndm-q-badge ${q.audio ? 'audio' : 'video'}">${escapeHtml(extBadge)}</span>
            <span class="ndm-q-label">${escapeHtml(q.label)}</span>
            ${noteText}
          </div>
          <div class="ndm-q-right">
            <span class="ndm-q-size ${q.size_bytes ? 'exact' : ''}">${escapeHtml(sizeLabel)}</span>
          </div>
        `;
        row.addEventListener('click', () => {
          selectedOption = q;
          for (const el of qListEl.querySelectorAll('.ndm-q-row')) {
            el.classList.remove('sel');
          }
          row.classList.add('sel');
          updateStartButtonLabel();
          // picking a quality is what starts it (IDM's panel pick
          // opens the already-downloading File Info dialog)
          vspecSwitch(q);
        });
        row.addEventListener('dblclick', () => {
          selectedOption = q;
          btnStart.click();
        });
        qListEl.appendChild(row);
      }

      allQualities = qualities;
      btnAll.disabled = false;
      btnStart.disabled = false;
      btnLater.disabled = false;
      updateStartButtonLabel();
      if (!vspec.started && !vspec.closed) vspecStart(selectedOption);
    }

    function updateStartButtonLabel() {
      if (!selectedOption) return;
      if (vspec.id) {
        btnStart.textContent = '✓ Keep downloading';
        return;
      }
      const sz = selectedOption.size_text || (selectedOption.size_bytes ? fmtBytes(selectedOption.size_bytes) : '');
      btnStart.textContent = sz
        ? `⬇ Start Download (${sz})`
        : '⬇ Start Download';
    }

    let pendingForce = false; // set after a duplicate refusal: the
    // next send is the user's explicit "download it again" (IDM asks
    // about duplicate links instead of refusing silently)

    /* ── IDM parity: the selected quality starts downloading while
       this dialog is open (its File Info dialog behavior). Switching
       quality restarts with the new one; Start = keep, Later =
       pause in the app, Cancel = discard completely. A failed
       speculative start falls back to the classic send-on-click. ── */
    const vspec = {
      started: false,
      inflight: false,
      id: null,
      selector: null,
      failed: false,
      pending: null,
      pendingSelector: null, // quality switch clicked mid-start
      closed: false,
    };
    let allQualities = [];

    async function vspecControl(action) {
      if (!vspec.id) return;
      try {
        await chrome.runtime.sendMessage({ action, entry: vspec.id });
      } catch (e) {}
    }

    function dismissVideoKeep() {
      vspec.closed = true;
      closeBrowserModal();
      flashPill(pillBtn, pillContainer, true, 'Downloading in NDM ✓');
      showBrowserToast(
        titleIn.value.trim() || fallbackTitle,
        selectedOption ? selectedOption.label || '' : '',
        '',
        false
      );
    }

    async function dismissVideoLater() {
      vspec.closed = true;
      closeBrowserModal();
      if (vspec.id) await vspecControl('ndm-pause-entry');
      flashPill(pillBtn, pillContainer, true, 'Paused in NDM ✓');
      showBrowserToast(
        titleIn.value.trim() || fallbackTitle,
        selectedOption ? selectedOption.label || '' : '',
        '',
        true
      );
    }

    function dismissVideoCancel() {
      if (vspec.closed) return;
      vspec.closed = true;
      if (vspec.id) {
        vspecControl('ndm-cancel-entry');
      } else if (vspec.inflight) {
        vspec.pending = 'cancel';
      }
      closeBrowserModal();
    }

    async function vspecStart(q) {
      if (!q || vspec.inflight) return;
      vspec.inflight = true;
      vspec.started = true;
      vspec.selector = q.selector;
      // lock the fields the entry is about to be created from (see
      // the file dialog: edits during a cold-start round trip used
      // to be silently discarded)
      titleIn.disabled = true;
      dirIn.disabled = true;
      btnApp.disabled = true;
      let resp = null;
      try {
        resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'video_direct',
          url: targetUrl,
          pageUrl: location.href,
          title: titleIn.value.trim() || fallbackTitle,
          selector: q.selector,
          audio: !!q.audio,
          dir: dirIn.value.trim() || '~/Downloads',
          later: false,
          total: Number(q.size_bytes) || 0,
          force: false,
        });
      } catch (e) {}
      vspec.inflight = false;
      btnApp.disabled = false;
      const gone = vspec.closed || myToken !== activeProbeToken;
      if (resp && resp.ok && resp.id) {
        vspec.id = resp.id;
        if (gone) {
          if (vspec.pending === 'cancel') vspecControl('ndm-cancel-entry');
          else if (vspec.pending === 'later') vspecControl('ndm-pause-entry');
          return;
        }
        if (vspec.pending === 'cancel') { dismissVideoCancel(); return; }
        if (vspec.pending === 'later') { dismissVideoLater(); return; }
        if (vspec.pending === 'keep') { dismissVideoKeep(); return; }
        if (vspec.pendingSelector && vspec.pendingSelector.selector !== q.selector) {
          // a quality switch clicked DURING the start was recorded,
          // not dropped: apply it now (cancel this entry, start the
          // chosen one) instead of silently downloading the quality
          // the user already moved away from
          const next = vspec.pendingSelector;
          vspec.pendingSelector = null;
          vspecSwitch(next);
          return;
        }
        statusEl.style.display = 'block';
        statusEl.className = 'ndm-m-status';
        statusEl.textContent = '⬇ Downloading in NDM…';
        btnStart.textContent = '✓ Keep downloading';
        btnLater.textContent = '⏸ Pause in NDM';
      } else if (
        resp &&
        typeof resp.error === 'string' &&
        resp.error.startsWith('duplicate:')
      ) {
        // this quality is already downloading: not ours to control —
        // Start becomes the explicit "Download again" (force path)
        vspec.failed = true;
        pendingForce = true;
        titleIn.disabled = false;
        dirIn.disabled = false;
        if (!gone) {
          showError('This quality is already in your download list. Press "Download again" to save a second copy.');
          btnStart.textContent = '⬇ Download again';
        }
      } else if (!gone) {
        vspec.failed = true;
        titleIn.disabled = false;
        dirIn.disabled = false;
      }
    }

    async function vspecSwitch(q) {
      if (vspec.failed || pendingForce || vspec.closed) return;
      if (!vspec.started) {
        vspecStart(q);
        return;
      }
      if (vspec.inflight) {
        // record it — vspecStart applies the switch when its ack
        // lands (dropping it used to download the wrong quality
        // while the dialog showed the new one selected)
        vspec.pendingSelector = q;
        return;
      }
      if (vspec.id && vspec.selector !== q.selector) {
        const old = vspec.id;
        vspec.id = null;
        vspec.selector = null;
        titleIn.disabled = false;
        dirIn.disabled = false;
        try {
          await chrome.runtime.sendMessage({ action: 'ndm-cancel-entry', entry: old });
        } catch (e) {}
        if (!vspec.closed) vspecStart(q);
      }
    }
    async function triggerDownload(later) {
      if (!selectedOption) return;
      const chosenDir = dirIn.value.trim() || '~/Downloads';
      const chosenTitle = titleIn.value.trim() || fallbackTitle;
      const qLabel = selectedOption.label || '';
      const szLabel = selectedOption.size_text || (selectedOption.size_bytes ? fmtBytes(selectedOption.size_bytes) : '');
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
          kind: 'video_direct',
          url: targetUrl,
          pageUrl: location.href,
          title: chosenTitle,
          selector: selectedOption.selector,
          audio: !!selectedOption.audio,
          dir: chosenDir,
          later: !!later,
          total: Number(selectedOption.size_bytes) || 0,
          force: pendingForce,
        });
        ok = !!(resp && resp.ok);
      } catch (e) {
        ok = false;
      }

      if (ok) {
        pendingForce = false;
        closeBrowserModal();
        flashPill(
          pillBtn,
          pillContainer,
          true,
          later ? 'Paused in NDM ✓' : 'Downloading in NDM ✓'
        );
        showBrowserToast(chosenTitle, qLabel, szLabel, later);
      } else if (resp && typeof resp.error === 'string' && resp.error.startsWith('duplicate:')) {
        pendingForce = true;
        showError('This quality is already in your download list. Press "Download again" to save a second copy.');
        btnStart.disabled = false;
        btnLater.disabled = false;
        btnStart.textContent = '⬇ Download again';
      } else {
        showError((resp && resp.error) || 'NDM could not be started. Make sure the NDM desktop app is installed.');
        btnStart.disabled = false;
        btnLater.disabled = false;
        updateStartButtonLabel();
      }
    }

    btnStart.addEventListener('click', () => {
      if (vspec.id) {
        dismissVideoKeep();
      } else if (vspec.inflight) {
        vspec.pending = 'keep';
        btnStart.disabled = true;
        btnLater.disabled = true;
        btnStart.textContent = 'Starting in NDM…';
      } else {
        triggerDownload(false);
      }
    });
    btnLater.addEventListener('click', () => {
      if (vspec.id) {
        dismissVideoLater();
      } else if (vspec.inflight) {
        vspec.pending = 'later';
        btnStart.disabled = true;
        btnLater.disabled = true;
      } else {
        triggerDownload(true);
      }
    });
    btnAll.addEventListener('click', async () => {
      if (!allQualities.length) return;
      btnAll.disabled = true;
      btnAll.textContent = 'Sending all…';
      let sent = 0;
      let skipped = 0;
      for (const q of allQualities) {
        try {
          const r = await chrome.runtime.sendMessage({
            action: 'ndm-download',
            kind: 'video_direct',
            url: targetUrl,
            pageUrl: location.href,
            title: titleIn.value.trim() || fallbackTitle,
            selector: q.selector,
            audio: !!q.audio,
            dir: dirIn.value.trim() || '~/Downloads',
            later: false,
            total: Number(q.size_bytes) || 0,
            force: false,
          });
          if (r && r.ok) sent++;
          else skipped++;
        } catch (e) {
          skipped++;
        }
      }
      vspec.closed = true;
      closeBrowserModal();
      flashPill(pillBtn, pillContainer, true, `Download all: ${sent} sent ✓`);
      showBrowserToast(
        titleIn.value.trim() || fallbackTitle,
        `Download all — ${sent} qualities sent to NDM${skipped ? `, ${skipped} skipped` : ''}`,
        '',
        false
      );
    });

    // 1. Check instant cache (0ms on YouTube or pre-probed pages!)
    const cached = localProbeCache.get(targetUrl) || localProbeCache.get(canonicalVideoKey(location.href));
    if (cached && (cached.options || cached.qualities || []).length) {
      applyVideoInfo(cached);
      return;
    }

    // 2. Listen for MAIN-world page-hook response if it arrives within milliseconds
    onInstantInfoCallback = info => {
      if (myToken !== activeProbeToken || renderedAlready) return;
      applyVideoInfo(info);
    };

    // 3. Otherwise await in-flight or fresh native probe
    try {
      const res = await fetchOrReuseProbe(targetUrl);
      if (myToken !== activeProbeToken || renderedAlready) return;
      if (res && res.ok && res.info) {
        applyVideoInfo(res.info);
      } else {
        renderQualities([]);
        if (res && res.error) {
          showError(res.error + ' — You can still start with automatic best quality below.');
        }
      }
    } catch (e) {
      if (myToken !== activeProbeToken || renderedAlready) return;
      renderQualities([]);
      showError('Could not probe exact sizes — you can still start download below.');
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
        <div class="ndm-f-summary">
          <span class="ndm-q-badge video ndm-f-cat">${escapeHtml(currentCat)}</span>
          <span class="ndm-f-size">${currentBytes > 0 ? escapeHtml(fmtBytes(currentBytes)) : 'Checking size…'}</span>
          <span class="ndm-f-resume">Checking…</span>
        </div>
        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">Save file as</label>
          <input class="ndm-m-input ndm-f-name" type="text" spellcheck="false" />
        </div>
        <div class="ndm-m-folder-row">
          <label class="ndm-m-lbl">Save to folder</label>
          <input class="ndm-m-input ndm-f-dir" type="text" placeholder="~/Downloads" />
        </div>
        <div class="ndm-m-status" style="display:none"></div>
      </div>
      <div class="ndm-m-foot">
        <button class="ndm-btn ndm-btn-ghost ndm-btn-app" type="button" title="Open this download in the NDM desktop app window">Open in NDM App</button>
        <div class="ndm-m-foot-right">
          <button class="ndm-btn ndm-btn-secondary ndm-btn-cancel" type="button">Cancel</button>
          <button class="ndm-btn ndm-btn-secondary ndm-btn-later" type="button">Download Later</button>
          <button class="ndm-btn ndm-btn-primary ndm-btn-start" type="button">⬇ Start Download${currentBytes > 0 ? ' (' + escapeHtml(fmtBytes(currentBytes)) + ')' : ''}</button>
        </div>
      </div>
    `;

    backdrop.appendChild(dialog);
    shadowRoot.appendChild(backdrop);
    modalEl = backdrop;
    syncHostParent();
    panelHost.style.display = 'block';

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
        dismissAsCancel();
      } else if (e.key === 'Enter' && !btnStart.disabled) {
        e.preventDefault();
        btnStart.click();
      }
    });

    btnClose.addEventListener('click', () => dismissAsCancel());
    btnCancel.addEventListener('click', () => dismissAsCancel());
    activeModalDismiss = dismissAsCancel;
    // the file dialog's backdrop behaves like the video dialog's:
    // an outside click is a Cancel, not a shrug
    backdrop.addEventListener('click', e => {
      if (e.target === backdrop) dismissAsCancel();
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
        btnStart.textContent = currentBytes > 0
          ? `⬇ Start Download (${fmtBytes(currentBytes)})`
          : '⬇ Start Download';
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

    function dismissAsKeep() {
      spec.closed = true;
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
      spec.closed = true;
      if (spec.timer) clearTimeout(spec.timer);
      if (spec.owned && spec.id) {
        specControl('ndm-cancel-entry');
      } else if (spec.inflight) {
        spec.pending = 'cancel';
      }
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
      if (spec.dupe) {
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
          if (!spec.id) btnStart.textContent = `⬇ Start Download (${fmtBytes(res.total)})`;
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

  function flashPill(btn, pill, ok, text) {
    if (!btn || !pill) return;
    pill.classList.toggle('sent', ok);
    pill.classList.toggle('error', !ok);
    btn.textContent = text;
    place();
    setTimeout(() => {
      pill.classList.remove('sent', 'error');
      btn.innerHTML = '<span>Download with NDM</span><span class="caret">▾</span>';
      btn.disabled = false;
      place();
    }, 2800);
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

  function refresh() {
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
  // IDM's escape hatches: hold Del (or Ins) while clicking to leave
  // one link to the browser; per-site exception list; per-tab off.
  let ndmPreventHeld = false;
  let ndmSiteExcepted = false;
  let ndmTabDisabled = false;
  addEventListener('keydown', e => {
    if (e.key === 'Delete' || e.key === 'Insert') ndmPreventHeld = true;
  }, true);
  addEventListener('keyup', e => {
    if (e.key === 'Delete' || e.key === 'Insert') ndmPreventHeld = false;
  }, true);
  addEventListener('blur', () => { ndmPreventHeld = false; });
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
      if (area !== 'local') return;
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
      if (ndmPreventHeld || ndmSiteExcepted || ndmTabDisabled) return;
      if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
      const target = e.target && e.target.closest ? e.target.closest('a[href]') : null;
      if (!target || !isDirectDownloadLink(target)) return;
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
    if (msg.action === 'ndm-open-video-modal') {
      dismissed = false;
      if (!panelHost || !panelHost.isConnected) buildPanel();
      const btn = cardEl ? cardEl.querySelector('.label') : null;
      // answer only once the open attempt has settled, and only
      // claim success if a modal actually exists — the old instant
      // ok:true left callers with no fallback when the open failed
      openBrowserModal(btn, cardEl)
        .then(() => sendResponse({ ok: !!(modalOpen && modalEl) }))
        .catch(() => sendResponse({ ok: false }));
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
