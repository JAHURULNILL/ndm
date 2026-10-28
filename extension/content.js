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

  function closeBrowserModal() {
    activeProbeToken++;
    modalOpen = false;
    if (modalEl) {
      modalEl.remove();
      modalEl = null;
    }
    place();
  }

  async function openBrowserModal(pillBtn, pillContainer) {
    if (!shadowRoot) return;
    closeBrowserModal();
    modalOpen = true;
    const myToken = ++activeProbeToken;

    const currentAnchor = anchor || pickAnchor();
    const targetUrl = await resolveVideoUrl(currentAnchor);
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
      if (e.target === backdrop) closeBrowserModal();
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
    const btnStart = dialog.querySelector('.ndm-btn-start');

    titleIn.value = fallbackTitle;
    dirIn.value = savedDir;

    // Prevent host page keyboard shortcuts (e.g. YouTube 'k', 'f', 'm', Space) from firing while typing in modal
    backdrop.addEventListener('keydown', e => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        e.preventDefault();
        closeBrowserModal();
      } else if (e.key === 'Enter' && !btnStart.disabled) {
        e.preventDefault();
        triggerDownload(false);
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

    btnClose.addEventListener('click', closeBrowserModal);
    btnCancel.addEventListener('click', closeBrowserModal);

    // Fallback: open directly in NDM app's own video dialog
    btnApp.addEventListener('click', async () => {
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

      // Default select first specific HD/standard resolution (e.g. 1080p or 720p) or first item
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
        });
        row.addEventListener('dblclick', () => {
          selectedOption = q;
          triggerDownload(false);
        });
        qListEl.appendChild(row);
      }

      btnStart.disabled = false;
      btnLater.disabled = false;
      updateStartButtonLabel();
    }

    function updateStartButtonLabel() {
      if (!selectedOption) return;
      const sz = selectedOption.size_text || (selectedOption.size_bytes ? fmtBytes(selectedOption.size_bytes) : '');
      btnStart.textContent = sz
        ? `⬇ Start Download (${sz})`
        : '⬇ Start Download';
    }

    async function triggerDownload(later) {
      if (!selectedOption) return;
      const chosenDir = dirIn.value.trim() || '~/Downloads';
      const chosenTitle = titleIn.value.trim() || fallbackTitle;
      try {
        chrome.storage.local.set({ ndmSaveDir: chosenDir }).catch(() => {});
      } catch (e) {}

      btnStart.disabled = true;
      btnLater.disabled = true;
      btnStart.textContent = later ? 'Queueing in NDM…' : 'Starting in NDM…';

      let ok = false;
      try {
        const resp = await chrome.runtime.sendMessage({
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
        });
        ok = !!(resp && resp.ok);
      } catch (e) {
        ok = false;
      }

      if (ok) {
        closeBrowserModal();
        flashPill(
          pillBtn,
          pillContainer,
          true,
          later ? 'Paused in NDM ✓' : 'Downloading in NDM ✓'
        );
      } else {
        showError('NDM could not be started. Make sure the NDM desktop app is installed.');
        btnStart.disabled = false;
        btnLater.disabled = false;
        updateStartButtonLabel();
      }
    }

    btnStart.addEventListener('click', () => triggerDownload(false));
    btnLater.addEventListener('click', () => triggerDownload(true));

    // Probe the video qualities + sizes via ndm-host
    try {
      const res = await chrome.runtime.sendMessage({
        action: 'ndm-probe',
        url: targetUrl,
        pageUrl: location.href,
      });
      if (myToken !== activeProbeToken) return;
      if (res && res.ok && res.info) {
        const info = res.info;
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
      } else {
        // Even if probe returned an error, provide default Best MP4 + Audio MP3 options
        // so the user can still download immediately with 1 click!
        renderQualities([]);
        if (res && res.error) {
          showError(res.error + ' — You can still start with automatic best quality below.');
        }
      }
    } catch (e) {
      if (myToken !== activeProbeToken) return;
      renderQualities([]);
      showError('Could not probe exact sizes — you can still start download below.');
    }
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

      /* In-Browser IDM Video Quality Modal */
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
    closeBrowserModal();
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
    if (videoDetected() || modalOpen) {
      if (!panelHost || !panelHost.isConnected) buildPanel();
      anchor = pickAnchor();
      place();
    } else {
      removePanel();
    }
  }

  // Listen for request from extension popup ("Download Video on This Page")
  chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (msg && msg.action === 'ndm-open-video-modal') {
      dismissed = false;
      if (!panelHost || !panelHost.isConnected) buildPanel();
      const btn = cardEl ? cardEl.querySelector('.label') : null;
      openBrowserModal(btn, cardEl);
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
      closeBrowserModal();
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
