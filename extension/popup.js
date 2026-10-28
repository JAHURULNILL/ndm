'use strict';
/* NDM popup: connection status, interception toggle, one-click video modal trigger. */

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

async function init() {
  const statusBox = document.getElementById('statusBox');
  const chk = document.getElementById('chkIntercept');
  const btn = document.getElementById('btnVideo');

  let stored = {};
  try {
    stored = await chrome.storage.local.get(['ndmIntercept']);
  } catch (e) {}
  try {
    await chrome.runtime.sendNativeMessage('com.ndm.host', { type: 'ping' });
    statusBox.className = 'status ok';
    statusBox.textContent = 'Connected to NDM ✓';
  } catch (e) {
    statusBox.className = 'status bad';
    statusBox.innerHTML = 'NDM bridge not found.'
      + '<div class="hint">Is NDM installed and the bridge set up?</div>';
  }

  chk.checked = stored.ndmIntercept !== false;
  chk.addEventListener('change', () => {
    chrome.storage.local.set({ ndmIntercept: chk.checked });
  });

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab && tab.url && /^https?:\/\//i.test(tab.url)) {
    btn.disabled = false;
    btn.textContent = isVideoPageUrl(tab.url)
      ? 'Choose Quality & Download Video'
      : 'Download Video on This Page';
    btn.addEventListener('click', async () => {
      try {
        const opened = await chrome.tabs.sendMessage(tab.id, {
          action: 'ndm-open-video-modal',
        });
        if (opened && opened.ok) {
          window.close();
          return;
        }
      } catch (e) {}
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
      } catch (e) {}
      if (ok) {
        btn.classList.add('sent');
        btn.textContent = 'Opened in NDM ✓';
        setTimeout(() => window.close(), 700);
      } else {
        btn.textContent = 'NDM is not reachable — open the app';
      }
    });
  }
}

init().catch(e => console.warn('NDM popup failed:', e));
