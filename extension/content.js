'use strict';
/* NDM — Nill Download Manager: content script.
 *
 * When a page looks like it contains a downloadable video, show one
 * small glass pill — "⬇ Download with NDM" — attached to the video
 * player itself, at its top-right corner, the way IDM parks its panel
 * on the player. Clicking it asks the background worker to hand the
 * page URL to the NDM desktop app via the native messaging host.
 *
 * The panel lives in a shadow root so page CSS can neither break it nor be
 * broken by it. DRM streaming sites are excluded by design: no panel there.
 */

(() => {
  // Streaming services whose videos are DRM-protected: never show a panel.
  const DRM_HOSTS = [
    'netflix.com',
    'primevideo.com',
    'disneyplus.com',
    'hotstar.com',
  ];

  const host = location.hostname.toLowerCase();
  if (DRM_HOSTS.some(d => host === d || host.endsWith('.' + d))) return;

  let panelHost = null;   // shadow-DOM host element, once created
  let cardEl = null;      // the pill element inside the shadow root
  let anchor = null;      // the <video> the pill is attached to
  let dismissed = false;  // user closed the panel on this page
  // the IDM-style download card that opens under the pill: thumbnail,
  // title and the quality list live IN the page, so the user never
  // has to leave the video to start a download
  let pillBtnEl = null;   // the pill's label button (classic hand-off feedback)
  let panelEl = null;     // the card element inside the shadow root
  let panelBody = null;   // the card's swappable body region
  let pThumbEl = null;
  let pTitleEl = null;
  let pSubEl = null;
  let panelOpen = false;
  let probeData = null;   // the app's last probe answer for this page

  function isYouTubeVideo() {
    return (host === 'youtube.com' || host.endsWith('.youtube.com'))
      && (location.pathname.startsWith('/watch')
        || location.pathname.startsWith('/shorts/'));
  }

  function isFacebookVideo() {
    return (host === 'facebook.com' || host.endsWith('.facebook.com'))
      && (location.pathname.includes('/videos/')
        || location.pathname.startsWith('/watch')
        || location.pathname.startsWith('/reel/'));
  }

  function hasVideoElement() {
    for (const v of document.querySelectorAll('video')) {
      const src = v.currentSrc || v.src || '';
      if (src && !src.startsWith('blob:')) return true;
      const s = v.querySelector('source[src]');
      if (s && !(s.getAttribute('src') || '').startsWith('blob:')) return true;
      // No size-only fallback: hero/background <video> elements on ordinary
      // pages are not downloadable videos, and blob: players on unknown
      // sites cannot be resolved from the element anyway. Known video
      // pages (YouTube/Facebook) are detected by URL above.
    }
    return false;
  }

  function isFacebookPageWithVideo() {
    // Facebook's player streams via blob: URLs, so hasVideoElement()
    // cannot see it — but on facebook.com the PAGE URL is what yt-dlp
    // resolves, so any <video> element is enough: feed, groups, profiles
    // and private posts included. A login wall shows no player at all.
    return (host === 'facebook.com' || host.endsWith('.facebook.com'))
      && document.querySelector('video') !== null;
  }

  function videoDetected() {
    return isYouTubeVideo() || isFacebookVideo() || isFacebookPageWithVideo()
      || hasVideoElement();
  }

  // The pill rides on the biggest visible video on the page — on
  // YouTube that is the player, on Facebook the feed/post player the
  // user is looking at. Recomputed on every refresh and placement.
  function pickAnchor() {
    let best = null;
    let bestArea = 0;
    for (const v of document.querySelectorAll('video')) {
      const r = v.getBoundingClientRect();
      if (r.width < 160 || r.height < 90) continue;
      const visW = Math.max(0, Math.min(r.right, innerWidth) - Math.max(r.left, 0));
      const visH = Math.max(0, Math.min(r.bottom, innerHeight) - Math.max(r.top, 0));
      const area = visW * visH;
      if (area > bestArea) { best = v; bestArea = area; }
    }
    return best;
  }

  // Park the pill at the anchor player's top-right corner, inside the
  // player. Called on refresh, on scroll/resize and on a slow tick, so
  // the pill follows the player — including into fullscreen, where the
  // player's rect simply becomes the screen.
  function place() {
    if (!panelHost || !cardEl) return;
    if (!anchor || !anchor.isConnected) anchor = pickAnchor();
    if (!anchor) { panelHost.style.display = 'none'; return; }
    const r = anchor.getBoundingClientRect();
    if (r.width < 160 || r.bottom < 48 || r.top > innerHeight - 24) {
      panelHost.style.display = 'none';
      return;
    }
    panelHost.style.display = 'block';
    const w = cardEl.offsetWidth || 196;
    cardEl.style.left = Math.max(4, r.right - w - 12) + 'px';
    cardEl.style.top = Math.max(4, r.top + 12) + 'px';
    if (panelEl && panelOpen) {
      panelEl.style.left = Math.max(4, r.right - 336 - 12) + 'px';
      panelEl.style.top = (r.top + 12 + (cardEl.offsetHeight || 32) + 8) + 'px';
    }
  }

  function buildPanel() {
    if (panelHost) panelHost.remove(); // drop a detached/stale host first
    panelHost = document.createElement('div');
    panelHost.id = 'ndm-panel-host';
    panelHost.style.cssText =
      'position:fixed;inset:0;pointer-events:none;z-index:2147483647;';
    const shadow = panelHost.attachShadow({ mode: 'closed' });

    const style = document.createElement('style');
    style.textContent = `
      .pill {
        position: absolute; pointer-events: auto;
        display: flex; align-items: center; justify-content: center;
        min-width: 178px; box-sizing: border-box; gap: 7px;
        background: rgba(13, 22, 38, .78); color: #fff;
        -webkit-backdrop-filter: blur(10px) saturate(1.35);
        backdrop-filter: blur(10px) saturate(1.35);
        border: 1px solid rgba(255, 255, 255, .28);
        border-radius: 999px; padding: 4px 11px 4px 4px;
        box-shadow: 0 8px 24px rgba(2, 10, 24, .38);
        font-family: "Segoe UI", Roboto, Arial, sans-serif;
        font-size: 13px; font-weight: 600; white-space: nowrap;
        opacity: 0; transition: opacity .18s ease;
      }
      .pill.on { opacity: 1; }
      .pill:hover { background: rgba(13, 22, 38, .9); }
      .chip {
        width: 23px; height: 23px; border-radius: 50%; flex: none;
        display: flex; align-items: center; justify-content: center;
        background: linear-gradient(180deg, #55c1ff, #1f6fd0);
        color: #fff; font-size: 13px; line-height: 1;
        box-shadow: inset 0 1px 0 rgba(255,255,255,.5);
      }
      .pill.sent { background: rgba(18, 96, 48, .86); }
      .pill.queued { background: rgba(146, 104, 26, .94); }
      .pill.error { background: rgba(146, 30, 30, .88); }
      .label { cursor: pointer; background: none; border: none; color: inherit;
        font: inherit; padding: 0; }
      .close {
        background: none; border: none; color: rgba(255,255,255,.75);
        cursor: pointer; font-size: 14px; line-height: 1; padding: 0 0 0 2px;
      }
      .close:hover { color: #fff; }
      .panel {
        position: absolute; width: 336px; box-sizing: border-box;
        pointer-events: auto; display: none;
        background: rgba(13, 22, 38, .92); color: #fff;
        -webkit-backdrop-filter: blur(14px) saturate(1.35);
        backdrop-filter: blur(14px) saturate(1.35);
        border: 1px solid rgba(255, 255, 255, .28);
        border-radius: 14px; box-shadow: 0 16px 40px rgba(2, 10, 24, .45);
        font-family: "Segoe UI", Roboto, Arial, sans-serif;
        overflow: hidden;
      }
      .panel.open { display: block; }
      .p-head { display: flex; gap: 10px; padding: 10px 10px 8px; align-items: flex-start; }
      .p-thumb { width: 104px; height: 59px; object-fit: cover; border-radius: 8px;
        flex: none; background: rgba(255,255,255,.08); }
      .p-meta { flex: 1; min-width: 0; }
      .p-title { font-size: 13px; font-weight: 600; line-height: 1.35;
        display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical;
        overflow: hidden; }
      .p-sub { font-size: 11.5px; color: rgba(255,255,255,.62); margin-top: 3px; }
      .p-close { background: none; border: none; color: rgba(255,255,255,.75);
        cursor: pointer; font-size: 15px; line-height: 1; padding: 0; flex: none; }
      .p-close:hover { color: #fff; }
      .p-body { padding: 2px 6px 8px; }
      .p-load { display: flex; align-items: center; gap: 9px;
        padding: 10px 8px 12px; font-size: 12.5px; color: rgba(255,255,255,.85); }
      .spin { width: 15px; height: 15px; flex: none; border-radius: 50%;
        border: 2px solid rgba(255,255,255,.25); border-top-color: #55c1ff;
        animation: ndmspin .8s linear infinite; }
      @keyframes ndmspin { to { transform: rotate(360deg); } }
      .p-list { max-height: 240px; overflow-y: auto; }
      .row { display: flex; align-items: baseline; justify-content: space-between;
        gap: 10px; width: 100%; box-sizing: border-box;
        background: none; border: none; color: #fff; font: inherit;
        text-align: left; cursor: pointer; padding: 7px 8px; border-radius: 8px; }
      .row:hover { background: rgba(255,255,255,.09); }
      .r-label { font-size: 13px; font-weight: 600; white-space: nowrap; }
      .r-note { font-size: 11.5px; color: rgba(255,255,255,.6); text-align: right;
        overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
      .p-note { margin: 4px 8px 2px; padding: 9px 10px; border-radius: 9px;
        font-size: 12.5px; line-height: 1.45; }
      .p-note.ok { background: rgba(24, 110, 55, .35); }
      .p-note.err { background: rgba(150, 35, 35, .35); }
      .p-hint { padding: 2px 10px 6px; font-size: 11.5px; color: rgba(255,255,255,.62); }
      .p-actions { display: flex; gap: 8px; padding: 6px 8px 4px; }
      .p-btn { flex: 1; background: rgba(255,255,255,.1);
        border: 1px solid rgba(255,255,255,.22); color: #fff; font: inherit;
        font-size: 12.5px; font-weight: 600; padding: 6px 0; border-radius: 8px;
        cursor: pointer; }
      .p-btn:hover { background: rgba(255,255,255,.18); }
      .p-link { display: block; margin: 4px auto 2px; background: none; border: none;
        color: rgba(255,255,255,.65); font: inherit; font-size: 11.5px;
        cursor: pointer; text-decoration: underline; }
      .p-link:hover { color: #fff; }
    `;

    const pill = document.createElement('div');
    pill.className = 'pill';

    const chip = document.createElement('span');
    chip.className = 'chip';
    chip.textContent = '⬇';

    const btn = document.createElement('button');
    btn.className = 'label';
    btn.textContent = 'Download with NDM';
    btn.title = 'Download this video with NDM — Nill Download Manager';
    // IDM's way: the click opens the download card right here on the
    // page (thumbnail + qualities); the app only enters the picture
    // when the user falls back to it from inside the card
    btn.addEventListener('click', () => { openPanel(); });
    pillBtnEl = btn;

    const close = document.createElement('button');
    close.className = 'close';
    close.textContent = '×';
    close.title = 'Hide';
    close.addEventListener('click', () => {
      dismissed = true;
      removePanel();
    });

    pill.appendChild(chip);
    pill.appendChild(btn);
    pill.appendChild(close);
    shadow.appendChild(style);
    shadow.appendChild(pill);
    cardEl = pill;

    const panel = document.createElement('div');
    panel.className = 'panel';
    const head = document.createElement('div');
    head.className = 'p-head';
    const thumb = document.createElement('img');
    thumb.className = 'p-thumb';
    thumb.alt = '';
    const meta = document.createElement('div');
    meta.className = 'p-meta';
    const ttl = document.createElement('div');
    ttl.className = 'p-title';
    const sub = document.createElement('div');
    sub.className = 'p-sub';
    sub.textContent = 'NDM — Nill Download Manager';
    meta.appendChild(ttl);
    meta.appendChild(sub);
    const pClose = document.createElement('button');
    pClose.className = 'p-close';
    pClose.textContent = '×';
    pClose.title = 'Close';
    pClose.addEventListener('click', () => closePanel());
    head.appendChild(thumb);
    head.appendChild(meta);
    head.appendChild(pClose);
    const body = document.createElement('div');
    body.className = 'p-body';
    panel.appendChild(head);
    panel.appendChild(body);
    shadow.appendChild(panel);
    panelEl = panel;
    panelBody = body;
    pThumbEl = thumb;
    pTitleEl = ttl;
    pSubEl = sub;
    document.documentElement.appendChild(panelHost);
    place();
    requestAnimationFrame(() => pill.classList.add('on'));
  }

  function fmtDur(sec) {
    if (!sec) return '';
    const s = Math.round(sec);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const ss = String(s % 60).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
  }

  function thumbUrl() {
    const yt = location.href.match(/[?&]v=([\w-]{11})/)
      || location.pathname.match(/\/shorts\/([\w-]{11})/);
    if (yt) return `https://i.ytimg.com/vi/${yt[1]}/hqdefault.jpg`;
    const og = document.querySelector('meta[property="og:image"]');
    return og ? og.content : '';
  }

  function pageTitle() {
    const og = document.querySelector('meta[property="og:title"]');
    const t = (og && og.content) || document.title || '';
    return t.replace(/\s*-\s*YouTube$/, '');
  }

  function clearBody() {
    while (panelBody && panelBody.firstChild) panelBody.firstChild.remove();
  }

  function setBodyLoading(text) {
    if (!panelBody) return;
    clearBody();
    const load = document.createElement('div');
    load.className = 'p-load';
    const spin = document.createElement('span');
    spin.className = 'spin';
    load.appendChild(spin);
    load.appendChild(document.createTextNode(text));
    panelBody.appendChild(load);
  }

  function setBodyNote(kind, text, hint) {
    if (!panelBody) return;
    clearBody();
    const note = document.createElement('div');
    note.className = 'p-note ' + kind;
    note.textContent = text;
    panelBody.appendChild(note);
    if (hint) {
      const h = document.createElement('div');
      h.className = 'p-hint';
      h.textContent = hint;
      panelBody.appendChild(h);
    }
  }

  function setBodyError(msg) {
    if (!panelBody) return;
    setBodyNote('err', msg);
    const actions = document.createElement('div');
    actions.className = 'p-actions';
    const retry = document.createElement('button');
    retry.className = 'p-btn';
    retry.textContent = 'Retry';
    retry.addEventListener('click', () => { probeData = null; openPanel(true); });
    const inApp = document.createElement('button');
    inApp.className = 'p-btn';
    inApp.textContent = 'Open in NDM app';
    inApp.addEventListener('click', () => classicHandoff());
    actions.appendChild(retry);
    actions.appendChild(inApp);
    panelBody.appendChild(actions);
  }

  function setBodyAlready(status) {
    const names = {
      finished: 'Finished', downloading: 'Downloading now',
      paused: 'Paused', queued: 'Queued', 'in list': '',
    };
    const suffix = names[status] ? ' — ' + names[status] : '';
    setBodyNote('ok', 'Already in your NDM list' + suffix,
      'To download it again, remove the entry in the NDM app first.');
  }

  function setBodyOptions(options) {
    if (!panelBody) return;
    clearBody();
    const list = document.createElement('div');
    list.className = 'p-list';
    for (const opt of options) {
      const row = document.createElement('button');
      row.className = 'row';
      const label = document.createElement('span');
      label.className = 'r-label';
      label.textContent = (opt.audio ? '♪ ' : '') + (opt.label || 'Quality');
      const note = document.createElement('span');
      note.className = 'r-note';
      note.textContent = opt.note || '';
      row.appendChild(label);
      row.appendChild(note);
      row.addEventListener('click', () => startWith(opt));
      list.appendChild(row);
    }
    panelBody.appendChild(list);
    const link = document.createElement('button');
    link.className = 'p-link';
    link.textContent = 'Open in NDM app instead';
    link.addEventListener('click', () => classicHandoff());
    panelBody.appendChild(link);
  }

  async function openPanel(retry) {
    if (!panelEl) return;
    if (panelOpen && !retry) { closePanel(); return; }
    panelOpen = true;
    probeData = null;
    panelEl.classList.add('open');
    pTitleEl.textContent = pageTitle();
    const th = thumbUrl();
    if (th) { pThumbEl.src = th; pThumbEl.style.display = ''; }
    else { pThumbEl.style.display = 'none'; pThumbEl.removeAttribute('src'); }
    pSubEl.textContent = 'NDM — Nill Download Manager';
    setBodyLoading('Getting qualities from NDM…');
    place();
    let resp = null;
    try {
      resp = await chrome.runtime.sendMessage({
        action: 'ndm-panel-probe',
        url: location.href,
        pageUrl: location.href,
        title: pageTitle(),
      });
    } catch (e) { /* extension context can vanish on reload */ }
    if (!panelOpen) return; // closed while the app was answering
    if (!resp || !resp.ok) {
      setBodyError((resp && resp.error) || 'NDM is not reachable — is the app installed and running?');
      return;
    }
    const r = resp.result || {};
    if (r.error) { setBodyError(r.error); return; }
    probeData = r;
    if (r.title) pTitleEl.textContent = r.title;
    const dur = fmtDur(r.duration);
    pSubEl.textContent = dur ? dur + ' • NDM' : 'NDM';
    if (r.already) setBodyAlready(r.already);
    else if (Array.isArray(r.options) && r.options.length) setBodyOptions(r.options);
    else setBodyError('NDM found no downloadable qualities for this page.');
  }

  async function startWith(opt) {
    if (!probeData) return;
    setBodyLoading('Starting the download…');
    let resp = null;
    try {
      resp = await chrome.runtime.sendMessage({
        action: 'ndm-panel-start',
        url: location.href,
        pageUrl: location.href,
        title: probeData.title || pageTitle(),
        selector: opt.selector,
        audio: !!opt.audio,
        hasAudio: !!probeData.has_audio,
      });
    } catch (e) { /* extension context can vanish on reload */ }
    if (!panelOpen) return;
    const r = resp && resp.result;
    if (resp && resp.ok && r && r.started) {
      setBodyNote('ok', 'Downloading in NDM ✓', 'Progress lives in the NDM app.');
      setTimeout(() => closePanel(), 2600);
      return;
    }
    const reason = (r && r.reason) || (resp && resp.error) || 'NDM did not answer.';
    if (/already in the download list/.test(reason)) setBodyAlready('in list');
    else setBodyError(reason);
  }

  function closePanel() {
    panelOpen = false;
    probeData = null;
    if (panelEl) panelEl.classList.remove('open');
  }

  // The pre-panel behaviour, kept as the card's fallback: hand the
  // page to the app and let its dialog take over. The pill itself
  // carries the verdict (green only on the app's own confirmation).
  async function classicHandoff() {
    closePanel();
    const pill = cardEl;
    const btn = pillBtnEl;
    if (!pill || !btn || btn.disabled) return;
    btn.disabled = true;
    let ok = false;
    let picked = false;
    try {
      const resp = await chrome.runtime.sendMessage({
        action: 'ndm-download',
        kind: 'video',
        url: location.href,
        pageUrl: location.href,
        title: document.title,
      });
      ok = !!(resp && resp.ok);
      picked = ok && resp.picked !== false;
    } catch (e) { /* extension context can vanish on reload */ }
    pill.classList.toggle('sent', ok && picked);
    pill.classList.toggle('queued', ok && !picked);
    pill.classList.toggle('error', !ok);
    btn.textContent = !ok
      ? 'NDM is not reachable — open the app'
      : picked ? 'Sent to NDM ✓' : 'Queued — open NDM';
    place();
    setTimeout(() => {
      pill.classList.remove('sent', 'queued', 'error');
      btn.textContent = 'Download with NDM';
      btn.disabled = false;
      place();
    }, ok && picked ? 2600 : 5200);
  }

  function removePanel() {
    if (panelHost) {
      panelHost.remove();
      panelHost = null;
      cardEl = null;
      anchor = null;
      panelEl = null;
      panelBody = null;
      pillBtnEl = null;
      panelOpen = false;
      probeData = null;
    }
  }

  function refresh() {
    if (dismissed) return;
    if (videoDetected()) {
      // isConnected: SPA frameworks and page sanitizers can detach our
      // host node while panelHost still points at it — rebuild then,
      // or the panel silently never appears again on this page
      if (!panelHost || !panelHost.isConnected) buildPanel();
      anchor = pickAnchor();
      place();
    } else {
      removePanel();
    }
  }

  // Initial check + SPA navigation handling: YouTube and Facebook change
  // the URL without reloading, so re-check on an interval and whenever the
  // DOM changes substantially.
  refresh();
  let lastUrl = location.href;
  setInterval(() => {
    if (location.href !== lastUrl) {
      lastUrl = location.href;
      dismissed = false;
      closePanel(); // the open card belongs to the previous video
      refresh();
    } else {
      place();
    }
  }, 800);
  addEventListener('scroll', place, { passive: true, capture: true });
  addEventListener('resize', place);
  // Debounced: mutation-heavy pages (YouTube, Facebook) would otherwise
  // trigger a full DOM scan on every mutation batch.
  let refreshPending = false;
  function scheduleRefresh() {
    if (refreshPending) return;
    refreshPending = true;
    setTimeout(() => { refreshPending = false; refresh(); }, 250);
  }
  const observer = new MutationObserver(scheduleRefresh);
  observer.observe(document.documentElement, { childList: true, subtree: true });
})();
