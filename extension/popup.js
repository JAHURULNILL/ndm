'use strict';
/* NDM popup: connection status, interception controls (global,
   per-tab, per-site — IDM's escape hatches), one-click video modal
   trigger. The video-host patterns live only in the background; the
   popup asks it to classify the tab URL instead of keeping a copy
   that can drift. */

async function init() {
  const statusBox = document.getElementById('statusBox');
  const chk = document.getElementById('chkIntercept');
  const chkTabOff = document.getElementById('chkTabOff');
  const btnSite = document.getElementById('btnSite');
  const btn = document.getElementById('btnVideo');

  let bridgeOk = false;
  try {
    // Via the background worker, so the result also refreshes the
    // shared host status that gates interception.
    const r = await chrome.runtime.sendMessage({ action: 'ndm-host-ping' });
    bridgeOk = !!(r && r.ok);
  } catch (e) {}
  if (bridgeOk) {
    statusBox.className = 'status ok';
    statusBox.textContent = 'Connected to NDM ✓';
  } else {
    statusBox.className = 'status bad';
    statusBox.innerHTML = 'NDM bridge not found.'
      + '<div class="hint">Is NDM installed and the bridge set up?</div>';
  }

  let stored = {};
  try {
    stored = await chrome.storage.local.get(['ndmIntercept', 'ndmTabOff', 'ndmSiteExceptions']);
  } catch (e) {}
  chk.checked = stored.ndmIntercept !== false;
  chk.addEventListener('change', () => {
    chrome.storage.local.set({ ndmIntercept: chk.checked });
  });

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab || !tab.url || !/^https?:\/\//i.test(tab.url)) return;
  const host = (() => {
    try {
      return new URL(tab.url).hostname.toLowerCase();
    } catch (e) {
      return '';
    }
  })();

  // Per-tab pause (link-click layer): stored per tab id.
  const tabOff = stored.ndmTabOff || {};
  chkTabOff.checked = !!(tab.id != null && tabOff[tab.id]);
  chkTabOff.addEventListener('change', async () => {
    const cur = (await chrome.storage.local.get({ ndmTabOff: {} })).ndmTabOff || {};
    if (tab.id != null) {
      if (chkTabOff.checked) cur[tab.id] = true;
      else delete cur[tab.id];
    }
    await chrome.storage.local.set({ ndmTabOff: cur });
  });

  // Per-site exception list (both interception layers).
  const exceptions = Array.isArray(stored.ndmSiteExceptions) ? stored.ndmSiteExceptions : [];
  const renderSiteBtn = list => {
    const excepted = host && list.includes(host);
    btnSite.disabled = !host;
    btnSite.textContent = excepted
      ? `✓ NDM never intercepts on ${host} — undo`
      : `Never intercept on ${host || 'this site'}`;
    btnSite.dataset.excepted = excepted ? '1' : '';
  };
  renderSiteBtn(exceptions);
  btnSite.addEventListener('click', async () => {
    const cur = (await chrome.storage.local.get({ ndmSiteExceptions: [] })).ndmSiteExceptions || [];
    const next = btnSite.dataset.excepted
      ? cur.filter(h => h !== host)
      : [...new Set([...cur, host])];
    await chrome.storage.local.set({ ndmSiteExceptions: next });
    renderSiteBtn(next);
  });

  // The video button is meaningful only when the bridge is alive;
  // it used to stay enabled with "NDM bridge not found" right above
  // it, and its click then silently failed.
  if (!bridgeOk) {
    btn.disabled = true;
    btn.textContent = 'NDM is not reachable — open the app';
    return;
  }
  let isVideo = false;
  try {
    const c = await chrome.runtime.sendMessage({ action: 'ndm-classify-url', url: tab.url });
    isVideo = !!(c && c.video);
  } catch (e) {}
  btn.disabled = false;
  btn.textContent = isVideo
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

init().catch(e => console.warn('NDM popup failed:', e));
