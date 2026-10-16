// @ts-check
// The web app's script, served at /ui.js. Type-checked by VS Code through the JSDoc types; no build step.

// JSON shapes returned by the /api endpoints in main.rs.
/** @typedef {{version: string, defaultUser: string, needKey: boolean, slideSecs: number}} Config */
/** @typedef {{id: string, title: string, count: number, created: number, thumb?: string}} Album */
/** @typedef {{nsid: string, albums: Album[]}} AlbumList */
/** @typedef {{id: string, title: string, thumb?: string}} Photo */
/** @typedef {{name: string, addr: string}} Device */
/** @typedef {{active: boolean, text: string, device?: string, addr?: string, album?: string}} CastStatus */
/** @typedef {Document & {webkitFullscreenElement?: Element, webkitFullscreenEnabled?: boolean, webkitExitFullscreen?: () => void}} WebkitDocument */
/** @typedef {HTMLElement & {webkitRequestFullscreen?: () => void}} WebkitElement */

let slideMs = 4000;
/** Server error when no valid key is set (`KEY_MISSING` in main.rs). */
const KEY_MISSING = 'Flickr API key missing';
/** @param {string} id */
const $ = id => /** @type {HTMLElement} */ (document.getElementById(id));
/** @param {string} id */
const $input = id => /** @type {HTMLInputElement} */ ($(id));
const store = {
  /** @param {string} k */
  get: k => { try { return localStorage.getItem(k) } catch { return null } },
  /** @param {string} k @param {string} v */
  set: (k, v) => { try { localStorage.setItem(k, v) } catch {} } };
/** @type {CastStatus | null} */ let casting = null;
/** @type {Album | null} */ let album = null;
/** @type {Photo[]} */ let photos = [];
/** @type {number | undefined} */ let timer;
/** @type {number | undefined} */ let statusTimer;
let nsid = '', idx = 0;

/**
 * Fetches a JSON API endpoint; a server error becomes an Error with the server's message. Callers cast the result.
 * @param {string} path
 * @param {RequestInit} [opts]
 * @returns {Promise<any>}
 */
async function api(path, opts) {
  // The server rejects POSTs without this header; other sites can't add it (see API_HEADER in main.rs).
  const r = await fetch(path, { ...opts, headers: { 'X-NymFlickrCast': '1' } });
  const body = await r.text();
  /** @type {any} */ let j;
  try { j = JSON.parse(body); } catch { j = { error: body || r.statusText }; } // e.g. a plain-text 403/404
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}
/**
 * @param {string | undefined} thumb
 * @param {string | null} title null for a photo card (no caption, `sub` becomes the tooltip)
 * @param {string} sub
 * @param {() => void} onClick
 */
function card(thumb, title, sub, onClick) {
  const b = document.createElement('button');
  b.className = 'card';
  const img = document.createElement('img');
  img.loading = 'lazy'; img.alt = ''; if (thumb) img.src = thumb;
  img.onload = fadeIn;
  b.append(img);
  if (title !== null) {
    const d = document.createElement('div');
    d.textContent = title;
    if (sub) { const s = document.createElement('small'); s.textContent = sub; d.append(document.createElement('br'), s); }
    b.append(d);
  } else b.title = sub;
  b.onclick = onClick;
  return b;
}
/** onload handler for images. @param {Event} e */
function fadeIn(e) { /** @type {HTMLElement} */ (e.target).animate([{ opacity: 0 }, { opacity: 1 }], 350); }
/** Error text for a caught value (always an Error in practice). @param {unknown} e */
const errText = e => e instanceof Error ? e.message : String(e);
/** @param {string} t */
function message(t) { $('msg').textContent = t; $('msg').hidden = !t; }

/** @param {string} user */
async function loadAlbums(user) {
  store.set('user', user);
  album = null; photos = []; updateCastBtn(); $('albumBar').hidden = true;
  $('grid').className = 'grid'; $('grid').replaceChildren();
  message('Loading albums…');
  try {
    const r = /** @type {AlbumList} */ (await api('/api/albums?user=' + encodeURIComponent(user)));
    nsid = r.nsid;
    message(r.albums.length ? '' : 'No public albums.');
    $('grid').replaceChildren(...r.albums.map(a => card(a.thumb, a.title,
      `${a.count} photos · ${new Date(a.created * 1000).toLocaleDateString()}`, () => openAlbum(a))));
  } catch (e) {
    // Flickr rejected the saved key after startup: ask for a new one.
    if (errText(e) === KEY_MISSING) start(); else message(errText(e));
  }
}

/** @param {Album} a */
async function openAlbum(a) {
  album = a; idx = 0;
  $('albumTitle').textContent = a.title; $('albumBar').hidden = false;
  $('grid').className = 'grid photos'; $('grid').replaceChildren();
  message('Loading photos…');
  try {
    const list = /** @type {Photo[]} */ (await api(`/api/photos?user=${encodeURIComponent(nsid)}&album=${encodeURIComponent(a.id)}`));
    if (album !== a) return; // another album was opened meanwhile
    photos = list;
    message(photos.length ? '' : 'No photos in this album.');
    $('grid').replaceChildren(...photos.map((p, i) => card(p.thumb, null, p.title, () => view(i))));
    updateCastBtn();
    // Keep casting: a running cast switches to this album on the same device.
    const st = /** @type {CastStatus} */ (await api('/api/status'));
    if (album !== a) return;
    if (st.active && st.addr && st.album !== a.id && photos.length) cast({ addr: st.addr, name: st.device || '' });
  } catch (e) { message(errText(e)); }
}

// Usable with an album open (to cast it) or while casting (to stop).
function updateCastBtn() { /** @type {HTMLButtonElement} */ ($('castBtn')).disabled = !(album && photos.length) && !casting; }

/** @param {number} i index, wraps around */
function view(i) {
  idx = (i + photos.length) % photos.length;
  $('viewer').hidden = false;
  const big = /** @type {HTMLImageElement} */ ($('big'));
  big.onload = fadeIn;
  big.src = '/img/' + photos[idx].id;
  big.alt = photos[idx].title || '';
  $('count').textContent = `${idx + 1} / ${photos.length}`;
}
/** @param {boolean} on */
function setPlaying(on) {
  clearInterval(timer); timer = on ? setInterval(() => view(idx + 1), slideMs) : undefined;
  $('slide').textContent = on ? '❚❚ Pause' : '▶ Play';
}
function closeViewer() { setPlaying(false); if (fullscreenEl()) exitFullscreen(); $('viewer').hidden = true; }

// Fullscreen, with the webkit-prefixed API for iPad Safari. iPhone Safari has neither, so the button is hidden there.
const doc = /** @type {WebkitDocument} */ (document);
const fullscreenEl = () => doc.fullscreenElement || doc.webkitFullscreenElement;
const exitFullscreen = () => (doc.exitFullscreen || doc.webkitExitFullscreen).call(doc);
function toggleFullscreen() {
  const v = /** @type {WebkitElement} */ ($('viewer'));
  if (fullscreenEl()) exitFullscreen();
  else (v.requestFullscreen || v.webkitRequestFullscreen).call(v);
}
$('full').hidden = !(doc.fullscreenEnabled || doc.webkitFullscreenEnabled);
for (const ev of ['fullscreenchange', 'webkitfullscreenchange'])
  document.addEventListener(ev, () => { $('full').textContent = fullscreenEl() ? '🗗' : '⛶'; $('full').title = fullscreenEl() ? 'Exit full screen (F)' : 'Full screen (F)'; });

async function openCastMenu() {
  const m = $('menu');
  if (!m.hidden) { m.hidden = true; return; }
  m.hidden = false;
  /** @param {string} t */
  const note = t => { const p = document.createElement('p'); p.textContent = t; return p; };
  m.replaceChildren(note('Searching for Cast devices…'));
  /** @type {HTMLElement[]} */
  const top = [];
  const st = /** @type {CastStatus} */ (await api('/api/status'));
  if (st.active) {
    const stop = document.createElement('button');
    stop.className = 'stop';
    stop.textContent = '■ Stop casting' + (st.device ? ' to ' + st.device : '');
    stop.onclick = async () => { m.hidden = true; await api('/api/stop', { method: 'POST' }); pollStatus(); };
    top.push(stop);
  }
  if (!album || !photos.length) { m.replaceChildren(...top, note('Open an album to cast it.')); return; }
  m.replaceChildren(...top, note('Searching for Cast devices…')); // Stop is usable during the ~3 s search
  /** @type {HTMLElement[]} */
  const items = [];
  try {
    const devs = /** @type {Device[]} */ (await api('/api/devices'));
    if (!devs.length) items.push(note('No Cast devices found.'));
    for (const d of devs) {
      const b = document.createElement('button');
      b.textContent = (st.active && st.addr === d.addr ? '✓ ' : '📺 ') + d.name;
      b.onclick = () => cast(d);
      items.push(b);
    }
  } catch (e) { items.push(note(errText(e))); }
  m.replaceChildren(...top, ...items);
}

/** Casts the open album to `d`, from the photo in the viewer if it's open. @param {Device} d */
async function cast(d) {
  $('menu').hidden = true;
  if (!album) return;
  const start = $('viewer').hidden ? 0 : idx;
  const q = new URLSearchParams({ device: d.addr, name: d.name, album: album.id, start: String(start) });
  try { await api('/api/cast?' + q, { method: 'POST' }); } catch (e) { $('status').textContent = errText(e); }
  pollStatus();
}

async function pollStatus() {
  clearTimeout(statusTimer);
  try {
    const s = /** @type {CastStatus} */ (await api('/api/status'));
    $('status').textContent = s.text;
    $('castBtn').classList.toggle('on', s.active);
    casting = s.active ? s : null; updateCastBtn();
    statusTimer = setTimeout(pollStatus, s.active ? 2000 : 10000); // idle: notice casts started elsewhere
  } catch { statusTimer = setTimeout(pollStatus, 5000); }
}

$('userForm').onsubmit = e => { e.preventDefault(); const u = $input('user').value.trim(); if (u) loadAlbums(u); };
$('back').onclick = () => loadAlbums(store.get('user') || $input('user').value);
$('play').onclick = () => { view(idx); setPlaying(true); };
$('prev').onclick = () => view(idx - 1);
$('next').onclick = () => view(idx + 1);
$('slide').onclick = () => setPlaying(!timer);
$('close').onclick = closeViewer;
$('full').onclick = toggleFullscreen;
$('castBtn').onclick = openCastMenu;
/** @type {number | null} */ let touchX = null;
$('viewer').addEventListener('touchstart', e => { touchX = e.touches.length === 1 ? e.touches[0].clientX : null; }, { passive: true });
$('viewer').addEventListener('touchend', e => {
  if (touchX === null) return;
  const dx = e.changedTouches[0].clientX - touchX; touchX = null;
  if (Math.abs(dx) > 50) view(idx + (dx < 0 ? 1 : -1));
});
document.onkeydown = e => {
  if ($('viewer').hidden) return;
  if (e.key === 'ArrowRight') view(idx + 1);
  else if (e.key === 'ArrowLeft') view(idx - 1);
  else if (e.key === 'Escape') closeViewer();
  else if (e.key === 'f' && !$('full').hidden) toggleFullscreen();
  else if (e.key === ' ') { e.preventDefault(); setPlaying(!timer); }
};

$('keyForm').onsubmit = async e => {
  e.preventDefault();
  $('keyMsg').textContent = 'Checking key with Flickr…';
  try {
    await api('/api/key?key=' + encodeURIComponent($input('key').value.trim()), { method: 'POST' });
    $('keyForm').hidden = true;
    start();
  } catch (err) { $('keyMsg').textContent = errText(err); }
};

/** @param {number} secs */
function showDelay(secs) { slideMs = secs * 1000; $input('delay').value = String(secs); }
$('delay').onchange = async () => {
  try {
    showDelay((await api('/api/settings?slideSecs=' + Math.round(+$input('delay').value), { method: 'POST' })).slideSecs);
    if (timer) setPlaying(true);
  } catch (e) { $('status').textContent = errText(e); }
};

// Ask for whatever the server is missing before loading albums.
async function start() {
  /** @type {Config} */
  let cfg;
  try {
    cfg = await api('/api/config');
    showDelay(cfg.slideSecs);
    if (cfg.needKey) {
      $('keyForm').hidden = false; message(''); $('key').focus();
      return;
    }
  } catch (e) { message('Cannot reach the NymFlickrCast server: ' + errText(e)); return; }
  message('Enter a Flickr username to see the albums.');
  const initial = new URLSearchParams(location.search).get('user') || store.get('user') || cfg.defaultUser;
  if (initial) { $input('user').value = initial; loadAlbums(initial); }
}
start();
pollStatus();
