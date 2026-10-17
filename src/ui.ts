// The web app's script, compiled to src/ui.js (served at /ui.js). Edit ui.ts, then run `npx -p typescript@5.9.2 tsc`.

// JSON shapes returned by the /api endpoints in main.rs.
interface Config { version: string; defaultUser: string; needKey: boolean; slideSecs: number }
interface Album { id: string; title: string; count: number; created: number; thumb?: string }
interface AlbumList { nsid: string; albums: Album[] }
interface Photo { id: string; title: string; thumb?: string }
interface Device { name: string; addr: string }
/** While casting, the server always says where (`post_cast_cmd` and `play` in main.rs). */
type CastStatus = { active: true; text: string; device: string; addr: string; album: string } | { active: false; text: string };
interface Ok { ok: true }

/** Every endpoint and its response, so a call can't expect the wrong shape. */
interface Routes {
  '/api/config': Config;
  '/api/albums': AlbumList;
  '/api/photos': Photo[];
  '/api/devices': Device[];
  '/api/status': CastStatus;
  '/api/settings': { slideSecs: number };
  '/api/cast': Ok;
  '/api/stop': Ok;
  '/api/key': Ok;
}
const POST_ROUTES: ReadonlySet<keyof Routes> = new Set(['/api/settings', '/api/cast', '/api/stop', '/api/key'] as const);

/** The elements in ui.html that the script uses, by id. */
interface Els {
  userForm: HTMLFormElement; user: HTMLInputElement; status: HTMLSpanElement; castBtn: HTMLButtonElement; menu: HTMLDivElement;
  keyForm: HTMLFormElement; key: HTMLInputElement; keyMsg: HTMLParagraphElement;
  albumBar: HTMLDivElement; back: HTMLButtonElement; albumTitle: HTMLHeadingElement; play: HTMLButtonElement; delay: HTMLInputElement;
  msg: HTMLParagraphElement; grid: HTMLDivElement;
  viewer: WebkitElement; big: HTMLImageElement; prev: HTMLButtonElement; next: HTMLButtonElement; slide: HTMLButtonElement;
  full: HTMLButtonElement; close: HTMLButtonElement; count: HTMLDivElement;
}
type WebkitDocument = Document & { webkitFullscreenElement?: Element; webkitFullscreenEnabled?: boolean; webkitExitFullscreen?: () => void };
type WebkitElement = HTMLDivElement & { webkitRequestFullscreen?: () => void };

let slideMs = 4000;
/** Server error when no valid key is set (`KEY_MISSING` in main.rs). */
const KEY_MISSING = 'Flickr API key missing';
const $ = <K extends keyof Els>(id: K) => document.getElementById(id) as Els[K];
const store = {
  get: (k: string) => { try { return localStorage.getItem(k) } catch { return null } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v) } catch {} } };
let casting: CastStatus | null = null;
let album: Album | null = null;
let photos: Photo[] = [];
let timer: number | undefined;
let statusTimer: number | undefined;
let nsid = '', idx = 0;

/** Calls an API endpoint with query `params`; a server error becomes an Error with the server's message. */
async function api<P extends keyof Routes>(path: P, params?: Record<string, string>): Promise<Routes[P]> {
  const url = params ? path + '?' + new URLSearchParams(params) : path;
  // The server rejects POSTs without this header; other sites can't add it (see API_HEADER in main.rs).
  const r = await fetch(url, { method: POST_ROUTES.has(path) ? 'POST' : 'GET', headers: { 'X-NymFlickrCast': '1' } });
  const body = await r.text();
  let j: any;
  try { j = JSON.parse(body); } catch { j = { error: body || r.statusText }; } // e.g. a plain-text 403/404
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}
/** `title` null makes a photo card (no caption, `sub` becomes the tooltip). */
function card(thumb: string | undefined, title: string | null, sub: string, onClick: () => void) {
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
/** onload handler for images. */
function fadeIn(e: Event) { (e.target as HTMLElement).animate([{ opacity: 0 }, { opacity: 1 }], 350); }
/** Error text for a caught value (always an Error in practice). */
const errText = (e: unknown) => e instanceof Error ? e.message : String(e);
function message(t: string) { $('msg').textContent = t; $('msg').hidden = !t; }

async function loadAlbums(user: string) {
  store.set('user', user);
  album = null; photos = []; updateCastBtn(); $('albumBar').hidden = true;
  $('grid').className = 'grid'; $('grid').replaceChildren();
  message('Loading albums…');
  try {
    const r = await api('/api/albums', { user });
    nsid = r.nsid;
    message(r.albums.length ? '' : 'No public albums.');
    $('grid').replaceChildren(...r.albums.map(a => card(a.thumb, a.title,
      `${a.count} photos · ${new Date(a.created * 1000).toLocaleDateString()}`, () => openAlbum(a))));
  } catch (e) {
    // Flickr rejected the saved key after startup: ask for a new one.
    if (errText(e) === KEY_MISSING) start(); else message(errText(e));
  }
}

async function openAlbum(a: Album) {
  album = a; idx = 0;
  $('albumTitle').textContent = a.title; $('albumBar').hidden = false;
  $('grid').className = 'grid photos'; $('grid').replaceChildren();
  message('Loading photos…');
  try {
    const list = await api('/api/photos', { user: nsid, album: a.id });
    if (album !== a) return; // another album was opened meanwhile
    photos = list;
    message(photos.length ? '' : 'No photos in this album.');
    $('grid').replaceChildren(...photos.map((p, i) => card(p.thumb, null, p.title, () => view(i))));
    updateCastBtn();
    // Keep casting: a running cast switches to this album on the same device.
    const st = await api('/api/status');
    if (album !== a) return;
    if (st.active && st.album !== a.id && photos.length) cast({ addr: st.addr, name: st.device });
  } catch (e) { message(errText(e)); }
}

// Usable with an album open (to cast it) or while casting (to stop).
function updateCastBtn() { $('castBtn').disabled = !(album && photos.length) && !casting; }

/** Shows photo `i` in the viewer; `i` wraps around. */
function view(i: number) {
  idx = (i + photos.length) % photos.length;
  $('viewer').hidden = false;
  const big = $('big');
  big.onload = fadeIn;
  big.src = '/img/' + photos[idx].id;
  big.alt = photos[idx].title || '';
  $('count').textContent = `${idx + 1} / ${photos.length}`;
}
function setPlaying(on: boolean) {
  clearInterval(timer); timer = on ? setInterval(() => view(idx + 1), slideMs) : undefined;
  $('slide').textContent = on ? '❚❚ Pause' : '▶ Play';
}
function closeViewer() { setPlaying(false); if (fullscreenEl()) exitFullscreen(); $('viewer').hidden = true; }

// Fullscreen, with the webkit-prefixed API for iPad Safari. iPhone Safari has neither, so the button is hidden there.
const doc = document as WebkitDocument;
const fullscreenEl = () => doc.fullscreenElement || doc.webkitFullscreenElement;
const exitFullscreen = () => (doc.exitFullscreen || doc.webkitExitFullscreen).call(doc);
function toggleFullscreen() {
  const v = $('viewer');
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
  const note = (t: string) => { const p = document.createElement('p'); p.textContent = t; return p; };
  m.replaceChildren(note('Searching for Cast devices…'));
  const top: HTMLElement[] = [];
  const st = await api('/api/status');
  if (st.active) {
    const stop = document.createElement('button');
    stop.className = 'stop';
    stop.textContent = '■ Stop casting to ' + st.device;
    stop.onclick = async () => { m.hidden = true; await api('/api/stop'); pollStatus(); };
    top.push(stop);
  }
  if (!album || !photos.length) { m.replaceChildren(...top, note('Open an album to cast it.')); return; }
  m.replaceChildren(...top, note('Searching for Cast devices…')); // Stop is usable during the ~3 s search
  const items: HTMLElement[] = [];
  try {
    const devs = await api('/api/devices');
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

/** Casts the open album to `d`, from the photo in the viewer if it's open. */
async function cast(d: Device) {
  $('menu').hidden = true;
  if (!album) return;
  const start = $('viewer').hidden ? 0 : idx;
  try { await api('/api/cast', { device: d.addr, name: d.name, album: album.id, start: String(start) }); }
  catch (e) { $('status').textContent = errText(e); }
  pollStatus();
}

async function pollStatus() {
  clearTimeout(statusTimer);
  try {
    const s = await api('/api/status');
    $('status').textContent = s.text;
    $('castBtn').classList.toggle('on', s.active);
    casting = s.active ? s : null; updateCastBtn();
    statusTimer = setTimeout(pollStatus, s.active ? 2000 : 10000); // idle: notice casts started elsewhere
  } catch { statusTimer = setTimeout(pollStatus, 5000); }
}

$('userForm').onsubmit = e => { e.preventDefault(); const u = $('user').value.trim(); if (u) loadAlbums(u); };
$('back').onclick = () => loadAlbums(store.get('user') || $('user').value);
$('play').onclick = () => { view(idx); setPlaying(true); };
$('prev').onclick = () => view(idx - 1);
$('next').onclick = () => view(idx + 1);
$('slide').onclick = () => setPlaying(!timer);
$('close').onclick = closeViewer;
$('full').onclick = toggleFullscreen;
$('castBtn').onclick = openCastMenu;
let touchX: number | null = null;
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
    await api('/api/key', { key: $('key').value.trim() });
    $('keyForm').hidden = true;
    start();
  } catch (err) { $('keyMsg').textContent = errText(err); }
};

function showDelay(secs: number) { slideMs = secs * 1000; $('delay').value = String(secs); }
$('delay').onchange = async () => {
  try {
    showDelay((await api('/api/settings', { slideSecs: String(Math.round(+$('delay').value)) })).slideSecs);
    if (timer) setPlaying(true);
  } catch (e) { $('status').textContent = errText(e); }
};

// Ask for whatever the server is missing before loading albums.
async function start() {
  let cfg: Config;
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
  if (initial) { $('user').value = initial; loadAlbums(initial); }
}
start();
pollStatus();
