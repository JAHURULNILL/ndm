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
