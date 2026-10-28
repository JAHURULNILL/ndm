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
    btn.addEventListener('click', async () => {
      if (btn.disabled) return;
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
        // green only on the APP's own confirmation (via the host's
        // ack wait); queued-but-unsurfaced is its own honest state
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
    });

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
    document.documentElement.appendChild(panelHost);
    place();
    requestAnimationFrame(() => pill.classList.add('on'));
  }

  function removePanel() {
    if (panelHost) {
      panelHost.remove();
      panelHost = null;
      cardEl = null;
      anchor = null;
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
