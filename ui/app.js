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
/* completion announcements (N-03): the backend's download_complete
   event does not fire for every finish — a download that was
   paused and resumed finalizes without one, so its "Download
   complete" dialog never appeared (fresh finishes got theirs).
   render() watches the list itself for the same transition and
   synthesizes the announcement; this set, shared with the real
   event listener, makes sure each finish is announced exactly
   once, whichever path saw it first. An id leaves the set when
   its entry is seen in a non-finished state again, so a file
   re-downloaded later announces its second finish too. */
const completeAnnounced = new Set();
const prevStatusById = new Map();
let selectedId = null;
const selectedIds = new Set(); // multi-selection (Ctrl/Shift+click, Ctrl+A); selectedId stays the primary row
let selectAnchor = null;       // row a Shift+click range extends from
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
  default_dir: '~/Downloads', max_connections: 8, speed_limit_bps: 0,
  max_simultaneous: 0, auto_resume: false, scheduler_enabled: false,
  sched_start: '22:00', sched_stop: '06:00', video_quality: 'best', proxy: '',
  proxy_mode: '', proxy_scheme: 'http', proxy_host: '', proxy_port: 0,
  proxy_user: '', proxy_pass: '', proxy_http: true, proxy_https: true, proxy_ftp: true,
  proxy_pac_url: '', proxy_from_browser_on_error: false,
  site_logins: [], power_when_done: '', power_force: false, grab_projects: [],
  category_dirs: {}, category_exts: {}, conn_exceptions: [], exit_when_done: false,
  sound_events: true, show_complete_dialog: true,
  queues: [{ name: 'Main download queue', use_schedule: false, start: '22:00', stop: '06:00', exit_when_done: false, power_when_done: '', max_simultaneous: 0 }],
  quota_mb: 0, quota_hours: 5, clipboard_monitor: false,
};

/* the built-in category → extensions table (mirrors the backend's
   category_for); the Options editor shows these until the user
   overrides a category, and only overrides are saved */
const BUILTIN_CATEGORY_EXTS = {
  Compressed: ['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'tgz', 'tbz2', 'txz'],
  Programs: ['exe', 'msi', 'msix', 'msixbundle', 'deb', 'rpm', 'dmg', 'pkg', 'appimage', 'apk', 'bin', 'run', 'cab', 'jar', 'war', 'vmdk', 'qcow2', 'vdi', 'iso', 'img', 'flatpakref'],
  Video: ['mp4', 'mkv', 'webm', 'avi', 'mov', 'flv', 'wmv', 'm4v', 'ts'],
  Music: ['mp3', 'm4a', 'm4b', 'flac', 'wav', 'ogg', 'opus', 'aac', 'wma'],
  Documents: ['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'txt', 'md', 'csv', 'rtf', 'epub', 'mobi'],
};
let settings = { ...DEFAULT_SETTINGS };

/* Fill a <select> with the configured queue names. */
function fillQueueSelect(sel, selectedName) {
  if (!sel) return;
  const names = (settings.queues || []).map(q => q.name);
  const list = names.length ? names : ['Main download queue'];
  sel.innerHTML = '';
  for (const n of list) {
    const o = document.createElement('option');
    o.value = n;
    o.textContent = n;
    if (n === selectedName) o.selected = true;
    sel.appendChild(o);
  }
}

/* every category name the File dialog may offer: the built-ins,
   plus any the user gave a folder or a type list in Options, plus
   any created from the dialog itself */
function categoryNames() {
  return [...new Set([
    'General',
    ...Object.keys(BUILTIN_CATEGORY_EXTS),
    ...Object.keys(settings.category_dirs || {}),
    ...Object.keys(settings.category_exts || {}),
  ])];
}

/* Fill a <select> with the category choices. The first entry is
   "Auto": no explicit category, so the backend routes by file type
   exactly as it did before the dropdown existed. */
function fillCategorySelect(sel, selected) {
  if (!sel) return;
  const names = categoryNames();
  sel.innerHTML = '';
  const auto = document.createElement('option');
  auto.value = '';
  auto.textContent = 'Auto (by file type)';
  sel.appendChild(auto);
  for (const n of names) {
    const o = document.createElement('option');
    o.value = n;
    o.textContent = n;
    sel.appendChild(o);
  }
  sel.value = selected && names.includes(selected) ? selected : '';
}

/* the Q column: the queue's 1-based number, IDM-style */
function queueShort(qname) {
  const names = (settings.queues || []).map(q => q.name);
  const i = names.indexOf(qname || 'Main download queue');
  return i >= 0 ? String(i + 1) : '1';
}

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
let sortKey = 'date'; // View menu + header clicks: name | date | size | status
let sortDir = 1;      // 1 = ascending (▲), -1 = descending (▼) — for EVERY key
/* which sort key a list column drives (header click, IDM-style) */
const SORT_FOR_COL = { name: 'name', size: 'size', status: 'status', added: 'date' };
function visibleItems() {
  const all = [...items.values()];
  let out = all;
  if (currentCat === 'Finished') out = all.filter(d => d.status === 'finished');
  else if (currentCat === 'Unfinished') out = all.filter(d => d.status !== 'finished');
  else if (currentCat.startsWith('queue:')) out = all.filter(d => d.queue === currentCat.slice(6));
  else if (currentCat !== 'All Downloads') out = all.filter(d => d.category === currentCat);
  const numId = id => parseInt(String(id).replace(/\D+/g, ''), 10) || 0;
  // every comparator is ASCENDING in the key's own terms, so the
  // ▲/▼ marker means the same thing on every column (Windows/IDM
  // convention). Size used to be the odd one out — descending
  // while its arrow claimed ascending (N-05).
  const by = {
    name: (a, b) => a.name.localeCompare(b.name),
    date: (a, b) => numId(a.id) - numId(b.id),
    size: (a, b) => (a.total || 0) - (b.total || 0),
    status: (a, b) => a.status.localeCompare(b.status),
  }[sortKey];
  return by ? [...out].sort((a, b) => by(a, b) * sortDir) : out;
}

let queueListSig = '';
function renderQueueFilters() {
  const ul = $('queueList');
  if (!ul) return;
  const counts = {};
  for (const d of items.values()) counts[d.queue || 'Main download queue'] = (counts[d.queue || 'Main download queue'] || 0) + 1;
  const names = (settings.queues || []).map(q => q.name);
  // JSON encoding: queue names containing '|' or ',' used to be
  // able to forge a collision and freeze the sidebar
  const sig = JSON.stringify([names, names.map(n => counts[n] || 0), currentCat]);
  if (sig === queueListSig) return;
  queueListSig = sig;
  ul.innerHTML = '';
  for (const n of names) {
    const li = document.createElement('li');
    li.textContent = `▸ ${n} (${counts[n] || 0})`;
    if (currentCat === 'queue:' + n) li.classList.add('active');
    li.onclick = () => {
      document.querySelectorAll('#catList li').forEach(x => x.classList.remove('active'));
      document.querySelectorAll('#queueList li').forEach(x => x.classList.remove('active'));
      li.classList.add('active');
      currentCat = 'queue:' + n;
      render();
    };
    ul.appendChild(li);
  }
}


/* ---------- list columns: IDM's "Columns…" customization ----------
   Which columns show, in which order, at which width — persisted in
   localStorage like the theme. The header and every row are built
   from this one definition, so the picker can never disagree with
   the list. */
const COLUMN_DEFS = [
  { id: 'name', label: 'File Name', defW: 0 },
  { id: 'size', label: 'Size', defW: 90 },
  { id: 'status', label: 'Status', defW: 175 },
  { id: 'timeleft', label: 'Time Left', defW: 80 },
  { id: 'rate', label: 'Transfer Rate', defW: 105 },
  { id: 'q', label: 'Q', defW: 36 },
  { id: 'desc', label: 'Description', defW: 150 },
  { id: 'added', label: 'Date Added', defW: 135 },
  { id: 'category', label: 'Category', defW: 100, defOff: true },
  { id: 'saveto', label: 'Save To', defW: 240, defOff: true },
];
function defaultColumnCfg() {
  return COLUMN_DEFS.map(c => ({ id: c.id, visible: !c.defOff, width: c.defW }));
}
function loadColumnCfg() {
  try {
    const raw = JSON.parse(localStorage.getItem('ndm-columns-v1') || 'null');
    if (!Array.isArray(raw)) return defaultColumnCfg();
    const out = [];
    for (const r of raw) {
      const def = COLUMN_DEFS.find(c => c.id === r.id);
      if (def && !out.some(o => o.id === r.id)) {
        out.push({ id: r.id, visible: r.visible !== false, width: Math.max(0, parseInt(r.width, 10) || 0) });
      }
    }
    for (const c of defaultColumnCfg()) {
      if (!out.some(o => o.id === c.id)) out.push(c);
    }
    // the name column is the list's anchor — it cannot be hidden
    const nm = out.find(o => o.id === 'name');
    if (nm) nm.visible = true;
    return out;
  } catch (e) { return defaultColumnCfg(); }
}
let columnCfg = loadColumnCfg();
function saveColumnCfg() {
  try { localStorage.setItem('ndm-columns-v1', JSON.stringify(columnCfg)); } catch (e) { /* private mode */ }
}
function visibleColumns() { return columnCfg.filter(c => c.visible); }

function applyColumns() {
  const cg = $('colGroup');
  const hr = $('headRow');
  if (!cg || !hr) return;
  cg.innerHTML = '';
  hr.innerHTML = '';
  for (const c of visibleColumns()) {
    const col = document.createElement('col');
    if (c.width > 0) col.style.width = c.width + 'px';
    cg.appendChild(col);
    const th = document.createElement('th');
    th.textContent = (COLUMN_DEFS.find(d => d.id === c.id) || {}).label || c.id;
    if (c.id === 'name') th.className = 'c-name';
    // IDM sorts from the header: clicking a sortable column sorts
    // by it; clicking the active column again reverses direction
    if (SORT_FOR_COL[c.id]) {
      th.classList.add('sortable');
      th.title = 'Click to sort';
      th.addEventListener('click', () => {
        const key = SORT_FOR_COL[c.id];
        if (sortKey === key) sortDir = -sortDir;
        else { sortKey = key; sortDir = 1; }
        updateSortIndicators();
        render();
      });
    }
    hr.appendChild(th);
  }
  updateSortIndicators();
  lastStructSig = '__columns_changed__'; // force one structural rebuild
}

/* paint the ▲/▼ marker on the header that owns the current sort */
function updateSortIndicators() {
  const hr = $('headRow');
  if (!hr) return;
  const cols = visibleColumns();
  [...hr.children].forEach((th, i) => {
    const c = cols[i];
    if (!c) return;
    const def = COLUMN_DEFS.find(d => d.id === c.id) || {};
    const arrow = SORT_FOR_COL[c.id] && SORT_FOR_COL[c.id] === sortKey
      ? (sortDir > 0 ? ' ▲' : ' ▼') : '';
    th.textContent = (def.label || c.id) + arrow;
  });
}

/* ---------- toolbar customization (IDM's toolbar "Customize…") ---------- */
const TOOLBAR_DEFS = [
  { id: 'btnAdd', label: 'Add URL' },
  { id: 'btnVideo', label: 'Add Video' },
  { id: 'btnGrabber', label: 'Site Grabber' },
  { id: 'btnResume', label: 'Resume' },
  { id: 'btnPause', label: 'Pause' },
  { id: 'btnStopAll', label: 'Stop All' },
  { id: 'btnDelete', label: 'Delete' },
  { id: 'btnStatus', label: 'Status' },
  { id: 'btnScheduler', label: 'Scheduler' },
  { id: 'btnOptions', label: 'Options' },
];
function loadToolbarCfg() {
  try {
    const raw = JSON.parse(localStorage.getItem('ndm-toolbar-v1') || 'null');
    if (!raw || !Array.isArray(raw.hidden)) return { hidden: [], size: 'large' };
    return { hidden: raw.hidden.filter(id => TOOLBAR_DEFS.some(d => d.id === id)), size: raw.size === 'small' ? 'small' : 'large' };
  } catch (e) { return { hidden: [], size: 'large' }; }
}
let toolbarCfg = loadToolbarCfg();
function saveToolbarCfg() {
  try { localStorage.setItem('ndm-toolbar-v1', JSON.stringify(toolbarCfg)); } catch (e) { /* private mode */ }
}
function applyToolbar() {
  for (const d of TOOLBAR_DEFS) {
    const el = $(d.id);
    if (el) el.style.display = toolbarCfg.hidden.includes(d.id) ? 'none' : '';
  }
  const tb = document.querySelector('.toolbar');
  if (tb) tb.classList.toggle('tb-small', toolbarCfg.size === 'small');
}

/* ---------- Columns / Toolbar dialogs ---------- */
let colEdit = null;
function openColumns() {
  if (anotherModalOpen('colmodal')) return; // one modal layer (N-08)
  colEdit = columnCfg.map(c => ({ ...c }));
  renderColEdit();
  revealModal($('colmodal'));
}
function renderColEdit() {
  const box = $('colList');
  box.innerHTML = '';
  colEdit.forEach((c, i) => {
    const def = COLUMN_DEFS.find(d => d.id === c.id) || {};
    const row = document.createElement('div');
    row.className = 'col-row';
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = c.visible;
    cb.disabled = c.id === 'name';
    cb.onchange = () => { c.visible = cb.checked; };
    const lab = document.createElement('span');
    lab.className = 'col-name';
    lab.textContent = def.label || c.id;
    const w = document.createElement('input');
    w.type = 'number';
    w.min = '0';
    w.max = '800';
    w.value = c.width;
    w.title = 'Width in pixels (0 = automatic)';
    w.onchange = () => { c.width = Math.max(0, parseInt(w.value, 10) || 0); };
    const up = document.createElement('button');
    up.className = 'tool';
    up.textContent = '↑';
    up.disabled = i === 0;
    up.onclick = () => { [colEdit[i - 1], colEdit[i]] = [colEdit[i], colEdit[i - 1]]; renderColEdit(); };
    const dn = document.createElement('button');
    dn.className = 'tool';
    dn.textContent = '↓';
    dn.disabled = i === colEdit.length - 1;
    dn.onclick = () => { [colEdit[i + 1], colEdit[i]] = [colEdit[i], colEdit[i + 1]]; renderColEdit(); };
    row.append(cb, lab, w, up, dn);
    box.appendChild(row);
  });
}
$('colModalClose').onclick = () => $('colmodal').classList.add('hidden');
$('colmodal').addEventListener('click', e => { if (e.target === $('colmodal')) $('colmodal').classList.add('hidden'); });
$('colReset').onclick = () => { colEdit = defaultColumnCfg(); renderColEdit(); };
$('colOk').onclick = () => {
  columnCfg = colEdit;
  saveColumnCfg();
  applyColumns();
  $('colmodal').classList.add('hidden');
  render();
};

let tbEdit = null;
function openToolbarCfg() {
  if (anotherModalOpen('tbmodal')) return; // one modal layer (N-08)
  tbEdit = { hidden: [...toolbarCfg.hidden], size: toolbarCfg.size };
  renderTbEdit();
  $('tbSize').value = tbEdit.size;
  revealModal($('tbmodal'));
}
function renderTbEdit() {
  const box = $('tbList');
  box.innerHTML = '';
  for (const d of TOOLBAR_DEFS) {
    const row = document.createElement('div');
    row.className = 'col-row';
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = !tbEdit.hidden.includes(d.id);
    cb.onchange = () => {
      if (cb.checked) tbEdit.hidden = tbEdit.hidden.filter(x => x !== d.id);
      else tbEdit.hidden.push(d.id);
    };
    const lab = document.createElement('span');
    lab.className = 'col-name';
    lab.textContent = d.label;
    row.append(cb, lab);
    box.appendChild(row);
  }
}
$('tbModalClose').onclick = () => $('tbmodal').classList.add('hidden');
$('tbmodal').addEventListener('click', e => { if (e.target === $('tbmodal')) $('tbmodal').classList.add('hidden'); });
$('tbReset').onclick = () => { tbEdit = { hidden: [], size: 'large' }; $('tbSize').value = 'large'; renderTbEdit(); };
$('tbOk').onclick = () => {
  tbEdit.size = $('tbSize').value === 'small' ? 'small' : 'large';
  toolbarCfg = tbEdit;
  saveToolbarCfg();
  applyToolbar();
  $('tbmodal').classList.add('hidden');
};

/* ---------- the list itself ----------
   IDM updates its list in place: at the 200 ms progress cadence only
   the cells of rows that are already on screen change — the row
   elements themselves are never rebuilt, so selection, scroll
   position and hover survive every tick. A full rebuild happens
   only when the visible SET of rows changes (add, delete, filter,
   sort, columns). */
const rowEls = new Map();   // id -> <tr>
let lastStructSig = '';

function cellHtml(colId, d, st, left) {
  switch (colId) {
    case 'name':
      return `${escapeHtml(d.name)}${d.status === 'downloading' && d.total === 0 ? '<div class="pbar ind"></div>' : `<div class="pbar"><i data-pct="${pctOf(d).toFixed(1)}"></i></div>`}`;
    case 'size':
      return d.total > 0 || st === 'finished' ? fmtSize(d.total) : 'Unknown';
    case 'status':
      return `${cap(st)}${st !== 'finished' && d.total > 0 ? ' ' + pctOf(d).toFixed(0) + '%' : ''}${st === 'error' && d.error ? ' — ' + escapeHtml(d.error.length > 60 ? d.error.slice(0, 60) + '…' : d.error) : ''}`;
    case 'timeleft':
      return st === 'finished' ? '—' : fmtLeft(left);
    case 'rate':
      return st === 'downloading' ? fmtRate(d.speed_bps) : '—';
    case 'q':
      return escapeHtml(queueShort(d.queue));
    case 'desc':
      return d.description ? escapeHtml(d.description) : '';
    case 'added':
      return escapeHtml(d.added || '');
    case 'category':
      return escapeHtml(d.category || '');
    case 'saveto':
      return escapeHtml(d.save_to || '');
    default:
      return '';
  }
}

function updateRowCells(tr, d) {
  const left = d.status === 'downloading' && d.speed_bps > 0 && d.total > 0
    ? (d.total - d.done_bytes) / d.speed_bps : null;
  // status is interpolated into HTML — pin it to the known vocabulary
  const st = KNOWN_STATUS.includes(d.status) ? d.status : 'error';
  const cols = visibleColumns();
  for (let i = 0; i < cols.length; i++) {
    const td = tr.children[i];
    if (!td) break;
    const colId = cols[i].id;
    const html = cellHtml(colId, d, st, left);
    if (td._h !== html) {
      td.innerHTML = html;
      td._h = html;
    }
    if (colId === 'status') {
      const cls = 'st-' + st;
      if (td.className !== cls) td.className = cls;
      const title = st === 'error' && d.error ? d.error : '';
      if (td.title !== title) td.title = title;
    }
    if (colId === 'q') {
      const t = d.queue || 'Main download queue';
      if (td.title !== t) td.title = t;
    }
    if (colId === 'name' && td.className !== 'name') td.className = 'name';
  }
  // the row fill goes through the DOM, never the innerHTML string:
  // this WebKit drops percentage widths written into inline styles
  const fill = tr.querySelector('.pbar i');
  if (fill) fill.style.width = fill.dataset.pct + '%';
  const sel = selectedIds.has(d.id) || d.id === selectedId;
  if (tr.classList.contains('selected') !== sel) tr.classList.toggle('selected', sel);
}

function buildRow(d) {
  const tr = document.createElement('tr');
  for (const _c of visibleColumns()) tr.appendChild(document.createElement('td'));
  // Single-click ONLY selects the row in the full-height table (like IDM);
  // Ctrl+click toggles one row, Shift+click extends a range — the
  // multi-selection batch actions (Delete above all) act on
  tr.addEventListener('click', e => {
    // ctrlHeld backs up e.ctrlKey: this WebKit can deliver the
    // click without the modifier even while Ctrl is physically
    // down, and a Ctrl+click then silently REPLACED the selection
    // instead of toggling the row into it (F-22)
    if (e.ctrlKey || e.metaKey || ctrlHeld) { toggleSelect(d.id); return; }
    if (e.shiftKey || shiftHeld) { rangeSelect(d.id); return; }
    selectAnchor = d.id;
    select(d.id, false);
  });
  // Double-click opens the Download Status / Properties dialog —
  // IDM never opens the file itself from the list; opening lives
  // on the dialog and in the context menu
  tr.addEventListener('dblclick', () => select(d.id, true));
  // Right-click: IDM's per-download context menu. Right-clicking a row
  // outside the selection selects just it; right-clicking inside a
  // multi-selection keeps the whole selection (IDM does the same)
  tr.addEventListener('contextmenu', e => {
    e.preventDefault();
    if (!selectedIds.has(d.id)) { selectAnchor = d.id; select(d.id, false); }
    else { selectedId = d.id; render(); }
    showCtxMenu(e.clientX, e.clientY, d.id);
  });
  updateRowCells(tr, d);
  return tr;
}

/* True while the user is in the middle of editing a field inside a
   dialog (the floating detail dialog excluded — it is built to
   update live). Background renders churn the DOM at the progress
   cadence; in this WebKit that churn eats keystrokes aimed at the
   focused field, and the corrupted value is then saved (F-08).
   While an edit is in progress the list render is skipped whole —
   the data keeps accumulating in `items`, and the repaint lands on
   the next tick after the field loses focus. */
function editingInBlockingModal() {
  const a = document.activeElement;
  if (!a || !/^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName)) return false;
  const m = a.closest('.modal');
  return !!m && m.id !== 'detail' && !m.classList.contains('hidden');
}
document.addEventListener('focusout', () => scheduleRender());

function render() {
  if (editingInBlockingModal()) return; // never churn under an in-progress edit
  /* finish detection (N-03): an entry that reaches 'finished'
     from a live state gets its completion announcement even when
     the backend event never arrives (resumed finishes). Runs over
     EVERY entry, not just the visible ones — the announcement is
     not a property of the current filter. Seeding matters: an
     entry first seen already-finished (present at launch) has no
     recorded previous state and stays silent. */
  for (const d of items.values()) {
    const prev = prevStatusById.get(d.id);
    prevStatusById.set(d.id, d.status);
    if (d.status !== 'finished') { completeAnnounced.delete(d.id); continue; }
    if (prev && prev !== 'finished' && !completeAnnounced.has(d.id)) {
      completeAnnounced.add(d.id);
      completeQueue.push({
        id: d.id, name: d.name, url: d.url,
        path: d.save_to || '', total: d.total || 0,
      });
      showNextComplete();
    }
  }
  renderQueueFilters();
  const list = visibleItems();
  /* selection repair: toolbar/menu actions act on the selection, so
     the selection must never point at a row the current filter
     hides — acting on an invisible download (Delete above all)
     is a nasty surprise (F-24). Keep the selection inside the
     visible set; fall back to the first visible row, like the
     app's startup auto-select. */
  {
    const visIds = new Set(list.map(d => d.id));
    for (const id of [...selectedIds]) {
      if (!visIds.has(id)) selectedIds.delete(id);
    }
    if (selectedId && !visIds.has(selectedId)) {
      selectedId = list.length ? list[0].id : null;
      if (selectedId) selectedIds.add(selectedId);
    }
  }
  const structSig = JSON.stringify([
    list.map(d => d.id),
    visibleColumns().map(c => c.id + ':' + c.width),
  ]);
  if (structSig !== lastStructSig) {
    // structural change: rebuild the row set once
    lastStructSig = structSig;
    rowsEl.innerHTML = '';
    rowEls.clear();
    for (const d of list) {
      const tr = buildRow(d);
      rowEls.set(d.id, tr);
      rowsEl.appendChild(tr);
    }
  } else {
    // the 200 ms cadence: patch the cells of the rows already shown
    for (const d of list) {
      const tr = rowEls.get(d.id);
      if (tr) updateRowCells(tr, d);
    }
  }
  const em = $('emptyMsg');
  em.style.display = list.length ? 'none' : 'block';
  if (!list.length) {
    // the empty text must reflect WHY the list is empty: with a
    // category/queue filter active, "No downloads yet" is a lie
    em.textContent = currentCat === 'All Downloads'
      ? 'No downloads yet — click “Add URL” to start one.'
      : currentCat.startsWith('queue:')
        ? 'No downloads in this queue.'
        : currentCat === 'Finished'
          ? 'No finished downloads.'
          : currentCat === 'Unfinished'
            ? 'No unfinished downloads.'
            : 'No downloads in this category.';
  }

  const all = [...items.values()];
  const dlAll = all.filter(d => d.status === 'downloading');
  // the counts describe the VIEW: with a category/queue filter
  // active the bar used to keep quoting the unfiltered totals
  // (N-06). Total speed stays global — it is a live system stat,
  // not a property of the filtered rows.
  const dl = list.filter(d => d.status === 'downloading');
  $('sideStatus').textContent = `${list.length} files (${dl.length} downloading)`;
  $('totalSpeed').textContent = 'Total speed: ' + fmtRate(dlAll.reduce((a, d) => a + d.speed_bps, 0)).replace('—', '0 B/s');

  const cur = items.get(selectedId);
  const curFinished = cur && cur.status === 'finished';
  const curDownloading = cur && cur.status === 'downloading';
  const curCanResume = cur && !curFinished && !curDownloading;
  if ($('btnPause')) $('btnPause').disabled = !curDownloading;
  if ($('btnResume')) $('btnResume').disabled = !curCanResume;
  if ($('btnDelete')) $('btnDelete').disabled = !cur && selectedIds.size === 0;

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
    if ($('dSaveTo')) $('dSaveTo').textContent = '—';
    if ($('dReferer')) $('dReferer').textContent = '—';
    if ($('dDesc') && document.activeElement !== $('dDesc')) $('dDesc').value = '';
    $('dBarFill').style.width = '0%'; $('dBarText').textContent = '0.0%';
    if ($('dPause')) $('dPause').disabled = true;
    if ($('dResumeBtn')) $('dResumeBtn').disabled = true;
    $('dOpen').disabled = $('dOpenFolder').disabled = $('dRefresh').disabled = true;
    drawSegMap('', '');
    return;
  }
  if ($('dTitle')) {
    // a finished download is 100% by definition — a 0-byte file
    // used to title itself "0%", looking broken when it was done
    const pctTxt = d.total > 0 ? pctOf(d).toFixed(0)
      : (d.status === 'finished' ? '100' : '0');
    $('dTitle').textContent = d.name
      ? `${pctTxt}% — ${d.name}`
      : 'Download Status';
  }
  const isFinished = d.status === 'finished';
  const isDownloading = d.status === 'downloading';
  if ($('dPause')) $('dPause').disabled = !isDownloading;
  if ($('dResumeBtn')) $('dResumeBtn').disabled = isFinished || isDownloading;
  // Open hands the backend a file — IDM enables it only once the
  // file is complete, not merely "not queued" (a paused/downloading
  // entry would open a partial file)
  $('dOpen').disabled = $('dOpenFolder').disabled = d.status !== 'finished';
  $('dRefresh').disabled = isDownloading || isFinished;
  // sync the completion radios from the entry — but never while
  // the user is interacting with them: a progress tick landing
  // between mousedown and click would flip the checked radio back
  // and the click would land on the wrong option
  {
    const ae = document.activeElement;
    const touching = ae && ae.name === 'compAct';
    if (!touching) {
      document.querySelectorAll('input[name="compAct"]').forEach(r => {
        r.checked = r.value === (d.completion || '');
      });
    }
  }
  // the Speed Limiter tab gets the same treatment while it is the
  // visible tab — it used to keep showing the previously viewed
  // download's limit (and Apply then wrote that value onto this one).
  // The guard keeps a re-render from clobbering a value mid-edit.
  if ($('panelLimiter') && !$('panelLimiter').classList.contains('hidden')
      && document.activeElement !== $('dLimitVal')
      && document.activeElement !== $('dLimitOn')) {
    syncLimiterPanel();
  }
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
  if ($('dSaveTo')) $('dSaveTo').textContent = d.save_to || '—';
  if ($('dReferer')) $('dReferer').textContent = d.referer || '—';
  // the description is an editable field — never clobber it while
  // the user is typing (this dialog re-renders on every progress tick)
  if ($('dDesc') && document.activeElement !== $('dDesc')) {
    $('dDesc').value = d.description || '';
  }
  // per-connection table (IDM shows one row per connection)
  const dc = $('dConns');
  if (dc) {
    const rows = (d.conns_detail || [])
      .map((b, i) => ({ b, i }))
      .filter(x => x.b > 0 || (d.status === 'downloading' && x.i < (d.conns || 0)));
    dc.innerHTML = rows.length
      ? rows.map(x => `<div class="d-conn-row"><span>Connection ${x.i + 1}</span><span>${fmtSize(x.b)} downloaded</span></div>`).join('')
      : '';
  }
  // queue assignment (moving queues is IDM's "Add to queue")
  const dq = $('dQueueSel');
  if (dq && document.activeElement !== dq) {
    fillQueueSelect(dq, d.queue || 'Main download queue');
  }
  const ind = d.status === 'downloading' && d.total === 0;
  $('dBarFill').classList.toggle('ind', ind);
  $('dBarFill').style.width = ind ? '' : (d.total > 0 ? pctOf(d).toFixed(1) : (isFinished ? 100 : 0)) + '%';
  $('dBarText').textContent = d.total > 0 ? pctOf(d).toFixed(1) + '%'
    : (d.status === 'downloading' ? fmtSize(d.done_bytes) : (isFinished ? '100%' : '—'));
  drawSegMap(d.blocks || '', d.status);
}

/* Segment map: one cell per block char — '1' done, '2' active (pulsing), else pending.
   Colors come from the active theme's CSS variables. */
function drawSegMap(blocks, status) {
  const cv = $('segMap'), ctx = cv.getContext('2d');
  // assigning canvas.width (even the same value) resets the bitmap
  // and forces a full repaint — only do it when the size changed
  const W = cv.clientWidth || 760;
  if (cv.width !== W) cv.width = W;
  const H = cv.height;
  ctx.clearRect(0, 0, W, H);
  if (!blocks && status !== 'finished') return;
  const css = getComputedStyle(document.documentElement);
  const cDone = css.getPropertyValue('--seg-done').trim() || '#3f9de0';
  const cActive = css.getPropertyValue('--seg-active').trim() || '#f5c542';
  const cActive2 = css.getPropertyValue('--seg-active2').trim() || '#c79a1d';
  const cPending = css.getPropertyValue('--seg-pending').trim() || '#c9d2da';
  const n = Math.min(blocks ? blocks.length : 128, 1024);
  const cols = Math.ceil(Math.sqrt(n * (W / H))) || 1;
  const rows = Math.ceil(n / cols) || 1;
  const totalCells = cols * rows;
  const cw = W / cols, ch = H / rows, gap = Math.min(2, cw * 0.18);
  for (let i = 0; i < totalCells; i++) {
    const x = (i % cols) * cw, y = Math.floor(i / cols) * ch;
    if (status === 'finished') {
      ctx.fillStyle = cDone;
    } else {
      const blockIdx = Math.floor((i / totalCells) * n);
      const c = blocks ? blocks[blockIdx] : '0';
      if (c === '1') ctx.fillStyle = cDone;
      else if (c === '2') ctx.fillStyle = pulse % 2 ? cActive : cActive2;
      else ctx.fillStyle = cPending;
    }
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

/* transient message line under the toolbar. The default style is
   the pink error banner; kind 'info' renders the SAME line in a
   neutral style — informational messages ("NDM is up to date",
   export/count confirmations) are not errors and must not look
   like one. */
let flashTimer = null;
function toast(msg, kind) {
  const el = $('flashMsg');
  el.textContent = msg;
  el.classList.toggle('info', kind === 'info');
  el.classList.remove('hidden');
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => el.classList.add('hidden'), 5000);
}

/* Force one full repaint of a dialog card a frame after it is
   shown. On this WebKit the FIRST paint of a card whose top
   lands near the top of the viewport drops the title bar's
   paint — the Scheduler's head stayed blank (box laid out,
   clicks landing, background/text never drawn; pixel-measured:
   only the strip's bottom sliver survives) until some later
   in-dialog rebuild happened to repaint it (F-11). Other
   dialogs escaped only because they incidentally force a
   post-reveal repaint (a focus call, an rAF style write); the
   Scheduler's open path did nothing after reveal.
   R2's one-frame transform toggle did NOT cure it (measured
   again in R3): with compositing disabled a transform neither
   promotes a layer nor invalidates paint, so the toggle was a
   no-op. The strip loss itself turned out to be stacking, not
   staleness — the menubar's rows ate the modal's paint, cured
   by the modal's z-index in styles.css (see the F-11 note
   there). What remains here is the repaint this function was
   always meant to force: a display toggle with a layout flush
   between invalidates paint in the software renderer where a
   transform does not, so the card is painted again from
   scratch a frame after reveal and once more after
   fonts/layout settle (the card's final height can land after
   the first paint). Skipped while a field inside the card
   holds focus — hiding an ancestor would drop the focus
   mid-edit (F-08). */
function nudgeRepaint(card) {
  if (!card) return;
  const repaint = () => {
    const a = document.activeElement;
    if (a && card.contains(a) && /^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName)) return;
    card.style.display = 'none';
    void card.offsetHeight; // flush layout while hidden
    card.style.display = '';
  };
  requestAnimationFrame(repaint);
  setTimeout(repaint, 350);
}

/* Show a modal scrolled to the top: the container and its card keep
   their scroll positions between opens, so a dialog used to reopen
   wherever it was last left — title off-screen (F-09). */
function revealModal(el) {
  el.classList.remove('hidden');
  el.scrollTop = 0;
  const card = el.querySelector('.modal-card');
  if (card) card.scrollTop = 0;
  nudgeRepaint(card);
}

/* The app's own styled confirmation (Scheduler guards used to pop
   raw browser confirm() dialogs titled "JavaScript - tauri://…").
   Resolves true on Confirm, false on Cancel/×/backdrop/Escape. */
let confirmResolve = null;
function showConfirm(msg, yesLabel) {
  return new Promise(res => {
    if (confirmResolve) confirmResolve(false); // never stack two asks
    confirmResolve = res;
    $('cfMsg').textContent = msg;
    $('cfYes').textContent = yesLabel || 'Confirm';
    revealModal($('cfmodal'));
    $('cfYes').focus();
  });
}
function settleConfirm(v) {
  $('cfmodal').classList.add('hidden');
  if (confirmResolve) { const r = confirmResolve; confirmResolve = null; r(v); }
}
$('cfYes').onclick = () => settleConfirm(true);
$('cfNo').onclick = $('cfModalClose').onclick = () => settleConfirm(false);
$('cfmodal').addEventListener('click', e => { if (e.target === $('cfmodal')) settleConfirm(false); });

/* WebKit's default context menu (Back / Reload / Inspect Element)
   is browser chrome, not app UI — suppress it everywhere except
   the download rows, whose own handler builds the app menu. */
document.addEventListener('contextmenu', e => {
  if (e.target.closest && e.target.closest('#rows tr')) return;
  e.preventDefault();
});

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
  selectedIds.clear();
  if (id) selectedIds.add(id);
  if (openDetail) {
    revealModal(detailEl);
    requestAnimationFrame(() => renderDetail());
  }
  render();
}

/* physical modifier state, tracked independently of click events
   (see the row click handler — e.ctrlKey is not reliable here) */
let ctrlHeld = false;
let shiftHeld = false;
window.addEventListener('keydown', e => {
  if (e.key === 'Control') ctrlHeld = true;
  if (e.key === 'Shift') shiftHeld = true;
}, true);
window.addEventListener('keyup', e => {
  if (e.key === 'Control') ctrlHeld = false;
  if (e.key === 'Shift') shiftHeld = false;
}, true);
window.addEventListener('blur', () => { ctrlHeld = false; shiftHeld = false; });

/* Ctrl+click: toggle one row in the multi-selection. The primary row
   (selectedId — what Pause/Resume/detail act on) follows the click;
   toggling the primary off hands the role to a remaining row. */
function toggleSelect(id) {
  selectAnchor = id;
  if (selectedIds.has(id)) {
    selectedIds.delete(id);
    if (selectedId === id) selectedId = selectedIds.values().next().value || null;
  } else {
    selectedIds.add(id);
    selectedId = id;
  }
  render();
}

/* Shift+click: the selection becomes the visible range from the
   anchor row (last plain/Ctrl click) to the clicked row. */
function rangeSelect(id) {
  const list = visibleItems();
  const to = list.findIndex(x => x.id === id);
  if (to < 0) return;
  let from = list.findIndex(x => x.id === selectAnchor);
  if (from < 0) from = list.findIndex(x => x.id === selectedId);
  if (from < 0) from = to;
  const a = Math.min(from, to), b = Math.max(from, to);
  selectedIds.clear();
  for (let i = a; i <= b; i++) selectedIds.add(list[i].id);
  selectedId = id;
  render();
}

/* ---------- row context menu (IDM's right-click menu) ---------- */
const ctxMenu = $('ctxMenu');
function hideCtxMenu() { ctxMenu.classList.add('hidden'); }
function showCtxMenu(x, y, id) {
  // look the item up NOW, and again inside the handlers: rows are
  // built once and progress payloads REPLACE the item object, so an
  // object captured at row-build time freezes the menu in the past
  // (a finished download kept offering Pause, with Open disabled)
  const d = items.get(id);
  if (!d) return;
  ctxMenu.innerHTML = '';
  const add = (label, fn, disabled) => {
    const b = document.createElement('button');
    b.textContent = label;
    if (disabled) b.disabled = true;
    b.onclick = () => { hideCtxMenu(); fn(); };
    ctxMenu.appendChild(b);
  };
  const head = document.createElement('div');
  head.className = 'ctx-head';
  head.textContent = d.name && d.name.length > 34 ? d.name.slice(0, 34) + '…' : (d.name || 'Download');
  ctxMenu.appendChild(head);
  const st = d.status;
  add('Resume', () => actOnSelected(id => TAURI ? invoke('resume_download', { id }) : demoResume(id)),
    st === 'downloading' || st === 'finished');
  add('Pause', () => actOnSelected(id => TAURI ? invoke('pause_download', { id }) : demoPause(id)),
    st !== 'downloading');
  add('Properties / Status…', () => select(id, true));
  const sep1 = document.createElement('div'); sep1.className = 'ctx-sep'; ctxMenu.appendChild(sep1);
  add('Open File', () => { selectedId = id; openSelected(false); }, st !== 'finished');
  add('Open Folder', () => { selectedId = id; openSelected(true); }, st === 'queued');
  const queues = (settings.queues || []).map(q => q.name);
  if (queues.length > 1) {
    const sep2 = document.createElement('div'); sep2.className = 'ctx-sep'; ctxMenu.appendChild(sep2);
    const qh = document.createElement('div'); qh.className = 'ctx-head'; qh.textContent = 'Move to queue'; ctxMenu.appendChild(qh);
    for (const qn of queues) {
      add((d.queue === qn ? '✓ ' : '') + qn, async () => {
        const cur = items.get(id);
        if (!TAURI || !cur || cur.queue === qn) return;
        try {
          await invoke('set_entry_queue', { id, queue: qn });
          cur.queue = qn;
          render();
        } catch (e) { toast('Could not move the download: ' + e); }
      });
    }
  }
  const sep3 = document.createElement('div'); sep3.className = 'ctx-sep'; ctxMenu.appendChild(sep3);
  add('Delete…', () => {
    // a right-clicked row inside a multi-selection deletes the whole
    // selection (one confirmation); outside it, just that row
    if (!selectedIds.has(id)) { selectAnchor = id; select(id, false); }
    else selectedId = id;
    openDeleteConfirm();
  });
  ctxMenu.classList.remove('hidden');
  const w = ctxMenu.offsetWidth, h = ctxMenu.offsetHeight;
  ctxMenu.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 8)) + 'px';
  ctxMenu.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 8)) + 'px';
}
document.addEventListener('click', e => { if (!e.target.closest('#ctxMenu')) hideCtxMenu(); });
window.addEventListener('blur', hideCtxMenu);

/* ---------- actions ---------- */
async function actOnSelected(fn) {
  const d = items.get(selectedId);
  if (!d) return;
  if (TAURI) { try { await fn(d.id); } catch (e) { console.error(e); toast(String(e)); } }
  else fn(d.id);
}
$('btnPause').onclick = () => actOnSelected(id => TAURI ? invoke('pause_download', { id }) : demoPause(id));
$('btnResume').onclick = () => actOnSelected(id => TAURI ? invoke('resume_download', { id }) : demoResume(id));
/* Delete asks first (IDM does) — removing the wrong row used to be
   one stray Delete keystroke away, with no way back. The dialog
   also offers deleting the file itself, not just the list entry.
   A multi-selection gets ONE dialog for the whole batch (IDM asks
   once, not once per file): one checkbox, one Delete click, then
   the per-entry removes run silently underneath. */
let deleteTargets = null;
function openDeleteConfirm() {
  if (anotherModalOpen('delmodal')) return; // one modal layer (N-08)
  // the rows Delete acts on: the whole multi-selection when the
  // primary row is part of one, otherwise just the primary row
  let targets;
  if (selectedId && selectedIds.has(selectedId) && selectedIds.size > 1) {
    targets = [...selectedIds].filter(id => items.has(id));
  } else {
    targets = selectedId && items.has(selectedId) ? [selectedId] : [];
  }
  if (!targets.length) return;
  deleteTargets = targets;
  if (targets.length === 1) {
    const d = items.get(targets[0]);
    $('delName').textContent = 'Delete "' + (d.name || 'download') + '" from the list?';
    $('delFileLbl').textContent = 'Also delete the downloaded file from the disk';
    $('delFileChk').disabled = d.status !== 'finished';
  } else {
    $('delName').textContent = 'Delete ' + targets.length + ' downloads from the list?';
    $('delFileLbl').textContent = 'Also delete the downloaded files from the disk';
    // the disk-file checkbox applies to the finished entries of the
    // batch; it stays disabled only when none of them is finished
    $('delFileChk').disabled = !targets.some(id => items.get(id).status === 'finished');
  }
  $('delFileChk').checked = false;
  revealModal($('delmodal'));
  $('delConfirm').focus();
}
$('btnDelete').onclick = openDeleteConfirm;
function closeDeleteConfirm() { deleteTargets = null; $('delmodal').classList.add('hidden'); }
$('delModalClose').onclick = $('delCancel').onclick = closeDeleteConfirm;
$('delmodal').addEventListener('click', e => { if (e.target === $('delmodal')) closeDeleteConfirm(); });
$('delConfirm').onclick = async () => {
  const targets = (deleteTargets || []).filter(id => items.has(id));
  deleteTargets = null;
  $('delmodal').classList.add('hidden');
  if (!targets.length) return;
  const delFile = $('delFileChk').checked;
  // the status dialog must not silently retarget to another
  // download when its own entry is deleted — close it instead
  const detailWasShowingIt = !detailEl.classList.contains('hidden') && targets.includes(selectedId);
  if (TAURI) {
    let failed = 0;
    for (const id of targets) {
      const d = items.get(id);
      try {
        // the same remove the single delete uses (it stops an active
        // download first); the disk-file choice only ever applied to
        // finished entries, so it is honoured per entry that way
        await invoke('remove_download', { id, deleteFile: delFile && d && d.status === 'finished' ? true : null });
      } catch (e) {
        console.error(e);
        if (targets.length === 1) { toast(String(e)); return; }
        failed++; // a batch keeps going, like Delete All Completed
        continue;
      }
      deletedIds.add(id); // a late progress emit must not resurrect the row
      items.delete(id);
      selectedIds.delete(id);
      prevStatusById.delete(id);
      completeAnnounced.delete(id);
    }
    if (selectedId && !items.has(selectedId)) selectedId = items.keys().next().value || null;
    if (detailWasShowingIt) detailEl.classList.add('hidden');
    render();
    if (failed) toast(failed + ' download' + (failed > 1 ? 's' : '') + ' could not be deleted');
  } else {
    for (const id of targets) { items.delete(id); selectedIds.delete(id); }
    if (selectedId && !items.has(selectedId)) selectedId = items.keys().next().value || null;
    if (detailWasShowingIt) detailEl.classList.add('hidden');
    render();
  }
};
if ($('btnStatus')) {
  $('btnStatus').onclick = () => {
    if (!selectedId && items.size > 0) selectedId = items.keys().next().value;
    if (!selectedId) {
      // say WHICH nothing: an empty list, or a filter that hides
      // every row (the silent no-op this used to be — F-24)
      toast(items.size > 0 && !visibleItems().length
        ? 'Nothing to show — the current filter hides every download'
        : 'Select a download first', 'info');
      return;
    }
    select(selectedId, true);
  };
}
if ($('dQueueSel')) $('dQueueSel').addEventListener('change', async () => {
  if (!selectedId || !TAURI) return;
  const d = items.get(selectedId);
  try {
    await invoke('set_entry_queue', { id: selectedId, queue: $('dQueueSel').value });
    // keep the local row in sync — without this the Q column and
    // the queue filters kept showing the old queue until the next
    // progress event happened to arrive
    if (d) { d.queue = $('dQueueSel').value; render(); }
  }
  catch (e) { toast('Could not move the download: ' + e); }
});
$('dPause').onclick = $('btnPause').onclick;
$('dResumeBtn').onclick = $('btnResume').onclick;
$('dHide').onclick = () => { detailEl.classList.add('hidden'); showNextComplete(); };
if ($('dModalClose')) $('dModalClose').onclick = () => { detailEl.classList.add('hidden'); showNextComplete(); };
detailEl.addEventListener('click', e => { if (e.target === detailEl) { detailEl.classList.add('hidden'); showNextComplete(); } });
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
    document.querySelectorAll('#queueList li').forEach(x => x.classList.remove('active'));
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
let fileCatTouched = false; // the user picked a category themselves —
                            // the probe must not override their choice
function queueFileLink(payload) {
  const url = payload && payload.url;
  if (!url) return;
  if (presentedFile && presentedFile.url === url) return;
  if (fileLinkQueue.some(q => q.url === url)) return;
  fileLinkQueue.push(payload);
  pumpFileLinks();
}
/* Incoming links (extension, clipboard, basket) must never stack
   their dialog over a dialog the user is already working in — an
   Options or Scheduler edit could be discarded by the surprise
   dialog's Escape. They wait in their queues; a slow pump retries
   while any other modal (except the floating status dialog) is up. */
function anyBlockingModalOpen() {
  return [...document.querySelectorAll('.modal:not(.hidden)')]
    .some(m => m.id !== 'detail');
}
/* One modal layer (N-08): the menu bar renders above the modal
   backdrop, so it stays clickable while a dialog is up — and
   without a guard a second dialog could be opened over the
   first, which then waited underneath as a zombie (an Add URL
   dialog with a stale error survived a whole batch add that
   way). Dialog openers refuse while a DIFFERENT dialog is
   visible. A dialog may always re-open itself (the Scheduler
   re-runs its opener when a queue is created or deleted). The
   floating detail window does not count — dialogs may open over
   it, as before. The styled Confirm and the Refresh-URL dialog
   are exempt by design: they are sub-dialogs of the dialog that
   invoked them, and they settle back to it. */
function anotherModalOpen(selfId) {
  return [...document.querySelectorAll('.modal:not(.hidden)')]
    .some(m => m.id !== 'detail' && m.id !== selfId);
}
function pumpFileLinks() {
  if (anyBlockingModalOpen()) return; // a dialog is busy
  const next = fileLinkQueue.shift();
  if (next) openFileDialog(next);
}
setInterval(() => {
  if ((fileLinkQueue.length || videoLinkQueue.length || grabLinkQueue.length) && !anyBlockingModalOpen()) {
    pumpFileLinks();
    pumpVideoLinks();
    pumpGrabLinks();
  }
  // queued completions surface as soon as no user dialog is in
  // the way (showNextComplete does the yielding check itself)
  if (completeQueue.length && !completeCurrent) showNextComplete();
  // a deferred update dialog (N-08) shows once the way is clear
  if (pendingUpdate && !anotherModalOpen('umodal')) showUpdate(pendingUpdate);
}, 700);

/* Drag & drop onto the main window (IDM parity): the basket's
   exact mechanism — an HTML5 window drop, the same candidate
   extraction (text/uri-list's first non-comment line, else
   text/plain) and the same http(s) gate — feeding the same
   incoming-file queue a basket drop reaches through basket_add,
   so the offer is the same probed File Info dialog under its own
   title and waits behind an open modal like every other incoming
   link. Non-URL drops (random text, local files) are ignored
   silently: a file path is not a download. A drop onto a text
   field keeps its native behaviour — the text lands in the
   field and no offer opens. */
const dropZone = document.querySelector('main.layout');
function dropHighlight(on) {
  if (dropZone) dropZone.style.boxShadow = on ? 'inset 0 0 0 3px #316ac5' : '';
}
function dropOnEditable(e) {
  const t = e.target;
  return !!t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable);
}
for (const ev of ['dragenter', 'dragover']) {
  window.addEventListener(ev, e => {
    if (dropOnEditable(e)) { dropHighlight(false); return; }
    e.preventDefault(); // allows the drop; keeps the webview from navigating
    dropHighlight(true);
  });
}
window.addEventListener('dragleave', e => {
  if (!e.relatedTarget) dropHighlight(false);
});
window.addEventListener('drop', e => {
  dropHighlight(false);
  if (dropOnEditable(e)) return; // native text drop into the field
  e.preventDefault();
  if (!e.dataTransfer) return;
  const uriList = e.dataTransfer.getData('text/uri-list') || '';
  const plain = e.dataTransfer.getData('text/plain') || '';
  const candidate = (uriList.split(/\r?\n/).find(l => l && !l.startsWith('#')) || plain).trim();
  if (!/^https?:\/\//i.test(candidate)) return; // not a web link: no junk entry
  queueFileLink({ url: candidate, title: '', pageUrl: '', cookies: '', source: 'drop' });
});
let fileProbeSeq = 0;
let fileProbeTimer = null;
let fileProbeTotal = 0; // bytes from the last good probe of the CURRENT
                        // address — handed to add_download so a
                        // Download Later entry is born knowing its
                        // size instead of "Unknown" (F-03)

/* validation / add failures belong INSIDE the dialog the user is
   looking at, in plain language — a banner in the main window
   renders behind the modal and reads as raw probe output (F-01/F-02) */
function setFileError(msg) {
  const e = $('fError');
  if (!e) return;
  e.textContent = msg || '';
  e.classList.toggle('hidden', !msg);
  $('inUrl').classList.toggle('invalid', !!msg);
}
function plainAddError(err) {
  const s = String(err);
  if (/bad url|parse|invalid url|not a valid/i.test(s)) {
    return 'That address could not be understood — check it and try again.';
  }
  if (/probe|connect|network|timed? ?out|dns|refused/i.test(s)) {
    return 'NDM could not reach that address — check the address and your connection, then try again.';
  }
  return 'Could not add the download. ' + (s.length > 120 ? s.slice(0, 120) + '…' : s);
}

function resetFileProbeUi() {
  fileProbeTotal = 0;
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
    const login = $('inAuth').checked ? ($('inLogin').value.trim() || null) : null;
    const password = $('inAuth').checked ? ($('inPassword').value || null) : null;
    const info = await invoke('probe_file', { url, cookies, referer, login, password });
    if (mySeq !== fileProbeSeq || $('inUrl').value.trim() !== url) return;
    fileProbeTotal = info.total > 0 ? info.total : 0;
    $('fCat').textContent = info.category || 'General';
    // preselect what the probe detected in the Category dropdown,
    // unless the user already picked a category in this session
    if (!fileCatTouched && $('inCat')) {
      const det = info.category || 'General';
      if (categoryNames().includes(det)) $('inCat').value = det;
    }
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
  setFileError('');
  fileProbeTotal = 0; // the address changed — the old probe is stale
  clearTimeout(fileProbeTimer);
  fileProbeTimer = setTimeout(runFileProbe, 350);
});
$('inAuth').addEventListener('change', () => {
  $('inAuthBox').classList.toggle('hidden', !$('inAuth').checked);
  clearTimeout(fileProbeTimer);
  fileProbeTimer = setTimeout(runFileProbe, 350);
});
if ($('inCat')) $('inCat').addEventListener('change', () => { fileCatTouched = true; });
if ($('inCatAdd')) $('inCatAdd').onclick = async () => {
  const name = (prompt('New category name:') || '').trim();
  if (!name) return;
  const existing = categoryNames().find(c => c.toLowerCase() === name.toLowerCase());
  if (existing) {
    $('inCat').value = existing;
    fileCatTouched = true;
    return;
  }
  // a new category persists as an (initially empty) type list —
  // the same slot the Options editor stores its overrides in, so
  // the name survives restarts and Options saves alike
  settings = {
    ...settings,
    category_exts: { ...(settings.category_exts || {}), [name]: [] },
  };
  await saveSettings();
  fillCategorySelect($('inCat'), name);
  fileCatTouched = true;
};

function openFileDialog(payload) {
  if (anotherModalOpen('modal')) return; // one modal layer (N-08)
  presentedFile = payload
    ? { url: payload.url, cookies: payload.cookies || '', referer: payload.pageUrl || '' }
    : null;
  fillQueueSelect($('inQueue'));
  fillCategorySelect($('inCat'), '');
  fileCatTouched = false;
  if ($('inRememberCat')) $('inRememberCat').checked = false;
  $('inAuth').checked = false;
  $('inAuthBox').classList.add('hidden');
  $('inLogin').value = '';
  $('inPassword').value = '';
  if ($('inDesc')) $('inDesc').value = '';
  // IDM names the clipboard prompt exactly; the basket and a
  // main-window drop get their own wording; everything else is
  // the plain File Info dialog
  if ($('fileModalTitle')) {
    $('fileModalTitle').textContent =
      payload && payload.source === 'clipboard'
        ? 'Download link was found in clipboard. Do you want to download it?'
        : payload && payload.source === 'basket'
          ? 'Link dropped on the basket — download it?'
          : payload && payload.source === 'drop'
            ? 'Link dropped — download it?'
            : 'Download File Info';
  }
  resetFileProbeUi();
  setFileError('');
  if (payload) {
    $('inUrl').value = payload.url;
    $('inName').value = (payload.title || '').split(/[\\/]/).pop() || '';
  } else {
    $('inUrl').value = '';
    $('inName').value = '';
  }
  revealModal(modal);
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
  if (!url) {
    // an empty submit must SAY why nothing happened, inside the
    // dialog — silently staying open reads as a hang (F-01)
    setFileError('Enter a download address first, then press Start Download.');
    $('inUrl').focus();
    return;
  }
  setFileError('');
  const beforeIds = new Set(items.keys()); // for selecting the new row after (F-07)
  const name = $('inName').value.trim() || null;
  const category = ($('inCat') && $('inCat').value) || null;
  const session = presentedFile;
  presentedFile = null; // the new entry owns the session from here
  modal.classList.add('hidden');
  if (TAURI) {
    try {
      const args = {
        url, dir, later, category, name,
        cookies: session ? (session.cookies || null) : null,
        referer: session ? (session.referer || null) : null,
        queue: $('inQueue').value || null,
        login: $('inAuth').checked ? ($('inLogin').value.trim() || null) : null,
        password: $('inAuth').checked ? ($('inPassword').value || null) : null,
        description: ($('inDesc') && $('inDesc').value.trim()) || null,
        // the size the probe just measured for this address — the
        // entry (Download Later above all) is born knowing it
        ...(fileProbeTotal > 0 ? { total: fileProbeTotal } : {}),
      };
      // "Remember this path for this category": the chosen folder
      // becomes that category's own folder for future downloads.
      // With no explicit pick, the category is the one the probe
      // detected (shown in the info box)
      if ($('inRememberCat') && $('inRememberCat').checked) {
        const det = $('fCat').textContent;
        const cat = category || (categoryNames().includes(det) ? det : null);
        if (cat) {
          settings = {
            ...settings,
            category_dirs: { ...(settings.category_dirs || {}), [cat]: dir },
          };
          await saveSettings();
        }
      }
      try {
        await invoke('add_download', args);
      } catch (e) {
        // IDM asks what to do about a duplicate link — so do we
        if (String(e).startsWith('duplicate:') &&
            confirm('This file is already in the download list.\nDownload it again as a new copy?')) {
          await invoke('add_download', { ...args, force: true });
        } else if (String(e).startsWith('duplicate:')) {
          // user chose to keep the existing entry — done, not an error
        } else {
          throw e;
        }
      }
      $('inUrl').value = '';
      $('inName').value = '';
      // the just-added entry becomes the selection (IDM behaviour),
      // so Pause/Stop act on IT — the Start path used to leave the
      // selection on some older row and Pause stayed greyed (F-07).
      // The entry is found by diffing the refreshed list, which
      // works whatever shape add_download returns.
      try {
        const list = await invoke('list_downloads');
        for (const nd of list) if (!deletedIds.has(nd.id)) items.set(nd.id, normalize(nd));
        const fresh = list.find(d => !beforeIds.has(d.id));
        if (fresh) {
          selectedId = fresh.id;
          selectedIds.clear();
          selectedIds.add(fresh.id);
          selectAnchor = fresh.id;
        }
        render();
      } catch (e) { /* selection is a nicety — never fail the add over it */ }
    } catch (e) {
      // reopen with the address intact: a failure must not eat the
      // URL the user typed — they may only need to fix the folder.
      // The reason renders INSIDE the dialog in plain language
      // (the old toast sat hidden behind this modal — F-02).
      console.error('add_download failed', e);
      presentedFile = session;
      revealModal(modal);
      setFileError(plainAddError(e));
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
  if (anyBlockingModalOpen()) return; // a dialog is busy
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
  if (anotherModalOpen('vmodal')) return; // one modal layer (N-08)
  gateComponents();
  presentedVideoUrl = url || null;
  presentedVideoCookies = cookies || '';
  fillQueueSelect($('vInQueue'));
  if ($('vInDesc')) $('vInDesc').value = '';
  revealModal(vmodal);
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
    // the options list is best-first: "Best quality" means the
    // FIRST row. It used to hunt for a 1080p row and silently
    // preselect it over an available 2160p.
    return 0;
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
  if ($('vAll')) $('vAll').disabled = true;
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
      // the badge must name the format the job will really
      // produce: the probe's own ext, else the selector's explicit
      // [ext=…] preference — never an invented container (an audio
      // option used to be badged "MP3" by pure assumption while
      // the job produced an .m4a file, N-09)
      const selExt = /\[ext=([a-z0-9]+)\]/i.exec(o.selector || '');
      const extBadge = (o.ext || (selExt && selExt[1]) || (o.audio ? 'Audio' : 'MP4')).toUpperCase();
      const sizeStr = o.size_text || (o.size_bytes ? fmtSize(o.size_bytes) : (o.selector === 'best' ? 'Auto' : 'Stream'));
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
    if ($('vAll')) $('vAll').disabled = false;
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
  if ($('vAll')) $('vAll').disabled = true;
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
  if (!TAURI) { closeVideoModal(); return; }
  // the dialog stays OPEN until the backend accepts the download —
  // it used to close first, so a failure (bad folder, offline)
  // destroyed the dialog and the user's edited title with it, the
  // exact bug the file dialog's submitAdd was already fixed for
  $('vStart').disabled = true;
  if ($('vLater')) $('vLater').disabled = true;
  if ($('vAll')) $('vAll').disabled = true;
  try {
    const args = {
      url,
      dir,
      selector: opt.selector,
      audio: !!opt.audio,
      title: customTitle,
      cookies,
      later: !!later,
      total: Number(opt.size_bytes) || 0,
      queue: $('vInQueue').value || null,
      description: ($('vInDesc') && $('vInDesc').value.trim()) || null,
    };
    try {
      await invoke('add_video', args);
    } catch (e) {
      // IDM asks what to do about a duplicate link — so do we
      if (String(e).startsWith('duplicate:') &&
          confirm('This video quality is already in the download list.\nDownload it again as a new copy?')) {
        await invoke('add_video', { ...args, force: true });
      } else if (!String(e).startsWith('duplicate:')) {
        throw e;
      }
    }
  } catch (e) {
    console.error('add_video failed', e);
    // inside the dialog, like its Fetch errors — a toast would
    // render behind this modal (same family as F-02)
    $('vStatus').textContent = plainAddError(e);
    $('vStatus').classList.add('error');
    $('vStart').disabled = false;
    if ($('vLater')) $('vLater').disabled = false;
    if ($('vAll')) $('vAll').disabled = false;
    return;
  }
  if (cookies) presentedTokenConsumed = true;
  closeVideoModal();
}
$('vStart').onclick = () => submitVideo(false);
if ($('vLater')) $('vLater').onclick = () => submitVideo(true);
if ($('vAll')) $('vAll').onclick = async () => {
  // IDM's "Download all": every listed quality, each as its own
  // download; ones already in the list are skipped, not duplicated
  if (!vInfo) return;
  const url = $('vInUrl').value.trim();
  if (vInfo.url !== url) return;
  const dir = $('vInDir').value.trim() || '~/Downloads';
  const customTitle = ($('vTitleInput') && $('vTitleInput').value.trim()) || vInfo.title || '';
  const cookies = (presentedVideoUrl && url === presentedVideoUrl)
    ? (presentedVideoCookies || null) : null;
  if (cookies) presentedTokenConsumed = true;
  const queue = $('vInQueue').value || null;
  const options = vInfo.options.slice();
  closeVideoModal();
  if (!TAURI) return;
  let sent = 0;
  let skipped = 0;
  for (const opt of options) {
    try {
      await invoke('add_video', {
        url, dir, selector: opt.selector, audio: !!opt.audio,
        title: customTitle, cookies, later: false,
        total: Number(opt.size_bytes) || 0, queue,
      });
      sent++;
    } catch (e) {
      skipped++;
    }
  }
  toast(`Download all: ${sent} qualities added${skipped ? `, ${skipped} skipped` : ''}`, 'info');
};

/* ---------- Site Logins editor (Options) ---------- */
let loginsEdit = [];
function renderLoginsEdit() {
  const box = $('oLoginList');
  if (!box) return;
  box.innerHTML = '';
  if (!loginsEdit.length) {
    const e = document.createElement('div');
    e.className = 'opt-note';
    e.textContent = 'No saved logins yet.';
    box.appendChild(e);
    return;
  }
  loginsEdit.forEach((l, i) => {
    const row = document.createElement('div');
    row.className = 'login-row';
    const s = document.createElement('span');
    s.className = 'login-site';
    s.textContent = l.site;
    // the cell clips long values — the full text stays one hover away
    s.title = l.site;
    const u = document.createElement('span');
    u.textContent = l.login;
    u.title = l.login;
    const p = document.createElement('span');
    p.textContent = '••••••';
    const del = document.createElement('button');
    del.className = 'tool';
    del.textContent = 'Remove';
    del.onclick = () => { loginsEdit.splice(i, 1); renderLoginsEdit(); };
    row.append(s, u, p, del);
    box.appendChild(row);
  });
}
if ($('oLoginAdd')) $('oLoginAdd').onclick = () => {
  const site = $('oLoginSite').value.trim();
  const login = $('oLoginUser').value.trim();
  const password = $('oLoginPass').value;
  if (!site || !login) { toast('Give the site and the user name first'); return; }
  const existing = loginsEdit.findIndex(l =>
    l.site.toLowerCase() === site.toLowerCase() && l.login.toLowerCase() === login.toLowerCase());
  if (existing >= 0) loginsEdit[existing] = { site, login, password };
  else loginsEdit.push({ site, login, password });
  $('oLoginSite').value = $('oLoginUser').value = $('oLoginPass').value = '';
  renderLoginsEdit();
};

/* ---------- Options modal ---------- */
const omodal = $('omodal');
/* the Options dialog is tabbed like IDM's configuration dialog —
   the existing sections/fields grouped under General / Connection /
   Downloads / Proxy & Site Logins / Categories. Field ids and the
   save/load logic are untouched; only the grouping changed. */
const OPT_PANELS = ['opGeneral', 'opConnection', 'opDownloads', 'opProxy', 'opCategories'];
function showOptionsTab(panelId) {
  document.querySelectorAll('#optTabs .tab').forEach(t => {
    t.classList.toggle('active', t.dataset.opanel === panelId);
  });
  for (const id of OPT_PANELS) {
    const p = $(id);
    if (p) p.classList.toggle('hidden', id !== panelId);
  }
  // the panels scroll internally now (N-02) — the newly shown one
  // starts at its top, and the card gets the same repaint nudge
  // as a fresh reveal: switching to a tall tab used to clip the
  // tab strip's paint the way F-11 clipped the Scheduler's head
  const panel = $(panelId);
  if (panel) panel.scrollTop = 0;
  nudgeRepaint(omodal.querySelector('.modal-card'));
}
document.querySelectorAll('#optTabs .tab').forEach(t => {
  t.onclick = () => showOptionsTab(t.dataset.opanel);
});
/* The proxy fields belong to their mode: the PAC address only
   means something in PAC mode, the server fields only in Manual
   mode. They used to stay editable under every mode (N-01) —
   values typed under "No proxy" were silently ignored when the
   settings composed the live proxy string, yet read as live
   settings. Gating is purely presentational: stored values are
   kept untouched, so switching back to a mode restores its
   fields exactly as they were. The browser-fallback checkbox is
   deliberately not gated — it is a behaviour toggle that applies
   under any mode. */
function syncProxyMode() {
  const mode = (document.querySelector('input[name="proxyMode"]:checked') || {}).value || 'none';
  if ($('oProxyPac')) $('oProxyPac').disabled = mode !== 'pac';
  for (const id of ['oProxyScheme', 'oProxyHost', 'oProxyPort', 'oProxyUser', 'oProxyPass', 'oProxyHttp', 'oProxyHttps', 'oProxyFtp']) {
    const el = $(id);
    if (el) el.disabled = mode !== 'manual';
  }
}
document.querySelectorAll('input[name="proxyMode"]').forEach(r => {
  r.addEventListener('change', syncProxyMode);
});

/* category types are long comma lists — a single-line input cut
   them off mid-list; the textareas grow with their content */
function growCatTypes(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(132, Math.max(38, el.scrollHeight)) + 'px';
}
document.querySelectorAll('.cat-types').forEach(t => {
  t.addEventListener('input', () => growCatTypes(t));
});
function openOptions() {
  if (anotherModalOpen('omodal')) return; // one modal layer (N-08)
  $('oDir').value = settings.default_dir;
  $('oConns').value = settings.max_connections;
  $('oLimitOn').checked = settings.speed_limit_bps > 0;
  $('oLimitVal').value = settings.speed_limit_bps > 0
    ? settings.speed_limit_bps / (1024 * 1024) : 2;
  $('oSimul').value = settings.max_simultaneous;
  $('oAutoResume').checked = settings.auto_resume;
  $('oVideoQ').value = settings.video_quality;
  {
    const mode = settings.proxy_mode
      || (settings.proxy ? 'manual' : 'none');
    document.querySelectorAll('input[name="proxyMode"]').forEach(r => {
      r.checked = r.value === mode;
    });
    $('oProxyPac').value = settings.proxy_pac_url || '';
    $('oProxyScheme').value = settings.proxy_scheme || 'http';
    $('oProxyHost').value = settings.proxy_host || '';
    $('oProxyPort').value = settings.proxy_port || '';
    $('oProxyUser').value = settings.proxy_user || '';
    $('oProxyPass').value = settings.proxy_pass || '';
    $('oProxyHttp').checked = settings.proxy_http !== false;
    $('oProxyHttps').checked = settings.proxy_https !== false;
    $('oProxyFtp').checked = settings.proxy_ftp !== false;
    $('oProxyBrowser').checked = !!settings.proxy_from_browser_on_error;
    syncProxyMode();
  }
  loginsEdit = (settings.site_logins || []).map(l => ({ ...l }));
  renderLoginsEdit();
  $('oPowerWhenDone').value = settings.power_when_done || '';
  $('oPowerForce').checked = !!settings.power_force;
  const cd = settings.category_dirs || {};
  for (const c of ['Compressed', 'Documents', 'Music', 'Video', 'Programs', 'General']) {
    $('oCat' + c).value = cd[c] || '';
  }
  const ce = settings.category_exts || {};
  for (const c of Object.keys(BUILTIN_CATEGORY_EXTS)) {
    $('oCatExt' + c).value = (ce[c] || BUILTIN_CATEGORY_EXTS[c]).join(', ');
  }
  $('oConnExc').value = (settings.conn_exceptions || []).join(', ');
  $('oExitDone').checked = !!settings.exit_when_done;
  $('oSounds').checked = settings.sound_events !== false;
  $('oCompleteDlg').checked = settings.show_complete_dialog !== false;
  $('oClipboard').checked = settings.clipboard_monitor !== false;
  $('oQuotaMb').value = settings.quota_mb || 0;
  $('oQuotaHours').value = settings.quota_hours || 5;
  $('oAutoUpdate').checked = settings.auto_update !== false;
  $('oEngine').textContent = '…';
  if (TAURI) invoke('components_status').then(st => {
    $('oEngine').textContent = st && st.ready
      ? 'Ready (yt-dlp + deno installed)'
      : 'Sets itself up automatically in the background';
  }).catch(() => { $('oEngine').textContent = '—'; });
  showOptionsTab('opGeneral');
  revealModal(omodal);
  // size the category-types textareas to their (just-filled) content
  requestAnimationFrame(() => document.querySelectorAll('.cat-types').forEach(growCatTypes));
}
$('btnOptions').onclick = openOptions;
$('oModalClose').onclick = $('oCancel').onclick = () => omodal.classList.add('hidden');
omodal.addEventListener('click', e => { if (e.target === omodal) omodal.classList.add('hidden'); });
$('oSave').onclick = async () => {
  settings = {
    ...settings,
    default_dir: $('oDir').value.trim() || '~/Downloads',
    max_connections: Math.min(32, Math.max(1, Number($('oConns').value) || 8)),
    speed_limit_bps: $('oLimitOn').checked
      ? Math.round(Math.max(0.1, Number($('oLimitVal').value) || 1) * 1024 * 1024) : 0,
    max_simultaneous: Math.min(10, Math.max(0, Number($('oSimul').value) || 0)),
    auto_resume: $('oAutoResume').checked,
    auto_update: $('oAutoUpdate').checked,
    video_quality: $('oVideoQ').value,
    proxy_mode: (document.querySelector('input[name="proxyMode"]:checked') || {}).value || 'none',
    proxy_pac_url: $('oProxyPac').value.trim(),
    proxy_scheme: $('oProxyScheme').value,
    proxy_host: $('oProxyHost').value.trim(),
    proxy_port: Math.min(65535, Math.max(0, parseInt($('oProxyPort').value, 10) || 0)),
    proxy_user: $('oProxyUser').value.trim(),
    proxy_pass: $('oProxyPass').value,
    proxy_http: $('oProxyHttp').checked,
    proxy_https: $('oProxyHttps').checked,
    proxy_ftp: $('oProxyFtp').checked,
    proxy_from_browser_on_error: $('oProxyBrowser').checked,
    site_logins: loginsEdit.map(l => ({ ...l })),
    power_when_done: $('oPowerWhenDone').value,
    power_force: $('oPowerForce').checked,
    category_dirs: Object.fromEntries(
      ['Compressed', 'Documents', 'Music', 'Video', 'Programs', 'General']
        .map(c => [c, $('oCat' + c).value.trim()])
        .filter(([, v]) => v)),
    // only categories whose type list differs from the built-in
    // table are stored — an untouched editor must not freeze the
    // built-ins forever. A field the user EMPTIED is a difference
    // too: the explicit [] is kept, or the built-in list silently
    // came back on the next open and the category could never be
    // cleared
    category_exts: Object.fromEntries([
      // categories created outside this editor (the File dialog's
      // "+" button) live in category_exts too — keep them across
      // an Options save instead of dropping unknown keys
      ...Object.entries(settings.category_exts || {})
        .filter(([c]) => !(c in BUILTIN_CATEGORY_EXTS)),
      ...Object.keys(BUILTIN_CATEGORY_EXTS)
        .map(c => {
          const list = [...new Set(
            $('oCatExt' + c).value.split(/[,\s]+/)
              .map(s => s.trim().replace(/^\./, '').toLowerCase())
              .filter(Boolean))];
          const builtin = BUILTIN_CATEGORY_EXTS[c];
          const same = list.length === builtin.length
            && list.every(x => builtin.includes(x));
          return [c, same ? null : list];
        })
        .filter(([, v]) => v !== null),
    ]),
    conn_exceptions: $('oConnExc').value.split(',').map(s => s.trim()).filter(Boolean),
    exit_when_done: $('oExitDone').checked,
    sound_events: $('oSounds').checked,
    show_complete_dialog: $('oCompleteDlg').checked,
    clipboard_monitor: $('oClipboard').checked,
    quota_mb: Math.max(0, parseInt($('oQuotaMb').value, 10) || 0),
    quota_hours: Math.min(168, Math.max(1, parseInt($('oQuotaHours').value, 10) || 5)),
  };
  await saveSettings();
  $('inDir').value = settings.default_dir;
  $('vInDir').value = settings.default_dir;
  omodal.classList.add('hidden');
};

/* ---------- Scheduler modal ---------- */
const smodal = $('smodal');
let schedSel = 'Main download queue';
let schedDirty = false; // unsaved edits in the scheduler fields
function schedCfgOf(name) {
  return (settings.queues || []).find(q => q.name === name)
    || { name, use_schedule: false, start: '22:00', stop: '06:00', exit_when_done: false, power_when_done: '', max_simultaneous: 0 };
}
function fillSchedulerFields() {
  const cfg = schedCfgOf(schedSel);
  $('sEnabled').checked = !!cfg.use_schedule;
  $('sStart').value = cfg.start || '22:00';
  $('sStop').value = cfg.stop || '06:00';
  $('schedExitDone').checked = !!cfg.exit_when_done;
  if ($('sPowerWhenDone')) $('sPowerWhenDone').value = cfg.power_when_done || '';
  $('sMax').value = cfg.max_simultaneous || 0;
  $('schedDelete').disabled = schedSel === 'Main download queue';
  schedDirty = false;
  // the staged file order starts from the queue's current order;
  // nothing reaches the backend until Save (F-13)
  schedOrder = [...items.values()]
    .filter(d => (d.queue || 'Main download queue') === schedSel)
    .map(d => d.id);
  renderSchedFiles();
}
/* IDM's Scheduler has a "Files in the queue" tab: the queue's
   downloads in start order, reorderable. The pump claims queued
   entries in list order, so the order shown here IS the priority.
   Reordering is a STAGED edit like the dialog's other fields: the
   arrows only rearrange the local staged order (schedOrder); the
   backend order changes on Save, and the discard confirm really
   does revert an unsaved reorder (F-13). */
let schedOrder = null; // staged entry-id order for the selected queue
function renderSchedFiles() {
  const box = $('sFiles');
  if (!box) return;
  const mine = [...items.values()]
    .filter(d => (d.queue || 'Main download queue') === schedSel);
  if (schedOrder) {
    // order by the staged list; entries that arrived while the
    // dialog is open (not yet staged) keep their natural order
    // at the end, and the staged list resyncs to reality
    const pos = new Map(schedOrder.map((id, i) => [id, i]));
    mine.sort((a, b) => (pos.has(a.id) ? pos.get(a.id) : 1e9)
      - (pos.has(b.id) ? pos.get(b.id) : 1e9));
    schedOrder = mine.map(d => d.id);
  }
  box.innerHTML = '';
  if (!mine.length) {
    const e = document.createElement('div');
    e.className = 's-empty';
    e.textContent = 'No downloads in this queue';
    box.appendChild(e);
    return;
  }
  mine.forEach((d, i) => {
    const row = document.createElement('div');
    row.className = 's-file-row';
    const nm = document.createElement('span');
    nm.className = 's-name';
    nm.textContent = d.name || d.url;
    nm.title = d.url;
    const st = document.createElement('span');
    st.className = 's-st';
    st.textContent = cap(d.status || '');
    const move = upWanted => {
      if (!schedOrder) return;
      const j = upWanted ? i - 1 : i + 1;
      if (j < 0 || j >= schedOrder.length) return;
      [schedOrder[i], schedOrder[j]] = [schedOrder[j], schedOrder[i]];
      schedDirty = true;
      renderSchedFiles();
    };
    const up = document.createElement('button');
    up.textContent = '↑';
    up.title = 'Start earlier';
    up.disabled = i === 0;
    up.onclick = () => move(true);
    const dn = document.createElement('button');
    dn.textContent = '↓';
    dn.title = 'Start later';
    dn.disabled = i === mine.length - 1;
    dn.onclick = () => move(false);
    row.append(nm, st, up, dn);
    box.appendChild(row);
  });
}
function openScheduler() {
  if (anotherModalOpen('smodal')) return; // one modal layer (N-08); self re-open stays legal
  fillQueueSelect($('schedQueueSel'), schedSel);
  schedSel = $('schedQueueSel').value || 'Main download queue';
  fillSchedulerFields();
  $('sStatus').textContent = '';
  revealModal(smodal);
}
$('schedQueueSel').addEventListener('change', async () => {
  // switching queues must not throw away unsaved edits silently —
  // ask with the app's own dialog, holding the select on the old
  // queue while the question is up
  const target = $('schedQueueSel').value;
  if (schedDirty) {
    $('schedQueueSel').value = schedSel;
    const ok = await showConfirm(`Discard the unsaved changes for "${schedSel}"?`, 'Discard');
    if (!ok) return;
  }
  schedSel = target;
  $('schedQueueSel').value = target;
  fillSchedulerFields();
});
for (const fid of ['sEnabled', 'sStart', 'sStop', 'schedExitDone', 'sMax', 'sPowerWhenDone']) {
  $(fid).addEventListener('change', () => { schedDirty = true; });
  $(fid).addEventListener('input', () => { schedDirty = true; });
}
$('schedNew').onclick = async () => {
  // the same dirty guard as the queue switch: creating a queue
  // refills every field, so unsaved edits must be confirmed first
  if (schedDirty && !await showConfirm(`Discard the unsaved changes for "${schedSel}"?`, 'Discard')) return;
  const name = $('schedNewName').value.trim();
  if (!name) { $('sStatus').textContent = 'Type a name for the new queue first'; return; }
  if ((settings.queues || []).some(q => q.name.toLowerCase() === name.toLowerCase())) {
    $('sStatus').textContent = 'A queue with that name already exists';
    return;
  }
  settings = {
    ...settings,
    queues: [...(settings.queues || []),
      { name, use_schedule: false, start: '22:00', stop: '06:00', exit_when_done: false, power_when_done: '', max_simultaneous: 0 }],
  };
  await saveSettings();
  schedSel = name;
  $('schedNewName').value = '';
  openScheduler();
  $('sStatus').textContent = `Queue "${name}" created`;
};
$('schedDelete').onclick = async () => {
  if (schedSel === 'Main download queue') return;
  // same dirty guard as the queue switch (deleting refills the fields)
  if (schedDirty && !await showConfirm(`Discard the unsaved changes for "${schedSel}"?`, 'Discard')) return;
  const gone = schedSel;
  if (!await showConfirm(`Delete the queue "${gone}"?\nIts downloads move to the Main download queue.`, 'Delete queue')) return;
  settings = {
    ...settings,
    queues: (settings.queues || []).filter(q => q.name !== gone),
  };
  await saveSettings();
  schedSel = 'Main download queue';
  openScheduler();
  // the sidebar may be filtered to the queue that just vanished —
  // fall back to All Downloads instead of an empty phantom filter
  if (currentCat === 'queue:' + gone) {
    currentCat = 'All Downloads';
    document.querySelectorAll('#queueList li').forEach(x => x.classList.remove('active'));
    document.querySelectorAll('#catList li').forEach(x =>
      x.classList.toggle('active', x.dataset.cat === 'All Downloads'));
  }
  // entries the backend moved to Main need their local queue updated
  for (const d of items.values()) {
    if ((d.queue || 'Main download queue') === gone) d.queue = 'Main download queue';
  }
  queueListSig = '';
  render();
};
$('btnScheduler').onclick = openScheduler;
$('sModalClose').onclick = $('sCancel').onclick = () => smodal.classList.add('hidden');
smodal.addEventListener('click', e => { if (e.target === smodal) smodal.classList.add('hidden'); });
$('sStartNow').onclick = async () => {
  if (TAURI) {
    try { await invoke('scheduler_start_now', { queue: schedSel }); }
    catch (e) { console.error(e); $('sStatus').textContent = 'Could not start the queue: ' + e; return; }
  }
  $('sStatus').textContent = `Queue "${schedSel}" started`;
};
$('sStopNow').onclick = async () => {
  if (TAURI) {
    try { await invoke('scheduler_stop_now', { queue: schedSel }); }
    catch (e) { console.error(e); $('sStatus').textContent = 'Could not stop the queue: ' + e; return; }
  }
  $('sStatus').textContent = `Queue "${schedSel}" paused`;
};
$('sSave').onclick = async () => {
  // write the edited window back into the selected queue; the
  // legacy global scheduler fields mirror the Main queue (backend
  // sanitize keeps them in sync too)
  const queues = (settings.queues || []).map(q => q.name === schedSel
    ? {
        ...q,
        use_schedule: $('sEnabled').checked,
        start: $('sStart').value || '22:00',
        stop: $('sStop').value || '06:00',
        exit_when_done: $('schedExitDone').checked,
        power_when_done: $('sPowerWhenDone') ? $('sPowerWhenDone').value : (q.power_when_done || ''),
        max_simultaneous: Math.min(10, Math.max(0, parseInt($('sMax').value, 10) || 0)),
      }
    : q);
  settings = { ...settings, queues };
  const main = queues.find(q => q.name === 'Main download queue');
  if (main) {
    settings = {
      ...settings,
      scheduler_enabled: !!main.use_schedule,
      sched_start: main.start,
      sched_stop: main.stop,
    };
  }
  await saveSettings();
  // apply the staged file order (arrows only rearranged it locally
  // until now): walk the target order, moving each out-of-place
  // entry up into its slot with the same move_in_queue the dialog
  // used to call per click
  if (TAURI && schedOrder) {
    try {
      const list = await invoke('list_downloads');
      const cur = list
        .filter(d => (d.queue || 'Main download queue') === schedSel)
        .map(d => d.id)
        .filter(id => schedOrder.includes(id));
      const target = schedOrder.filter(id => cur.includes(id));
      for (let i = 0; i < target.length; i++) {
        if (cur[i] === target[i]) continue;
        const j = cur.indexOf(target[i], i);
        for (let k = j; k > i; k--) {
          await invoke('move_in_queue', { id: target[i], up: true });
        }
        cur.splice(j, 1);
        cur.splice(i, 0, target[i]);
      }
      const fresh = await invoke('list_downloads');
      items.clear();
      for (const nd of fresh) if (!deletedIds.has(nd.id)) items.set(nd.id, normalize(nd));
      render();
    } catch (e) {
      // the settings saved but the order did not — keep the dialog
      // open and say so, rather than closing over a half-save
      $('sStatus').textContent = 'Could not save the file order: ' + e;
      return;
    }
  }
  schedDirty = false;
  // Save closes the dialog, like Options (F-12) — a lingering
  // "Saved" note left users unsure whether saving had finished
  smodal.classList.add('hidden');
};

/* ---------- Site Grabber wizard (IDM's 4-step Grabber + projects) ---------- */
const gmodal = $('gmodal');
let grabItems = [];              // items from the last exploration
const grabUnchecked = new Set(); // ROW INDICES the user unchecked (items start
                                 // checked) — keyed by index, not URL: two links
                                 // to the same URL used to share one checkbox
const grabKindsOff = new Set();  // kinds hidden via their filter chip
let grabStep = 1;                // 1..4 wizard steps, 5 = results

const GRAB_TEMPLATES = {
  custom: {},
  pictures: { include: ['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg', 'ico', 'tif', 'tiff'], depth_same: 10, whole: true },
  audio: { include: ['mp3', 'm4a', 'm4b', 'flac', 'wav', 'ogg', 'opus', 'aac', 'wma'], depth_same: 1 },
  video: { include: ['mp4', 'mkv', 'webm', 'avi', 'mov', 'flv', 'wmv', 'm4v', 'ts'], depth_same: 10, whole: true },
  all_except_pages: { exclude: ['html', 'htm', 'php', 'asp', 'aspx', 'png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg'], depth_same: 10, whole: true },
  all_files: { depth_same: 10, whole: true },
  complete_page: { include: ['html', 'htm', 'js', 'css', 'png', 'jpg', 'jpeg', 'gif', 'svg', 'webp', 'ico', 'woff', 'woff2', 'ttf'], depth_same: 0 },
};
const GRAB_PRESETS = {
  images: ['gif', 'jpg', 'jpeg', 'jpe', 'bmp', 'png', 'webp', 'svg', 'ico', 'tif', 'tiff'],
  video: ['mp4', 'mkv', 'webm', 'avi', 'mov', 'flv', 'wmv', 'm4v', 'ts'],
  audio: ['mp3', 'm4a', 'm4b', 'flac', 'wav', 'ogg', 'opus', 'aac', 'wma'],
  pdf: ['pdf'],
  exe: ['exe', 'msi', 'deb', 'rpm', 'dmg', 'pkg', 'appimage', 'apk', 'bin', 'run'],
  compressed: ['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'tgz'],
  page_assets: ['html', 'htm', 'js', 'css', 'png', 'jpg', 'jpeg', 'gif', 'svg', 'ico'],
};

function setGrabStatus(msg, isError) {
  const st = $('gStatus');
  st.textContent = msg;
  st.classList.toggle('error', !!isError);
}

const GRAB_STEP_TITLES = {
  1: 'Site Grabber — Step 1: Set start page',
  2: 'Site Grabber — Step 2: Save files to',
  3: 'Site Grabber — Step 3: Set explorer filters',
  4: 'Site Grabber — Step 4: Set file filters',
  5: 'Site Grabber — Exploration results',
};
function showGrabStep(n) {
  grabStep = n;
  for (const i of [1, 2, 3, 4]) $('gStep' + i).classList.toggle('hidden', n !== i);
  $('gStepR').classList.toggle('hidden', n !== 5);
  document.querySelectorAll('.g-step').forEach(el => {
    el.classList.toggle('on', Number(el.dataset.step) === n);
  });
  $('gStepTitle').textContent = GRAB_STEP_TITLES[n];
  $('gBack').disabled = n === 1;
  $('gNext').classList.toggle('hidden', n >= 4);
  $('gFetch').classList.toggle('hidden', n !== 4);
  $('gDownload').classList.toggle('hidden', n !== 5);
  if (n === 1) $('gUrl').focus();
}
$('gBack').onclick = () => showGrabStep(Math.max(1, grabStep - 1));
$('gNext').onclick = () => {
  if (grabStep === 1 && !$('gUrl').value.trim()) { $('gUrl').focus(); return; }
  showGrabStep(Math.min(4, grabStep + 1));
};

function fillGrabProjects() {
  const sel = $('gProjSel');
  sel.innerHTML = '';
  const none = document.createElement('option');
  none.value = '';
  none.textContent = '(no saved project selected)';
  sel.appendChild(none);
  for (const p of settings.grab_projects || []) {
    const o = document.createElement('option');
    o.value = p.name;
    o.textContent = p.name;
    sel.appendChild(o);
  }
}
function grabWizardState() {
  const mode = (document.querySelector('input[name="gExploreMode"]:checked') || {}).value || 'levels';
  return {
    name: $('gProjName').value.trim(),
    start_url: $('gUrl').value.trim(),
    template: $('gTemplate').value,
    save_dir: $('gDir').value.trim(),
    use_subfolders: $('gSubfolders').checked,
    add_as_queue: $('gAddQueue').checked,
    depth_same: Math.min(10, Math.max(0, parseInt($('gDepthSame').value, 10) || 0)),
    depth_other: Math.min(10, Math.max(0, parseInt($('gDepthOther').value, 10) || 0)),
    whole_site: mode === 'whole',
    no_parent: $('gNoParent').checked,
    same_site_files_only: $('gSameSiteOnly').checked,
    include_exts: $('gInclExts').value.split(/[,\s]+/).map(s => s.trim().replace(/^\./, '').toLowerCase()).filter(Boolean),
    exclude_exts: $('gExclExts').value.split(/[,\s]+/).map(s => s.trim().replace(/^\./, '').toLowerCase()).filter(Boolean),
    min_kb: Math.max(0, parseInt($('gMinKb').value, 10) || 0),
    max_mb: Math.max(0, parseInt($('gMaxMb').value, 10) || 0),
  };
}
function applyGrabProject(p) {
  if (!p) return;
  $('gProjName').value = p.name || '';
  $('gUrl').value = p.start_url || '';
  $('gTemplate').value = p.template || 'custom';
  $('gDir').value = p.save_dir || settings.default_dir || '~/Downloads';
  $('gSubfolders').checked = !!p.use_subfolders;
  $('gAddQueue').checked = !!p.add_as_queue;
  $('gDepthSame').value = p.depth_same != null ? p.depth_same : 1;
  $('gDepthOther').value = p.depth_other != null ? p.depth_other : 0;
  document.querySelectorAll('input[name="gExploreMode"]').forEach(r => {
    r.checked = r.value === (p.whole_site ? 'whole' : 'levels');
  });
  $('gNoParent').checked = !!p.no_parent;
  $('gSameSiteOnly').checked = p.same_site_files_only !== false;
  $('gInclExts').value = (p.include_exts || []).join(', ');
  $('gExclExts').value = (p.exclude_exts || []).join(', ');
  $('gMinKb').value = p.min_kb || 0;
  $('gMaxMb').value = p.max_mb || 0;
}
$('gProjSave').onclick = async () => {
  const p = grabWizardState();
  if (!p.name) { $('gProjName').focus(); toast('Give the project a name first'); return; }
  if (!p.start_url) { $('gUrl').focus(); toast('Give the project a start page first'); return; }
  const list = (settings.grab_projects || []).filter(x => x.name.toLowerCase() !== p.name.toLowerCase());
  list.push(p);
  settings = { ...settings, grab_projects: list };
  await saveSettings();
  fillGrabProjects();
  $('gProjSel').value = p.name;
  toast(`Grabber project "${p.name}" saved`, 'info');
};
$('gProjLoad').onclick = () => {
  const name = $('gProjSel').value;
  const p = (settings.grab_projects || []).find(x => x.name === name);
  if (p) { applyGrabProject(p); toast(`Project "${name}" loaded`, 'info'); }
};
$('gProjDelete').onclick = async () => {
  const name = $('gProjSel').value;
  if (!name) return;
  settings = {
    ...settings,
    grab_projects: (settings.grab_projects || []).filter(x => x.name !== name),
  };
  await saveSettings();
  fillGrabProjects();
  toast(`Project "${name}" deleted`, 'info');
};
/* choosing a template pre-fills the filters and the explore depth,
   exactly what IDM's template dropdown does on Step 1 */
$('gTemplate').addEventListener('change', () => {
  const t = GRAB_TEMPLATES[$('gTemplate').value] || {};
  if (t.include) $('gInclExts').value = t.include.join(', ');
  if (t.exclude) $('gExclExts').value = t.exclude.join(', ');
  if (t.whole) {
    document.querySelectorAll('input[name="gExploreMode"]').forEach(r => {
      r.checked = r.value === 'whole';
    });
  } else {
    // a depth-limited template must also switch the mode back —
    // it used to only ever SET whole-site, so "Audio, 1 level"
    // chosen after a whole-site template crawled the entire site
    document.querySelectorAll('input[name="gExploreMode"]').forEach(r => {
      r.checked = r.value === 'levels';
    });
  }
  if (t.depth_same != null) $('gDepthSame').value = t.depth_same;
});
$('gInclPreset').addEventListener('change', () => {
  const list = GRAB_PRESETS[$('gInclPreset').value];
  if (list) $('gInclExts').value = list.join(', ');
});

function openGrabber() {
  if (anotherModalOpen('gmodal')) return; // one modal layer (N-08)
  grabItems = [];
  grabUnchecked.clear();
  grabKindsOff.clear();
  $('gUrl').value = '';
  $('gProjName').value = '';
  $('gDir').value = settings.default_dir || '~/Downloads';
  $('gFilter').value = '';
  $('gKinds').innerHTML = '';
  $('gList').innerHTML = '';
  setGrabStatus('The exploration results will appear here');
  updateGrabCount();
  fillGrabProjects();
  showGrabStep(1);
  revealModal(gmodal);
}
$('btnGrabber').onclick = openGrabber;

/* Pages handed over for grabbing (the extension's grab-all-links)
   wait in this queue like file/video links while another dialog is
   busy; when pumped, the wizard opens with the address filled in
   and the exploration already running */
const grabLinkQueue = [];
function queueGrabLink(payload) {
  const url = payload && payload.url;
  if (!url) return;
  if (grabLinkQueue.some(q => q.url === url)) return;
  grabLinkQueue.push({ url });
  pumpGrabLinks();
}
function pumpGrabLinks() {
  if (anyBlockingModalOpen()) return; // a dialog is busy
  const next = grabLinkQueue.shift();
  if (!next) return;
  openGrabber();
  $('gUrl').value = next.url;
  fetchGrab();
}
$('gModalClose').onclick = $('gCancel').onclick = () => gmodal.classList.add('hidden');
gmodal.addEventListener('click', e => { if (e.target === gmodal) gmodal.classList.add('hidden'); });

function grabVisible(it) {
  if (grabKindsOff.has(it.kind)) return false;
  const q = $('gFilter').value.trim().toLowerCase();
  return !q || `${it.name} ${it.url}`.toLowerCase().includes(q);
}

function updateGrabCount() {
  const sel = grabItems.filter((it, i) => !grabUnchecked.has(i));
  const vis = sel.filter(grabVisible).length;
  // the selection is global — it survives filter changes, like IDM's
  // grabber — so say plainly when part of it is filtered out of view
  $('gCount').textContent = vis === sel.length
    ? `${sel.length} selected`
    : `${sel.length} selected (${vis} in current view)`;
  // and the button itself carries the true count it will download:
  // a hidden-but-checked file must never be a surprise (F-25)
  $('gDownload').textContent = `Download Selected (${sel.length})`;
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
  for (const [i, it] of grabItems.entries()) {
    if (!grabVisible(it)) continue;
    const row = document.createElement('div');
    row.className = 'grab-item';
    row.title = it.url;
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = !grabUnchecked.has(i);
    cb.addEventListener('change', () => {
      if (cb.checked) grabUnchecked.delete(i); else grabUnchecked.add(i);
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
  const p = grabWizardState();
  if (!p.start_url) { showGrabStep(1); $('gUrl').focus(); return; }
  showGrabStep(5);
  if (!TAURI) { demoGrab(); return; }
  $('gFetch').disabled = true;
  setGrabStatus('Exploring…');
  try {
    // struct arguments cross IPC with their serde (snake_case) field
    // names — the same convention the Settings object already uses
    const res = await invoke('grab_explore', {
      args: {
        url: p.start_url,
        depth_same: p.depth_same,
        depth_other: p.depth_other,
        whole_site: p.whole_site,
        no_parent: p.no_parent,
        same_site_files_only: p.same_site_files_only,
        include_exts: p.include_exts,
        exclude_exts: p.exclude_exts,
        min_kb: p.min_kb,
        max_mb: p.max_mb,
      },
    });
    grabItems = res.items || [];
    grabUnchecked.clear();
    grabKindsOff.clear();
    renderGrabKinds();
    renderGrabList();
    setGrabStatus(`Explored ${res.pages_visited} page${res.pages_visited === 1 ? '' : 's'} — found ${grabItems.length} files on "${res.title || p.start_url}"`);
  } catch (e) {
    setGrabStatus(String(e), true);
  } finally {
    $('gFetch').disabled = false;
  }
}
$('gFetch').onclick = fetchGrab;
$('gUrl').addEventListener('keydown', e => {
  if (e.key !== 'Enter') return;
  // the same empty-URL guard as the Next button — Enter used to
  // jump to Step 2 unconditionally
  if (!$('gUrl').value.trim()) { $('gUrl').focus(); return; }
  showGrabStep(2);
});
$('gFilter').addEventListener('input', renderGrabList);

/* Select All / Select None mean the ENTIRE result set — the
   category chips and the text filter only change the VIEW, never
   the selection's scope (F-25) */
$('gSelAll').onclick = () => {
  grabUnchecked.clear();
  renderGrabList();
};
$('gSelNone').onclick = () => {
  grabItems.forEach((it, i) => { grabUnchecked.add(i); });
  renderGrabList();
};

/* a grabbed file keeps its place on the site when "use original
   relative subfolders" is on: <save dir>/<the file's folders below
   the start page's own folder> — relative to where the grab
   started, not to the host root (grabbing …/site/ must not add a
   "site" level IDM does not create) */
function grabDirFor(baseDir, startUrl, itemUrl) {
  try {
    const u = new URL(itemUrl);
    const parts = u.pathname.split('/').filter(Boolean);
    parts.pop(); // the file name itself
    try {
      const s = new URL(startUrl);
      if (s.host === u.host) {
        const root = s.pathname.split('/').filter(Boolean);
        if (!s.pathname.endsWith('/')) root.pop(); // the start page itself
        if (root.every((seg, i) => parts[i] === seg)) parts.splice(0, root.length);
      }
    } catch (e) { /* unparsable start URL: keep the item's full path */ }
    if (!parts.length) return baseDir;
    const sep = baseDir.includes('\\') && !baseDir.includes('/') ? '\\' : '/';
    return baseDir.replace(/[/\\]+$/, '') + sep + parts.join(sep);
  } catch (e) {
    return baseDir;
  }
}

$('gDownload').onclick = async () => {
  const chosen = grabItems.filter((it, i) => !grabUnchecked.has(i));
  if (!chosen.length) return;
  if (!TAURI) { gmodal.classList.add('hidden'); return; }
  const p = grabWizardState();
  const baseDir = p.save_dir || '~/Downloads';
  $('gDownload').disabled = true;
  let failed = 0;
  let firstErr = '';
  for (const it of chosen) {
    const dir = p.use_subfolders ? grabDirFor(baseDir, p.start_url, it.url) : baseDir;
    try {
      await invoke('add_download', {
        url: it.url, dir, later: p.add_as_queue, category: 'Grabber',
      });
    }
    catch (e) { failed++; if (!firstErr) firstErr = String(e); console.error('add_download failed', it.url, e); }
  }
  if (failed) {
    // keep the modal open so the actual reason can be read, not just
    // counted — in plain language: a raw internal id ("duplicate:
    // dl-16") tells the user nothing
    const why = firstErr.startsWith('duplicate')
      ? 'already in the download list'
      : (firstErr.length > 90 ? firstErr.slice(0, 90) + '…' : firstErr);
    setGrabStatus(`${failed} of ${chosen.length} could not be added — ${why}`, true);
    $('gDownload').disabled = false;
    return;
  }
  gmodal.classList.add('hidden');
};

/* demo data so the grabber is fully previewable in a plain browser.
   The wizard's own filters still apply — templates write their
   extension lists into the Step 4 fields, so filtering by the
   wizard state exercises the template wiring too */
function demoGrab() {
  const base = $('gUrl').value.trim().replace(/\/+$/, '');
  const p = grabWizardState();
  const all = [
    { url: `${base}/brochure-2026.pdf`, name: 'brochure-2026.pdf', kind: 'Documents', size: 4821760, label: 'Company brochure (PDF)' },
    { url: `${base}/theme-song.mp3`, name: 'theme-song.mp3', kind: 'Music', size: 8388608, label: '' },
    { url: `${base}/assets-backup.zip`, name: 'assets-backup.zip', kind: 'Compressed', size: null, label: 'Full asset backup' },
    { url: `${base}/promo-clip.mp4`, name: 'promo-clip.mp4', kind: 'Video', size: 157286400, label: 'Promo clip — 1080p' },
  ];
  grabItems = all.filter(it => {
    const ext = (it.name.split('.').pop() || '').toLowerCase();
    if (p.include_exts.length && !p.include_exts.includes(ext)) return false;
    if (p.exclude_exts.includes(ext)) return false;
    if (it.size != null) {
      if (p.min_kb > 0 && it.size < p.min_kb * 1024) return false;
      if (p.max_mb > 0 && it.size > p.max_mb * 1024 * 1024) return false;
    }
    return true;
  });
  grabUnchecked.clear();
  grabKindsOff.clear();
  renderGrabKinds();
  renderGrabList();
  setGrabStatus(`Explored 3 pages — found ${grabItems.length} files on "Demo Page"`);
}


/* ---------- detail tabs + completion options + speed limiter + open/refresh ---------- */
function syncLimiterPanel() {
  // IDM's Speed Limiter tab is PER DOWNLOAD — it used to read and
  // write the GLOBAL limit, so "limiting this download" silently
  // throttled everything (and the label disagreed with Options)
  const d = items.get(selectedId);
  const bps = (d && d.speed_limit) || 0;
  if ($('dLimitOn')) $('dLimitOn').checked = bps > 0;
  if ($('dLimitVal')) {
    $('dLimitVal').value = bps > 0
      ? (bps / (1024 * 1024)).toFixed(1).replace(/\.0$/, '')
      : '2';
  }
}

function showDetailTab(which) {
  $('tabStatus').classList.toggle('active', which === 'status');
  $('tabCompletion').classList.toggle('active', which === 'completion');
  if ($('tabLimiter')) $('tabLimiter').classList.toggle('active', which === 'limiter');
  $('panelStatus').classList.toggle('hidden', which !== 'status');
  $('panelCompletion').classList.toggle('hidden', which !== 'completion');
  if ($('panelLimiter')) $('panelLimiter').classList.toggle('hidden', which !== 'limiter');
  if (which === 'limiter') syncLimiterPanel();
}
$('tabStatus').onclick = () => showDetailTab('status');
$('tabCompletion').onclick = () => showDetailTab('completion');
if ($('tabLimiter')) $('tabLimiter').onclick = () => showDetailTab('limiter');

if ($('dLimitApply')) {
  $('dLimitApply').onclick = async () => {
    const d = items.get(selectedId);
    if (!d) return;
    const on = $('dLimitOn').checked;
    const valMb = Math.max(0.1, Number($('dLimitVal').value) || 1);
    const bps = on ? Math.round(valMb * 1024 * 1024) : 0;
    if (TAURI) {
      try { await invoke('set_entry_limit', { id: d.id, limit: bps }); }
      catch (e) { toast('Could not set the limit: ' + e); return; }
    }
    d.speed_limit = bps;
    toast(on ? `This download is limited to ${valMb} MB/s` : 'Per-download speed limit removed', 'info');
  };
}

/* description edits save on change (IDM's Properties field) */
if ($('dDesc')) {
  $('dDesc').addEventListener('change', async () => {
    const d = items.get(selectedId);
    if (!d) return;
    const text = $('dDesc').value.trim();
    if ((d.description || '') === text) return;
    if (TAURI) {
      try { await invoke('set_entry_description', { id: d.id, description: text }); }
      catch (e) { toast('Could not save the description: ' + e); return; }
    }
    d.description = text;
    render();
  });
}

document.querySelectorAll('input[name="compAct"]').forEach(r => {
  r.addEventListener('change', async () => {
    const d = items.get(selectedId);
    if (!d) return;
    if (TAURI) {
      try { await invoke('set_completion', { id: d.id, action: r.value }); }
      catch (e) {
        console.error(e); toast(String(e));
        // the backend never saved it — put the radios back on the
        // value the entry actually has instead of showing a lie
        const prev = d.completion || '';
        document.querySelectorAll('input[name="compAct"]').forEach(x => {
          x.checked = x.value === prev;
        });
        return;
      }
    }
    d.completion = r.value;
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
  revealModal(rmodal);
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
  if (e.key === 'Delete' || e.key === 'Del') {
    const active = document.activeElement;
    if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA')) return;
    if ((selectedId || selectedIds.size) && !document.querySelector('.modal:not(.hidden)')) {
      $('btnDelete').click();
    }
    return;
  }
  if ((e.ctrlKey || e.metaKey) && !e.shiftKey && e.key.toLowerCase() === 'a') {
    // Ctrl+A marks every visible download (IDM does the same) — the
    // selection Delete then confirms once for the whole batch
    const active = document.activeElement;
    if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA')) return;
    if (document.querySelector('.modal:not(.hidden)')) return;
    e.preventDefault();
    const list = visibleItems();
    selectedIds.clear();
    for (const d of list) selectedIds.add(d.id);
    if (!selectedId || !selectedIds.has(selectedId)) selectedId = list.length ? list[0].id : null;
    render();
    return;
  }
  if (e.key !== 'Escape') return;
  // menus and the context menu are the topmost layer
  if (!ctxMenu.classList.contains('hidden')) { hideCtxMenu(); return; }
  if ([...document.querySelectorAll('.menu-drop')].some(d => !d.classList.contains('hidden'))) {
    closeMenus();
    return;
  }
  const open = [...document.querySelectorAll('.modal:not(.hidden)')]
    // a mandatory update dialog cannot be dismissed, not even by Esc
    .filter(m => !(m.id === 'umodal' && m.dataset.mandatory === '1'));
  if (!open.length) return;
  const top = open[open.length - 1];
  // the styled confirm must SETTLE (as a "no"), not just hide —
  // hiding it would leave the awaiting guard pending forever
  if (top.id === 'cfmodal') { settleConfirm(false); return; }
  // the video and Add dialogs own cleanup — a captured browser
  // session must be discarded and the queue pumped — so Escape goes
  // through the same close path as their Cancel buttons
  if (top.id === 'vmodal') { closeVideoModal(); return; }
  if (top.id === 'modal') { closeFileDialog(); return; }
  // the completion dialog owns a QUEUE: hiding it directly (the old
  // behaviour) left completeCurrent set forever, so no completion
  // dialog ever appeared again for the rest of the session
  if (top.id === 'cmodal') { closeComplete(null); return; }
  top.classList.add('hidden');
  // closing a dialog may un-block a queued completion dialog
  showNextComplete();
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
  await listen('quota_exceeded', () => {
    toast(`Download limit reached (${settings.quota_mb} MB per ${settings.quota_hours} h). Downloads are paused until the next period.`);
  });
  await listen('download_complete', async e => {
    const p = e.payload || {};
    // the render-side detector (N-03) may have announced this
    // finish already — the real event is then the duplicate and
    // must not queue a second dialog for the same finish
    if (p.id && completeAnnounced.has(p.id)) {
      // fall through: the list refresh below still applies
    } else {
      if (p.id) completeAnnounced.add(p.id);
      completeQueue.push(p);
    }
    // make the finish land in the list/detail even if this entry's
    // last progress payload never arrived — one authoritative pull
    // (F-16: a row frozen at "Paused 67%" after finishing under an
    // open detail dialog)
    try {
      const list = await invoke('list_downloads');
      for (const nd of list) if (!deletedIds.has(nd.id)) items.set(nd.id, normalize(nd));
      scheduleRender();
    } catch (err) { /* the progress events usually carry it */ }
    showNextComplete();
  });
  // while the detail dialog is open it (and the list behind it)
  // must keep tracking the live state: re-sync from the backend
  // every few seconds so a dropped payload can never freeze the
  // displayed state again (F-16/F-17 UI half — the engine-side
  // stall that caused it is a backend finding)
  setInterval(async () => {
    if (detailEl.classList.contains('hidden')) return;
    try {
      const list = await invoke('list_downloads');
      for (const nd of list) if (!deletedIds.has(nd.id)) items.set(nd.id, normalize(nd));
      scheduleRender();
    } catch (err) { /* next tick retries */ }
  }, 4000);
  await listen('components_progress', e => { onComponentsProgress(e.payload || {}); });
  await listen('update_available', e => { showUpdate(e.payload || {}); });
  await listen('update_installed', e => {
    const v = (e.payload && e.payload.version) || '';
    toast(v ? `NDM ${v} is installed — restart NDM to use it.` : 'Update installed — restart NDM to use it.', 'info');
  });
  await listen('update_progress', e => { onUpdateProgress(e.payload || {}); });
  // an entry removed on the backend's side (the extension's host
  // path cancelling/discarding it) leaves the list too — without
  // this the row stayed behind as a ghost the backend no longer
  // knew about (X-REF). The tombstone keeps the progress/render
  // paths from resurrecting it from a late payload.
  await listen('entry_removed', e => {
    const id = e.payload.id;
    deletedIds.add(id);
    items.delete(id);
    render();
  });
  // file downloads caught by the extension open the Add URL dialog
  await listen('incoming_file', e => {
    invoke('take_pending_files').catch(() => {});
    queueFileLink(e.payload || {});
  });
  try {
    const pendingFiles = await invoke('take_pending_files');
    for (const p of pendingFiles || []) queueFileLink(p);
  } catch (e) { console.error('take_pending_files failed', e); }
  // a page sent for grabbing opens the Site Grabber on its results
  await listen('incoming_grab', e => {
    invoke('take_pending_grabs').catch(() => {});
    queueGrabLink(e.payload || {});
  });
  try {
    const pendingGrabs = await invoke('take_pending_grabs');
    for (const p of pendingGrabs || []) queueGrabLink(p);
  } catch (e) { console.error('take_pending_grabs failed', e); }
  render();
}
function normalize(d) {
  return { resume_capability: null, blocks: '', added: '', ...d };
}

/* ---------- Download Complete dialog (IDM shows one by default) -- */
const completeQueue = [];
let completeCurrent = null;
function showNextComplete() {
  const cm = $('cmodal');
  if (!cm || completeCurrent || !completeQueue.length) return;
  // a completion whose entry has since been deleted must not
  // surface — the announcement outlived the download it was
  // about (R3: the dialog appeared after its entry was deleted).
  // Checked at show time, not queue time: the entry exists when
  // the finish is queued, so only a later deletion can stale it.
  // Payloads without an id cannot be checked and are left alone.
  for (let i = completeQueue.length - 1; i >= 0; i--) {
    const p = completeQueue[i];
    if (p.id && (!items.has(p.id) || deletedIds.has(p.id))) {
      completeQueue.splice(i, 1);
    }
  }
  if (!completeQueue.length) return;
  if (settings.show_complete_dialog === false) { completeQueue.length = 0; return; }
  // completions YIELD to user-initiated dialogs: a Grabber, an
  // incoming-link dialog or the detail dialog must never be buried
  // under a stack of "Download complete" boxes. The completion
  // waits in the queue — the pump and every modal-close path call
  // back here — and is shown once the way is clear (it is never
  // dropped: a download that finished under an open detail dialog
  // still gets its dialog, right after the detail closes)
  const blocked = [...document.querySelectorAll('.modal:not(.hidden)')]
    .some(m => m.id !== 'cmodal');
  if (blocked) return;
  if (completeQueue.length > 1) {
    // rapid successive completions (Start All finishing a batch)
    // fold into ONE summary instead of N dialogs in a row (F-26)
    const batch = completeQueue.splice(0, completeQueue.length);
    completeCurrent = { summary: true, batch };
    $('cName').textContent = `${batch.length} downloads completed`;
    if ($('cAddr')) {
      const names = batch.map(p => p.name || 'Download');
      $('cAddr').textContent = names.slice(0, 12).join('\n')
        + (names.length > 12 ? `\n…and ${names.length - 12} more` : '');
    }
    $('cPath').textContent = '';
    const totalBytes = batch.reduce((a, p) => a + (p.total || 0), 0);
    $('cSize').textContent = totalBytes
      ? 'Downloaded ' + fmtSize(totalBytes) + ' in total'
      : '';
    // a summary has no single file to open — Close only
    if ($('cOpen')) $('cOpen').style.display = 'none';
    if ($('cOpenFolder')) $('cOpenFolder').style.display = 'none';
    $('cDontShow').checked = false;
    revealModal(cm);
    if ($('cClose')) $('cClose').focus();
    return;
  }
  completeCurrent = completeQueue.shift();
  if ($('cOpen')) $('cOpen').style.display = '';
  if ($('cOpenFolder')) $('cOpenFolder').style.display = '';
  $('cName').textContent = completeCurrent.name || 'Download';
  if ($('cAddr')) {
    $('cAddr').textContent = completeCurrent.url ? 'Address: ' + completeCurrent.url : '';
  }
  $('cPath').textContent = 'Saved to: ' + (completeCurrent.path || '');
  $('cSize').textContent = completeCurrent.total
    ? 'Downloaded ' + fmtSize(completeCurrent.total) + ' (' + completeCurrent.total + ' bytes)'
    : '';
  $('cDontShow').checked = false;
  revealModal(cm);
  // IDM focuses Open — Enter opens the file right away
  if ($('cOpen')) $('cOpen').focus();
}
async function closeComplete(openMode) {
  const cm = $('cmodal');
  const cur = completeCurrent;
  completeCurrent = null;
  cm.classList.add('hidden');
  if ($('cDontShow').checked) {
    settings = { ...settings, show_complete_dialog: false };
    await saveSettings();
  }
  if (cur && openMode && !cur.summary && cur.id) {
    try { await invoke('open_path', { id: cur.id, folder: openMode === 'folder' }); }
    catch (e) {
      // a failed Open used to fail silently — the dialog closed
      // and nothing happened, indistinguishable from success
      console.error('open_path failed', e);
      toast('Could not open it: ' + e);
    }
  }
  showNextComplete();
}
if ($('cOpen')) $('cOpen').onclick = () => closeComplete('file');
if ($('cOpenFolder')) $('cOpenFolder').onclick = () => closeComplete('folder');
if ($('cClose')) $('cClose').onclick = () => closeComplete(null);
if ($('cModalClose')) $('cModalClose').onclick = () => closeComplete(null);

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

/* ---------- boot ---------- */
setInterval(() => {
  pulse++;
  // only repaint the segment map while its dialog is actually
  // visible — it used to redraw a hidden canvas twice a second
  if (detailEl.classList.contains('hidden')) return;
  const d = items.get(selectedId);
  if (d && d.status === 'downloading') drawSegMap(d.blocks || '', d.status);
}, 500);
window.addEventListener('resize', () => { if (selectedId) renderDetail(); });
/* =============== 1.5: menu bar, batch, import, about, updater =============== */
let basketShown = false;
const umodal = $('umodal');
let updateInfo = null;
let updateInstalling = false;
let updateInstalled = false; // a mandatory dialog unlocks once installed

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
      // these used to call a null invoke() in the browser demo and
      // report "Nothing to start" while demo items sat paused
      if (TAURI) { try { await invoke('resume_download', { id: d.id }); n++; } catch (e) { /* keep going */ } }
      else { demoResume(d.id); n++; }
    }
  }
  toast(n ? `Started ${n} download${n > 1 ? 's' : ''}` : 'Nothing to start', 'info');
}
async function pauseAllDownloads() {
  let n = 0;
  for (const d of items.values()) {
    if (d.status === 'downloading') {
      if (TAURI) { try { await invoke('pause_download', { id: d.id }); n++; } catch (e) { /* keep going */ } }
      else { demoPause(d.id); n++; }
    }
  }
  toast(n ? `Paused ${n} download${n > 1 ? 's' : ''}` : 'Nothing is downloading', 'info');
}
async function deleteCompleted() {
  const done = [...items.values()].filter(d => d.status === 'finished');
  if (!done.length) { toast('No completed downloads', 'info'); return; }
  for (const d of done) {
    if (TAURI) {
      try { await invoke('remove_download', { id: d.id }); } catch (e) { continue; }
    }
    deletedIds.add(d.id);
    items.delete(d.id);
    selectedIds.delete(d.id);
    prevStatusById.delete(d.id);
    completeAnnounced.delete(d.id);
  }
  if (selectedId && !items.has(selectedId)) selectedId = items.keys().next().value || null;
  render();
  toast(`Deleted ${done.length} completed download${done.length > 1 ? 's' : ''} from the list`, 'info');
}

let pendingUpdate = null; // an update that arrived while a dialog was up (N-08)
function showUpdate(info) {
  if (!info || info.status !== 'available') return;
  // one modal layer (N-08): the update dialog waits while another
  // dialog is up instead of stacking over it; the 700 ms pump
  // retries until the way is clear. A mandatory update still
  // cannot be dismissed once it is actually shown.
  if (anotherModalOpen('umodal')) { pendingUpdate = info; return; }
  pendingUpdate = null;
  updateInfo = info;
  updateInstalled = false;
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
  revealModal(umodal);
}
async function manualUpdateCheck() {
  if (!TAURI) { toast('Update checks need the NDM desktop app'); return; }
  toast('Checking for updates…', 'info');
  try {
    const r = await invoke('check_update');
    if (r.status === 'available') showUpdate(r);
    else if (r.status === 'current') toast(`NDM is up to date (${r.version})`, 'info');
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
    updateInstalled = true;
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
$('uModalClose').onclick = () => { if (!updateInfo || !updateInfo.mandatory || updateInstalled) umodal.classList.add('hidden'); };
umodal.addEventListener('click', e => { if (e.target === umodal && (!updateInfo || !updateInfo.mandatory || updateInstalled)) umodal.classList.add('hidden'); });

/* batch add */
const bmodal = $('bmodal');
function openBatch() {
  if (anotherModalOpen('bmodal')) return; // one modal layer (N-08)
  $('bDir').value = settings.default_dir;
  $('bStatus').textContent = '';
  $('bStatus').classList.remove('error');
  revealModal(bmodal);
  // the textarea is the whole point of this dialog — focus it,
  // so links typed or pasted the moment it opens actually land
  // (it used to open with focus nowhere and the first typing
  // was lost). Being a field inside the modal, the focus also
  // arms the F-08 editing guard for the typing that follows.
  $('bUrls').focus();
}
$('bModalClose').onclick = $('bCancel').onclick = () => bmodal.classList.add('hidden');
bmodal.addEventListener('click', e => { if (e.target === bmodal) bmodal.classList.add('hidden'); });
$('bAdd').onclick = async () => {
  const urls = $('bUrls').value.split(/\r?\n/).map(x => x.trim()).filter(x => x.startsWith('http'));
  if (!urls.length) { $('bStatus').textContent = 'No valid links found — one http(s) link per line.'; return; }
  if (!TAURI) { $('bStatus').textContent = 'Batch downloads need the NDM desktop app'; return; }
  const dir = $('bDir').value.trim() || settings.default_dir;
  $('bAdd').disabled = true;
  let ok = 0;
  const failed = []; // { url, reason } — the backend returns an
                     // accurate per-URL reason now (N-07)
  for (const url of urls) {
    try { await invoke('add_download', { url, dir, later: false }); ok++; }
    catch (e) {
      let reason = String(e || 'unknown error');
      // the duplicate marker is an internal id string ("duplicate:
      // dl-16") — say what it means instead of echoing it raw
      if (reason.startsWith('duplicate:')) reason = 'already in the download list';
      failed.push({ url, reason: reason.length > 140 ? reason.slice(0, 140) + '…' : reason });
    }
  }
  $('bAdd').disabled = false;
  if (!failed.length) {
    $('bUrls').value = '';
    $('bStatus').textContent = '';
    $('bStatus').classList.remove('error');
    bmodal.classList.add('hidden');
    toast(`Added ${ok} of ${urls.length} downloads`, 'info');
    return;
  }
  // partial (or total) failure: keep the dialog open and name
  // every failed URL with its reason — a bare "Added 1 of 3"
  // count used to be the only trace of the failures (N-07).
  // The URLs that DID add leave the textarea, so what remains
  // is exactly what still needs attention — and a retry cannot
  // duplicate the successes.
  const failedUrls = new Set(failed.map(f => f.url));
  $('bUrls').value = urls.filter(u => failedUrls.has(u)).join('\n');
  const shown = failed.slice(0, 12);
  $('bStatus').textContent =
    `Added ${ok} of ${urls.length} — ${failed.length} failed:\n`
    + shown.map(f => `• ${f.url} — ${f.reason}`).join('\n')
    + (failed.length > shown.length ? `\n…and ${failed.length - shown.length} more` : '');
  $('bStatus').classList.add('error');
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
  toast(`Imported ${ok} download${ok === 1 ? '' : 's'} (paused — start them when ready)`, 'info');
};

/* about */
const amodal = $('amodal');
async function openAbout() {
  if (anotherModalOpen('amodal')) return; // one modal layer (N-08)
  if (!TAURI) { $('aVersion').textContent = 'Version — (browser preview)'; revealModal(amodal); return; }
  // fill BEFORE revealing: the dialog used to open on its
  // placeholder text and swap the real values in whenever the
  // IPC answers landed — under load that left "Version —" on
  // screen for a visible beat (R3). The version comes from the
  // backend's app_version command (the packaged crate version,
  // never a string hardcoded here); the Tauri app API reports
  // the same packaged version if that call ever fails.
  try { $('aVersion').textContent = 'Version ' + await invoke('app_version'); }
  catch (e) {
    try { $('aVersion').textContent = 'Version ' + await window.__TAURI__.app.getVersion(); }
    catch (e2) { /* keep the placeholder rather than invent one */ }
  }
  try {
    const st = await invoke('components_status');
    $('aEngine').textContent = 'Video engine: ' + (st.ready ? 'ready' : 'setting up in the background');
  } catch (e) {}
  revealModal(amodal);
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
  import: () => {
    if (anotherModalOpen('imodal')) return; // one modal layer (N-08)
    $('iStatus').textContent = ''; revealModal(imodal);
  },
  export: async () => {
    if (!TAURI) { toast('Export needs the NDM desktop app'); return; }
    try { toast('Exported to ' + await invoke('export_downloads'), 'info'); }
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
  basket: async () => {
    if (!TAURI) { toast('The drop basket needs the NDM desktop app'); return; }
    basketShown = !basketShown;
    try { await invoke('set_basket_visible', { visible: basketShown }); }
    catch (e) { basketShown = !basketShown; toast('Could not toggle the basket: ' + e); }
  },
  'sort-name': () => { sortKey = 'name'; sortDir = 1; updateSortIndicators(); render(); },
  'sort-date': () => { sortKey = 'date'; sortDir = 1; updateSortIndicators(); render(); },
  'sort-size': () => { sortKey = 'size'; sortDir = 1; updateSortIndicators(); render(); },
  'sort-status': () => { sortKey = 'status'; sortDir = 1; updateSortIndicators(); render(); },
  columns: () => openColumns(),
  toolbar: () => openToolbarCfg(),
  refresh: async () => { if (TAURI) { try { const list = await invoke('list_downloads'); for (const d of list) if (!deletedIds.has(d.id)) items.set(d.id, normalize(d)); render(); } catch (e) {} } },
  update: manualUpdateCheck,
  shortcuts: () => {
    if (anotherModalOpen('kmodal')) return; // one modal layer (N-08)
    revealModal(kmodal);
  },
  about: openAbout,
};
document.querySelectorAll('.menu-drop > button').forEach(b => {
  b.addEventListener('click', () => {
    const act = b.dataset.act;
    closeMenus();
    if (act && menuActions[act]) menuActions[act]();
  });
});

/* global shortcuts (the ones the Help dialog lists — all real).
   Escape is handled by the single stack-aware handler above —
   there used to be a second one here racing it. */
document.addEventListener('keydown', e => {
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement && document.activeElement.tagName || '');
  if (typing) return;
  if (e.ctrlKey && !e.shiftKey && e.key.toLowerCase() === 'n') { e.preventDefault(); $('btnAdd').click(); }
  else if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === 'v') { e.preventDefault(); $('btnVideo').click(); }
  else if (e.ctrlKey && e.key.toLowerCase() === 'q') { if (TAURI) invoke('quit_app'); }
  else if (e.key === 'F5') { e.preventDefault(); menuActions.refresh(); }
});

applyColumns();
applyToolbar();
if (TAURI) initTauri(); else { loadSettings(); demoSeed(); }
