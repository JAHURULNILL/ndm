'use strict';
/* NDM frontend — works against the Tauri backend via IPC, and falls back to a
   self-contained demo simulation when opened in a plain browser. */

const TAURI = typeof window !== 'undefined' && !!window.__TAURI__;
let invoke = null, listen = null;
if (TAURI) {
  invoke = window.__TAURI__.core.invoke;
  listen = window.__TAURI__.event.listen;
}

/* ---------- state ---------- */
const items = new Map();       // id -> DownloadInfo
const deletedIds = new Set();  // ids removed this session (tombstones)
let selectedId = null;
let currentCat = 'All Downloads';
let pulse = 0;                 // animation phase for active blocks

/* ---------- formatting ---------- */
const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];
function fmtSize(b) {
  if (!b && b !== 0) return '—';
  let i = 0, v = b;
  while (v >= 1024 && i < UNITS.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 2)} ${UNITS[i]}`;
}
const fmtRate = b => (b > 0 ? fmtSize(b) + '/s' : '—');
function fmtLeft(sec) {
  if (!isFinite(sec) || sec == null) return '—';
  sec = Math.max(0, Math.round(sec));
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
  return h ? `${h}h ${m}m` : m ? `${m}m ${s}s` : `${s}s`;
}
const pctOf = d => (d.total > 0 ? Math.min(100, (100 * d.done_bytes) / d.total) : 0);
const KNOWN_STATUS = ['downloading', 'paused', 'queued', 'finished', 'error', 'cancelled'];
const cap = s => s.charAt(0).toUpperCase() + s.slice(1);

/* ---------- DOM refs ---------- */
const $ = id => document.getElementById(id);
const rowsEl = $('rows'), detailEl = $('detail');

/* ---------- settings ---------- */
const DEFAULT_SETTINGS = {
  default_dir: '~/Downloads', max_connections: 32, speed_limit_bps: 0,
  max_simultaneous: 0, auto_resume: false, scheduler_enabled: false,
  sched_start: '22:00', sched_stop: '06:00', video_quality: 'best', proxy: '',
};
let settings = { ...DEFAULT_SETTINGS };

function syncLimiterLabel() {
  const bps = settings.speed_limit_bps || 0;
  $('limiterLabel').innerHTML = bps > 0
    ? `Speed Limiter: <b>${fmtSize(bps)}/s</b>`
    : 'Speed Limiter: <b>OFF</b>';
}

async function loadSettings() {
  if (TAURI) {
    try { settings = { ...DEFAULT_SETTINGS, ...(await invoke('get_settings')) }; }
    catch (e) { console.error('get_settings failed', e); }
  }
  $('inDir').value = settings.default_dir || '~/Downloads';
  $('vInDir').value = settings.default_dir || '~/Downloads';
  syncLimiterLabel();
}

async function saveSettings() {
  if (TAURI) {
    try { await invoke('set_settings', { settings }); }
    catch (e) { console.error('set_settings failed', e); }
  }
  syncLimiterLabel();
}

/* ---------- rendering ---------- */
let sortKey = 'date'; // View menu: name | date | size | status
function visibleItems() {
  const all = [...items.values()];
  let out = all;
  if (currentCat === 'Finished') out = all.filter(d => d.status === 'finished');
  else if (currentCat === 'Unfinished') out = all.filter(d => d.status !== 'finished');
  else if (currentCat !== 'All Downloads') out = all.filter(d => d.category === currentCat);
  const numId = id => parseInt(String(id).replace(/\D+/g, ''), 10) || 0;
  const by = {
    name: (a, b) => a.name.localeCompare(b.name),
    date: (a, b) => numId(a.id) - numId(b.id),
    size: (a, b) => (b.total || 0) - (a.total || 0),
    status: (a, b) => a.status.localeCompare(b.status),
  }[sortKey];
  return by ? [...out].sort(by) : out;
}

function render() {
  const list = visibleItems();
  rowsEl.innerHTML = '';
  for (const d of list) {
    const tr = document.createElement('tr');
    if (d.id === selectedId) tr.className = 'selected';
    const left = d.status === 'downloading' && d.speed_bps > 0 && d.total > 0
      ? (d.total - d.done_bytes) / d.speed_bps : null;
    // status is interpolated into HTML — pin it to the known vocabulary
    const st = KNOWN_STATUS.includes(d.status) ? d.status : 'error';
    tr.innerHTML = `
      <td class="name">${escapeHtml(d.name)}${d.status === 'downloading' && d.total === 0 ? '<div class="pbar ind"></div>' : `<div class="pbar"><i data-pct="${pctOf(d).toFixed(1)}"></i></div>`}</td>
      <td>${d.total > 0 || st === 'finished' ? fmtSize(d.total) : 'Unknown'}</td>
      <td class="st-${st}">${cap(st)}${st !== 'finished' && d.total > 0 ? ' ' + pctOf(d).toFixed(0) + '%' : ''}</td>
      <td>${st === 'finished' ? '—' : fmtLeft(left)}</td>
      <td>${st === 'downloading' ? fmtRate(d.speed_bps) : '—'}</td>
      <td>${escapeHtml(d.added || '')}</td>`;
    // Single-click ONLY selects the row in the full-height table (like IDM)
    tr.addEventListener('click', () => select(d.id, false));
    // Double-click opens a finished file, or opens the floating Download Status dialog for active/paused items (like IDM)
    tr.addEventListener('dblclick', () => {
      if (d.status === 'finished') {
        select(d.id, false);
        openSelected(false);
      } else {
        select(d.id, true);
      }
    });
    // Right-click opens the floating Download Status / Details dialog for any row
    tr.addEventListener('contextmenu', e => {
      e.preventDefault();
      select(d.id, true);
    });
    rowsEl.appendChild(tr);
  }
  // row progress fills go through the DOM, never the innerHTML
  // string: this WebKit drops percentage widths and var()-gradient
  // stops written into inline style attributes (measured live with
  // a DOM probe), while DOM-set widths — the detail bar's way — work
  rowsEl.querySelectorAll('.pbar i').forEach(el => {
    el.style.width = el.dataset.pct + '%';
  });
  $('emptyMsg').style.display = list.length ? 'none' : 'block';

  const all = [...items.values()];
  const dl = all.filter(d => d.status === 'downloading');
  $('sideStatus').textContent = `${all.length} files (${dl.length} downloading)`;
  $('totalSpeed').textContent = 'Total speed: ' + fmtRate(dl.reduce((a, d) => a + d.speed_bps, 0)).replace('—', '0 B/s');
  if (!detailEl.classList.contains('hidden')) {
    renderDetail();
  }
}

function renderDetail() {
  const d = items.get(selectedId);
  if (!d) {
    if ($('dTitle')) $('dTitle').textContent = 'Download Status';
    $('dUrl').textContent = $('dStatus').textContent = $('dSize').textContent = '—';
    $('dDone').textContent = $('dRate').textContent = $('dLeft').textContent = $('dResume').textContent = '—';
    $('dBarFill').style.width = '0%'; $('dBarText').textContent = '0.0%';
    $('dOpen').disabled = $('dOpenFolder').disabled = $('dRefresh').disabled = true;
    drawSegMap('');
    return;
  }
  if ($('dTitle')) {
    $('dTitle').textContent = d.name
      ? `${pctOf(d).toFixed(0)}% — ${d.name}`
      : 'Download Status';
  }
  $('dOpen').disabled = $('dOpenFolder').disabled = d.status === 'queued';
  $('dRefresh').disabled = d.status === 'downloading';
  document.querySelectorAll('input[name="compAct"]').forEach(r => {
    r.checked = r.value === (d.completion || '');
  });
  $('dUrl').textContent = d.url;
  $('dStatus').textContent = cap(d.status) + (d.error ? ' — ' + d.error : '');
  $('dSize').textContent = d.total > 0 || d.status === 'finished' ? fmtSize(d.total) : 'Unknown';
  $('dDone').textContent = d.total > 0
    ? `${fmtSize(d.done_bytes)} (${pctOf(d).toFixed(1)}%)`
    : `${fmtSize(d.done_bytes)} (size unknown)`;
  $('dRate').textContent = d.status === 'downloading' ? fmtRate(d.speed_bps) : '—';
  $('dLeft').textContent = d.status === 'downloading' && d.speed_bps > 0 && d.total > 0
    ? fmtLeft((d.total - d.done_bytes) / d.speed_bps) : '—';
  $('dResume').textContent = d.resume_capability || '—';
  const ind = d.status === 'downloading' && d.total === 0;
  $('dBarFill').classList.toggle('ind', ind);
  $('dBarFill').style.width = ind ? '' : pctOf(d).toFixed(1) + '%';
  $('dBarText').textContent = d.total > 0 ? pctOf(d).toFixed(1) + '%'
    : (d.status === 'downloading' ? fmtSize(d.done_bytes) : '—');
  drawSegMap(d.blocks || '');
}

/* Segment map: one cell per block char — '1' done, '2' active (pulsing), else pending.
   Colors come from the active theme's CSS variables. */
function drawSegMap(blocks) {
  const cv = $('segMap'), ctx = cv.getContext('2d');
  const W = cv.width = cv.clientWidth || 760;
  const H = cv.height = 86;
  ctx.clearRect(0, 0, W, H);
  if (!blocks) return;
  const css = getComputedStyle(document.documentElement);
  const cDone = css.getPropertyValue('--seg-done').trim() || '#3f9de0';
  const cActive = css.getPropertyValue('--seg-active').trim() || '#f5c542';
  const cActive2 = css.getPropertyValue('--seg-active2').trim() || '#c79a1d';
  const cPending = css.getPropertyValue('--seg-pending').trim() || '#c9d2da';
  const n = Math.min(blocks.length, 1024);
  const cols = Math.ceil(Math.sqrt(n * (W / H)));
  const rows = Math.ceil(n / cols);
  const cw = W / cols, ch = H / rows, gap = Math.min(2, cw * 0.18);
  for (let i = 0; i < n; i++) {
    const x = (i % cols) * cw, y = Math.floor(i / cols) * ch, c = blocks[i];
    if (c === '1') ctx.fillStyle = cDone;
    else if (c === '2') ctx.fillStyle = pulse % 2 ? cActive : cActive2;
    else ctx.fillStyle = cPending;
    ctx.fillRect(x + gap / 2, y + gap / 2, Math.max(1, cw - gap), Math.max(1, ch - gap));
  }
}

/* ---------- theme (light by default, choice persists) ---------- */
function syncThemeButton() {
  const dark = document.documentElement.dataset.theme === 'dark';
  $('themeToggle').textContent = dark ? '☀️ Light' : '🌙 Dark';
}
$('themeToggle').onclick = () => {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('ndm-theme', next); } catch (e) { /* private mode */ }
  syncThemeButton();
  if (!detailEl.classList.contains('hidden')) renderDetail();
};
syncThemeButton();

/* transient error/info line under the toolbar */
let flashTimer = null;
function toast(msg) {
  const el = $('flashMsg');
  el.textContent = msg;
  el.classList.remove('hidden');
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => el.classList.add('hidden'), 5000);
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

/* throttled re-render (progress events can arrive fast) */
let renderQueued = false;
function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  setTimeout(() => { renderQueued = false; render(); }, 200);
}

function select(id, openDetail) {
  selectedId = id;
  if (openDetail) {
    detailEl.classList.remove('hidden');
    requestAnimationFrame(() => renderDetail());
  }
  render();
}

/* ---------- actions ---------- */
async function actOnSelected(fn) {
  const d = items.get(selectedId);
  if (!d) return;
  if (TAURI) { try { await fn(d.id); } catch (e) { console.error(e); toast(String(e)); } }
  else fn(d.id);
}
$('btnPause').onclick = () => actOnSelected(id => TAURI ? invoke('pause_download', { id }) : demoPause(id));
$('btnResume').onclick = () => actOnSelected(id => TAURI ? invoke('resume_download', { id }) : demoResume(id));
$('btnDelete').onclick = async () => {
  const d = items.get(selectedId);
  if (!d) return;
  if (TAURI) {
    try { await invoke('remove_download', { id: d.id }); } catch (e) { console.error(e); toast(String(e)); return; }
    deletedIds.add(d.id); // a late progress emit must not resurrect the row
    items.delete(d.id);
    if (selectedId === d.id) selectedId = items.keys().next().value || null;
    if (!selectedId) detailEl.classList.add('hidden');
    render();
  } else demoRemove(d.id);
};
if ($('btnStatus')) {
  $('btnStatus').onclick = () => {
    if (!selectedId && items.size > 0) selectedId = items.keys().next().value;
    if (!selectedId) { toast('Select a download first'); return; }
    select(selectedId, true);
  };
}
$('dPause').onclick = $('btnPause').onclick;
$('dResumeBtn').onclick = $('btnResume').onclick;
$('dHide').onclick = () => detailEl.classList.add('hidden');
if ($('dModalClose')) $('dModalClose').onclick = () => detailEl.classList.add('hidden');
detailEl.addEventListener('click', e => { if (e.target === detailEl) detailEl.classList.add('hidden'); });
$('btnStopAll').onclick = async () => {
  for (const d of items.values()) {
    if (d.status !== 'downloading') continue;
    if (TAURI) { try { await invoke('pause_download', { id: d.id }); } catch (e) { /* keep going */ } }
    else demoPause(d.id);
  }
  scheduleRender();
};

document.querySelectorAll('#catList li').forEach(li => {
  li.onclick = () => {
    document.querySelectorAll('#catList li').forEach(x => x.classList.remove('active'));
    li.classList.add('active');
    currentCat = li.dataset.cat;
    render();
  };
});

/* ---------- Add URL modal ---------- */
const modal = $('modal');
$('btnAdd').onclick = () => openFileDialog(null);
$('modalClose').onclick = $('btnCancel').onclick = () => closeFileDialog();
modal.addEventListener('click', e => { if (e.target === modal) closeFileDialog(); });

/* File downloads caught in the browser are presented in this same
   dialog, IDM-style: one at a time, prefilled with the address, the
   browser's suggested file name and the page it came from. The
   captured session token is handed to the new entry on Start/Later
   (the entry then owns it) and discarded on Cancel, so a session the
   user did not use never lingers on disk. */
const fileLinkQueue = [];
let presentedFile = null; // { url, cookies, referer } while shown
function queueFileLink(payload) {
  const url = payload && payload.url;
  if (!url) return;
  if (presentedFile && presentedFile.url === url) return;
  if (fileLinkQueue.some(q => q.url === url)) return;
  fileLinkQueue.push(payload);
  pumpFileLinks();
}
function pumpFileLinks() {
  if (!modal.classList.contains('hidden')) return; // dialog is busy
  const next = fileLinkQueue.shift();
  if (next) openFileDialog(next);
}
let fileProbeSeq = 0;
let fileProbeTimer = null;

function resetFileProbeUi() {
  $('fCat').textContent = '—';
  $('fSize').textContent = '—';
  $('fResume').textContent = '—';
  $('fResume').className = '';
}

async function runFileProbe() {
  const url = $('inUrl').value.trim();
  if (!/^https?:\/\//i.test(url) || !TAURI) {
    resetFileProbeUi();
    return;
  }
  const mySeq = ++fileProbeSeq;
  $('fSize').textContent = 'Checking…';
  $('fResume').textContent = '…';
  $('fResume').className = '';
  try {
    const cookies = (presentedFile && presentedFile.url === url)
      ? (presentedFile.cookies || null) : null;
    const referer = (presentedFile && presentedFile.url === url)
      ? (presentedFile.referer || null) : null;
    const info = await invoke('probe_file', { url, cookies, referer });
    if (mySeq !== fileProbeSeq || $('inUrl').value.trim() !== url) return;
    $('fCat').textContent = info.category || 'General';
    $('fSize').textContent = info.total > 0 ? fmtSize(info.total) : 'Unknown';
    $('fResume').textContent = info.resumable ? 'Yes' : 'No';
    $('fResume').className = info.resumable ? 'yes' : 'no';
    if (!$('inName').value.trim() && info.suggested_name) {
      $('inName').value = info.suggested_name;
    }
  } catch (e) {
    if (mySeq !== fileProbeSeq) return;
    $('fSize').textContent = '—';
    $('fResume').textContent = '—';
  }
}

$('inUrl').addEventListener('input', () => {
  clearTimeout(fileProbeTimer);
  fileProbeTimer = setTimeout(runFileProbe, 350);
});

function openFileDialog(payload) {
  presentedFile = payload
    ? { url: payload.url, cookies: payload.cookies || '', referer: payload.pageUrl || '' }
    : null;
  resetFileProbeUi();
  if (payload) {
    $('inUrl').value = payload.url;
    $('inName').value = (payload.title || '').split(/[\\/]/).pop() || '';
  }
  modal.classList.remove('hidden');
  $('inUrl').focus();
  if ($('inUrl').value.trim()) runFileProbe();
}
function closeFileDialog() {
  fileProbeSeq++;
  clearTimeout(fileProbeTimer);
  const abandoned = (presentedFile && presentedFile.cookies) ? presentedFile.cookies : '';
  presentedFile = null;
  modal.classList.add('hidden');
  if (abandoned && TAURI) {
    invoke('discard_cookies', { token: abandoned }).catch(() => {});
  }
  pumpFileLinks();
}

async function submitAdd(later) {
  const url = $('inUrl').value.trim();
  const dir = $('inDir').value.trim() || '~/Downloads';
  if (!url) { $('inUrl').focus(); return; }
  const name = $('inName').value.trim() || null;
  const session = presentedFile;
  presentedFile = null; // the new entry owns the session from here
  modal.classList.add('hidden');
  if (TAURI) {
    try {
      await invoke('add_download', {
        url, dir, later, category: null, name,
        cookies: session ? (session.cookies || null) : null,
        referer: session ? (session.referer || null) : null,
      });
      $('inUrl').value = '';
      $('inName').value = '';
    } catch (e) {
      // reopen with the address intact: a failure must not eat the
      // URL the user typed — they may only need to fix the folder
      console.error('add_download failed', e);
      toast('Could not add the download: ' + e);
      presentedFile = session;
      modal.classList.remove('hidden');
      $('inUrl').focus();
    }
  } else {
    $('inUrl').value = '';
    $('inName').value = '';
    demoAdd(url, later);
  }
  pumpFileLinks();
}
$('btnStart').onclick = () => submitAdd(false);
$('btnLater').onclick = () => submitAdd(true);

/* ---------- Add Video modal ---------- */
const vmodal = $('vmodal');
let vInfo = null; // last probe result {title, duration, options/qualities, url}
let probeSeq = 0;  // bumped on every probe start and every URL edit

/* Browser-extension video links can arrive in bursts — an event per
   link, with the startup pending-pull repeating them. Present them
   one at a time: opening the dialog per link raced the probes against
   each other and could pair one video's formats with another's URL. */
const videoLinkQueue = [];
let presentedVideoUrl = null;
let presentedVideoCookies = '';
function queueVideoLink(url, cookies) {
  if (!url) return;
  if (url === presentedVideoUrl || videoLinkQueue.some(q => q.url === url)) return;
  videoLinkQueue.push({ url, cookies: cookies || '' });
  pumpVideoLinks();
}
function pumpVideoLinks() {
  if (!vmodal.classList.contains('hidden')) return; // a video is being handled
  const next = videoLinkQueue.shift();
  if (next) openVideoModal(next.url, next.cookies);
}
let presentedTokenConsumed = false; // vStart took the session into a download
function closeVideoModal() {
  // closing without starting: the captured browser session for this
  // link must not linger on disk (entry deletion normally removes it)
  const abandoned = (!presentedTokenConsumed && presentedVideoCookies)
    ? presentedVideoCookies : '';
  presentedTokenConsumed = false;
  vmodal.classList.add('hidden');
  presentedVideoUrl = null;
  presentedVideoCookies = '';
  if (abandoned && TAURI) {
    invoke('discard_cookies', { token: abandoned }).catch(() => {});
  }
  pumpVideoLinks();
}

function openVideoModal(url, cookies) {
  gateComponents();
  presentedVideoUrl = url || null;
  presentedVideoCookies = cookies || '';
  vmodal.classList.remove('hidden');
  if (url) { $('vInUrl').value = url; }
  $('vInUrl').focus();
  if (url && TAURI) fetchVideoInfo();
}
$('btnVideo').onclick = () => openVideoModal('');
$('vModalClose').onclick = $('vCancel').onclick = () => closeVideoModal();
vmodal.addEventListener('click', e => { if (e.target === vmodal) closeVideoModal(); });

/* Which quality option starts selected, per the default in Options. */
function pickQualityIndex(options) {
  const q = settings.video_quality || 'best';
  if (q === 'best') {
    const hd = options.findIndex(o => !o.audio && (o.height === 1080 || o.label.startsWith('1080p')));
    return hd >= 0 ? hd : 0;
  }
  if (q === 'audio') {
    const i = options.findIndex(o => o.audio);
    return i >= 0 ? i : 0;
  }
  const target = Number(q);
  let i = options.findIndex(o => !o.audio && o.label.startsWith(q));
  if (i >= 0) return i;
  // otherwise the first height at or below the target (list is best-first)
  i = options.findIndex(o => {
    const h = o.height || parseInt(o.label, 10);
    return !o.audio && !isNaN(h) && h <= target;
  });
  return i >= 0 ? i : 0;
}

function fmtDuration(sec) {
  if (!sec) return '';
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = Math.floor(sec % 60);
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`
           : `${m}:${String(s).padStart(2, '0')}`;
}

/* The slim installer ships without the video engine (yt-dlp + deno);
   the app fetches it once, on first use, with progress shown in this
   dialog. Until it is ready, probing would only fail — gate it. */
let componentsReady = false;
let setupWanted = false; // a probe is waiting for the setup to finish
let ffmpegNoteShown = false;
async function gateComponents() {
  // demo mode (plain browser): nothing to set up — let the demo flow run
  if (!TAURI) return true;
  if (componentsReady) return true;
  try {
    const st = await invoke('components_status');
    if (st && st.ready) {
      componentsReady = true;
      if (st.ffmpeg === false && !ffmpegNoteShown) {
        ffmpegNoteShown = true;
        $('vSetup').classList.remove('hidden');
        $('vSetupBar').parentElement.style.display = 'none';
        $('vSetupText').textContent = 'Note: ffmpeg is not installed, so videos that arrive as separate audio + video streams cannot be merged yet. Install ffmpeg (e.g. sudo apt install ffmpeg) to enable them.';
      }
      return true;
    }
  } catch (e) { /* status unavailable: let the probe try anyway */ return true; }
  $('vSetup').classList.remove('hidden');
  $('vSetupText').classList.remove('error');
  $('vSetupText').textContent = 'Setting up the video engine — a one-time download (about 83 MB)…';
  invoke('ensure_components').catch(() => {});
  return false;
}
function onComponentsProgress(p) {
  const bar = $('vSetupBar');
  if (p.phase === 'downloading') {
    $('vSetupText').classList.remove('error');
    const mb = b => (b / 1048576).toFixed(1);
    $('vSetupText').textContent = p.total
      ? `Setting up the video engine — ${p.name}: ${mb(p.downloaded)} of ${mb(p.total)} MB`
      : `Setting up the video engine — ${p.name}: ${mb(p.downloaded)} MB`;
    if (p.total) bar.style.width = Math.min(100, (p.downloaded / p.total) * 100) + '%';
  } else if (p.phase === 'verifying' || p.phase === 'extracting') {
    $('vSetupText').textContent = `Setting up the video engine — ${p.name}: ${p.phase}…`;
    bar.style.width = '100%';
  } else if (p.phase === 'done') {
    componentsReady = true;
    $('vSetup').classList.add('hidden');
    bar.style.width = '0%';
    if (setupWanted && !$('vmodal').classList.contains('hidden')) {
      setupWanted = false;
      fetchVideoInfo();
    }
  } else if (p.phase === 'error') {
    setupWanted = false;
    $('vSetupText').innerHTML = '';
    $('vSetupText').textContent = 'Video engine setup failed: ' + (p.message || 'unknown error') + ' — check the connection, then press Fetch Info to try again.';
    $('vSetupText').classList.add('error');
  }
}

async function fetchVideoInfo() {
  const url = $('vInUrl').value.trim();
  if (!url) { $('vInUrl').focus(); return; }
  if (!componentsReady) {
    setupWanted = true;
    if (await gateComponents()) {
      setupWanted = false;
    } else {
      $('vStatus').textContent = '';
      return;
    }
  }
  const st = $('vStatus');
  if (!TAURI) {
    st.textContent = 'Video downloads need the NDM desktop app';
    st.classList.add('error');
    return;
  }
  st.textContent = 'Reading video qualities & file sizes…';
  st.classList.remove('error');
  $('vInfo').classList.add('hidden');
  $('vStart').disabled = true;
  if ($('vLater')) $('vLater').disabled = true;
  vInfo = null;
  const mySeq = ++probeSeq;
  try {
    const info = await invoke('probe_video', {
      url,
      cookies: (presentedVideoUrl && url === presentedVideoUrl)
        ? (presentedVideoCookies || null) : null,
    });
    if (mySeq !== probeSeq || $('vInUrl').value.trim() !== url) return;
    const opts = info.qualities || info.options || [];
    if (!opts.length) {
      st.textContent = 'No downloadable formats found for this page';
      st.classList.add('error');
      return;
    }
    vInfo = { ...info, options: opts, url };
    $('vTitle').textContent = info.title || '';
    if ($('vTitleInput')) $('vTitleInput').value = info.title || '';
    const durText = fmtDuration(info.duration);
    if ($('vDur')) {
      if (durText) {
        $('vDur').textContent = durText;
        $('vDur').classList.remove('hidden');
      } else {
        $('vDur').classList.add('hidden');
      }
    }
    if ($('vThumb')) {
      if (info.thumbnail) {
        $('vThumb').onload = () => {
          $('vThumb').classList.remove('hidden');
          if ($('vThumbPh')) $('vThumbPh').classList.add('hidden');
        };
        $('vThumb').onerror = () => {
          $('vThumb').classList.add('hidden');
          if ($('vThumbPh')) $('vThumbPh').classList.remove('hidden');
        };
        $('vThumb').src = info.thumbnail;
      } else {
        $('vThumb').classList.add('hidden');
        if ($('vThumbPh')) $('vThumbPh').classList.remove('hidden');
      }
    }
    $('vMeta').textContent = [
      info.uploader || '',
      durText,
      `${opts.length} quality options`,
    ].filter(Boolean).join(' • ');
    const box = $('vOptions');
    box.innerHTML = '';
    const preselect = pickQualityIndex(opts);
    opts.forEach((o, i) => {
      const lab = document.createElement('label');
      if (i === preselect) lab.classList.add('sel');
      const extBadge = (o.ext || (o.audio ? 'MP3' : 'MP4')).toUpperCase();
      const sizeStr = o.size_text || (o.size_bytes ? fmtSize(o.size_bytes) : (o.id === 'best' ? 'Auto' : 'Stream'));
      const noteOnly = (o.note || '').split('•').filter(p => !/MB|GB|KB/i.test(p)).join('•').trim();
      lab.innerHTML = `<input type="radio" name="vq" value="${i}" ${i === preselect ? 'checked' : ''}>
        <span class="q-badge ${o.audio ? 'audio' : ''}">${escapeHtml(extBadge)}</span>
        <span class="q-label">${escapeHtml(o.label)}</span>
        ${noteOnly ? `<span class="q-note">${escapeHtml(noteOnly)}</span>` : ''}
        <span class="q-size ${o.size_bytes ? 'exact' : ''}">${escapeHtml(sizeStr)}</span>`;
      lab.addEventListener('change', () => {
        box.querySelectorAll('label').forEach(el => el.classList.remove('sel'));
        lab.classList.add('sel');
      });
      lab.addEventListener('dblclick', () => {
        const inp = lab.querySelector('input');
        if (inp) inp.checked = true;
        submitVideo(false);
      });
      box.appendChild(lab);
    });
    $('vInfo').classList.remove('hidden');
    $('vStart').disabled = false;
    if ($('vLater')) $('vLater').disabled = false;
    st.textContent = '';
  } catch (e) {
    st.textContent = String(e);
    st.classList.add('error');
  }
}
$('vFetch').onclick = fetchVideoInfo;
$('vInUrl').addEventListener('input', () => {
  probeSeq++;
  if (!vInfo) return;
  vInfo = null;
  $('vInfo').classList.add('hidden');
  $('vStart').disabled = true;
  if ($('vLater')) $('vLater').disabled = true;
  $('vStatus').textContent = '';
});

async function submitVideo(later) {
  if (!vInfo) return;
  const url = $('vInUrl').value.trim();
  if (vInfo.url !== url) return;
  const sel = document.querySelector('input[name="vq"]:checked');
  const opt = vInfo.options[sel ? Number(sel.value) : 0];
  if (!opt) return;
  const dir = $('vInDir').value.trim() || '~/Downloads';
  const customTitle = ($('vTitleInput') && $('vTitleInput').value.trim()) || vInfo.title || '';
  const cookies = (presentedVideoUrl && url === presentedVideoUrl)
    ? (presentedVideoCookies || null) : null;
  if (cookies) presentedTokenConsumed = true;
  closeVideoModal();
  if (TAURI) {
    try {
      await invoke('add_video', {
        url,
        dir,
        selector: opt.selector,
        audio: !!opt.audio,
        title: customTitle,
        cookies,
        later: !!later,
        total: Number(opt.size_bytes) || 0,
      });
    } catch (e) {
      console.error('add_video failed', e);
      toast('Could not add the video: ' + e);
    }
  }
}
$('vStart').onclick = () => submitVideo(false);
if ($('vLater')) $('vLater').onclick = () => submitVideo(true);

/* ---------- Options modal ---------- */
const omodal = $('omodal');
function openOptions() {
  $('oDir').value = settings.default_dir;
  $('oConns').value = settings.max_connections;
  $('oLimitOn').checked = settings.speed_limit_bps > 0;
  $('oLimitVal').value = settings.speed_limit_bps > 0
    ? settings.speed_limit_bps / (1024 * 1024) : 2;
  $('oSimul').value = settings.max_simultaneous;
  $('oAutoResume').checked = settings.auto_resume;
  $('oVideoQ').value = settings.video_quality;
  $('oProxy').value = settings.proxy || '';
  $('oAutoUpdate').checked = settings.auto_update !== false;
  $('oEngine').textContent = '…';
  if (TAURI) invoke('components_status').then(st => {
    $('oEngine').textContent = st && st.ready
      ? 'Ready (yt-dlp + deno installed)'
      : 'Sets itself up automatically in the background';
  }).catch(() => { $('oEngine').textContent = '—'; });
  omodal.classList.remove('hidden');
}
$('btnOptions').onclick = openOptions;
$('oModalClose').onclick = $('oCancel').onclick = () => omodal.classList.add('hidden');
omodal.addEventListener('click', e => { if (e.target === omodal) omodal.classList.add('hidden'); });
$('oSave').onclick = async () => {
  settings = {
    ...settings,
    default_dir: $('oDir').value.trim() || '~/Downloads',
    max_connections: Math.min(32, Math.max(1, Number($('oConns').value) || 16)),
    speed_limit_bps: $('oLimitOn').checked
      ? Math.round(Math.max(0.1, Number($('oLimitVal').value) || 1) * 1024 * 1024) : 0,
    max_simultaneous: Math.min(10, Math.max(0, Number($('oSimul').value) || 0)),
    auto_resume: $('oAutoResume').checked,
    auto_update: $('oAutoUpdate').checked,
    video_quality: $('oVideoQ').value,
    proxy: $('oProxy').value.trim(),
  };
  await saveSettings();
  $('inDir').value = settings.default_dir;
  $('vInDir').value = settings.default_dir;
  omodal.classList.add('hidden');
};

/* ---------- Scheduler modal ---------- */
const smodal = $('smodal');
function openScheduler() {
  $('sEnabled').checked = settings.scheduler_enabled;
  $('sStart').value = settings.sched_start;
  $('sStop').value = settings.sched_stop;
  $('sSimulVal').textContent = settings.max_simultaneous > 0
    ? String(settings.max_simultaneous) : 'Unlimited';
  $('sStatus').textContent = '';
  smodal.classList.remove('hidden');
}
$('btnScheduler').onclick = openScheduler;
$('sModalClose').onclick = $('sCancel').onclick = () => smodal.classList.add('hidden');
smodal.addEventListener('click', e => { if (e.target === smodal) smodal.classList.add('hidden'); });
$('sStartNow').onclick = async () => {
  if (TAURI) {
    try { await invoke('scheduler_start_now'); }
    catch (e) { console.error(e); $('sStatus').textContent = 'Could not start the queue: ' + e; return; }
  }
  $('sStatus').textContent = 'Queue started';
};
$('sStopNow').onclick = async () => {
  if (TAURI) {
    try { await invoke('scheduler_stop_now'); }
    catch (e) { console.error(e); $('sStatus').textContent = 'Could not stop the queue: ' + e; return; }
  }
  $('sStatus').textContent = 'All downloads paused';
};
$('sSave').onclick = async () => {
  settings = {
    ...settings,
    scheduler_enabled: $('sEnabled').checked,
    sched_start: $('sStart').value || '22:00',
    sched_stop: $('sStop').value || '06:00',
  };
  await saveSettings();
  $('sStatus').textContent = 'Saved';
};

/* ---------- Site Grabber modal ---------- */
const gmodal = $('gmodal');
let grabItems = [];              // items from the last grab_page result
const grabUnchecked = new Set(); // urls the user unchecked (items start checked)
const grabKindsOff = new Set();  // kinds hidden via their filter chip

function setGrabStatus(msg, isError) {
  const st = $('gStatus');
  st.textContent = msg;
  st.classList.toggle('error', !!isError);
}

function openGrabber() {
  grabItems = [];
  grabUnchecked.clear();
  grabKindsOff.clear();
  $('gUrl').value = '';
  $('gDir').value = settings.default_dir || '~/Downloads';
  $('gFilter').value = '';
  $('gKinds').innerHTML = '';
  $('gList').innerHTML = '';
  setGrabStatus('Enter a page URL and click Explore');
  updateGrabCount();
  gmodal.classList.remove('hidden');
  $('gUrl').focus();
}
$('btnGrabber').onclick = openGrabber;
$('gModalClose').onclick = $('gCancel').onclick = () => gmodal.classList.add('hidden');
gmodal.addEventListener('click', e => { if (e.target === gmodal) gmodal.classList.add('hidden'); });

function grabVisible(it) {
  if (grabKindsOff.has(it.kind)) return false;
  const q = $('gFilter').value.trim().toLowerCase();
  return !q || `${it.name} ${it.url}`.toLowerCase().includes(q);
}

function updateGrabCount() {
  const sel = grabItems.filter(it => !grabUnchecked.has(it.url));
  const vis = sel.filter(grabVisible).length;
  // the selection is global — it survives filter changes, like IDM's
  // grabber — so say plainly when part of it is filtered out of view
  $('gCount').textContent = vis === sel.length
    ? `${sel.length} selected`
    : `${sel.length} selected (${vis} in current view)`;
  $('gDownload').disabled = sel.length === 0;
}

function renderGrabKinds() {
  const counts = new Map();
  for (const it of grabItems) counts.set(it.kind, (counts.get(it.kind) || 0) + 1);
  const box = $('gKinds');
  box.innerHTML = '';
  for (const [kind, n] of counts) {
    const lab = document.createElement('label');
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = !grabKindsOff.has(kind);
    cb.addEventListener('change', () => {
      if (cb.checked) grabKindsOff.delete(kind); else grabKindsOff.add(kind);
      renderGrabList();
    });
    lab.appendChild(cb);
    lab.appendChild(document.createTextNode(`${kind} (${n})`));
    box.appendChild(lab);
  }
}

function renderGrabList() {
  const box = $('gList');
  box.innerHTML = '';
  for (const it of grabItems) {
    if (!grabVisible(it)) continue;
    const row = document.createElement('div');
    row.className = 'grab-item';
    row.title = it.url;
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = !grabUnchecked.has(it.url);
    cb.addEventListener('change', () => {
      if (cb.checked) grabUnchecked.delete(it.url); else grabUnchecked.add(it.url);
      updateGrabCount();
    });
    const name = document.createElement('span');
    name.className = 'g-name';
    name.textContent = it.name;
    row.append(cb, name);
    if (it.label && it.label !== it.name) {
      const lab = document.createElement('span');
      lab.className = 'g-label';
      lab.textContent = it.label;
      row.appendChild(lab);
    }
    const kind = document.createElement('span');
    kind.className = 'g-kind';
    kind.textContent = it.kind;
    const size = document.createElement('span');
    size.className = 'g-size';
    size.textContent = it.size != null ? fmtSize(it.size) : '—';
    row.append(kind, size);
    box.appendChild(row);
  }
  updateGrabCount();
}

async function fetchGrab() {
  const url = $('gUrl').value.trim();
  if (!url) { $('gUrl').focus(); return; }
  if (!TAURI) { demoGrab(); return; }
  $('gFetch').disabled = true;
  setGrabStatus('Loading…');
  try {
    const res = await invoke('grab_page', { url });
    grabItems = res.items || [];
    grabUnchecked.clear();
    grabKindsOff.clear();
    renderGrabKinds();
    renderGrabList();
    setGrabStatus(`Found ${grabItems.length} files on “${res.title || url}”`);
  } catch (e) {
    setGrabStatus(String(e), true);
  } finally {
    $('gFetch').disabled = false;
  }
}
$('gFetch').onclick = fetchGrab;
$('gUrl').addEventListener('keydown', e => { if (e.key === 'Enter') fetchGrab(); });
$('gFilter').addEventListener('input', renderGrabList);

$('gSelAll').onclick = () => {
  for (const it of grabItems) if (grabVisible(it)) grabUnchecked.delete(it.url);
  renderGrabList();
};
$('gSelNone').onclick = () => {
  for (const it of grabItems) if (grabVisible(it)) grabUnchecked.add(it.url);
  renderGrabList();
};

$('gDownload').onclick = async () => {
  const chosen = grabItems.filter(it => !grabUnchecked.has(it.url));
  if (!chosen.length) return;
  if (!TAURI) { gmodal.classList.add('hidden'); return; }
  const dir = $('gDir').value.trim() || '~/Downloads';
  $('gDownload').disabled = true;
  let failed = 0;
  let firstErr = '';
  for (const it of chosen) {
    try { await invoke('add_download', { url: it.url, dir, later: false, category: 'Grabber' }); }
    catch (e) { failed++; if (!firstErr) firstErr = String(e); console.error('add_download failed', it.url, e); toast('Could not add ' + it.name + ': ' + e); }
  }
  if (failed) {
    // keep the modal open so the actual reason can be read, not just counted
    setGrabStatus(`${failed} of ${chosen.length} could not be added — ${firstErr}`, true);
    $('gDownload').disabled = false;
    return;
  }
  gmodal.classList.add('hidden');
};

/* demo data so the grabber is fully previewable in a plain browser */
function demoGrab() {
  const base = $('gUrl').value.trim().replace(/\/+$/, '');
  grabItems = [
    { url: `${base}/brochure-2026.pdf`, name: 'brochure-2026.pdf', kind: 'Documents', size: 4821760, label: 'Company brochure (PDF)' },
    { url: `${base}/theme-song.mp3`, name: 'theme-song.mp3', kind: 'Music', size: 8388608, label: '' },
    { url: `${base}/assets-backup.zip`, name: 'assets-backup.zip', kind: 'Compressed', size: null, label: 'Full asset backup' },
    { url: `${base}/promo-clip.mp4`, name: 'promo-clip.mp4', kind: 'Video', size: 157286400, label: 'Promo clip — 1080p' },
  ];
  grabUnchecked.clear();
  grabKindsOff.clear();
  renderGrabKinds();
  renderGrabList();
  setGrabStatus(`Found ${grabItems.length} files on “Demo Page”`);
}

/* ---------- detail tabs + completion options + open/refresh ---------- */
function showDetailTab(which) {
  $('tabStatus').classList.toggle('active', which === 'status');
  $('tabCompletion').classList.toggle('active', which === 'completion');
  $('panelStatus').classList.toggle('hidden', which !== 'status');
  $('panelCompletion').classList.toggle('hidden', which !== 'completion');
}
$('tabStatus').onclick = () => showDetailTab('status');
$('tabCompletion').onclick = () => showDetailTab('completion');

document.querySelectorAll('input[name="compAct"]').forEach(r => {
  r.addEventListener('change', async () => {
    const d = items.get(selectedId);
    if (!d) return;
    d.completion = r.value;
    if (TAURI) {
      try { await invoke('set_completion', { id: d.id, action: r.value }); }
      catch (e) { console.error(e); toast(String(e)); }
    }
  });
});

async function openSelected(folder) {
  const d = items.get(selectedId);
  if (!d || !TAURI) return;
  try { await invoke('open_path', { id: d.id, folder }); }
  catch (e) { console.error(e); toast(String(e)); }
}
$('dOpen').onclick = () => openSelected(false);
$('dOpenFolder').onclick = () => openSelected(true);

/* refresh-URL modal (for expired download addresses) */
const rmodal = $('rmodal');
$('dRefresh').onclick = () => {
  if (!items.get(selectedId)) return;
  $('rUrl').value = '';
  rmodal.classList.remove('hidden');
  $('rUrl').focus();
};
$('rModalClose').onclick = $('rCancel').onclick = () => rmodal.classList.add('hidden');
rmodal.addEventListener('click', e => { if (e.target === rmodal) rmodal.classList.add('hidden'); });
$('rSave').onclick = async () => {
  const d = items.get(selectedId);
  const newUrl = $('rUrl').value.trim();
  if (!d || !newUrl) { $('rUrl').focus(); return; }
  rmodal.classList.add('hidden');
  if (TAURI) {
    try {
      await invoke('refresh_url', { id: d.id, newUrl });
      d.url = newUrl;
      render();
    } catch (e) { console.error(e); toast(String(e)); }
  }
};

/* ---------- keyboard ---------- */
// Escape closes the topmost open modal; Enter submits the URL field of
// the Add / Refresh dialogs and probes in the Add Video dialog
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape') return;
  const open = [...document.querySelectorAll('.modal:not(.hidden)')]
    // a mandatory update dialog cannot be dismissed, not even by Esc
    .filter(m => !(m.id === 'umodal' && m.dataset.mandatory === '1'));
  if (!open.length) return;
  const top = open[open.length - 1];
  // the video and Add dialogs own cleanup — a captured browser
  // session must be discarded and the queue pumped — so Escape goes
  // through the same close path as their Cancel buttons
  if (top.id === 'vmodal') { closeVideoModal(); return; }
  if (top.id === 'modal') { closeFileDialog(); return; }
  top.classList.add('hidden');
});
$('inUrl').addEventListener('keydown', e => { if (e.key === 'Enter') submitAdd(false); });
$('vInUrl').addEventListener('keydown', e => { if (e.key === 'Enter') fetchVideoInfo(); });
$('rUrl').addEventListener('keydown', e => { if (e.key === 'Enter') $('rSave').click(); });

/* ---------- Tauri wiring ---------- */
async function initTauri() {
  await loadSettings();
  try {
    const list = await invoke('list_downloads');
    for (const d of list) items.set(d.id, normalize(d));
    if (list.length) selectedId = list[0].id;
  } catch (e) { console.error('list_downloads failed', e); }
  await listen('progress', e => {
    const d = normalize(e.payload);
    if (deletedIds.has(d.id)) return;
    items.set(d.id, d);
    if (!selectedId) selectedId = d.id;
    scheduleRender();
  });
  // a video link sent from the browser extension opens the video dialog
  await listen('incoming_video', e => {
    invoke('take_pending_videos').catch(() => {});
    queueVideoLink(e.payload.url || '', e.payload.cookies || '');
  });
  // links that arrived before the listener was registered are held by
  // the backend; pull them now so none are silently lost.
  try {
    const pending = await invoke('take_pending_videos');
    for (const p of pending || []) queueVideoLink(p.url || '', p.cookies || '');
  } catch (e) { console.error('take_pending_videos failed', e); }
  await listen('components_progress', e => { onComponentsProgress(e.payload || {}); });
  await listen('update_available', e => { showUpdate(e.payload || {}); });
  await listen('update_progress', e => { onUpdateProgress(e.payload || {}); });
  // file downloads caught by the extension open the Add URL dialog
  await listen('incoming_file', e => {
    invoke('take_pending_files').catch(() => {});
    queueFileLink(e.payload || {});
  });
  try {
    const pendingFiles = await invoke('take_pending_files');
    for (const p of pendingFiles || []) queueFileLink(p);
  } catch (e) { console.error('take_pending_files failed', e); }
  render();
}
function normalize(d) {
  return { resume_capability: null, blocks: '', added: '', ...d };
}

/* ---------- demo mode (plain browser, no Tauri) ---------- */
const DEMO_BLOCK_BYTES = 4 * 1024 * 1024;
let demoSeq = 0;

function categorize(name) {
  const ext = (name.split('.').pop() || '').toLowerCase();
  if (['zip', 'rar', '7z', 'tar', 'gz'].includes(ext)) return 'Compressed';
  if (['mp4', 'mkv', 'avi', 'webm', 'mov'].includes(ext)) return 'Video';
  if (['mp3', 'flac', 'wav', 'ogg'].includes(ext)) return 'Music';
  if (['pdf', 'doc', 'docx', 'txt', 'xlsx', 'pptx'].includes(ext)) return 'Documents';
  return 'Programs';
}
function makeBlocks(n) { return new Array(n).fill('0'); }
function blocksString(d) {
  const done = Math.floor((d.done_bytes / d.total) * d._blocks.length);
  return d._blocks.map((_, i) => {
    if (i < done) return '1';
    if (d.status === 'downloading' && i >= done && i < done + d.conns) return '2';
    return '0';
  }).join('');
}
function demoItem(name, url, total, status, doneRatio) {
  const nBlocks = Math.max(16, Math.min(512, Math.round(total / DEMO_BLOCK_BYTES)));
  const d = {
    id: 'demo-' + (++demoSeq), name, url, total,
    done_bytes: Math.floor(total * doneRatio), speed_bps: 0,
    conns: 4 + Math.floor(Math.random() * 9), status,
    category: categorize(name), added: 'Today',
    resume_capability: 'Yes (Range supported)',
    _blocks: makeBlocks(nBlocks),
  };
  d.blocks = blocksString(d);
  return d;
}
function demoSeed() {
  const a = demoItem('Horizon_Documentary_Ep01.mp4',
    'https://videos.example.org/docs/Horizon_Documentary_Ep01.mp4', 1_894_256_640, 'downloading', 0.14);
  const b = demoItem('Annual_Report_2026.pdf',
    'https://files.example.com/reports/Annual_Report_2026.pdf', 24_517_632, 'finished', 1);
  items.set(a.id, a); items.set(b.id, b);
  selectedId = a.id;
  setInterval(demoTick, 450);
  render();
}
function demoTick() {
  for (const d of items.values()) {
    if (d.status !== 'downloading') { d.speed_bps = 0; continue; }
    const base = d.total / 55; // ~25 s to finish at full speed
    d.speed_bps = Math.max(0, Math.round(base * (0.65 + Math.random() * 0.7)));
    d.done_bytes = Math.min(d.total, d.done_bytes + d.speed_bps * 0.45);
    if (d.done_bytes >= d.total) { d.status = 'finished'; d.speed_bps = 0; }
    d.blocks = blocksString(d);
  }
  scheduleRender();
}
function demoAdd(url, later) {
  let name = 'download.bin';
  try { name = decodeURIComponent(new URL(url).pathname.split('/').pop() || name); } catch (e) { /* keep default */ }
  const total = 180_000_000 + Math.floor(Math.random() * 900_000_000);
  const d = demoItem(name, url, total, later ? 'paused' : 'downloading', 0);
  items.set(d.id, d);
  selectedId = d.id;
  render();
}
function demoPause(id) { const d = items.get(id); if (d && d.status === 'downloading') { d.status = 'paused'; d.speed_bps = 0; d.blocks = blocksString(d); } scheduleRender(); }
function demoResume(id) { const d = items.get(id); if (d && (d.status === 'paused' || d.status === 'queued')) d.status = 'downloading'; scheduleRender(); }
function demoRemove(id) { items.delete(id); if (selectedId === id) selectedId = items.keys().next().value || null; render(); }

/* ---------- boot ---------- */
setInterval(() => {
  pulse++;
  const d = items.get(selectedId);
  if (d && d.status === 'downloading') drawSegMap(d.blocks || '');
}, 500);
window.addEventListener('resize', () => { if (selectedId) renderDetail(); });
/* =============== 1.5: menu bar, batch, import, about, updater =============== */
const umodal = $('umodal');
let updateInfo = null;
let updateInstalling = false;

function closeMenus() {
  document.querySelectorAll('.menu-drop').forEach(d => d.classList.add('hidden'));
  document.querySelectorAll('.menu-btn').forEach(b => b.classList.remove('open'));
}
for (const btn of document.querySelectorAll('.menu-btn')) {
  const drop = $('menu-' + btn.dataset.menu);
  btn.addEventListener('click', e => {
    e.stopPropagation();
    const wasHidden = drop.classList.contains('hidden');
    closeMenus();
    if (wasHidden) { drop.classList.remove('hidden'); btn.classList.add('open'); }
  });
  btn.addEventListener('mouseenter', () => {
    const anyOpen = [...document.querySelectorAll('.menu-drop')].some(d => !d.classList.contains('hidden'));
    if (anyOpen && drop.classList.contains('hidden')) {
      closeMenus();
      drop.classList.remove('hidden');
      btn.classList.add('open');
    }
  });
}
document.addEventListener('click', e => {
  if (!e.target.closest('.menu')) closeMenus();
});

async function startAllDownloads() {
  let n = 0;
  for (const d of items.values()) {
    if (d.status === 'paused' || d.status === 'error' || d.status === 'queued') {
      try { await invoke('resume_download', { id: d.id }); n++; } catch (e) { /* keep going */ }
    }
  }
  toast(n ? `Started ${n} download${n > 1 ? 's' : ''}` : 'Nothing to start');
}
async function pauseAllDownloads() {
  let n = 0;
  for (const d of items.values()) {
    if (d.status === 'downloading') {
      try { await invoke('pause_download', { id: d.id }); n++; } catch (e) { /* keep going */ }
    }
  }
  toast(n ? `Paused ${n} download${n > 1 ? 's' : ''}` : 'Nothing is downloading');
}
async function deleteCompleted() {
  const done = [...items.values()].filter(d => d.status === 'finished');
  if (!done.length) { toast('No completed downloads'); return; }
  for (const d of done) {
    try { await invoke('remove_download', { id: d.id }); } catch (e) { continue; }
    deletedIds.add(d.id);
    items.delete(d.id);
  }
  if (selectedId && !items.has(selectedId)) selectedId = items.keys().next().value || null;
  render();
  toast(`Deleted ${done.length} completed download${done.length > 1 ? 's' : ''} from the list`);
}

function showUpdate(info) {
  if (!info || info.status !== 'available') return;
  updateInfo = info;
  $('uTitle').textContent = `NDM ${info.version} is available`;
  $('uNotes').textContent = info.mandatory
    ? 'This update is required — downloads pause until it is installed. ' + (info.notes || '')
    : (info.notes || '');
  $('uBarWrap').classList.add('hidden');
  $('uUpdate').classList.remove('hidden');
  $('uRestart').classList.add('hidden');
  $('uLater').classList.toggle('hidden', !!info.mandatory);
  $('uModalClose').style.display = info.mandatory ? 'none' : '';
  umodal.dataset.mandatory = info.mandatory ? '1' : '';
  umodal.classList.remove('hidden');
}
async function manualUpdateCheck() {
  if (!TAURI) { toast('Update checks need the NDM desktop app'); return; }
  toast('Checking for updates…');
  try {
    const r = await invoke('check_update');
    if (r.status === 'available') showUpdate(r);
    else if (r.status === 'current') toast(`NDM is up to date (${r.version})`);
    else toast('Could not reach the update server — try again later');
  } catch (e) { toast('Update check failed: ' + e); }
}
function onUpdateProgress(p) {
  if (p.phase === 'downloading') {
    $('uBarWrap').classList.remove('hidden');
    const mb = b => (b / 1048576).toFixed(1);
    $('uBarText').textContent = p.total
      ? `Downloading the update — ${mb(p.downloaded)} of ${mb(p.total)} MB`
      : `Downloading the update — ${mb(p.downloaded)} MB`;
    if (p.total) $('uBar').style.width = Math.min(100, (p.downloaded / p.total) * 100) + '%';
  } else if (p.phase === 'installing') {
    $('uBarText').textContent = 'Installing — your password may be asked by the system…';
    $('uBar').style.width = '100%';
  } else if (p.phase === 'installed') {
    updateInstalling = false;
    $('uBarText').textContent = 'Update installed.';
    $('uUpdate').classList.add('hidden');
    $('uLater').classList.add('hidden');
    $('uRestart').classList.remove('hidden');
    $('uModalClose').style.display = '';
  } else if (p.phase === 'opened') {
    updateInstalling = false;
    $('uBarText').textContent = 'The installer opened — finish the installation there, then restart NDM.';
    $('uUpdate').classList.add('hidden');
    $('uRestart').classList.remove('hidden');
  } else if (p.phase === 'error') {
    updateInstalling = false;
    $('uBarText').textContent = 'Update failed: ' + (p.message || 'unknown error');
    $('uUpdate').classList.remove('hidden');
  }
}
$('uUpdate').onclick = async () => {
  if (updateInstalling) return;
  updateInstalling = true;
  $('uUpdate').classList.add('hidden');
  $('uBarWrap').classList.remove('hidden');
  $('uBarText').textContent = 'Downloading the update…';
  try { await invoke('install_update'); }
  catch (e) { updateInstalling = false; $('uBarText').textContent = 'Update failed: ' + e; $('uUpdate').classList.remove('hidden'); }
};
$('uRestart').onclick = () => { if (TAURI) invoke('restart_app'); };
$('uLater').onclick = () => umodal.classList.add('hidden');
$('uModalClose').onclick = () => { if (!updateInfo || !updateInfo.mandatory) umodal.classList.add('hidden'); };
umodal.addEventListener('click', e => { if (e.target === umodal && (!updateInfo || !updateInfo.mandatory)) umodal.classList.add('hidden'); });

/* batch add */
const bmodal = $('bmodal');
function openBatch() { $('bDir').value = settings.default_dir; $('bStatus').textContent = ''; bmodal.classList.remove('hidden'); }
$('bModalClose').onclick = $('bCancel').onclick = () => bmodal.classList.add('hidden');
bmodal.addEventListener('click', e => { if (e.target === bmodal) bmodal.classList.add('hidden'); });
$('bAdd').onclick = async () => {
  const urls = $('bUrls').value.split(/\r?\n/).map(x => x.trim()).filter(x => x.startsWith('http'));
  if (!urls.length) { $('bStatus').textContent = 'No valid links found — one http(s) link per line.'; return; }
  if (!TAURI) { $('bStatus').textContent = 'Batch downloads need the NDM desktop app'; return; }
  const dir = $('bDir').value.trim() || settings.default_dir;
  $('bAdd').disabled = true;
  let ok = 0;
  for (const url of urls) {
    try { await invoke('add_download', { url, dir, later: false }); ok++; }
    catch (e) { /* count at the end */ }
  }
  $('bAdd').disabled = false;
  $('bUrls').value = '';
  bmodal.classList.add('hidden');
  toast(`Added ${ok} of ${urls.length} downloads`);
};

/* import */
const imodal = $('imodal');
$('iModalClose').onclick = $('iCancel').onclick = () => imodal.classList.add('hidden');
imodal.addEventListener('click', e => { if (e.target === imodal) imodal.classList.add('hidden'); });
$('iGo').onclick = async () => {
  let data;
  try { data = JSON.parse($('iText').value); }
  catch (e) { $('iStatus').textContent = 'That is not valid JSON.'; return; }
  const rows = Array.isArray(data) ? data : [];
  if (!rows.length) { $('iStatus').textContent = 'No downloads found in that list.'; return; }
  if (!TAURI) { $('iStatus').textContent = 'Import needs the NDM desktop app'; return; }
  let ok = 0;
  for (const r of rows) {
    if (!r || !r.url) continue;
    let dir = settings.default_dir;
    if (r.path) { const cut = r.path.lastIndexOf('/'); if (cut > 0) dir = r.path.slice(0, cut); }
    try { await invoke('add_download', { url: r.url, dir, later: true }); ok++; }
    catch (e) { /* counted below */ }
  }
  $('iText').value = '';
  imodal.classList.add('hidden');
  toast(`Imported ${ok} download${ok === 1 ? '' : 's'} (paused — start them when ready)`);
};

/* about */
const amodal = $('amodal');
async function openAbout() {
  amodal.classList.remove('hidden');
  if (!TAURI) { $('aVersion').textContent = 'Version — (browser preview)'; return; }
  try { $('aVersion').textContent = 'Version ' + await invoke('app_version'); } catch (e) {}
  try {
    const st = await invoke('components_status');
    $('aEngine').textContent = 'Video engine: ' + (st.ready ? 'ready' : 'setting up in the background');
  } catch (e) {}
}
$('aModalClose').onclick = $('aOk').onclick = () => amodal.classList.add('hidden');
amodal.addEventListener('click', e => { if (e.target === amodal) amodal.classList.add('hidden'); });

/* shortcuts dialog */
const kmodal = $('kmodal');
$('sOk').onclick = $('kModalClose').onclick = () => kmodal.classList.add('hidden');
kmodal.addEventListener('click', e => { if (e.target === kmodal) kmodal.classList.add('hidden'); });

/* menu actions */
const menuActions = {
  add: () => $('btnAdd').click(),
  video: () => $('btnVideo').click(),
  batch: openBatch,
  import: () => { $('iStatus').textContent = ''; imodal.classList.remove('hidden'); },
  export: async () => {
    if (!TAURI) { toast('Export needs the NDM desktop app'); return; }
    try { toast('Exported to ' + await invoke('export_downloads')); }
    catch (e) { toast('Export failed: ' + e); }
  },
  quit: () => { if (TAURI) invoke('quit_app'); },
  startall: startAllDownloads,
  pauseall: pauseAllDownloads,
  stopall: () => $('btnStopAll').click(),
  del: () => $('btnDelete').click(),
  deldone: deleteCompleted,
  grabber: () => $('btnGrabber').click(),
  scheduler: () => $('btnScheduler').click(),
  options: openOptions,
  theme: () => $('themeToggle').click(),
  sidebar: () => document.body.classList.toggle('no-side'),
  'sort-name': () => { sortKey = 'name'; render(); },
  'sort-date': () => { sortKey = 'date'; render(); },
  'sort-size': () => { sortKey = 'size'; render(); },
  'sort-status': () => { sortKey = 'status'; render(); },
  refresh: async () => { if (TAURI) { try { const list = await invoke('list_downloads'); for (const d of list) if (!deletedIds.has(d.id)) items.set(d.id, normalize(d)); render(); } catch (e) {} } },
  update: manualUpdateCheck,
  shortcuts: () => kmodal.classList.remove('hidden'),
  about: openAbout,
};
document.querySelectorAll('.menu-drop > button').forEach(b => {
  b.addEventListener('click', () => {
    const act = b.dataset.act;
    closeMenus();
    if (act && menuActions[act]) menuActions[act]();
  });
});

/* global shortcuts (the ones the Help dialog lists — all real) */
document.addEventListener('keydown', e => {
  if (e.key === 'Escape') closeMenus();
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement && document.activeElement.tagName || '');
  if (typing) return;
  if (e.ctrlKey && !e.shiftKey && e.key.toLowerCase() === 'n') { e.preventDefault(); $('btnAdd').click(); }
  else if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === 'v') { e.preventDefault(); $('btnVideo').click(); }
  else if (e.ctrlKey && e.key.toLowerCase() === 'q') { if (TAURI) invoke('quit_app'); }
  else if (e.key === 'F5') { e.preventDefault(); menuActions.refresh(); }
});

if (TAURI) initTauri(); else { loadSettings(); demoSeed(); }
