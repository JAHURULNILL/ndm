'use strict';
/* NDM drop basket: a small always-on-top target. Dropping a link
   offers it in the main window's Add URL dialog (basket_add), the
   same path a browser-extension catch takes. */

const invoke = window.__TAURI__ && window.__TAURI__.core
  ? window.__TAURI__.core.invoke : null;
const basket = document.getElementById('basket');
const msg = document.getElementById('msg');
let msgTimer = null;

function note(text) {
  msg.textContent = text;
  clearTimeout(msgTimer);
  msgTimer = setTimeout(() => { msg.textContent = ''; }, 2600);
}

document.getElementById('close').addEventListener('click', () => {
  if (invoke) invoke('set_basket_visible', { visible: false }).catch(() => {});
});

/* W193-19: a right-click on the basket used to open the raw
   WebKitGTK context menu (Reload / Inspect Element…) — the main
   window suppresses its own, and the basket now does too */
document.addEventListener('contextmenu', e => e.preventDefault());

/* W193-4: the backend answers an OS-level file drop on the basket
   with a basket_note — "Sent to NDM ✓" for a link or shortcut,
   or the reason a plain file cannot be taken. (HTML5 drops of
   link text are answered inline by the drop handler below.) */
try {
  window.__TAURI__.event.listen('basket_note', e => {
    note((e.payload && e.payload.text) || '');
  });
} catch (e) { /* not in Tauri */ }

for (const ev of ['dragenter', 'dragover']) {
  window.addEventListener(ev, e => {
    e.preventDefault();
    basket.classList.add('over');
  });
}
window.addEventListener('dragleave', e => {
  if (!e.relatedTarget) basket.classList.remove('over');
});
window.addEventListener('drop', async e => {
  e.preventDefault();
  basket.classList.remove('over');
  if (!invoke || !e.dataTransfer) return;
  const uriList = e.dataTransfer.getData('text/uri-list') || '';
  const plain = e.dataTransfer.getData('text/plain') || '';
  const candidate = (uriList.split(/\r?\n/).find(l => l && !l.startsWith('#')) || plain).trim();
  if (!/^https?:\/\//i.test(candidate)) {
    note('That was not a web link');
    return;
  }
  try {
    await invoke('basket_add', { url: candidate });
    note('Sent to NDM ✓');
  } catch (err) {
    note('Could not send the link');
  }
});
