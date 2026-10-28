'use strict';
/* NDM popup: connection status, interception toggle, one-click video send. */

// Keep in sync with VIDEO_HOST_PATTERNS in background.js.
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
  // host + path only — see background.js for why the raw URL is unsafe
  let s;
  try {
    const u = new URL(url);
    s = u.hostname + u.pathname;
  } catch (e) {
    return false;
  }
  return VIDEO_HOST_PATTERNS.some(re => re.test(s));
}

async function init() {
  const statusBox = document.getElementById('statusBox');
  const chk = document.getElementById('chkIntercept');
  const btn = document.getElementById('btnVideo');

  // --- connection status: probed live on open — a stored "ok" from
  // an old send goes stale if the app/bridge is later removed. The
  // host answers ANY message (even an error reply proves it exists);
  // only a missing/broken bridge rejects. ---
  let stored = {};
  try {
    stored = await chrome.storage.local.get(['ndmIntercept']);
  } catch (e) { /* storage unavailable: fall back to defaults below */ }
  try {
    await chrome.runtime.sendNativeMessage('com.ndm.host', { type: 'ping' });
    statusBox.className = 'status ok';
    statusBox.textContent = 'Connected to NDM ✓';
  } catch (e) {
    statusBox.className = 'status bad';
    statusBox.innerHTML = 'NDM bridge not found.'
      + '<div class="hint">Is NDM installed and the bridge set up?</div>';
  }

  // --- interception toggle (default on) ---
  chk.checked = stored.ndmIntercept !== false;
  chk.addEventListener('change', () => {
    chrome.storage.local.set({ ndmIntercept: chk.checked });
  });

  // --- "download current page video" button ---
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab && isVideoPageUrl(tab.url)) {
    btn.disabled = false;
    btn.addEventListener('click', async () => {
      let ok = false;
      try {
        const resp = await chrome.runtime.sendMessage({
          action: 'ndm-download',
          kind: 'video',
          url: tab.url,
          pageUrl: tab.url,
          title: tab.title || '',
        });
        ok = !!(resp && resp.ok);
      } catch (e) { /* ignore: reported below as unreachable */ }
      if (ok) {
        btn.classList.add('sent');
        btn.textContent = 'Sent to NDM ✓';
        setTimeout(() => window.close(), 700);
      } else {
        btn.textContent = 'NDM is not reachable — open the app';
      }
    });
  }
}

init().catch(e => console.warn('NDM popup failed:', e));
