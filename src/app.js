// Atlas for Rhythia: the window. It has no network access of its own (CSP connect-src ipc:);
// everything goes through Rust commands, covers through Atlas' cover protocol. Text from the game,
// Rhythia or the server is put on the page only as text (h() below), never as HTML.

// Nothing can change what every object inherits (prototype pollution). Done here, not with Tauri's
// freezePrototype, which would also freeze it on rhythia.com in the sign-in window and break the site.
Object.freeze(Object.prototype);

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const view = document.getElementById('view');
const kinds = new Map(); // map hash -> kind entry ({kind, quantum, ...}), or null while unknown
let quota = null;
let job = null; // current install: { items: [{id, title, state, at}], done, error }
let lib = null; // { maps, attempts, byOnline, byHash }
let onJob = null; // the open view's redraw for install progress
let bridge = null; // link with Reimagined: { port, paired: [{id, browser, connected, lastSeen}], asking }
let onBridge = null; // the open view's redraw for the link
let onGame = null; // the open view's redraw when the game's database changed

// ---------- preferences (this computer only) ----------

const VIEWS_OF = { library: 'list', catalog: 'grid', collection: 'list', collections: 'grid' };
const SORT_OF = { library: { key: 'title', dir: 'asc' }, catalog: { key: 'newest', dir: 'desc' }, game: { key: 'why', dir: 'desc' } };
const ZOOMS = [0.9, 1, 1.1, 1.25, 1.5, 1.75, 2];
// Maps on one page: your own lists (0: all) and lists from Rhythia (at most 50, one page of Rhythia's).
const PER_PAGE = [10, 20, 30, 50, 100, 200, 0];
const PER_PAGE_RHYTHIA = [10, 20, 30, 50];
const prefs = (() => {
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem('atlas.prefs') || '{}') || {}; } catch { /* private storage */ }
  const sort = {};
  for (const [page, def] of Object.entries(SORT_OF)) {
    const s = saved.sort?.[page];
    sort[page] = s && typeof s.key === 'string' && (s.dir === 'asc' || s.dir === 'desc') ? { key: s.key, dir: s.dir } : { ...def };
  }
  return {
    theme: saved.theme === 'light' ? 'light' : 'dark',
    zoom: ZOOMS.includes(saved.zoom) ? saved.zoom : 1,
    view: { ...VIEWS_OF, ...(saved.view || {}) },
    sort,
    volume: Number.isFinite(saved.volume) && saved.volume >= 0 && saved.volume <= 1 ? saved.volume : 0.5,
    // Where downloads go: the game, loose files or a ZIP in downloadDir (install.rs Mode).
    downloadTo: ['game', 'files', 'zip'].includes(saved.downloadTo) ? saved.downloadTo : 'game',
    // Ask where at every Install, until the player ticks Remember my choice.
    askWhere: saved.askWhere !== false,
    downloadDir: typeof saved.downloadDir === 'string' ? saved.downloadDir : '',
    // Maps on one page of a list: your own lists (0 = all) and lists from Rhythia (at most 50).
    perPage: PER_PAGE.includes(saved.perPage) ? saved.perPage : 30,
    perPageRhythia: PER_PAGE_RHYTHIA.includes(saved.perPageRhythia) ? saved.perPageRhythia : 30,
    // The collection Add to collection offered last ('new', 'a<id>' in Atlas, 'r<id>' on Rhythia).
    lastCollection: typeof saved.lastCollection === 'string' ? saved.lastCollection : 'new',
  };
})();
function savePrefs() {
  try { localStorage.setItem('atlas.prefs', JSON.stringify(prefs)); } catch { /* not kept */ }
}
const applyTheme = () => document.documentElement.setAttribute('data-theme', prefs.theme);
const applyZoom = () => invoke('set_zoom', { scale: prefs.zoom }).catch(() => {});
applyTheme();
applyZoom();

// ---------- helpers ----------

function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'style') el.style.cssText = v; // CSSOM: the CSP blocks style attributes
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) if (kid != null && kid !== false) el.append(kid);
  return el;
}

// Replaces an element's children; like h(), false and null are left out (a plain replaceChildren would show "false").
function fill(el, ...kids) {
  el.replaceChildren(...kids.flat().filter(k => k != null && k !== false));
}

const SVG = 'http://www.w3.org/2000/svg';
const ICONS = {
  list: 'M1.5 2.5h3v3h-3zM7 4h7.5M1.5 10.5h3v3h-3zM7 12h7.5',
  grid: 'M1.5 1.5h5v5h-5zM9.5 1.5h5v5h-5zM1.5 9.5h5v5h-5zM9.5 9.5h5v5h-5z',
  compact: 'M1.5 3h13M1.5 6.3h13M1.5 9.6h13M1.5 12.9h13',
  done: 'M2.5 8.5l3.5 3.5 7.5-8',
  failed: 'M3.5 3.5l9 9M12.5 3.5l-9 9',
  sent: 'M8 1.5a6.5 6.5 0 1 1 0 13a6.5 6.5 0 1 1 0-13zM8 4.5V8l2.5 1.5',
  waiting: 'M8 1.5a6.5 6.5 0 1 1 0 13a6.5 6.5 0 1 1 0-13z',
  cancelled: 'M8 1.5a6.5 6.5 0 1 1 0 13a6.5 6.5 0 1 1 0-13zM5 8h6',
  back: 'M10 3L5 8l5 5',
  down: 'M8 2.5v11M4 9.5l4 4 4-4',
  up: 'M8 13.5v-11M4 6.5l4-4 4 4',
  noimage: 'M1.5 3h13v10h-13zM1.5 11l4-3.5 3 2.5 2-1.5 4 3M10.5 6.5h.01',
  close: 'M3.5 3.5l9 9M12.5 3.5l-9 9',
  play: 'M4.5 2.5v11l9-5.5z',
  pause: 'M4.5 2.5v11M11.5 2.5v11',
};
function icon(name, cls = '') {
  const s = document.createElementNS(SVG, 'svg');
  s.setAttribute('viewBox', '0 0 16 16');
  s.setAttribute('class', 'ic ' + cls);
  s.setAttribute('aria-hidden', 'true');
  const p = document.createElementNS(SVG, 'path');
  p.setAttribute('d', ICONS[name]);
  s.append(p);
  return s;
}

const num = n => Number(n || 0).toLocaleString('en-US');
const none = () => h('span', { class: 'none' }, '–');
const stars = s => (s == null ? none() : s.toFixed(2));
const acc = a => (a == null ? none() : a.toFixed(2) + '%');
const rp = r => (r == null ? none() : num(Math.round(r)));
const pct = (a, b) => (b ? Math.round((100 * a) / b) : 0);
function clock(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}
const length = ms => (ms == null ? none() : clock(ms));
function lastDay(t) {
  if (!t) return none();
  const d = new Date(t);
  if (Number.isNaN(d.getTime())) return String(t).slice(0, 10);
  const days = Math.round((new Date().setHours(0, 0, 0, 0) - new Date(d).setHours(0, 0, 0, 0)) / 864e5);
  const text = days <= 0 ? 'Today' : days === 1 ? 'Yesterday' : days < 7 ? `${days} days ago` : String(t).slice(0, 10);
  return h('span', { title: String(t).slice(0, 16).replace('T', ' ') }, text);
}
const when = t => (t ? String(t).slice(0, 16).replace('T', ' ') : '');
// Approved maps give RP like Ranked ones, so Atlas shows them as Ranked (Reimagined does the same).
const STATUS = { RANKED: 'Ranked', APPROVED: 'Ranked', QUALIFIED: 'Qualified', UNRANKED: 'Unranked' };
const isRanked = m => m.status === 'RANKED' || m.status === 'APPROVED';
const chip = (text, cls = '') => h('span', { class: 'chip ' + cls }, text);
const statusChip = s => chip(STATUS[s] || 'Local', s === 'APPROVED' ? 'RANKED' : s || 'LOCAL');
const modName = m => String(m).replace(/^mod_/, '').split('_').map(w => w[0]?.toUpperCase() + w.slice(1)).join(' ');
const plural = (n, one, many = one + 's') => `${num(n)} ${n === 1 ? one : many}`;

let toastTimer;
// A short note in the corner, above everything (a popover: also over the map sheet and questions, which are
// modal dialogs); a bad one stays longer.
function toast(text, bad) {
  const t = document.getElementById('toast');
  t.textContent = text;
  t.className = 'toast' + (bad ? ' bad' : '');
  try { t.hidePopover(); } catch { /* not shown */ }
  t.showPopover();
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { try { t.hidePopover(); } catch { /* gone */ } }, bad ? 9000 : 5000);
}

// A question with OK/Cancel, or with a text field when `field` is given (then resolves to the text or null).
function ask(title, text, yes = 'OK', field = null) {
  const d = document.getElementById('ask');
  const input = document.getElementById('ask-input');
  document.getElementById('ask-title').textContent = title;
  document.getElementById('ask-text').textContent = text || '';
  document.getElementById('ask-text').hidden = !text;
  document.getElementById('ask-yes').textContent = yes;
  document.getElementById('ask-yes').classList.toggle('danger', field === 'danger' || !!field?.danger);
  if (field === 'danger') field = null;
  input.hidden = !field || field.noName;
  input.value = field?.value || '';
  input.placeholder = field?.placeholder || '';
  // field.more: more fields under the name (the caller reads them after).
  fill(document.getElementById('ask-more'), field?.more || []);
  d.returnValue = '';
  d.showModal();
  if (field) input.focus();
  return new Promise(done => d.addEventListener('close', () => {
    const ok = d.returnValue === 'yes';
    done(field && !field.noName ? (ok && input.value.trim() ? input.value.trim() : null) : ok);
  }, { once: true }));
}

const head = (title, lead, ...right) =>
  h('div', { class: 'head' }, h('div', {}, h('h1', {}, title), lead && h('p', { class: 'lead' }, lead)), ...right);
const empty = (title, text) => h('div', { class: 'empty' }, h('b', {}, title), text);
// A loading bar: moving until `set(done, of)` says how far it is.
function loader() {
  const el = h('div', { class: 'loader', role: 'progressbar', 'aria-label': 'Loading' }, h('div', { class: 'track' }, h('i')));
  el.set = (done, of) => {
    if (!(of > 0)) return;
    el.classList.add('known');
    el.style.setProperty('--p', `${Math.min(100, Math.round((100 * done) / of))}%`);
    el.setAttribute('aria-valuenow', String(Math.round((100 * done) / of)));
  };
  return el;
}
const stat = (value, label) => h('div', { class: 'stat' }, h('b', {}, value), h('span', {}, label));
const button = (label, onclick, cls = 'btn') => h('button', { class: cls, type: 'button', onclick }, label);
// Every list: filters on the left of the first bar, sort, maps a page and view on its right (`tools`); the count
// on the left of the second bar, actions on its right (`acts`).
const tools = (...els) => h('div', { class: 'row tools' }, ...els);
const acts = (...els) => h('div', { class: 'row acts' }, ...els);

function seg(options, current, onPick, label) {
  const box = h('div', { class: 'seg', role: 'group', 'aria-label': label });
  for (const [value, text] of options) {
    box.append(h('button', {
      type: 'button',
      'data-v': value,
      'aria-pressed': String(value === current),
      onclick: () => {
        for (const b of box.children) b.setAttribute('aria-pressed', String(b.dataset.v === value));
        onPick(value);
      },
    }, text));
  }
  return box;
}

function select(options, current, onPick, label) {
  return h('select', { class: 'field', 'aria-label': label, onchange: e => onPick(e.target.value) },
    options.map(([v, t]) => h('option', { value: v, selected: v === current }, t)));
}

// Sorting: a list of keys plus a button for the direction. Header clicks go largest first, then
// smallest first (A to Z first for titles), then back to the page's usual order.
function nextSort(cur, key, def) {
  const first = key === 'title' ? 'asc' : 'desc';
  if (cur.key !== key) return { key, dir: first };
  if (cur.dir === first) return { key, dir: first === 'asc' ? 'desc' : 'asc' };
  return { ...def };
}
function sortControls(options, sort, onChange) {
  const pick = select(options, sort.key, v => onChange({ key: v, dir: v === 'title' ? 'asc' : 'desc' }), 'Sort');
  const dirText = d => (d === 'asc' ? 'Smallest first (A to Z)' : 'Largest first (Z to A)');
  const flip = h('button', { class: 'vsw-one', type: 'button', title: dirText(sort.dir), 'aria-label': dirText(sort.dir),
    onclick: () => onChange({ key: sort.key, dir: sort.dir === 'asc' ? 'desc' : 'asc' }) }, icon(sort.dir === 'asc' ? 'up' : 'down'));
  return h('span', { class: 'row tight' }, pick, flip);
}

// Maps on one page, next to a list: your own lists (key perPage) or lists from Rhythia (perPageRhythia, at most 50).
function perPagePick(key, redraw) {
  const options = (key === 'perPage' ? PER_PAGE : PER_PAGE_RHYTHIA).map(n => [String(n), n ? `${n} a page` : 'All']);
  return select(options, String(prefs[key]), v => { prefs[key] = Number(v); savePrefs(); redraw(); }, 'Maps a page');
}

// List, grid or compact: the icons next to a list; the choice is kept per page.
function viewSwitch(page, onPick) {
  const box = h('div', { class: 'vsw', role: 'group', 'aria-label': 'View' });
  for (const [mode, title] of [['list', 'List'], ['grid', 'Grid'], ['compact', 'Compact']]) {
    box.append(h('button', {
      type: 'button', title, 'aria-label': title, 'data-v': mode,
      'aria-pressed': String(prefs.view[page] === mode),
      onclick: () => {
        prefs.view[page] = mode;
        savePrefs();
        for (const b of box.children) b.setAttribute('aria-pressed', String(b.dataset.v === mode));
        onPick(mode);
      },
    }, icon(mode)));
  }
  return box;
}

// ---------- covers ----------

const coverSrc = m => {
  const local = m.cover || (m.onlineId && lib?.byOnline.get(m.onlineId)?.cover);
  if (local) return convertFileSrc('l-' + local, 'cover');
  if (m.onlineCover && m.onlineId) return convertFileSrc('o-' + m.onlineId, 'cover');
  return null;
};
function cover(m, cls = '') {
  const src = coverSrc(m);
  const blank = h('div', { class: 'cover blank ' + cls, title: 'No image' }, icon('noimage'), h('span', {}, 'No image'));
  if (!src) return blank;
  const img = h('img', { class: 'cover ' + cls, src, alt: '', loading: 'lazy', decoding: 'async' });
  img.addEventListener('error', () => img.replaceWith(blank), { once: true });
  return img;
}

// ---------- shared data ----------

// Your whole tries of each map at each speed, from the game (speed_tries): Rhythia id -> [{speed, tries, passes}].
let myTries = new Map();
async function getLibrary(force) {
  if (lib && !force) return lib;
  const got = await invoke('library');
  const byOnline = new Map(), byHash = new Map();
  for (const m of got.maps) {
    m.onlineId = m.onlineId ?? null;
    m.installed = true;
    if (m.onlineId) byOnline.set(m.onlineId, m);
    if (m.hash) byHash.set(m.hash, m);
  }
  lib = { ...got, byOnline, byHash };
  myTries = new Map();
  for (const t of await invoke('speed_tries').catch(() => [])) {
    if (!myTries.has(t.id)) myTries.set(t.id, []);
    myTries.get(t.id).push(t);
  }
  return lib;
}

// One shape for every map on screen, from the game, from Rhythia or from an Atlas collection.
function fromOnline(m) {
  const mine = lib?.byOnline.get(m.id) || (m.hash && lib?.byHash.get(m.hash));
  return {
    // getCollection sends no map hashes: the game's copy of the map has one (types are found by hash).
    onlineId: m.id, hash: m.hash || mine?.hash || null, title: m.title, mappers: m.mapper ? [m.mapper] : [], stars: m.stars,
    durationMs: m.lengthMs, status: m.status, onlinePlays: m.plays, onlineCover: m.cover, tags: m.tags || [],
    installed: m.installed || !!mine, cover: mine?.cover, plays: mine?.plays || 0, passes: mine?.passes || 0, bestRp: mine?.bestRp,
    bestAccuracy: mine?.bestAccuracy, lastPlayed: mine?.lastPlayed, difficultyName: mine?.difficultyName,
  };
}
function fromItem(it) {
  const mine = (it.onlineId && lib?.byOnline.get(it.onlineId)) || (it.hash && lib?.byHash.get(it.hash));
  if (mine) return { ...mine, item: it };
  return { onlineId: it.onlineId, hash: it.hash, title: it.title, mappers: it.mapper ? [it.mapper] : [], plays: 0, installed: false, item: it };
}
const itemOf = m => m.item || { onlineId: m.onlineId || null, hash: m.hash || null, title: m.title || '', mapper: m.mappers?.[0] || '' };

// Types come from the list of map types Atlas keeps (vps.rs; no request per map). Maps in the game the list
// has no type for get one Atlas works out from the map file (basic_kinds.rs; approximate, never Tech).
const fileOf = m => m.file || (m.onlineId && lib?.byOnline.get(m.onlineId)?.file) || (m.hash && lib?.byHash.get(m.hash)?.file) || null;
async function loadKinds(maps, after) {
  const wanted = [...new Set(maps.filter(m => m.hash && !kinds.has(m.hash)).map(m => m.hash))];
  for (const hash of wanted) kinds.set(hash, null);
  if (wanted.length) {
    try {
      const got = await invoke('map_kinds', { hashes: wanted });
      for (const [hash, k] of Object.entries(got)) kinds.set(hash, k);
    } catch (e) {
      for (const hash of wanted) kinds.delete(hash);
      toast('Map types: ' + e, true);
    }
  }
  const local = maps.filter(m => m.hash && kinds.get(m.hash) == null && !basicTried.has(m.hash) && fileOf(m));
  if (local.length) {
    for (const m of local) basicTried.add(m.hash);
    const got = await invoke('basic_kinds', { files: [...new Set(local.map(fileOf))] }).catch(() => ({}));
    for (const m of local) if (got[fileOf(m)] && !kinds.get(m.hash)) kinds.set(m.hash, got[fileOf(m)]);
  }
  if (wanted.length || local.length) after?.();
}
const basicTried = new Set(); // hashes Atlas already tried to type from the map file
const kindOf = m => (m.hash ? kinds.get(m.hash) : null);
// The server gives stream, jump and tech as shares of the map (0 to 1).
const mixText = k => ['stream', 'jump', 'tech'].filter(x => k[x] != null)
  .map(x => `${KIND_NAME[x]} ${Math.round(k[x] <= 1 ? k[x] * 100 : k[x])}%`).join(' · ');
const KIND_NAME = { jump: 'Jump', stream: 'Stream', tech: 'Tech', varied: 'Varied' };
// The Type filter of a list ('all', a kind or 'quantum').
const KIND_PICKS = [['all', 'Any type'], ['jump', 'Jump'], ['stream', 'Stream'], ['tech', 'Tech'], ['varied', 'Varied'], ['quantum', 'Quantum']];
const kindIs = (m, pick) => pick === 'all' || (pick === 'quantum' ? !!kindOf(m)?.quantum : kindOf(m)?.kind === pick);
function kindChip(m) {
  const k = kindOf(m);
  if (!k) return null;
  const title = k.approximate ? 'Approximate, from the map file' : null;
  return h('span', { class: 'kinds', title }, chip(KIND_NAME[k.kind] + (k.approximate ? ' ≈' : ''), 'k-' + k.kind), k.quantum && chip('Quantum', 'k-quantum'));
}

// ---------- daily limit and installing ----------

async function refreshQuota() {
  try { quota = await invoke('quota_status'); } catch { return; }
  for (const el of document.querySelectorAll('[data-quota]')) fillQuota(el);
}
// Today's downloads (Downloads and Settings → Data); refreshQuota keeps every one up to date.
function quotaBox() {
  const el = h('div', { class: 'quota', 'data-quota': '' });
  fillQuota(el);
  return el;
}
function fillQuota(el) {
  if (!quota) { fill(el, loader()); return; }
  const reset = quota.resetsAt ? new Date(quota.resetsAt * 1000).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' }) : null;
  fill(el,
    h('div', { class: 'quota-top' }, h('b', {}, `${num(quota.left)} of ${num(quota.limit)} maps left today`),
      h('span', { class: 'dim small' }, reset ? `Back to ${num(quota.limit)} on ${reset}` : 'Nothing downloaded in the last 24 hours')),
    h('i', { class: 'meter' + (quota.left === 0 ? ' full' : '') }, h('span', { style: `width:${(100 * quota.used / quota.limit).toFixed(1)}%` })));
}

// The folder for file and ZIP downloads; asked for the first time it is needed.
async function downloadFolder(pickAgain = false) {
  if (prefs.downloadDir && !pickAgain) return prefs.downloadDir;
  const dir = await invoke('download_pick_folder').catch(() => null);
  if (dir) { prefs.downloadDir = dir; savePrefs(); }
  return dir || null;
}

// Where maps go, asked at Install: the game, map files or a ZIP; "Remember my choice" stops asking (Settings
// → Data asks again). Null when the player closes it.
async function askWhere(ids, what) {
  let to = prefs.downloadTo;
  const ways = [['game', 'To the game'], ['files', 'Map files'], ...(ids.length > 1 ? [['zip', 'ZIP']] : [])];
  if (!ways.some(([w]) => w === to)) to = 'game';
  const remember = h('input', { type: 'checkbox' });
  const ok = await ask(`Install ${what}`, `${plural(ids.length, 'map')} · ${num(quota?.left ?? 0)} left today`, 'Install', {
    noName: true,
    more: [seg(ways, to, v => { to = v; }, 'Where'), h('label', { class: 'check' }, remember, 'Remember my choice')],
  });
  if (!ok) return null;
  if (remember.checked) { prefs.downloadTo = to; prefs.askWhere = false; savePrefs(); }
  return to;
}

// Downloads maps: to the game, or as files or a ZIP in the picked folder (asked, Settings → Data, or opts.to).
// opts.name names the ZIP.
async function install(ids, what, confirmed = false, opts = {}) {
  if (!ids.length) return null;
  let to = opts.to;
  if (!to && prefs.askWhere && !confirmed) {
    to = await askWhere(ids, what);
    if (!to) return null;
    confirmed = true;
  }
  to ||= prefs.downloadTo;
  const dir = to === 'game' ? null : await downloadFolder();
  if (to !== 'game' && !dir) return null;
  const where = to === 'zip' ? `into a ZIP in ${dir}` : to === 'files' ? `as files to ${dir}` : null;
  if ((ids.length > 1 || to !== 'game' || opts.ask) && !confirmed) {
    const ok = await ask(to === 'game' ? `Install ${what}?` : `Download ${what}?`,
      `${plural(ids.length, 'map')}${where ? `, ${where}` : ''}. ${to === 'game' ? 'Maps you already have are skipped. ' : ''}${num(quota?.left ?? 0)} left today.`, to === 'game' ? 'Install' : 'Download');
    if (!ok) return null;
  }
  const mode = to === 'game' ? { kind: 'game' } : to === 'zip' ? { kind: 'zip', dir, name: opts.name || `Atlas maps ${new Date().toISOString().slice(0, 10)}` } : { kind: 'files', dir };
  try {
    const plan = await invoke('install_maps', { ids, mode });
    await refreshQuota();
    if (plan.queued === 0) toast(plan.already ? 'Already in your game.' : 'Nothing to install.');
    else if (to !== 'game') toast(`Downloading ${plural(plan.queued, 'map')} ${where}.`);
    else toast(`Installing ${plural(plan.queued, 'map')}.` + (plan.already ? ` ${num(plan.already)} already in game.` : ''));
    return plan;
  } catch (e) {
    toast(String(e), true);
    return null;
  }
}

// Removes maps from the game (Rhythia closed; game_write.rs uninstall). Scores and replays stay.
async function uninstall(maps, after) {
  const local = m => (m.onlineId && lib?.byOnline.get(m.onlineId)?.id) ?? (m.hash && lib?.byHash.get(m.hash)?.id) ?? (lib?.maps.includes(m) ? m.id : null);
  const ids = [...new Set(maps.map(local).filter(n => n != null))];
  if (!ids.length) { toast('None of these maps is in the game.'); return false; }
  const copy = await invoke('backup_status').then(i => i.settings.afterDownload).catch(() => false);
  const ok = await ask(`Remove ${plural(ids.length, 'map')} from the game?`,
    `Close Rhythia first.${copy ? ' Atlas makes a copy first.' : ''}`, 'Remove', 'danger');
  if (!ok) return false;
  try {
    const r = await invoke('uninstall_maps', { ids });
    toast(`Removed ${plural(r.maps, 'map')} from the game.`);
    await getLibrary(true).catch(() => {});
    after?.();
    return true;
  } catch (e) { toast(String(e), true); return false; }
}

function installButton(m, small = true) {
  if (m.installed) return chip('In game', 'ok');
  if (!m.onlineId) return null;
  const btn = h('button', { class: small ? 'mini' : 'btn', type: 'button', onclick: async e => {
    e.stopPropagation();
    if (await install([m.onlineId], 'map')) { btn.replaceWith(chip('Sent', 'ok')); }
  } }, 'Install');
  return btn;
}

// ---------- map list: list, grid or compact ----------

// cols: [{ label, cell(m), cls, sort }]; card(m) gives the lines under a grid cover. With onSort,
// the headers of columns that have a sort key sort the list (titleSort: the Map column's key).
// pick: { has(m), set(m, on) } adds a box to tick maps in the list and compact views: a click ticks one,
// Shift+click a run from the last one, Ctrl+click on a row ticks it instead of opening it.
// perPage: maps on one page (0: all); page (the list's name) keeps its page while Atlas runs.
const listPages = new Map(); // list name -> page shown
function mapList({ mode, maps, cols, card, onOpen, perPage = 0, page: name, sort, onSort, titleSort, pick }) {
  const th = (label, key, cls = '') => {
    if (!key || !onSort) return h('th', { class: cls }, label);
    const on = sort?.key === key;
    return h('th', { class: cls + ' sortable' + (on ? ' on' : ''), 'aria-sort': on ? (sort.dir === 'asc' ? 'ascending' : 'descending') : 'none' },
      h('button', { type: 'button', class: 'th-btn', onclick: () => onSort(key) }, label, icon(on && sort.dir === 'asc' ? 'up' : 'down')));
  };
  const pages = perPage > 0 ? Math.max(1, Math.ceil(maps.length / perPage)) : 1;
  let at = Math.min(Math.max(1, listPages.get(name) || 1), pages);
  const box = h('div');
  const pager = h('div', { class: 'pages' });
  let anchor = null; // index in maps of the last box ticked
  function draw() {
    if (name) listPages.set(name, at);
    const from = perPage > 0 ? (at - 1) * perPage : 0;
    const part = perPage > 0 ? maps.slice(from, from + perPage) : maps;
    let body;
    if (mode === 'grid') {
      body = h('div', { class: 'cards' }, part.map(m => h('article', { class: 'card', tabindex: 0,
        onclick: () => onOpen?.(m), onkeydown: e => { if (e.key === 'Enter') onOpen?.(m); } },
        h('div', { class: 'card-art' }, cover(m)),
        h('div', { class: 'card-body' },
          h('div', { class: 't-title clamp2', title: m.title }, m.title || 'Untitled'),
          h('div', { class: 't-sub one', title: m.mappers?.join(', ') }, m.mappers?.join(', ') || ' '),
          card(m)))));
    } else {
      const withCover = mode === 'list';
      const rows = [], boxes = [];
      // Ticks maps[from + i] (and with Shift the run from the last one) to on; the boxes follow.
      const tick = (i, on, run) => {
        const n = from + i;
        const [a, b] = run && anchor != null ? [Math.min(anchor, n), Math.max(anchor, n)] : [n, n];
        for (let k = a; k <= b; k++) {
          pick.set(maps[k], on);
          const j = k - from;
          if (boxes[j]) { boxes[j].checked = on; rows[j].classList.toggle('picked', on); }
        }
        anchor = n;
        syncAll();
      };
      const box = (m, i) => {
        const input = h('input', { type: 'checkbox', 'aria-label': 'Pick', checked: pick.has(m), tabindex: -1 });
        boxes[i] = input;
        return h('td', { class: 'c-pick',
          onmousedown: e => { if (e.shiftKey) e.preventDefault(); },
          onclick: e => {
            e.stopPropagation();
            if (e.target !== input) input.checked = !input.checked;
            tick(i, input.checked, e.shiftKey);
          } }, input);
      };
      const allBox = pick && h('input', { type: 'checkbox', 'aria-label': 'Pick all on this page', onchange: e => { for (let i = 0; i < part.length; i++) tick(i, e.target.checked, false); } });
      const syncAll = () => {
        if (!allBox) return;
        const n = part.filter(pick.has).length;
        allBox.checked = n > 0 && n === part.length;
        allBox.indeterminate = n > 0 && n < part.length;
      };
      const all = pick && h('th', { class: 'c-pick' }, allBox);
      body = h('div', { class: 'tbl' + (withCover ? '' : ' compact') }, h('table', {},
        h('thead', {}, h('tr', {}, all, withCover && h('th', { class: 'c-art' }), th('Map', titleSort, 'c-map'), cols.map(c => th(c.label, c.sort, c.cls)))),
        h('tbody', {}, part.map((m, i) => rows[i] = h('tr', { tabindex: 0, class: pick?.has(m) ? 'picked' : '',
          onmousedown: e => { if (pick && e.shiftKey) e.preventDefault(); },
          onclick: e => {
            if (pick && (e.ctrlKey || e.metaKey)) { tick(i, !pick.has(m), false); return; }
            if (pick && e.shiftKey) { tick(i, true, true); return; }
            onOpen?.(m);
          },
          onkeydown: e => { if (e.key === 'Enter') onOpen?.(m); if (pick && e.key === ' ') { e.preventDefault(); tick(i, !pick.has(m), e.shiftKey); } } },
          pick && box(m, i),
          withCover && h('td', { class: 'c-art' }, cover(m)),
          h('td', { class: 'c-map' },
            h('div', { class: 't-title one', title: m.title }, m.title || 'Untitled'),
            h('div', { class: 't-sub one' }, [m.mappers?.join(', '), m.difficultyName].filter(Boolean).join(' · ') || ' ')),
          cols.map(c => h('td', { class: c.cls }, c.cell(m))))))));
      syncAll();
    }
    const go = n => { at = n; draw(); box.scrollIntoView({ block: 'start', behavior: 'smooth' }); };
    fill(pager, pages > 1 && [
      button('Previous', () => go(at - 1), 'btn ghost'),
      h('span', { class: 'dim' }, `Page ${num(at)} of ${num(pages)}`),
      button('Next', () => go(at + 1), 'btn ghost'),
    ]);
    if (pages > 1) { pager.firstChild.disabled = at <= 1; pager.children[2].disabled = at >= pages; }
    fill(box, maps.length ? body : empty('No maps here', ''), pager);
  }
  draw();
  return { el: box, redraw: draw };
}

// Ticked maps (list and compact views) and a bar for what to do with them. Picks stay while the list
// changes (another catalog page, a filter). extra(list) adds buttons for the page's own actions.
function picker(redraw, extra) {
  const picked = new Map(); // key -> map
  const keyOf = m => m.onlineId || m.hash || m.id;
  const bar = h('div', { class: 'btns pickbar', hidden: true });
  const draw = () => {
    const list = [...picked.values()];
    const get = list.filter(m => !m.installed && m.onlineId);
    bar.hidden = !list.length;
    const dl = button(get.length ? `Download picked (${num(get.length)})` : 'Picked are in game', () => install(get.map(m => m.onlineId), 'the picked maps'), 'btn');
    dl.disabled = !get.length;
    fill(bar,
      h('span', { class: 'count' }, `${plural(list.length, 'map')} picked`),
      dl,
      button('Add to collection', () => addToCollection(list.map(itemOf)), 'btn ghost'),
      extra?.(list),
      button('Clear', () => { picked.clear(); redraw(); }, 'btn ghost'));
  };
  return {
    bar,
    draw,
    pick: { has: m => picked.has(keyOf(m)), set: (m, on) => { if (on) picked.set(keyOf(m), m); else picked.delete(keyOf(m)); draw(); } },
  };
}

// ---------- your account on Rhythia, countries ----------

// Your places on Rhythia (public profile, of the account Atlas knows). Asked once.
let myRank, rankAsking = null;
const loadMyRank = () => (myRank !== undefined ? Promise.resolve(myRank)
  : (rankAsking ||= invoke('my_rank').catch(() => null).then(r => { myRank = r; rankAsking = null; return r; })));
// The countries Rhythia lets players pick (rhythia-api, utils/countryList).
const COUNTRIES = ('US AE AM AR AT AU AZ BA BB BD BE BG BH BN BO BR BT BW BY CA CH CI CL CN CO CR CU CW CY CZ DE DJ DK DO DZ EC EE EG ES ET FI FJ FO FR GA GB GE GH GR GT GU HK HN HR HU ID IE IL IM IN IQ IR IS IT JE JM JO JP KE KG KH KR KW LI LK LT LU LV MA MC MD MG MK MM MN MT MU MV MX MY NA NC NG NL NO NP NZ OM PA PE PG PH PK PL PR PS PT PY QA RE RO RU SA SD SE SG SI SK SL SN SR SV SY TG TH TN TR TT TW TZ UA UY UZ KZ VE VN XK ZA ZW').split(' ');
const regions = (() => { try { return new Intl.DisplayNames(['en'], { type: 'region' }); } catch { return null; } })();
const countryName = c => { try { return regions?.of(c) || c; } catch { return c; } };
function countryPick(current, onPick, mine) {
  const all = COUNTRIES.filter(c => c !== mine).map(c => [c, countryName(c)]).sort((a, b) => a[1].localeCompare(b[1]));
  return select([['', 'Everyone'], ...(mine ? [[mine, `${countryName(mine)} (yours)`]] : []), ...all], current, onPick, 'Country');
}

// ---------- speed check ----------
// At which speed you could pass a map, with about how many misses and how much RP: guessed on Shuriel's server
// from your public Top 100 and how players near your rank play the map (foryou.rs, /api/foryou). Atlas only
// shows the rows; your tries at each speed come from the game.

const givesRp = m => !!m.status && m.status !== 'UNRANKED';
// The chance to pass within 5 tries, from the chance a try.
const in5 = p => 1 - (1 - p) ** 5;
const CHANCE = [[0.85, 'Likely', 'pass'], [0.55, 'Maybe', ''], [0.25, 'Hard', 'dim'], [0, 'Unlikely', 'fail']];
const chanceOf = p => CHANCE.find(([at]) => in5(p) >= at);
const triesText = p => (p >= 0.5 ? 'first or second try' : p > 0.05 ? `about ${Math.round(1 / p)} tries` : 'over 20 tries');

// What the server said, by map, for a while here (foryou.rs keeps it on disk for a day, and for a week without the
// server); after no answer (no account, no plays of yours on the server) Atlas waits before asking again.
const FOR_YOU_KEEP_MS = 15 * 60 * 1000;
const FOR_YOU_WAIT_MS = 60 * 1000;
const forYouKept = new Map(); // map key -> { at, check } (check null: nothing to show)
const forYouAsking = new Map(); // map key -> the request asking about it
let forYouOffUntil = 0;
const keyOfMap = m => {
  const g = (m && inGame(m)) || m;
  return m?.onlineId ? `o${m.onlineId}` : g?.onlineId ? `o${g.onlineId}` : g?.hash ? `h${g.hash}` : null;
};
// [Rhythia id or 0, stars, notes or 0]: what the server gets for a map.
function forYouAsk(m) {
  const g = inGame(m);
  return [m.onlineId || g?.onlineId || 0, m.stars ?? g?.stars, m.notes || g?.notes || 0];
}
// Asks the server about the maps it has not answered lately; resolves when they (and the ones already being
// asked about) are answered. Never fails.
async function loadForYou(maps) {
  const now = Date.now();
  if (now < forYouOffUntil) return;
  const want = new Map(), waits = new Set();
  for (const m of maps || []) {
    const key = keyOfMap(m);
    if (!key || want.has(key) || !(m.stars ?? inGame(m)?.stars)) continue;
    if (forYouAsking.has(key)) { waits.add(forYouAsking.get(key)); continue; }
    const kept = forYouKept.get(key);
    if (kept && now - kept.at < FOR_YOU_KEEP_MS) continue;
    if (want.size < 1000) want.set(key, m);
  }
  if (want.size) {
    const keys = [...want.keys()], list = [...want.values()];
    const asking = (async () => {
      try {
        const got = await invoke('for_you', { maps: list.map(forYouAsk) });
        if (!got) { forYouOffUntil = Date.now() + FOR_YOU_KEEP_MS; return; }
        list.forEach((m, i) => forYouKept.set(keys[i], { at: now, check: readCheck(got.speeds, got.maps[i], m) }));
      } catch {
        forYouOffUntil = Date.now() + FOR_YOU_WAIT_MS;
      } finally {
        keys.forEach(k => forYouAsking.delete(k));
      }
    })();
    keys.forEach(k => forYouAsking.set(k, asking));
    waits.add(asking);
  }
  await Promise.all(waits);
}
// A row: [chance a try in %, few, middle and many misses, and with RP its low, middle and high].
function readCheck(speeds, g, m) {
  if (!g) return null;
  const notes = m.notes || inGame(m)?.notes || null;
  const rows = speeds.map((speed, i) => {
    const r = g.rows[i], misses = r.slice(1, 4);
    return { speed, chance: r[0] / 100, misses, rp: r.length === 7 ? r.slice(4, 7) : null,
      acc: notes ? misses.map(x => 100 * (1 - x / notes)).reverse() : null };
  });
  // Your best as Rhythia scored it (the game's own RP can differ: it works out the stars itself).
  return { rows, best: g.best ?? inGame(m)?.bestRp ?? m.bestRp ?? null };
}
// The server knows only your passes on Rhythia, not your fails. Your tries of the map in the game move its chance:
// a pass at a speed or faster counts as a pass, a fail at a speed or slower as a fail, and the server's chance
// counts as OWN_WEIGHT tries.
const OWN_WEIGHT = 3;
const LIKELY = CHANCE[0][0];
// The best bet: the most RP over your best at a speed you likely pass (in 5 tries); without RP the fastest such speed.
function realistic(c, m) {
  if (!c) return null;
  const id = m.onlineId || inGame(m)?.onlineId;
  const own = (id && myTries.get(id)) || [];
  const sum = (list, f) => list.reduce((n, t) => n + f(t), 0);
  const rows = !own.length ? c.rows : c.rows.map(r => {
    const passes = sum(own.filter(t => t.speed >= r.speed - 0.01), t => t.passes);
    const fails = sum(own.filter(t => t.speed <= r.speed + 0.01), t => t.tries - t.passes);
    return { ...r, chance: (OWN_WEIGHT * r.chance + passes) / (OWN_WEIGHT + passes + fails) };
  });
  const likely = rows.filter(r => in5(r.chance) >= LIKELY);
  const bet = rows.some(r => r.rp)
    ? likely.filter(r => r.rp && r.rp[1] > (c.best ?? 0)).sort((a, b) => b.rp[1] - a.rp[1])[0] ?? null
    : likely.at(-1) ?? null;
  return { ...c, rows, bet, likely: likely.length > 0 };
}
const speedCheck = m => realistic(forYouKept.get(keyOfMap(m))?.check ?? null, m);
// "1.25× · 310" (speed and RP around it) for lists.
function betText(s) {
  const b = s?.bet;
  if (!b) return none();
  return h('span', { title: `${chanceOf(b.chance)[1]} to pass (${triesText(b.chance)})${b.rp ? `, around ${num(b.rp[1])} RP` : ''}` },
    `${b.speed.toFixed(2)}×`, b.rp ? ` · ${num(b.rp[1])}` : '');
}
function speedPart(m) {
  const box = h('div', { class: 'speed' });
  const draw = () => {
    const c = speedCheck(m);
    if (!c) return;
    const at = r => here.filter(t => t.speed != null && Math.abs(t.speed - r.speed) < 0.03);
    const s = { ...c, rows: c.rows.map(r => ({ ...r, tries: at(r).length, passes: at(r).filter(t => t.passed).length })) };
    const b = s.bet && s.rows[c.rows.indexOf(c.bet)];
    const last = s.rows.reduce((n, r, i) => (in5(r.chance) >= 0.1 || r.tries ? i : n), 0);
    const rows = s.rows.slice(0, Math.min(s.rows.length, last + 2));
    const range = x => (x[0] === x[2] ? num(x[1]) : `${num(x[1])} (${num(x[0])}–${num(x[2])})`);
    fill(box,
      h('h3', {}, 'At which speed'),
      b ? h('p', {}, h('b', {}, `Best bet: ${b.speed.toFixed(2)}×`), ` · ${chanceOf(b.chance)[1].toLowerCase()} to pass, ${triesText(b.chance)}`,
        b.rp && ` · around ${num(b.rp[1])} RP`, b.rp && s.best != null && b.rp[1] > s.best && h('span', { class: 'pass' }, ` (+${num(Math.round(b.rp[1] - s.best))} over your best)`))
        : h('p', {}, c.likely ? 'No speed you likely pass beats your best yet.' : 'Too hard at every speed for now.'),
      h('div', { class: 'tbl' }, h('table', { class: 'speeds' },
        h('thead', {}, h('tr', {}, ['Speed', 'Pass', 'Accuracy', 'Misses', 'RP', 'You here'].map((x, i) => h('th', { class: i === 1 ? '' : 'n' }, x)))),
        h('tbody', {}, rows.map(r => {
          const [, word, cls] = chanceOf(r.chance);
          return h('tr', { class: r === b ? 'best' : '' },
            h('td', { class: 'n' }, `${r.speed.toFixed(2)}×`),
            h('td', {}, h('span', { class: cls }, word), h('span', { class: 'dim small' }, ` · ${triesText(r.chance)}`)),
            h('td', { class: 'n' }, r.acc ? `${r.acc[1].toFixed(2)}%` : none()),
            h('td', { class: 'n' }, r.misses ? range(r.misses) : none()),
            h('td', { class: 'n' }, r.rp ? range(r.rp) : none()),
            h('td', { class: 'n dim' }, r.tries ? `${num(r.passes)} of ${plural(r.tries, 'try', 'tries')}` : ''));
        })))),
      !givesRp(m) && h('p', { class: 'dim small' }, 'No RP on this map.'));
  };
  // Your tries at each speed, from the game.
  let here = [];
  const g = inGame(m);
  if (g) invoke('map_attempts', { onlineId: g.onlineId || null, hash: g.hash || null }).then(list => { here = list; draw(); }, () => {});
  loadForYou([m]).then(() => (speedCheck(m) ? draw() : box.remove()));
  return box;
}
// What the map's details show for you: the speed check and the players near your rank.
const forYou = m => h('div', { class: 'for-you' }, speedPart(m), nearPart(m));

// ---------- map details ----------

// The details slide in from the right and slide back out when closed (Esc, ×, or a click beside).
const sheet = document.getElementById('sheet');
function closeSheet() {
  song.pause();
  if (!sheet.open || sheet.classList.contains('closing')) return;
  sheet.classList.add('closing');
  const done = () => { if (sheet.classList.contains('closing')) { sheet.classList.remove('closing'); sheet.close(); } };
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) { done(); return; }
  sheet.addEventListener('animationend', function end(e) {
    if (e.target !== sheet) return;
    sheet.removeEventListener('animationend', end);
    done();
  });
  setTimeout(done, 500); // in case the animation never ends
}
sheet.addEventListener('cancel', e => { e.preventDefault(); closeSheet(); });
sheet.addEventListener('click', e => { if (e.target === sheet) closeSheet(); });

let catalogTags = null; // a tag clicked in a map's details: the catalog opens filtered by it
function showTag(tag) {
  catalogTags = tag;
  closeSheet();
  if (location.hash === '#catalog') show('catalog'); else location.hash = '#catalog';
}

async function openMap(m) {
  const d = sheet;
  const body = h('div', { class: 'sheet-body' });
  fill(d, h('button', { class: 'sheet-x', type: 'button', 'aria-label': 'Close', onclick: closeSheet }, icon('close')), body);
  const k = kindOf(m);
  body.append(
    h('div', { class: 'sheet-top' },
      cover(m, 'big'),
      h('div', { class: 'sheet-info' },
        h('h2', {}, m.title || 'Untitled'),
        h('p', { class: 'dim' }, [m.mappers?.join(', '), m.difficultyName].filter(Boolean).join(' · ')),
        h('div', { class: 'chips' }, statusChip(m.status), kindChip(m), m.installed ? chip('In game', 'ok') : null),
        h('dl', { class: 'facts' },
          h('dt', {}, 'Stars'), h('dd', {}, stars(m.stars)),
          h('dt', {}, 'Length'), h('dd', {}, length(m.durationMs)),
          k && mixText(k) && [h('dt', {}, 'Mix'), h('dd', {}, mixText(k))]),
        nearLine(m),
        m.tags?.length > 0 && h('div', { class: 'tags' }, m.tags.map(t =>
          h('button', { class: 'tag', type: 'button', title: `Maps tagged ${t}`, onclick: () => showTag(t) }, t))))),
    h('div', { class: 'btns' },
      !m.installed && m.onlineId && button('Install', async () => { if (await install([m.onlineId], 'map')) closeSheet(); }),
      m.onlineId && button('Watch preview', () => invoke('map_preview', { id: m.onlineId }).catch(e => toast(String(e), true)), 'btn ghost'),
      button('Add to collection', () => addToCollection([itemOf(m)]), 'btn ghost'),
      m.onlineId && button('Open on rhythia.com', () => invoke('open_on_rhythia', { kind: 'map', id: m.onlineId }), 'btn ghost'),
      m.installed && button('Remove from game', async () => { if (await uninstall([m], () => show(location.hash.slice(1)))) closeSheet(); }, 'btn danger')));
  const player = songPlayer(m);
  if (player) body.querySelector('.sheet-info').append(player);
  const tabs = [m.installed && ['mine', 'Your tries'], m.onlineId && ['top', 'Top on Rhythia']].filter(Boolean);
  const lower = h('div', { class: 'tries' });
  const tabBox = h('div', { class: 'sheet-tabs' });
  let mine = null; // the player's attempts, once read
  const drawTab = tab => {
    fill(tabBox, tabs.length > 1 ? seg(tabs, tab, drawTab, 'Scores') : '');
    lower.ticket = (lower.ticket || 0) + 1; // an answer for the other tab that comes late is dropped
    if (tab === 'top') boardPart(m, lower, mine);
    else triesPart(m, lower).then(list => { mine = list; });
  };
  body.append(forYou(m), tabBox, lower);
  d.showModal();
  if (tabs.length) drawTab(tabs[0][0]);
}

// Near your rank from Reimagined (bridge.rs; memory only): where the player's play of a map of their top
// stands among players ranked near them. Sent by Exodus from the player's own profile.
let near = new Map();
const loadNear = () => invoke('near_maps').then(n => { near = new Map(Object.entries(n)); }).catch(() => {});
listen('near-changed', loadNear);
// The list of map types changed (first download, or new maps): forget what was looked up and redraw.
listen('kinds-changed', () => { kinds.clear(); basicTried.clear(); onGame?.(); });
loadNear();
// The players ranked near you and their top plays (near.rs, from Rhythia, kept 6 hours in Atlas).
let nearGroup = null, nearAsking = null;
function loadNearGroup(force = false) {
  if (!force && (nearGroup || nearAsking)) return nearAsking || Promise.resolve(nearGroup);
  nearAsking = invoke('near_group', { force }).then(g => { nearGroup = g; return g; }).finally(() => { nearAsking = null; });
  return nearAsking;
}
const median = list => { const s = [...list].filter(v => v != null).sort((a, b) => a - b); return s.length ? s[Math.floor((s.length - 1) / 2)] : null; };
const inGame = m => (m.onlineId && lib?.byOnline.get(m.onlineId)) || (m.hash && lib?.byHash.get(m.hash)) || null;
// The group's plays of a map: by its song key (maps in the game), or the play a near list found it by.
function groupPlay(m, g) {
  if (m.near) return m.near;
  const song = m.legacyId || inGame(m)?.legacyId;
  return song ? g.plays.find(p => p.song === song) : null;
}
// "Around": the middle play of some players (RP, speed, accuracy, misses; near.rs Around).
function aroundText(a) {
  const x = a.misses == null ? null : Math.round(a.misses);
  return [`${num(Math.round(a.rp))} RP`, a.speed != null && `${a.speed.toFixed(2)}×`, a.accuracy != null && `${a.accuracy.toFixed(2)}%`,
    x != null && plural(x, 'miss', 'misses')].filter(Boolean).join(' · ');
}
// How the players ranked near you play a map (their top 100 plays): all of them, those above and those below.
function nearPart(m) {
  if (!m.near && !(m.legacyId || inGame(m)?.legacyId)) return null;
  const box = h('div', { class: 'near-you' });
  const draw = g => {
    const play = groupPlay(m, g);
    if (!play) { box.remove(); return; }
    const best = inGame(m)?.bestRp ?? m.bestRp;
    const line = (label, a) => a && h('p', {}, h('b', {}, label), ` (${num(a.players)}): around `, aroundText(a));
    fill(box,
      h('h3', {}, 'Players near your rank'),
      line('Everyone near you', play.all),
      line('Above you', play.above),
      line('Below you', play.below),
      best != null && h('p', { class: 'dim' }, `Your best: ${num(Math.round(best))} RP`));
  };
  if (nearGroup) draw(nearGroup);
  else loadNearGroup().then(draw).catch(() => box.remove());
  return box;
}
function nearLine(m) {
  const x = m.hash && near.get(m.hash);
  if (!x) return null;
  const mid = [x.rpMid != null && `${num(Math.round(x.rpMid))} RP`, x.accMid != null && `${x.accMid.toFixed(2)}%`].filter(Boolean).join(', ');
  return h('p', { class: 'near small', title: 'From Reimagined' },
    h('b', {}, 'Near your rank: '), `better than ${num(x.of - x.place)} of ${num(x.of - 1)} players`, mid && ` · middle player ${mid}`);
}

// One song plays at a time (song protocol): from the game's own files, or for a map the game does not have,
// from its map file on Rhythia (songs.rs, fetched on the first Play). Closing the details stops it.
const song = new Audio();
song.preload = 'none';
function songPlayer(m) {
  const found = m.audio ? m : (m.onlineId && lib?.byOnline.get(m.onlineId)) || (m.hash && lib?.byHash.get(m.hash));
  const local = found?.audio ? found : null;
  if (!local && !m.onlineId) return null;
  let src = local ? convertFileSrc(local.audio, 'song') : convertFileSrc(`o-${m.onlineId}`, 'song');
  let fetched = !!local;
  const play = h('button', { class: 'mini play', type: 'button', 'aria-label': 'Play' }, icon('play'));
  const bar = h('input', { class: 'seek', type: 'range', min: 0, max: 1000, value: 0, 'aria-label': 'Position' });
  const time = h('span', { class: 'dim small n' }, '0:00');
  const vol = h('input', { class: 'vol', type: 'range', min: 0, max: 100, value: Math.round(prefs.volume * 100), 'aria-label': 'Volume', title: 'Volume' });
  const mine = () => song.src === src;
  const paint = () => {
    const on = mine() && !song.paused;
    fill(play, icon(on ? 'pause' : 'play'));
    play.setAttribute('aria-label', on ? 'Pause' : 'Play');
    if (mine() && song.duration) {
      bar.value = Math.round((1000 * song.currentTime) / song.duration);
      time.textContent = `${clock(song.currentTime * 1000)} / ${clock(song.duration * 1000)}`;
    }
  };
  song.onplay = song.onpause = song.ontimeupdate = song.onloadedmetadata = paint;
  song.onerror = () => { if (mine()) { toast('This song could not be played.', true); paint(); } };
  play.addEventListener('click', async () => {
    if (!fetched) {
      play.disabled = true;
      time.textContent = 'Loading…';
      try { src = convertFileSrc(await invoke('preview_song', { id: m.onlineId }), 'song'); fetched = true; } catch (e) { toast(String(e), true); }
      play.disabled = false;
      time.textContent = '0:00';
      if (!fetched) return;
    }
    if (!mine()) {
      song.src = src;
      song.volume = prefs.volume;
      // The map's own preview start, as the game has it.
      song.addEventListener('loadedmetadata', () => { if (local?.previewMs && local.previewMs / 1000 < song.duration) song.currentTime = local.previewMs / 1000; }, { once: true });
    }
    if (song.paused) song.play().catch(() => {}); else song.pause();
  });
  bar.addEventListener('input', () => { if (mine() && song.duration) song.currentTime = (bar.value / 1000) * song.duration; });
  vol.addEventListener('input', () => { prefs.volume = vol.value / 100; song.volume = prefs.volume; });
  vol.addEventListener('change', savePrefs);
  paint();
  return h('div', { class: 'player' }, play, bar, time, vol);
}

// The player's own attempts at the map, from the game's database; answers the list.
async function triesPart(m, box) {
  fill(box, loader());
  let list;
  try { list = await invoke('map_attempts', { onlineId: m.onlineId || null, hash: m.hash || null }); } catch (e) {
    fill(box, h('p', { class: 'err' }, String(e)));
    return null;
  }
  if (!list.length) { fill(box, h('p', { class: 'dim' }, 'You have not played it yet.')); return list; }
  const passed = list.filter(a => a.passed);
  const best = bestOf(list);
  fill(box,
    h('div', { class: 'stats small' },
      stat(num(list.length), list.length === 1 ? 'try' : 'tries'),
      stat(num(passed.length), 'passed'),
      stat(best ? rp(best.rp) : '–', 'best RP'),
      stat(passed.length ? Math.max(...passed.map(a => a.accuracy ?? 0)).toFixed(2) + '%' : '–', 'best accuracy')),
    h('div', { class: 'tbl' }, h('table', {},
      h('thead', {}, h('tr', {}, ['When', 'Result', 'Accuracy', 'RP', 'Misses', 'Speed', 'Mods'].map((t, i) => h('th', { class: i > 1 ? 'n' : '' }, t)))),
      h('tbody', {}, list.map((a, i) => h('tr', { class: a === best ? 'best' : '', style: `--i:${Math.min(i, 15)}` },
        h('td', {}, when(a.at)),
        h('td', {}, a.passed ? h('span', { class: 'pass' }, 'Passed') : h('span', { class: 'fail' }, a.failMs ? `Failed at ${clock(a.failMs)}` : 'Failed')),
        h('td', { class: 'n' }, acc(a.accuracy)),
        h('td', { class: 'n' }, a.passed ? rp(a.rp) : none()),
        h('td', { class: 'n' }, a.misses == null ? none() : num(a.misses)),
        h('td', { class: 'n' }, speedText(a.speed)),
        h('td', { class: 'n' }, a.mods.length ? a.mods.map(modName).join(', ') : none())))))));
  return list;
}
const bestOf = list => list.filter(a => a.passed).reduce((b, a) => (a.rp != null && (b == null || a.rp > b.rp) ? a : b), null);
const speedText = v => (v && v !== 1 ? v.toFixed(2) + '×' : none());

// The map's top 50 on Rhythia (everyone, or one country), the player's place there and where their best in
// the game would stand. Rhythia shows no more than 50 on a map.
let boardCountry = '';
async function boardPart(m, outer, tries) {
  const ticket = (outer.ticket = (outer.ticket || 0) + 1);
  const rank = await loadMyRank();
  if (ticket !== outer.ticket) return;
  const box = h('div', {}, loader());
  fill(outer, h('div', { class: 'bar' }, countryPick(boardCountry, v => { boardCountry = v; boardPart(m, outer, tries); }, rank?.flag)), box);
  let b;
  try { b = await invoke('map_board', { id: m.onlineId, country: boardCountry || null }); } catch (e) {
    fill(box, h('p', { class: 'err' }, String(e)));
    return;
  }
  if (ticket !== outer.ticket) return;
  if (!b.scores.length) { fill(box, h('p', { class: 'dim' }, boardCountry ? `Nobody from ${countryName(boardCountry)} has a score here yet.` : 'Nobody has a score here yet.')); return; }
  if (tries === null && m.installed) tries = await invoke('map_attempts', { onlineId: m.onlineId, hash: m.hash || null }).catch(() => []);
  const best = tries && bestOf(tries);
  const meId = b.mine?.userId;
  const lines = [];
  if (b.mine) lines.push(stat(`#${num(b.mine.place)}`, b.total ? `of ${num(b.total)} on Rhythia` : 'your place'), stat(rp(b.mine.rp), 'your RP there'));
  if (best?.rp != null) {
    const above = b.scores.filter(s => s.rp != null && s.rp > best.rp).length;
    const place = above < b.scores.length || b.scores.length < 50 ? `#${num(above + 1)}` : 'past 50';
    lines.push(stat(rp(best.rp), 'your best in game'), stat(place, boardCountry ? `it would be in ${countryName(boardCountry)}` : 'it would be'));
  }
  fill(box,
    lines.length ? h('div', { class: 'stats small' }, lines) : h('p', { class: 'dim' }, 'You have no score on this map yet.'),
    h('div', { class: 'tbl' }, h('table', {},
      h('thead', {}, h('tr', {}, ['#', 'Player', 'RP', 'Accuracy', 'Misses', 'Speed', 'Mods', 'When'].map((t, i) => h('th', { class: i === 1 ? '' : 'n' }, t)))),
      h('tbody', {}, b.scores.map((s, i) => h('tr', { class: meId && s.userId === meId ? 'best' : '', style: `--i:${Math.min(i, 15)}` },
        h('td', { class: 'n' }, num(s.place)),
        h('td', {}, s.flag ? h('span', { class: 'flag' }, s.flag) : '', s.username || 'Unknown'),
        h('td', { class: 'n' }, rp(s.rp)),
        h('td', { class: 'n' }, acc(s.accuracy)),
        h('td', { class: 'n' }, s.misses == null ? none() : num(s.misses)),
        h('td', { class: 'n' }, speedText(s.speed)),
        h('td', { class: 'n' }, s.mods.length ? s.mods.map(modName).join(', ') : none()),
        h('td', { class: 'n dim' }, lastDay(s.at))))))));
}

// Adds maps to an Atlas collection (or a new one; Atlas collections can be shown in the game) or to one of the
// player's collections on the Rhythia website. The two are kept apart: first where, then which.
async function addToCollection(items) {
  const mine = await invoke('my_collections');
  let place = prefs.lastCollection.startsWith('r') ? 'rhythia' : 'atlas';
  let online = null;
  const box = h('div');
  const where = h('select', { class: 'field', 'aria-label': 'Collection' });
  const name = h('input', { class: 'field', maxlength: 80, placeholder: 'Name of the new collection' });
  const named = () => { name.hidden = place !== 'atlas' || where.value !== 'new'; };
  where.addEventListener('change', named);
  where.addEventListener('input', named);
  const choose = list => {
    fill(where, list);
    if ([...where.options].some(o => o.value === prefs.lastCollection)) where.value = prefs.lastCollection;
    named();
  };
  async function drawPlace() {
    if (place === 'atlas') {
      choose([h('option', { value: 'new' }, 'New collection'),
        ...mine.map(c => h('option', { value: `a${c.id}` }, c.name + (c.gameId ? ' · in game' : '')))]);
      fill(box, where, name);
      return;
    }
    named();
    const state = rhythiaState();
    if (!state.ok) { fill(box, rhythiaSignIn(state, drawPlace)); return; }
    fill(box, h('p', { class: 'dim small' }, state.text), loader());
    if (!online) online = await invoke('online_collections', { page: 1, search: '', mine: true, minMaps: 0 }).catch(e => { toast(String(e), true); return []; });
    if (place !== 'rhythia') return;
    choose(online.length ? online.map(c => h('option', { value: `r${c.id}` }, c.title || `Collection ${c.id}`)) : [h('option', { value: '' }, 'No collections on Rhythia yet')]);
    fill(box, h('p', { class: 'dim small' }, state.text), where);
  }
  const places = seg([['atlas', 'Atlas'], ['rhythia', 'Rhythia']], place, v => { place = v; drawPlace(); }, 'Where');
  drawPlace();
  if (!await ask('Add to collection', plural(items.length, 'map'), 'Add', { noName: true, more: [places, box] })) return;
  const value = place === 'rhythia' && !online ? '' : where.value;
  if (place === 'rhythia' && !value) { toast(rhythiaState().ok ? 'Make a collection on Rhythia first (Collections → My Rhythia).' : 'Sign in to Rhythia first.', true); return; }
  prefs.lastCollection = value;
  savePrefs();
  try {
    if (value === 'new') {
      const n = name.value.trim();
      if (!n) { toast('Give the collection a name.', true); return; }
      await invoke('collection_create', { name: n, maps: items });
      toast(`Made “${n}”.`);
    } else if (value.startsWith('a')) {
      const found = mine.find(c => `a${c.id}` === value);
      const n = await invoke('collection_add', { id: found.id, maps: items });
      toast((n ? `Added ${plural(n, 'map')} to “${found.name}”.` : `Already in “${found.name}”.`) + (n && found.gameId ? ' The game gets it when Rhythia is closed.' : ''));
    } else {
      const found = online.find(c => `r${c.id}` === value);
      const ids = items.map(i => i.onlineId).filter(Boolean);
      if (!ids.length) { toast('These maps are not on Rhythia.', true); return; }
      if (!await rhythiaReady()) return;
      const r = await onRhythia(found.id, ids, true);
      toast(sentText(r, true, found.title) + (ids.length < items.length ? ` ${num(items.length - ids.length)} not on Rhythia.` : ''), r.failed > 0);
    }
  } catch (e) { toast(String(e), true); }
}

// Whether changes on Rhythia can go now: a sign-in that still works, or one that renews by itself (Atlas' own
// window) or through Reimagined while it is connected (that sign-in lasts about an hour). { ok, text, ended }.
function rhythiaState() {
  const a = account || {};
  const reimagined = bridge?.paired?.some(p => p.connected);
  if (!a.signedIn) return { ok: false, text: 'Not signed in to Rhythia.' };
  if (a.expired && a.via === 'reimagined' && !reimagined) return { ok: false, ended: true, text: 'Rhythia sign-in ended. Open rhythia.com with Reimagined, or sign in here.' };
  const who = a.player ? `Signed in as ${a.player.name}` : 'Signed in';
  return { ok: true, text: who + (a.via === 'reimagined' ? (reimagined ? ' · with Reimagined' : ' · with Reimagined (not connected)') : '') };
}
// The state and a way to sign in; `after` runs once changes can go.
function rhythiaSignIn(state, after) {
  let waiting = true;
  const check = () => { if (waiting && rhythiaState().ok) { waiting = false; after?.(); } };
  const prevAccount = onAccount, prevBridge = onBridge;
  onAccount = () => { prevAccount?.(); check(); };
  onBridge = () => { prevBridge?.(); check(); };
  return h('div', {}, h('p', { class: state.ended ? 'err small' : 'dim small' }, state.text),
    h('div', { class: 'btns' }, button('Sign in here', signInWindow, 'btn mini'),
      button('Sign in with Reimagined', signInReimagined, 'btn ghost mini')));
}
// Before a change on Rhythia: true when it can go; else offers Atlas' own sign-in window and waits for it.
async function rhythiaReady() {
  await loadAccount();
  const state = rhythiaState();
  if (state.ok) return true;
  if (!await ask(state.ended ? 'Rhythia sign-in ended' : 'Sign in to Rhythia', state.ended ? 'Open rhythia.com with Reimagined, or sign in here.' : '', 'Sign in here')) return false;
  signInWindow();
  for (let i = 0; i < 180; i++) {
    await new Promise(r => setTimeout(r, 1000));
    await loadAccount();
    if (account?.signedIn && !account.expired) return true;
  }
  toast('Not signed in to Rhythia.', true);
  return false;
}

// Adds maps to one of the player's Rhythia collections, or removes them, with the count in a toast meanwhile.
async function onRhythia(id, ids, add) {
  toast(add ? 'Adding on Rhythia…' : 'Removing on Rhythia…');
  const off = await listen('collection-progress', e => toast(`${add ? 'Adding' : 'Removing'} on Rhythia… ${num(e.payload.done)} of ${num(e.payload.of)}`));
  try { return await invoke('rhythia_collection_maps', { id, maps: ids, add }); } finally { off(); }
}
// "Added 12 to “MegaPack”. 3 already there. 2 not added (Rhythia: Can't find beatmap page)."
function sentText(r, add, title) {
  if (add && !r.added && !r.failed && r.already) return `Already in “${title}”.`;
  const done = add ? `Added ${plural(r.added, 'map')} to “${title}”.` : `Removed ${plural(r.added, 'map')}.`;
  return done + (r.already ? ` ${num(r.already)} already ${add ? 'there' : 'out'}.` : '')
    + (r.failed ? ` ${num(r.failed)} not ${add ? 'added' : 'removed'}${r.reason ? ` (${r.reason})` : ''}.` : '');
}

// A new collection on the player's Rhythia profile with these maps (those that are on Rhythia).
async function saveToRhythia(title, maps) {
  if (!await rhythiaReady()) return;
  const ids = maps.map(m => m.onlineId).filter(Boolean);
  const name = await ask('Save to Rhythia', `${plural(ids.length, 'map')} go to a new collection on your profile.`, 'Save', { value: title });
  if (!name) return;
  toast('Saving to Rhythia…');
  try {
    const id = await invoke('rhythia_collection_create', { title: name });
    const r = ids.length ? await onRhythia(id, ids.slice(0, 1000), true) : { added: 0, already: 0, failed: 0 };
    toast(`Saved to Rhythia with ${plural(r.added, 'map')}.` + (r.failed ? ` ${num(r.failed)} not added${r.reason ? ` (${r.reason})` : ''}.` : ''), r.failed > 0);
  } catch (e) { toast(String(e), true); }
}

// "Save to Rhythia", there even while none of the maps is on Rhythia (then off).
function saveButton(title, maps) {
  const b = button('Save to Rhythia', () => saveToRhythia(title, maps), 'btn ghost');
  b.disabled = !maps.some(m => m.onlineId);
  return b;
}

// ---------- Library ----------

const LIB_SORTS = [['title', 'Title'], ['lastPlayed', 'Last played'], ['passes', 'Plays'], ['plays', 'Tries'], ['bestRp', 'Best RP'],
  ['bestAccuracy', 'Best accuracy'], ['stars', 'Stars'], ['durationMs', 'Length'], ['status', 'Status'], ['kind', 'Type']];
// Largest first puts Ranked before Unranked, and Jump, Stream, Tech, Varied in that order.
const STATUS_RANK = { RANKED: 3, APPROVED: 3, QUALIFIED: 2, UNRANKED: 1 };
const KIND_RANK = { jump: 4, stream: 3, tech: 2, varied: 1 };
const sortValue = (m, key) => (key === 'status' ? STATUS_RANK[m.status] ?? 0
  : key === 'kind' ? KIND_RANK[kindOf(m)?.kind] ?? null
  : key === 'why' ? m.why.length
  : key === 'title' ? (m.title || '').toLowerCase() : m[key]);
function sortMaps(list, { key, dir }) {
  const sign = dir === 'asc' ? 1 : -1;
  return list.sort((a, b) => {
    const x = sortValue(a, key), y = sortValue(b, key);
    if (x == null && y == null) return 0;
    if (x == null) return 1; // no value: always last
    if (y == null) return -1;
    return (typeof x === 'string' ? x.localeCompare(y) : x - y) * sign;
  });
}

async function viewLibrary(root) {
  root.append(head('Library'));
  const body = h('div', {}, loader());
  root.append(body);
  try { await getLibrary(true); } catch (e) { fill(body, empty('Could not read your game', String(e))); return; }

  const f = { status: 'all', played: 'all', kind: 'all', text: '' };
  const count = h('span', { class: 'count' });
  const out = h('div');
  const sortBox = h('span');
  const cols = [
    { label: 'Status', sort: 'status', cell: m => statusChip(m.status) },
    { label: 'Type', sort: 'kind', cell: m => kindChip(m) || none() },
    { label: 'Stars', sort: 'stars', cls: 'n', cell: m => stars(m.stars) },
    { label: 'Length', sort: 'durationMs', cls: 'n', cell: m => length(m.durationMs) },
    { label: 'Plays', sort: 'passes', cls: 'n', cell: m => (m.passes ? num(m.passes) : none()) },
    { label: 'Tries', sort: 'plays', cls: 'n', cell: m => (m.plays ? num(m.plays) : none()) },
    { label: 'Best RP', sort: 'bestRp', cls: 'n', cell: m => rp(m.bestRp) },
    { label: 'Accuracy', sort: 'bestAccuracy', cls: 'n', cell: m => acc(m.bestAccuracy) },
    { label: 'Last played', sort: 'lastPlayed', cls: 'n', cell: m => lastDay(m.lastPlayed) },
  ];
  const setSort = s => {
    prefs.sort.library = s;
    savePrefs();
    draw();
  };
  const picks = picker(() => draw(), list => button(`Remove from game (${num(list.length)})`, () => uninstall(list, () => show('library')), 'btn danger'));
  const card = m => h('div', { class: 'card-meta' },
    h('div', { class: 'chips' }, statusChip(m.status), kindChip(m)),
    h('div', { class: 'card-line' }, h('span', {}, '★ ', stars(m.stars)), h('span', {}, length(m.durationMs))),
    h('div', { class: 'card-line dim' }, m.plays
      ? [h('span', {}, `${plural(m.passes || 0, 'play')} · ${plural(m.plays, 'try', 'tries')}`), h('span', {}, m.bestRp != null ? `${num(Math.round(m.bestRp))} RP` : 'not passed')]
      : h('span', {}, 'Not played')));

  function list() {
    const text = f.text.toLowerCase();
    const outList = lib.maps.filter(m =>
      (f.status === 'all' || (f.status === 'LOCAL' ? !m.status : f.status === 'RANKED' ? isRanked(m) : m.status === f.status)) &&
      (f.played === 'all' || (f.played === 'yes' ? m.plays > 0 : m.plays === 0)) &&
      kindIs(m, f.kind) &&
      (!text || m.title.toLowerCase().includes(text) || m.mappers.some(p => p.toLowerCase().includes(text))));
    return sortMaps(outList, prefs.sort.library);
  }
  function draw() {
    const maps = list();
    const sort = prefs.sort.library;
    count.textContent = plural(maps.length, 'map');
    fill(sortBox, sortControls(LIB_SORTS, sort, setSort));
    fill(out, mapList({ mode: prefs.view.library, maps, cols, card, onOpen: openMap, pick: picks.pick, perPage: prefs.perPage, page: 'library',
      sort, titleSort: 'title', onSort: key => setSort(nextSort(sort, key, SORT_OF.library)) }).el);
    picks.draw();
  }

  let typing;
  const statsBox = h('div', { class: 'stats' });
  const drawStats = () => {
    const played = lib.maps.filter(m => m.plays > 0).length;
    const ranked = lib.maps.filter(isRanked).length;
    const unranked = lib.maps.filter(m => m.status === 'UNRANKED').length;
    fill(statsBox,
      stat(num(lib.maps.length), 'maps'),
      stat(num(ranked), 'ranked'),
      stat(num(unranked), 'unranked'),
      stat(num(played), 'played'),
      stat(num(lib.maps.length - played), 'not played yet'),
      stat(num(lib.attempts), 'tries'));
  };
  drawStats();
  // New attempts and new maps show up while the player plays.
  onGame = () => { drawStats(); draw(); loadKinds(lib.maps, draw); };
  fill(body,
    statsBox,
    h('div', { class: 'bar' },
      h('input', { class: 'field grow', type: 'search', placeholder: 'Search title or mapper', oninput: e => {
        clearTimeout(typing);
        typing = setTimeout(() => { f.text = e.target.value.trim(); draw(); }, 150);
      } }),
      seg([['all', 'All'], ['yes', 'Played'], ['no', 'Not played']], f.played, v => { f.played = v; draw(); }, 'Played'),
      select([['all', 'Any status'], ['RANKED', 'Ranked'], ['UNRANKED', 'Unranked'], ['LOCAL', 'Local']], f.status, v => { f.status = v; draw(); }, 'Status'),
      select(KIND_PICKS, f.kind, v => { f.kind = v; draw(); }, 'Type'),
      tools(sortBox, perPagePick('perPage', draw), viewSwitch('library', draw))),
    h('div', { class: 'bar' }, count),
    picks.bar,
    out);
  draw();
  loadKinds(lib.maps, draw);
}

// ---------- Catalog ----------

// Rhythia sorts the catalog itself (getBeatmaps sort + sortDirection).
const SORTS = [['newest', 'Date added'], ['playCount', 'Plays'], ['likeRatio', 'Likes'], ['starRating', 'Stars'], ['length', 'Length']];

let catalogMode = 'all';
const catalogModes = () => seg([['all', 'All maps'], ['near', 'Played near your rank'], ['players', 'Top players']], catalogMode, v => { catalogMode = v; show('catalog'); }, 'Catalog');
const forYouCol = { label: 'For you', cls: 'n', cell: m => betText(speedCheck(m)) };

// Maps the players ranked closest to you have in their top 100 and you don't have (near.rs).
let nearForce = false; // Refresh: ask Rhythia again
async function viewNearCatalog(root) {
  root.append(head('Catalog'), h('div', { class: 'bar' }, catalogModes()));
  const wait = loader();
  const out = h('div', {}, wait);
  root.append(out);
  const off = await listen('near-progress', e => wait.set(e.payload.done, e.payload.of));
  let g;
  try {
    await getLibrary().catch(() => {}); // "in game" needs the library
    g = await loadNearGroup(nearForce);
  } catch (e) { fill(out, empty('Not available', String(e))); return; } finally { off(); nearForce = false; }
  const have = new Set((lib?.maps || []).map(m => m.legacyId).filter(Boolean));
  const plays = g.plays.filter(p => !have.has(p.song) && !(p.map && lib?.byOnline.get(p.map.id)) && p.all.players >= 2);
  const per = prefs.perPageRhythia;
  let page = 1;
  const cols = [
    { label: 'Status', cell: m => statusChip(m.status) },
    { label: 'Stars', cls: 'n', cell: m => stars(m.stars) },
    { label: 'Players', cls: 'n', cell: m => num(m.near.all.players) },
    { label: 'Their RP', cls: 'n', cell: m => num(Math.round(m.near.all.rp)) },
    { label: 'Their speed', cls: 'n', cell: m => speedText(m.near.all.speed) },
    forYouCol,
    { label: '', cls: 'n', cell: m => installButton(m) },
  ];
  const card = m => h('div', { class: 'card-meta' },
    h('div', { class: 'chips' }, statusChip(m.status)),
    h('div', { class: 'card-line' }, h('span', {}, `${num(m.near.all.players)} near you`), h('span', {}, `${num(Math.round(m.near.all.rp))} RP`)),
    h('div', { class: 'card-act' }, installButton(m)));
  const picks = picker(() => draw());
  const list = h('div');
  const pager = h('div', { class: 'pages' });
  const getAll = button('Install missing', () => install(maps.filter(m => !m.installed && m.onlineId).map(m => m.onlineId), 'the missing maps on this page'));
  let maps = [], loads = 0;
  // A map found in the game by its number (not by its song key) is not new after all.
  const fresh = list => list.filter(m => !m.installed && !lib?.byOnline.get(m.onlineId));
  async function load() {
    const ticket = ++loads;
    fill(pager);
    getAll.disabled = true;
    const part = plays.slice((page - 1) * per, page * per);
    const songs = part.map(p => p.song);
    const found = () => invoke('near_maps_found', { songs }).then(got => { const by = new Map(got.map(f => [f.song, f.map])); return part.filter(p => by.has(p.song)).map(p => ({ ...fromOnline(by.get(p.song)), near: p })); });
    if (part.every(p => p.map)) {
      // From the server: shown at once; covers and Rhythia's own details come after.
      maps = fresh(part.map(p => ({ ...fromOnline(p.map), near: p })));
      shown();
      found().then(got => { if (ticket === loads && got.length) { maps = fresh(got); shown(); } }).catch(() => {});
      return;
    }
    fill(list, loader());
    try { maps = fresh(await found()); } catch (e) { fill(list, h('p', { class: 'err' }, String(e))); return; }
    if (ticket === loads) shown();
  }
  function shown() {
    getAll.disabled = !maps.length;
    getAll.textContent = maps.length ? `Install missing (${maps.length})` : 'All in game';
    draw();
    loadKinds(maps, draw);
    loadForYou(maps).then(draw);
    const pages = Math.max(1, Math.ceil(plays.length / per));
    if (pages > 1) {
      fill(pager, button('Previous', () => { page--; load(); }, 'btn ghost'), h('span', { class: 'dim' }, `Page ${num(page)} of ${num(pages)}`), button('Next', () => { page++; load(); }, 'btn ghost'));
      pager.firstChild.disabled = page <= 1;
      pager.lastChild.disabled = page >= pages;
    }
  }
  function draw() {
    fill(list, mapList({ mode: prefs.view.catalog, maps, cols, card, onOpen: openMap, pick: picks.pick }).el);
    picks.draw();
  }
  const when = new Date(g.at * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  fill(out,
    h('div', { class: 'bar' }, tools(perPagePick('perPageRhythia', () => { page = 1; load(); }), viewSwitch('catalog', draw))),
    h('div', { class: 'bar' },
      h('span', { class: 'count' }, `${plural(plays.length, 'map')} you don't have`),
      h('span', { class: 'dim small' }, `as of ${when}`),
      acts(button('Refresh', () => { nearForce = true; show('catalog'); }, 'btn ghost'), getAll)),
    picks.bar, list, pager);
  if (plays.length) load(); else fill(list, empty('Nothing new', 'You have every map the players near you play.'));
}

// Rhythia can't search by map type, and only Ranked and Qualified maps have one (vps.rs). With a type picked, Atlas
// reads every page of the search once (Ranked: about 25 pages of 50, then kept 10 minutes by net.rs), keeps the
// maps of that type and pages them itself; at most SCAN_PAGES pages.
const TYPED_STATUSES = ['RANKED', 'QUALIFIED'];
const SCAN_PAGES = 60;
const RHYTHIA_AT_ONCE = 4; // as many as net.rs sends to Rhythia at once

async function viewCatalog(root) {
  if (catalogMode === 'near') return viewNearCatalog(root);
  if (catalogMode === 'players') return viewPlayers(root);
  const q = { page: 1, text: '', tags: catalogTags || '', status: '', minStars: null, maxStars: null, kind: 'all' };
  catalogTags = null;
  const info = h('span', { class: 'count' });
  const out = h('div');
  const sortBox = h('span');
  const getAll = button('Install missing', null);
  let maps = [];
  let scan = null; // with a type: every map of the search ({ id, maps, capped })
  try { await getLibrary(); } catch { /* covers and "in game" need the library; the catalog works without */ }
  if (!SORTS.some(([k]) => k === prefs.sort.catalog.key)) prefs.sort.catalog = { ...SORT_OF.catalog };

  const cols = [
    { label: 'Status', cell: m => statusChip(m.status) },
    { label: 'Type', cell: m => kindChip(m) || none() },
    { label: 'Stars', sort: 'starRating', cls: 'n', cell: m => stars(m.stars) },
    { label: 'Length', sort: 'length', cls: 'n', cell: m => length(m.durationMs) },
    { label: 'Plays', sort: 'playCount', cls: 'n', cell: m => num(m.onlinePlays) },
    forYouCol,
    { label: '', cls: 'n', cell: m => installButton(m) },
  ];
  const setSort = s => {
    prefs.sort.catalog = s;
    savePrefs();
    q.page = 1;
    load();
  };
  const picks = picker(() => draw());
  const card = m => h('div', { class: 'card-meta' },
    h('div', { class: 'chips' }, statusChip(m.status), kindChip(m)),
    h('div', { class: 'card-line' }, h('span', {}, '★ ', stars(m.stars)), h('span', {}, length(m.durationMs))),
    h('div', { class: 'card-act' }, installButton(m)));

  const starsIn = (key, ph) => h('input', { class: 'field tiny', type: 'number', min: 0, max: 20, step: 0.5, placeholder: ph, 'aria-label': ph,
    onchange: e => { const v = e.target.value === '' ? null : Number(e.target.value); q[key] = Number.isFinite(v) ? v : null; q.page = 1; load(); } });
  const pager = h('div', { class: 'pager' });
  // A type keeps the status at Ranked or Qualified; any other status clears the type.
  const statusPick = select([['', 'Any status'], ['RANKED', 'Ranked'], ['QUALIFIED', 'Qualified'], ['UNRANKED', 'Unranked'], ['APPROVED', 'Legacy']], q.status, v => {
    q.status = v;
    if (!TYPED_STATUSES.includes(v)) kindPick.value = q.kind = 'all';
    q.page = 1;
    load();
  }, 'Status');
  const kindPick = select(KIND_PICKS, q.kind, v => {
    q.kind = v;
    if (v !== 'all' && !TYPED_STATUSES.includes(q.status)) statusPick.value = q.status = 'RANKED';
    q.page = 1;
    load();
  }, 'Type');

  root.append(
    head('Catalog'),
    h('div', { class: 'bar' }, catalogModes()),
    h('div', { class: 'bar' },
      h('form', { class: 'grow row', onsubmit: e => {
        e.preventDefault();
        q.text = e.target.elements.q.value.trim();
        q.tags = e.target.elements.tags.value.trim();
        q.page = 1;
        load();
      } },
        h('input', { class: 'field grow', name: 'q', type: 'search', placeholder: 'Search maps' }),
        h('input', { class: 'field tags-in', name: 'tags', type: 'search', placeholder: 'Tags', 'aria-label': 'Tags', maxlength: 60, value: q.tags }),
        h('button', { class: 'btn ghost', type: 'submit' }, 'Search')),
      statusPick,
      kindPick,
      h('span', { class: 'row' }, starsIn('minStars', 'Min ★'), starsIn('maxStars', 'Max ★')),
      tools(sortBox, perPagePick('perPageRhythia', () => { q.page = 1; load(); }), viewSwitch('catalog', draw))),
    h('div', { class: 'bar' }, info, acts(getAll)),
    picks.bar,
    out, pager);

  getAll.addEventListener('click', () => install(maps.filter(m => !m.installed && m.onlineId).map(m => m.onlineId), 'the missing maps on this page'));

  function draw() {
    const sort = prefs.sort.catalog;
    fill(sortBox, sortControls(SORTS, sort, setSort));
    fill(out, mapList({ mode: prefs.view.catalog, maps, cols, card, onOpen: openMap, pick: picks.pick,
      sort, onSort: key => setSort(nextSort(sort, key, SORT_OF.catalog)) }).el);
    picks.draw();
  }

  // One of Rhythia's pages of 50 for the search.
  const ask = n => {
    const { key, dir } = prefs.sort.catalog;
    const { kind, ...search } = q;
    return invoke('catalog', { query: { ...search, page: n, sort: key, ascending: dir === 'asc', status: q.status || null } });
  };
  // Atlas pages of `per` maps over Rhythia's pages of 50: one or two of them.
  async function rhythiaPage(per) {
    const start = (q.page - 1) * per;
    const first = Math.floor(start / 50) + 1, last = Math.floor((start + per - 1) / 50) + 1;
    const got = await Promise.all(first === last ? [ask(first)] : [ask(first), ask(last)]);
    const skip = start - (first - 1) * 50;
    return { total: got[0].total, maps: got.flatMap(p => p.maps).slice(skip, skip + per), info: `${num(got[0].total)} maps` };
  }
  async function typedPage(per, wait, stale) {
    const { key, dir } = prefs.sort.catalog;
    const id = JSON.stringify([q.text, q.tags, q.status, q.minStars, q.maxStars, key, dir]);
    if (scan?.id !== id) {
      const first = await ask(1);
      const pages = Math.min(SCAN_PAGES, Math.ceil(first.total / 50));
      const all = [...first.maps];
      for (let n = 2; n <= pages; n += RHYTHIA_AT_ONCE) {
        if (stale()) return null;
        const part = await Promise.all(Array.from({ length: Math.min(RHYTHIA_AT_ONCE, pages - n + 1) }, (_, i) => ask(n + i)));
        all.push(...part.flatMap(p => p.maps));
        wait.set(n + part.length - 1, pages);
      }
      const ids = new Set(); // a map that moved between pages while they were read comes once
      const found = all.filter(m => !ids.has(m.id) && ids.add(m.id));
      await loadKinds(found.map(fromOnline));
      scan = { id, maps: found, capped: first.total > pages * 50 };
    }
    const found = scan.maps.filter(m => kindIs(fromOnline(m), q.kind));
    q.page = Math.min(q.page, Math.max(1, Math.ceil(found.length / per)));
    const start = (q.page - 1) * per;
    return { total: found.length, maps: found.slice(start, start + per),
      info: `${num(found.length)} maps${scan.capped ? ` in the first ${num(SCAN_PAGES * 50)}` : ''}` };
  }

  let loading = 0;
  async function load() {
    const ticket = ++loading;
    getAll.disabled = true;
    fill(sortBox, sortControls(SORTS, prefs.sort.catalog, setSort));
    const wait = loader();
    fill(out, wait);
    fill(pager);
    const per = prefs.perPageRhythia;
    let got;
    try { got = await (q.kind === 'all' ? rhythiaPage(per) : typedPage(per, wait, () => ticket !== loading)); } catch (e) {
      if (ticket === loading) fill(out, h('p', { class: 'err' }, String(e)));
      return;
    }
    if (!got || ticket !== loading) return;
    maps = got.maps.map(fromOnline);
    const pages = Math.max(1, Math.ceil(got.total / per));
    info.textContent = got.info;
    const missing = maps.filter(m => !m.installed).length;
    getAll.disabled = missing === 0;
    getAll.textContent = missing ? `Install missing (${missing})` : 'All in game';
    fill(pager,
      button('Previous', () => { q.page--; load(); }, 'btn ghost'),
      h('span', { class: 'dim' }, `Page ${num(q.page)} of ${num(pages)}`),
      button('Next', () => { q.page++; load(); }, 'btn ghost'));
    pager.firstChild.disabled = q.page <= 1;
    pager.lastChild.disabled = q.page >= pages;
    draw();
    view.scrollTop = 0;
    loadKinds(maps, draw);
    loadForYou(maps).then(draw);
  }
  load();
}

// Rhythia's ranking of active players by RP, 50 a page, of everyone or of one country (getLeaderboard).
const playersQ = { page: 1, country: '' };
async function viewPlayers(root) {
  root.append(head('Catalog'), h('div', { class: 'bar' }, catalogModes()));
  const bar = h('div', { class: 'bar' });
  const info = h('div', { class: 'bar' });
  const out = h('div', {}, loader());
  const pager = h('div', { class: 'pager' });
  root.append(bar, info, out, pager);
  const rank = await loadMyRank();
  let ticket = 0;
  // Your place: among everyone, or in your own country.
  const myPlace = () => (!rank ? null : !playersQ.country ? rank.position : playersQ.country === rank.flag ? rank.countryPosition : null);
  async function load() {
    const t = ++ticket;
    const place = myPlace();
    const count = h('span', { class: 'count' });
    fill(bar, countryPick(playersQ.country, v => { playersQ.country = v; playersQ.page = 1; load(); }, rank?.flag));
    fill(info, count, place && acts(button(`Find me (#${num(place)})`, () => { playersQ.page = Math.ceil(place / 50); load(); }, 'btn ghost')));
    fill(out, loader());
    fill(pager);
    let p;
    try { p = await invoke('top_players', { page: playersQ.page, country: playersQ.country || null }); } catch (e) {
      if (t === ticket) fill(out, h('p', { class: 'err' }, String(e)));
      return;
    }
    if (t !== ticket) return;
    count.textContent = plural(p.total, 'player');
    const world = !!playersQ.country;
    const change = c => (!c ? none() : h('span', { class: c > 0 ? 'pass' : 'fail' }, c > 0 ? `+${num(c)}` : `−${num(-c)}`));
    fill(out, p.players.length ? h('div', { class: 'tbl' }, h('table', { class: 'players' },
      h('thead', {}, h('tr', {}, ['#', 'Player', world && 'In the world', 'RP', 'Plays', 'Lately'].filter(Boolean).map((x, i) => h('th', { class: i === 1 ? '' : 'n' }, x)))),
      h('tbody', {}, p.players.map((x, i) => h('tr', { class: rank && x.id === rank.id ? 'best click' : 'click', style: `--i:${Math.min(i, 15)}`, title: 'Open on rhythia.com',
        onclick: () => invoke('open_on_rhythia', { kind: 'player', id: x.id }).catch(e => toast(String(e), true)) },
        h('td', { class: 'n' }, num(x.place)),
        h('td', {}, x.flag ? h('span', { class: 'flag', title: countryName(x.flag) }, x.flag) : '', x.clan ? h('span', { class: 'dim' }, `[${x.clan}] `) : '', x.name || 'Unknown'),
        world && h('td', { class: 'n dim' }, x.position ? `#${num(x.position)}` : none()),
        h('td', { class: 'n' }, rp(x.rp)),
        h('td', { class: 'n' }, x.plays == null ? none() : num(x.plays)),
        h('td', { class: 'n' }, change(x.change))))))) : empty('Nobody here', 'No active players on this page.'));
    const go = h('input', { class: 'field tiny', type: 'number', min: 1, max: p.pages, value: p.page, 'aria-label': 'Page',
      onchange: e => { const n = Math.round(Number(e.target.value)); if (n >= 1 && n <= p.pages) { playersQ.page = n; load(); } } });
    fill(pager,
      button('Previous', () => { playersQ.page--; load(); }, 'btn ghost'),
      h('span', { class: 'dim row' }, 'Page ', go, ` of ${num(p.pages)}`),
      button('Next', () => { playersQ.page++; load(); }, 'btn ghost'));
    pager.firstChild.disabled = p.page <= 1;
    pager.lastChild.disabled = p.page >= p.pages;
    view.scrollTop = 0;
  }
  load();
}

// ---------- Collections ----------

let collectionsTab = 'atlas';
let openCollection = null; // { source: 'atlas'|'rhythia'|'game', id }

function collectionLink(text) {
  const m = String(text).trim().match(/^(?:https?:\/\/(?:www\.)?rhythia\.com\/collections\/)?(\d{1,10})\/?$/);
  return m ? Number(m[1]) : null;
}

async function viewCollections(root) {
  if (openCollection) return collectionDetail(root, openCollection);
  root.append(head('Collections'));
  const out = h('div');
  root.append(h('div', { class: 'bar' },
    seg([['atlas', 'Atlas'], ['mine', 'My Rhythia'], ['rhythia', 'Rhythia'], ['game', 'In game']], collectionsTab, v => { collectionsTab = v; draw(); }, 'Where')), out);
  const pick = (source, id) => { openCollection = { source, id }; show('collections'); };
  try { await getLibrary(); } catch { /* covers, types and "in your game" need the library */ }

  // Cards for the collections' maps known here (Atlas, game): filled at once.
  const known = (list, source, toMaps, chipsOf) => {
    const cards = list.map(c => {
      const maps = toMaps(c);
      const card = collCard({ title: c.name || 'Untitled', sub: plural(maps.length, 'map'), count: maps.length, chips: chipsOf?.(c), onOpen: () => pick(source, c.id) });
      card.fill(maps);
      return card.el;
    });
    loadKinds(list.flatMap(toMaps), () => out.querySelectorAll('[data-refill]').forEach(el => el.refill?.()));
    return collGrid(cards);
  };

  async function drawAtlas() {
    const mine = await invoke('my_collections');
    const add = h('form', { class: 'bar', onsubmit: async e => {
      e.preventDefault();
      const name = e.target.elements.name.value.trim();
      if (!name) return;
      try { const c = await invoke('collection_create', { name, maps: [] }); pick('atlas', c.id); } catch (err) { toast(String(err), true); }
    } }, h('input', { class: 'field grow', name: 'name', placeholder: 'New collection name', maxlength: 80 }), h('button', { class: 'btn', type: 'submit' }, 'Create'),
      tools(viewSwitch('collections', draw)));
    fill(out, add, mine.length
      ? known(mine, 'atlas', c => c.maps.map(fromItem),
        c => [c.gameId && chip('In game', 'ok'), c.wantGame && chip('After Rhythia closes', 'wait')])
      : empty('No collections yet', 'Make one above.'));
  }

  // Rhythia collections as cards (covers, types and "in your game" from the first 100 maps, asked once on screen).
  const onlineCards = list => collGrid(list.map(c => {
    const card = collCard({
      title: c.title || `Collection ${c.id}`,
      sub: [c.ownerName ? `by ${c.ownerName}` : '', c.created?.slice(0, 10)].filter(Boolean).join(' · '),
      desc: c.description, count: c.maps, stars: c.stars, onOpen: () => pick('rhythia', c.id),
    });
    if (c.maps > 0) card.later(() => peekCollection(c.id));
    else card.fill([]);
    return card.el;
  }));

  // The player's own Rhythia collections: make one here; open one to rename it, delete it or change its maps.
  async function drawMine() {
    if (!account?.signedIn) {
      fill(out, empty('Sign in to Rhythia', ''),
        h('div', { class: 'btns' }, button('Sign in to Rhythia', signInWindow), button('Sign in with Reimagined', signInReimagined, 'btn ghost')));
      onAccount = () => { if (collectionsTab === 'mine' && account?.signedIn) draw(); };
      return;
    }
    const add = h('form', { class: 'bar', onsubmit: async e => {
      e.preventDefault();
      const title = e.target.elements.name.value.trim();
      if (!title) return;
      if (!await rhythiaReady()) return;
      try { const id = await invoke('rhythia_collection_create', { title }); toast(`Made “${title}” on Rhythia.`); pick('rhythia', id); } catch (err) { toast(String(err), true); }
    } }, h('input', { class: 'field grow', name: 'name', placeholder: 'New collection on Rhythia', maxlength: 60 }), h('button', { class: 'btn', type: 'submit' }, 'Create'),
      tools(viewSwitch('collections', draw)));
    // Who is signed in, and a way to sign in again when changes could not go (rhythiaState).
    const state = rhythiaState();
    const who = state.ok ? h('p', { class: 'dim small' }, state.text) : rhythiaSignIn(state, () => { if (collectionsTab === 'mine') draw(); });
    fill(out, who, add, loader());
    let list;
    try { list = await invoke('online_collections', { page: 1, search: '', mine: true, minMaps: 0 }); } catch (e) { fill(out, who, add, h('p', { class: 'err' }, String(e))); return; }
    if (collectionsTab !== 'mine') return;
    fill(out, who, add, list.length ? onlineCards(list)
      : empty('No collections on Rhythia yet', 'Make one above.'));
  }

  async function drawRhythia() {
    const s = { page: 1, search: '', min: 1 };
    const listBox = h('div');
    const pager = h('div', { class: 'pager' });
    const form = h('form', { class: 'bar', onsubmit: e => {
      e.preventDefault();
      const text = e.target.elements.q.value.trim();
      const id = /^\d+$|rhythia\.com\/collections\//.test(text) ? collectionLink(text) : null;
      if (id) { pick('rhythia', id); return; }
      s.search = text; s.page = 1; load();
    } },
      h('input', { class: 'field grow', name: 'q', type: 'search', placeholder: 'Search, or paste a collection link' }),
      h('button', { class: 'btn ghost', type: 'submit' }, 'Search'),
      select([['1', '1+ maps'], ['5', '5+ maps'], ['20', '20+ maps'], ['0', 'Include empty']], '1', v => { s.min = Number(v); s.page = 1; load(); }, 'Size'),
      tools(viewSwitch('collections', () => load())));
    fill(out, form, listBox, pager);
    let ticket = 0;
    async function load() {
      const mine = ++ticket;
      fill(listBox, loader());
      fill(pager);
      let list;
      try { list = await invoke('online_collections', { page: s.page, search: s.search, mine: false, minMaps: s.min }); } catch (e) {
        if (mine === ticket) fill(listBox, h('p', { class: 'err' }, String(e)));
        return;
      }
      if (mine !== ticket) return;
      fill(listBox, list.length ? onlineCards(list)
        : empty('Nothing found', s.min > 1 ? 'Smaller collections are hidden.' : ''));
      if (s.page > 1 || list.length >= 30) {
        pager.append(button('Previous', () => { s.page--; load(); }, 'btn ghost'), h('span', { class: 'dim' }, `Page ${s.page}`), button('Next', () => { s.page++; load(); }, 'btn ghost'));
        pager.firstChild.disabled = s.page <= 1;
        pager.lastChild.disabled = list.length < 30;
      }
    }
    load();
  }

  async function drawGame() {
    let list;
    try { list = await invoke('game_collections'); } catch (e) { fill(out, h('p', { class: 'err' }, String(e))); return; }
    const byId = new Map((lib?.maps || []).map(m => [m.id, m]));
    fill(out, h('div', { class: 'bar' }, h('span', { class: 'count' }, plural(list.length, 'collection')), tools(viewSwitch('collections', draw))),
      list.length
        ? known(list, 'game', c => c.maps.map(id => byId.get(id)).filter(Boolean))
        : empty('No collections in the game', ''));
  }

  function draw() {
    fill(out, loader());
    ({ atlas: drawAtlas, mine: drawMine, rhythia: drawRhythia, game: drawGame })[collectionsTab]();
  }
  draw();
}

// ---------- collection cards ----------

// The first 100 maps of a Rhythia collection, kept while Atlas runs; at most two asked at a time.
const peeked = new Map(); // collection id -> Promise of maps
let peekRunning = 0;
const peekQueue = [];
function peekCollection(id) {
  if (!peeked.has(id)) {
    peeked.set(id, new Promise((done, fail) => {
      peekQueue.push(async () => {
        try {
          const c = await invoke('online_collection', { id, firstPage: true });
          const maps = c.maps.map(fromOnline);
          await cachedKinds(maps);
          done(maps);
        } catch (e) { peeked.delete(id); fail(e); }
      });
      nextPeek();
    }));
  }
  return peeked.get(id);
}
function nextPeek() {
  while (peekRunning < 2 && peekQueue.length) {
    peekRunning++;
    peekQueue.shift()().finally(() => { peekRunning--; nextPeek(); });
  }
}
// Types from the kept list only, without working any out from map files (cards).
async function cachedKinds(maps) {
  const hashes = [...new Set(maps.filter(m => m.hash && !kinds.has(m.hash)).map(m => m.hash))];
  if (!hashes.length) return;
  try {
    const got = await invoke('map_kinds', { hashes });
    for (const [hash, k] of Object.entries(got)) kinds.set(hash, k);
  } catch { /* cards show no types */ }
}

// Grid, list or compact, as chosen for collections.
function collGrid(cards) {
  const mode = prefs.view.collections || 'grid';
  return h('div', { class: 'ccards ' + mode }, cards);
}

const avgStars = bands => {
  const n = bands.reduce((a, b) => a + b, 0);
  return n ? bands.reduce((a, b, i) => a + b * (i + 1), 0) / n : null;
};
// A collection: covers of its first maps, its name and maker, how hard (star bands), its map types and how many
// of its maps are in the game. fill(maps) draws what comes from its maps; later(load) fills it once on screen.
function collCard({ title, sub, desc, count, stars, chips, onOpen }) {
  const art = h('div', { class: 'mosaic' }, h('div', { class: 'cover blank' }, icon('noimage')));
  const types = h('div', { class: 'ctypes' });
  const game = h('span', { class: 'cgame dim small' });
  const bands = stars && stars.some(Boolean) ? stars : null;
  const avg = bands ? avgStars(bands) : null;
  // Maps by whole stars, from 1 up to the hardest band used (at least 5 bands, so a few maps don't make one wide bar).
  const hist = bands && (() => {
    const last = Math.max(5, bands.reduce((a, n, i) => (n ? i + 1 : a), 0)), shown = bands.slice(0, last), top = Math.max(...shown);
    return h('div', { class: 'hist-box' },
      h('div', { class: 'hist' }, shown.map((n, i) => h('i', { style: `height:${n ? Math.max(14, Math.round(100 * n / top)) : 0}%`, title: `${i + 1} stars: ${plural(n, 'map')}` }))),
      h('div', { class: 'hist-scale' }, h('span', {}, '1★'), h('span', {}, `${last}★`)));
  })();
  const el = h('article', { class: 'ccard', tabindex: 0, onclick: onOpen, onkeydown: e => { if (e.key === 'Enter') onOpen(); } },
    art,
    h('div', { class: 'cc-body' },
      h('div', { class: 't-title clamp2', title }, title),
      sub && h('div', { class: 't-sub one' }, sub),
      desc && h('div', { class: 'cc-desc one dim small', title: desc }, desc),
      h('div', { class: 'cc-line' }, h('b', {}, plural(count, 'map')), avg != null && h('span', { class: 'mono small' }, `★ ${avg.toFixed(1)} average`), game),
      hist,
      types,
      chips && h('div', { class: 'chips' }, chips)));
  let maps = null;
  function fillMaps(list) {
    maps = list;
    const withArt = list.filter(m => coverSrc(m)).slice(0, 4);
    if (withArt.length) fill(art, ...withArt.map(m => cover(m)));
    art.dataset.n = String(withArt.length);
    refill();
  }
  function refill() {
    if (!maps) return;
    const counts = { jump: 0, stream: 0, tech: 0, varied: 0 };
    let q = 0, typed = 0;
    for (const m of maps) { const k = kindOf(m); if (!k) continue; typed++; counts[k.kind]++; if (k.quantum) q++; }
    fill(types, ...(typed ? [
      h('div', { class: 'mix' }, Object.entries(counts).filter(([, n]) => n).map(([k, n]) => h('span', { class: 'mix-' + k, style: `flex:${n}`, title: `${KIND_NAME[k]} ${n}` }))),
      h('div', { class: 'legend' }, Object.entries(counts).filter(([, n]) => n).map(([k, n]) => h('span', {}, h('i', { class: 'mix-' + k }), `${KIND_NAME[k]} ${pct(n, typed)}%`)),
        q > 0 && h('span', {}, `Quantum ${pct(q, typed)}%`),
        h('span', { class: 'dim', title: 'Types are known for Ranked maps' }, `of ${num(typed)} with a type`)),
    ] : []));
    const inGame = maps.filter(m => m.installed).length;
    game.textContent = maps.length ? (inGame === maps.length ? 'All in your game' : `${num(inGame)} in your game`) + (count > maps.length ? ` (first ${num(maps.length)})` : '') : '';
  }
  el.dataset.refill = '';
  el.refill = refill;
  function later(load) {
    const go = () => load().then(fillMaps, () => { game.textContent = ''; });
    if (!('IntersectionObserver' in window)) { go(); return; }
    const seen = new IntersectionObserver(entries => {
      if (entries.some(e => e.isIntersecting)) { seen.disconnect(); go(); }
    }, { root: view, rootMargin: '200px' });
    seen.observe(el);
  }
  return { el, fill: fillMaps, later };
}

// What a list of maps is made of: in game, played, types, stars.
function breakdown(maps) {
  const n = maps.length;
  const inGame = maps.filter(m => m.installed).length;
  const played = maps.filter(m => m.plays > 0).length;
  const counts = { jump: 0, stream: 0, tech: 0, varied: 0 };
  let quantum = 0, known = 0;
  for (const m of maps) {
    const k = kindOf(m);
    if (!k) continue;
    known++;
    counts[k.kind]++;
    if (k.quantum) quantum++;
  }
  const starList = maps.map(m => m.stars).filter(s => s != null);
  const meter = (label, a) => h('div', { class: 'bd-row' },
    h('div', { class: 'bd-label' }, h('span', {}, label), h('b', {}, `${num(a)} of ${num(n)} · ${pct(a, n)}%`)),
    h('i', { class: 'meter' }, h('span', { style: `width:${pct(a, n)}%` })));
  const mix = h('div', { class: 'mix' }, Object.entries(counts).filter(([, c]) => c).map(([k, c]) =>
    h('span', { class: 'mix-' + k, style: `flex:${c}`, title: `${KIND_NAME[k]} ${c}` })), n - known > 0 && h('span', { class: 'mix-none', style: `flex:${n - known}`, title: `Unknown ${n - known}` }));
  const ranked = maps.filter(isRanked).length;
  return h('div', { class: 'breakdown' },
    meter('In your game', inGame),
    meter('Played', played),
    h('div', { class: 'bd-row' },
      h('div', { class: 'bd-label' }, h('span', {}, 'Ranked'), h('b', {}, `${num(ranked)} ranked · ${num(n - ranked)} unranked`)),
      h('i', { class: 'meter' }, h('span', { style: `width:${pct(ranked, n)}%` }))),
    h('div', { class: 'bd-row' },
      h('div', { class: 'bd-label' }, h('span', {}, 'Types'), h('b', {}, known ? `${num(known)} of ${num(n)} known` : maps.some(isRanked) ? 'Loading…' : 'Only Ranked maps have types')),
      mix,
      h('div', { class: 'legend' },
        Object.entries(counts).map(([k, c]) => h('span', {}, h('i', { class: 'mix-' + k }), `${KIND_NAME[k]} ${num(c)}${known ? ` · ${pct(c, known)}%` : ''}`)),
        h('span', {}, chip('Quantum', 'k-quantum'), ` ${num(quantum)}`))),
    starList.length > 0 && h('div', { class: 'bd-row' }, h('div', { class: 'bd-label' }, h('span', {}, 'Stars'),
      h('b', {}, `${Math.min(...starList).toFixed(2)} to ${Math.max(...starList).toFixed(2)} · average ${(starList.reduce((a, b) => a + b, 0) / starList.length).toFixed(2)}`))));
}

async function collectionDetail(root, which) {
  const back = h('button', { class: 'back', type: 'button', onclick: () => { openCollection = null; show('collections'); } }, icon('back'), 'Collections');
  root.append(back);
  const body = h('div', {}, loader());
  root.append(body);
  try { await getLibrary(); } catch { /* "in game" and "played" need the library */ }

  let title = '', sub = '', maps = [], actions = [], atlas = null, own = null;
  try {
    if (which.source === 'rhythia') {
      const c = await invoke('online_collection', { id: which.id });
      title = c.title || `Collection ${c.id}`;
      sub = `On Rhythia · by ${c.ownerName || 'unknown'}`;
      maps = c.maps.map(fromOnline);
      // The player's own: Atlas can change it on Rhythia.
      own = account?.player && c.owner === account.player.id ? c : null;
      if (own) sub = `On Rhythia · yours${c.isList ? ' · ranked list' : ''}`;
      actions = [
        own && button('Edit', async () => {
          const desc = h('textarea', { class: 'field', maxlength: 500, placeholder: 'Description' });
          desc.value = own.description || '';
          const list = h('input', { type: 'checkbox', checked: own.isList });
          const name = await ask('Edit collection', '', 'Save', { value: own.title, more: [desc, h('label', {}, list, 'A ranked list instead of a grid')] });
          if (!name || !await rhythiaReady()) return;
          try {
            await invoke('rhythia_collection_edit', { id: own.id, title: name, description: desc.value, isList: list.checked });
            toast('Saved on Rhythia.');
            show('collections');
          } catch (e) { toast(String(e), true); }
        }, 'btn ghost'),
        own && button('Delete', async () => {
          if (!await ask(`Delete “${title}” from Rhythia?`, '', 'Delete', 'danger') || !await rhythiaReady()) return;
          try {
            await invoke('rhythia_collection_delete', { id: own.id });
            toast('Deleted from Rhythia.');
            openCollection = null;
            collectionsTab = 'mine';
            show('collections');
          } catch (e) { toast(String(e), true); }
        }, 'btn danger'),
        button('Download as collection', () => asCollection(null), 'btn ghost'),
        button('Save to Atlas', async () => {
          try { const made = await invoke('collection_create', { name: title, maps: maps.map(itemOf) }); toast(`Saved as “${made.name}”.`); } catch (e) { toast(String(e), true); }
        }, 'btn ghost'),
        !own && saveButton(title, maps),
        button('Open on rhythia.com', () => invoke('open_on_rhythia', { kind: 'collection', id: c.id }), 'btn ghost'),
      ];
    } else if (which.source === 'game') {
      const c = (await invoke('game_collections')).find(x => x.id === which.id);
      if (!c) throw new Error('This collection is gone from the game.');
      const byId = new Map(lib.maps.map(m => [m.id, m]));
      title = c.name || 'Untitled';
      sub = 'In the game';
      maps = c.maps.map(id => byId.get(id)).filter(Boolean);
      actions = [button('Copy to Atlas', async () => {
        try { const made = await invoke('collection_create', { name: title, maps: maps.map(itemOf) }); toast(`Copied as “${made.name}”.`); } catch (e) { toast(String(e), true); }
      }, 'btn ghost')];
    } else {
      atlas = (await invoke('my_collections')).find(x => x.id === which.id);
      if (!atlas) throw new Error('This collection is gone.');
      title = atlas.name;
      sub = atlas.wantGame ? 'In Atlas · after Rhythia closes' : atlas.gameId ? 'In Atlas · shown in the game' : 'In Atlas';
      maps = atlas.maps.map(fromItem);
      actions = [
        maps.some(m => !m.installed && m.onlineId) && button('Download as collection', () => asCollection(atlas), 'btn ghost'),
        saveButton(title, maps),
        button(atlas.gameId ? 'Update in game' : 'Show in game', async () => {
          const ok = await ask(atlas.gameId ? 'Update it in the game?' : 'Show it in the game?',
            'Close Rhythia first. Maps you don’t have are left out.', 'Go');
          if (!ok) return;
          try {
            const w = await invoke('collection_to_game', { id: atlas.id });
            toast(`In the game with ${plural(w.maps, 'map')}.` + (w.missing ? ` ${num(w.missing)} not installed, left out.` : ''));
            show('collections');
          } catch (e) {
            // The game is open: offer to do it by itself once it closes.
            if (!/Close Rhythia/.test(String(e))) { toast(String(e), true); return; }
            if (!await ask('Rhythia is open', 'Put it in the game when you close Rhythia?', 'Do it then')) return;
            await invoke('collection_want_game', { id: atlas.id, want: true });
            toast('It goes in the game when Rhythia closes.');
            show('collections');
          }
        }, 'btn ghost'),
        atlas.wantGame && button('Don’t put it in the game', async () => {
          await invoke('collection_want_game', { id: atlas.id, want: false });
          show('collections');
        }, 'btn ghost'),
        button('Rename', async () => {
          const name = await ask('Rename collection', '', 'Rename', { value: atlas.name });
          if (!name) return;
          try { await invoke('collection_rename', { id: atlas.id, name }); show('collections'); } catch (e) { toast(String(e), true); }
        }, 'btn ghost'),
        button('Delete', async () => {
          if (!await ask(`Delete “${atlas.name}”?`, 'Maps stay in the game.', 'Delete', 'danger')) return;
          await invoke('collection_delete', { id: atlas.id });
          openCollection = null;
          show('collections');
        }, 'btn danger'),
      ];
    }
  } catch (e) {
    fill(body, empty('Could not open the collection', String(e.message || e)));
    return;
  }

  const missing = maps.filter(m => !m.installed && m.onlineId);
  const getAll = button(missing.length ? `Download maps (${num(missing.length)})` : 'All in game',
    () => install(missing.map(m => m.onlineId), `the missing maps of “${title}”`, false, { to: 'game' }));
  getAll.disabled = !missing.length;
  // Every map of the collection into a ZIP (parts of up to 1.5 GB), whatever is in the game.
  const online = maps.filter(m => m.onlineId);
  const getZip = online.length > 0 && button('Download ZIP', () => install(online.map(m => m.onlineId), `“${title}” as a ZIP`, false, { to: 'zip', name: title }), 'btn ghost');

  // "Download as collection": the missing maps are installed, the collection is kept in Atlas (a Rhythia one is
  // saved first) and goes into the game by itself once Rhythia is closed (lib.rs show_waiting).
  async function asCollection(existing) {
    const n = missing.length;
    const ok = await ask('Download as a collection?',
      `${n ? `Atlas installs the ${plural(n, 'missing map')}, ` : ''}keeps “${title}” as an Atlas collection and puts it in the game as soon as Rhythia is closed. ${num(quota?.left ?? 0)} maps left today.`, 'Download');
    if (!ok) return;
    try {
      const c = existing || await invoke('collection_create', { name: title, maps: maps.map(itemOf) });
      await invoke('collection_want_game', { id: c.id, want: true });
      if (n) await install(missing.map(m => m.onlineId), `“${title}”`, true, { to: 'game' });
      toast(n ? `Downloading. “${c.name}” goes in the game when Rhythia closes.` : `“${c.name}” goes in the game when Rhythia closes.`);
    } catch (e) { toast(String(e), true); }
  }

  // Picked maps (list and compact views): download them or add them to a collection; out of the player's own
  // Rhythia collection.
  async function removeOnline(list) {
    const ids = list.map(m => m.onlineId).filter(Boolean);
    if (!await ask(`Remove ${plural(ids.length, 'map')} from “${title}”?`, '', 'Remove', 'danger')) return;
    if (!await rhythiaReady()) return;
    try {
      const r = await onRhythia(own.id, ids, false);
      toast(sentText(r, false, title), r.failed > 0);
      show('collections');
    } catch (e) { toast(String(e), true); }
  }
  const picks = picker(() => draw(), list => [
    own && button(`Remove from collection (${num(list.length)})`, () => removeOnline(list), 'btn ghost'),
    list.some(m => m.installed) && button('Remove from game', () => uninstall(list, () => show('collections')), 'btn danger')]);

  const listBox = h('div');
  const bdBox = h('div');
  const cols = [
    { label: 'Status', sort: 'status', cell: m => statusChip(m.status) },
    { label: 'Type', sort: 'kind', cell: m => kindChip(m) || none() },
    { label: 'Stars', sort: 'stars', cls: 'n', cell: m => stars(m.stars) },
    { label: 'Length', sort: 'durationMs', cls: 'n', cell: m => length(m.durationMs) },
    { label: 'Plays', sort: 'passes', cls: 'n', cell: m => (m.passes ? num(m.passes) : none()) },
    { label: 'Tries', sort: 'plays', cls: 'n', cell: m => (m.plays ? num(m.plays) : none()) },
    { label: 'Best RP', sort: 'bestRp', cls: 'n', cell: m => rp(m.bestRp) },
    { label: '', cls: 'n', cell: m => [installButton(m), (atlas || own) && removeButton(m)] },
  ];
  // The collection's own order until a header is clicked; not kept between visits.
  const ORDER = { key: null, dir: 'desc' };
  let sort = ORDER;
  function removeButton(m) {
    return h('button', { class: 'mini ghost', type: 'button', title: 'Remove from collection', onclick: async e => {
      e.stopPropagation();
      if (own) { removeOnline([m]); return; }
      await invoke('collection_remove', { id: atlas.id, maps: [itemOf(m)] });
      show('collections');
    } }, 'Remove');
  }
  const card = m => h('div', { class: 'card-meta' },
    h('div', { class: 'chips' }, statusChip(m.status), kindChip(m)),
    h('div', { class: 'card-line' }, h('span', {}, '★ ', stars(m.stars)), h('span', {}, m.plays ? `${plural(m.passes || 0, 'play')} · ${plural(m.plays, 'try', 'tries')}` : 'Not played')),
    h('div', { class: 'card-act' }, installButton(m), (atlas || own) && removeButton(m)));
  function draw() {
    fill(bdBox, breakdown(maps));
    const shown = sort.key ? sortMaps([...maps], sort) : maps;
    fill(listBox, mapList({ mode: prefs.view.collection, maps: shown, cols, card, onOpen: openMap,
      perPage: which.source === 'rhythia' ? prefs.perPageRhythia : prefs.perPage, page: `collection-${which.source}-${which.id}`,
      sort, titleSort: 'title', onSort: key => { sort = nextSort(sort, key, ORDER); draw(); }, pick: picks.pick }).el);
    picks.draw();
  }
  fill(body,
    head(title, sub),
    h('div', { class: 'btns spaced' }, getAll, getZip, actions),
    bdBox,
    h('div', { class: 'bar' }, h('span', { class: 'count' }, plural(maps.length, 'map')),
      tools(perPagePick(which.source === 'rhythia' ? 'perPageRhythia' : 'perPage', draw), viewSwitch('collection', draw))),
    picks.bar,
    listBox);
  draw();
  loadKinds(maps, draw);
}

// ---------- Downloads ----------

const STATE_TEXT = { waiting: 'Waiting', sent: 'Sent, waiting for the game', done: 'In game', failed: 'Not added', cancelled: 'Cancelled' };
// The same for files and ZIPs.
const FILE_STATE_TEXT = { waiting: 'Waiting', sent: 'Downloading', done: 'Saved', failed: 'Not saved', cancelled: 'Cancelled' };
const stateText = it => (it.to ? FILE_STATE_TEXT : STATE_TEXT)[it.state];
const ago = at => {
  const d = new Date(at * 1000);
  return d.toDateString() === new Date().toDateString() ? d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : d.toLocaleDateString();
};
const itemRow = (it, pick) => h('li', { class: 'dl ' + it.state },
  pick && h('input', { type: 'checkbox', 'aria-label': 'Pick', disabled: it.state !== 'done', checked: pick.has(it.id), onchange: e => pick.set(it, e.target.checked) }),
  icon(it.state, 'st-' + it.state),
  h('span', { class: 'one grow', title: it.title }, it.title),
  h('span', { class: 'dim small' }, stateText(it)),
  h('span', { class: 'dim small mono' }, ago(it.at)));

async function viewDownloads(root) {
  const now = h('div');
  const recent = h('div');
  root.append(head('Downloads'), quotaBox(), h('h2', { class: 'sec' }, 'Now'), now, h('h2', { class: 'sec' }, 'Recent'), recent);
  let history = [];

  function drawNow() {
    if (!job || job.done) {
      const saved = job?.done && job.folder;
      fill(now, h('p', { class: 'dim' }, 'Nothing is downloading.'),
        saved && h('div', { class: 'set' }, h('span', { class: 'small' }, job.zips?.length ? `Last ZIP: ${job.zips.map(z => z.split(/[\\/]/).pop()).join(', ')}` : `Last files went to ${job.folder}`),
          button('Open folder', () => invoke('open_folder', { path: job.folder }).catch(e => toast(String(e), true)), 'mini')),
        job?.done && job.files?.length > 0 && h('div', { class: 'drag-out', title: 'Drop onto the Rhythia window',
          onmousedown: e => { if (e.button === 0) { e.preventDefault(); invoke('drag_saved_maps').catch(err => toast(String(err), true)); } } },
          icon('down'), `Drag ${plural(job.files.length, 'map')} onto the game`),
        job?.done && job.error && h('p', { class: 'err small' }, job.error));
      return;
    }
    const sent = job.items.filter(i => i.state !== 'waiting').length;
    const total = job.items.length;
    fill(now,
      h('div', { class: 'job' },
        h('div', { class: 'job-top' }, h('b', {}, `${num(sent)} of ${num(total)} ${job.to ? 'downloaded' : 'sent'}`), button('Cancel', () => invoke('cancel_install'), 'btn ghost')),
        h('i', { class: 'meter' }, h('span', { style: `width:${pct(sent, total)}%` }))),
      h('ul', { class: 'dls' }, job.items.slice(0, 200).map(it => itemRow(it))));
  }
  // Maps that came into the game can be made into an Atlas collection.
  const picked = new Set();
  const pick = { has: id => picked.has(id), set: (it, on) => { if (on) picked.add(it.id); else picked.delete(it.id); drawRecent(); } };
  function drawRecent() {
    const done = history.filter(it => it.state === 'done');
    const today = done.filter(it => new Date(it.at * 1000).toDateString() === new Date().toDateString());
    const make = button(picked.size ? `Make a collection (${num(picked.size)})` : 'Make a collection', async () => {
      const list = done.filter(it => picked.has(it.id));
      const name = await ask('Make a collection', plural(list.length, 'map'), 'Make', { placeholder: 'Collection name' });
      if (!name) return;
      try {
        const c = await invoke('collection_create', { name, maps: list.map(it => ({ onlineId: it.id, hash: null, title: it.title, mapper: '' })) });
        picked.clear();
        toast(`Made “${c.name}”.`);
        drawRecent();
      } catch (e) { toast(String(e), true); }
    });
    make.disabled = !picked.size;
    fill(recent, ...(history.length
      ? [h('div', { class: 'btns spaced' },
        make,
        today.length > 0 && button(`Pick today’s (${num(today.length)})`, () => { for (const it of today) picked.add(it.id); drawRecent(); }, 'btn ghost'),
        picked.size > 0 && button('Remove picked from game', async () => {
          if (await uninstall(done.filter(it => picked.has(it.id)).map(it => ({ onlineId: it.id })))) { picked.clear(); drawRecent(); }
        }, 'btn ghost'),
        picked.size > 0 && button('Clear picks', () => { picked.clear(); drawRecent(); }, 'btn ghost'),
        button('Clear list', async () => { await invoke('clear_downloads'); history = []; picked.clear(); drawRecent(); }, 'btn ghost')),
      h('ul', { class: 'dls' }, history.map(it => itemRow(it, pick)))]
      : [h('p', { class: 'dim' }, 'No downloads yet.')]));
  }
  const d = await invoke('downloads');
  job = d.job;
  history = d.history;
  drawNow();
  drawRecent();
  return async () => {
    drawNow();
    if (job?.done) { history = (await invoke('downloads')).history; drawRecent(); }
  };
}

// ---------- Settings ----------

let settingsTab = 'look';
const mb = b => (b >= 1048576 ? `${(b / 1048576).toFixed(1)} MB` : `${Math.ceil(b / 1024)} KB`);

async function viewSettings(root) {
  let info = await invoke('app_info');
  const box = h('div', { class: 'sets' });
  root.append(head('Settings'),
    h('div', { class: 'bar' }, seg([['look', 'Look'], ['game', 'Game'], ['account', 'Account'], ['data', 'Data'], ['about', 'About']],
      settingsTab, v => { settingsTab = v; draw(); }, 'Settings')),
    box);
  const set = (label, ...right) => h('div', { class: 'set' }, h('span', {}, label), h('span', { class: 'set-r' }, ...right));

  function look() {
    const modes = [['list', 'List'], ['grid', 'Grid'], ['compact', 'Compact']];
    const viewRow = (page, label) => set(label, seg(modes, prefs.view[page], v => { prefs.view[page] = v; savePrefs(); }, label));
    return [
      set('Theme', seg([['dark', 'Dark'], ['light', 'Light']], prefs.theme, v => { prefs.theme = v; savePrefs(); applyTheme(); }, 'Theme')),
      set('Size', select(ZOOMS.map(z => [String(z), `${Math.round(z * 100)}%`]), String(prefs.zoom),
        v => { prefs.zoom = Number(v); savePrefs(); applyZoom(); }, 'Size')),
      h('h2', { class: 'sec' }, 'Lists'),
      viewRow('library', 'Library'),
      viewRow('catalog', 'Catalog'),
      viewRow('collections', 'Collections'),
      viewRow('collection', 'Collection maps'),
    ];
  }

  function game() {
    const pick = async auto => {
      try {
        info = await invoke(auto ? 'game_folder_auto' : 'game_folder_pick');
        lib = null;
        draw();
        if (info.gameData) toast('Found Rhythia’s data.');
      } catch (e) { toast(String(e), true); }
    };
    return [
      set('Data folder', info.gameData ? h('span', { class: 'mono small path', title: info.gameData }, info.gameData) : h('span', { class: 'err small' }, info.gameError)),
      set(info.gameChosen ? 'Picked by you' : 'Found by Atlas',
        button('Choose folder…', () => pick(false), 'mini'),
        info.gameChosen && button('Find it again', () => pick(true), 'mini')),
    ];
  }

  function data() {
    const covers = h('span', { class: 'small dim' }, '…');
    const folder = h('span', { class: 'mono small path' }, '…');
    invoke('data_info').then(d => {
      covers.textContent = `${num(d.coverFiles)} covers, ${mb(d.coverBytes)}`;
      folder.textContent = d.folder;
      folder.title = d.folder;
    });
    return [
      h('h2', { class: 'sec' }, 'Downloads'),
      quotaBox(),
      set('Install', seg([['ask', 'Ask every time'], ['game', 'To the game'], ['files', 'Map files'], ['zip', 'ZIP']], prefs.askWhere ? 'ask' : prefs.downloadTo,
        async v => {
          prefs.askWhere = v === 'ask';
          if (v !== 'ask') prefs.downloadTo = v;
          if (v !== 'game' && v !== 'ask' && !prefs.downloadDir) await downloadFolder();
          savePrefs();
          draw();
        }, 'Install')),
      (prefs.askWhere || prefs.downloadTo !== 'game') && set('Folder', h('span', { class: 'mono small path', title: prefs.downloadDir }, prefs.downloadDir || 'Not picked'),
        button('Choose…', async () => { await downloadFolder(true); draw(); }, 'mini')),
      backupPanel(),
      h('h2', { class: 'sec' }, 'On this computer'),
      set('Covers', covers, button('Clear', async () => { await invoke('clear_covers'); draw(); toast('Covers cleared.'); }, 'mini')),
      set('Atlas data', folder, button('Open', () => invoke('open_data_folder').catch(e => toast(String(e), true)), 'mini')),
      set('Settings', button('Reset to defaults', async () => {
        if (!await ask('Reset settings?', 'Your collections stay.', 'Reset')) return;
        try { localStorage.removeItem('atlas.prefs'); } catch { /* nothing kept */ }
        location.reload();
      }, 'mini')),
      set('Uninstall', h('span', { class: 'small dim' }, 'Your maps, scores and game collections stay.'), button('Uninstall Atlas…', uninstallAtlas, 'mini danger')),
    ];
  }

  function about() {
    const state = h('span', { class: 'small dim' });
    const check = button('Check now', async () => {
      check.disabled = true;
      state.textContent = 'Checking…';
      try {
        const found = await invoke('update_check');
        state.textContent = found ? `Atlas ${found.version} is out.` : 'You have the newest Atlas.';
        if (found) offerUpdate(found);
      } catch (e) { state.textContent = String(e); }
      check.disabled = false;
    }, 'mini');
    return [
      set('Version', h('span', { class: 'mono small' }, `v${info.version} ${STAGE}`)),
      set('License', h('span', { class: 'small' }, '© 2026 Shuriel · free software under GPL-3.0, with no warranty')),
      set('Updates', state, check),
      set('Limits', h('span', { class: 'small' }, `${num(info.mapsPerDay)} maps a day, ${info.rhythiaPerSecond} requests a second to Rhythia`)),
    ];
  }

  function draw() {
    onBridge = null;
    fill(box, ...({ look, game, account: () => [accountPanel(), bridgePanel()], data, about })[settingsTab]());
  }
  draw();
}

// Settings → Data → Uninstall: Windows' own uninstaller in its quick mode, and Atlas closes (lib.rs uninstall_atlas).
async function uninstallAtlas() {
  let u;
  try { u = await invoke('uninstall_info'); } catch (e) { toast(String(e), true); return; }
  if (!u.installed) { toast('Only an installed Atlas can uninstall itself. Use Windows Settings → Apps.', true); return; }
  const wipe = h('input', { type: 'checkbox', checked: true });
  const ok = await ask('Uninstall Atlas?', 'Atlas closes and Windows removes it. Your maps, scores and collections in the game stay.', 'Uninstall', {
    noName: true, danger: true,
    more: [h('label', {}, wipe, `Also delete my Atlas data: Atlas collections, settings and the Rhythia sign-in${u.copiesInData ? ', and the copies of your maps' : ''}.`),
      !u.copiesInData && h('p', { class: 'small' }, `Your copies in ${u.copiesDir} stay.`)],
  });
  if (!ok) return;
  try { await invoke('uninstall_atlas', { deleteData: wipe.checked }); } catch (e) { toast(String(e), true); }
}

// ---------- copies of maps and replays ----------

const gb = b => (b >= 1024 ** 3 ? `${(b / 1024 ** 3).toFixed(1)} GB` : mb(b));
// Settings → Data: what the game's files take, and copies of them (backup.rs). Copies are made only
// while the game is closed.
function backupPanel() {
  const box = h('div', {}, h('h2', { class: 'sec' }, 'Copies of maps and replays'), loader());
  let info = null, sizes = null;
  const save = async change => {
    try { info = await invoke('backup_set', { settings: { ...info.settings, ...change } }); draw(); } catch (e) { toast(String(e), true); }
  };
  const now = async () => {
    try {
      const made = await invoke('backup_now');
      if (!made) toast('Made when you close Rhythia.');
      else toast(`Copy made: ${plural(made.newFiles, 'new file')}, ${gb(made.newBytes + made.databaseBytes)}.`);
    } catch (e) { toast(String(e), true); }
    load();
  };
  async function load() {
    try { [info, sizes] = await Promise.all([invoke('backup_status'), invoke('game_sizes').catch(() => null)]); } catch (e) {
      fill(box, h('h2', { class: 'sec' }, 'Copies of maps and replays'), h('p', { class: 'err small' }, String(e)));
      return;
    }
    draw();
  }
  const set = (label, ...right) => h('div', { class: 'set' }, h('span', {}, label), h('span', { class: 'set-r' }, ...right));
  function draw() {
    const s = info.settings;
    const last = s.lastAt ? new Date(s.lastAt * 1000).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' }) : 'never';
    fill(box,
      h('h2', { class: 'sec' }, 'Copies of maps and replays'),
      sizes && h('div', { class: 'stats small' },
        stat(gb(sizes.maps + sizes.audio + sizes.covers), 'maps, songs, covers'),
        stat(gb(sizes.replays), `${plural(sizes.replayCount, 'replay')}`),
        stat(gb(sizes.database), 'game database'),
        stat(gb(sizes.copies), 'copies')),
      set('Last copy', h('span', { class: 'small' + (s.lastError ? ' err' : '') }, s.lastError ? `Failed: ${s.lastError}` : last),
        button('Back up now', now, 'mini'),
        s.pending && button('Don’t wait for Rhythia', async () => { await invoke('backup_cancel_pending'); load(); }, 'mini ghost')),
      s.pending && h('p', { class: 'dim small hint' }, 'Made when you close Rhythia.'),
      set('Every', select([['0', 'Only by hand'], ['1', 'Day'], ['3', '3 days'], ['7', '7 days'], ['14', '14 days'], ['30', '30 days']],
        String(s.everyDays), v => save({ everyDays: Number(v) }), 'Every')),
      set('Maps', select([['all', 'All'], ['ranked', 'Ranked only'], ['unranked', 'Not Ranked only'], ['none', 'None']], s.maps, v => save({ maps: v }), 'Maps')),
      set('Replays', select([['all', 'All'], ['passes', 'Passes only'], ['best', 'Best pass of each map'], ['none', 'None']], s.replays, v => save({ replays: v }), 'Replays')),
      set('Keep', select(['1', '2', '3', '5', '10'].map(n => [n, n === '1' ? '1 copy (replaced each time)' : plural(Number(n), 'copy', 'copies')]), String(s.keep), v => save({ keep: Number(v) }), 'Keep')),
      set('At most', select(['5', '10', '20', '50', '100', '500'].map(n => [n, `${n} GB`]), String(s.maxGb), v => save({ maxGb: Number(v) }), 'At most')),
      sizes && sizes.copies > s.maxGb * 1024 ** 3 && h('p', { class: 'err small' }, `Copies take ${gb(sizes.copies)}, over the ${s.maxGb} GB limit.`),
      set('With downloads and removals', seg([['on', 'On'], ['off', 'Off']], s.afterDownload ? 'on' : 'off', v => save({ afterDownload: v === 'on' }), 'With downloads and removals')),
      set('Offer a first copy', seg([['on', 'On'], ['off', 'Off']], s.nudge ? 'on' : 'off', v => save({ nudge: v === 'on' }), 'Offer a first copy')),
      set('Folder', h('span', { class: 'mono small path', title: info.dir }, info.dir),
        button('Change…', async () => { try { info = await invoke('backup_pick_folder'); draw(); } catch (e) { toast(String(e), true); } }, 'mini'),
        button('Open', () => invoke('open_backup_folder').catch(e => toast(String(e), true)), 'mini'),
        info.copies.length > 0 && button('Delete all', async () => {
          if (!await ask('Delete all copies?', 'Every copy and the map files kept for them. This cannot be undone.', 'Delete all', 'danger')) return;
          try { await invoke('backup_delete', { name: null }); toast('Copies deleted.'); load(); } catch (e) { toast(String(e), true); }
        }, 'mini danger')),
      info.copies.length > 0 && h('div', { class: 'copies' }, info.copies.map(c => set(
        new Date(c.at * 1000).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' }),
        h('span', { class: 'small dim' }, gb(c.bytes)),
        button('Export as ZIP', () => exportCopy(c), 'mini ghost'),
        button('Put back', () => restore(c), 'mini ghost'),
        button('Delete', async () => {
          if (!await ask('Delete this copy?', 'This cannot be undone.', 'Delete', 'danger')) return;
          try { await invoke('backup_delete', { name: c.name }); load(); } catch (e) { toast(String(e), true); }
        }, 'mini danger')))));
  }
  // One copy with its map files in a ZIP (parts of up to 1.5 GB), to keep elsewhere or move.
  async function exportCopy(c) {
    const dir = await invoke('download_pick_folder').catch(() => null);
    if (!dir) return;
    toast('Making the ZIP…');
    try {
      const r = await invoke('backup_export', { name: c.name, dir });
      toast(`Saved ${r.parts.map(p => p.split(/[\\/]/).pop()).join(', ')}.` + (r.missing ? ` ${plural(r.missing, 'file')} missing from the copy.` : ''));
    } catch (e) { toast(String(e), true); }
  }
  async function restore(c) {
    const ok = await ask('Put this copy back?', `Close Rhythia first. Your game goes back to ${new Date(c.at * 1000).toLocaleString()}.`, 'Put back');
    if (!ok) return;
    try {
      const r = await invoke('backup_restore', { name: c.name });
      lib = null;
      toast(`Copy put back.` + (r.files ? ` ${plural(r.files, 'file')} restored.` : '') + (r.missing ? ` ${num(r.missing)} not in the copy.` : ''));
    } catch (e) { toast(String(e), true); }
  }
  load();
  return box;
}

// Once: the game has over 1 GB of maps and replays and no copy yet (can be turned off).
// A newer Atlas from GitHub (updates.rs): offered in the corner; Update downloads it, checks its signature
// and installs it, then Atlas starts again.
function offerUpdate(found) {
  document.querySelector('.corner.update')?.remove();
  const note = h('div', { class: 'corner update', role: 'status' },
    h('b', {}, `Atlas ${found.version} is out`),
    found.notes && h('span', { class: 'small dim clamp3' }, found.notes),
    h('div', { class: 'btns' },
      button('Update', async () => {
        note.remove();
        toast('Downloading the update. Atlas will restart.');
        try { await invoke('update_install'); } catch (e) { toast(String(e), true); }
      }),
      button('Later', () => note.remove(), 'btn ghost')));
  document.body.append(note);
}
listen('update-found', e => offerUpdate(e.payload));

async function offerFirstCopy() {
  let info;
  try { info = await invoke('backup_status'); } catch { return; }
  const s = info.settings;
  if (!s.nudge || s.lastAt || s.pending) return;
  const sizes = await invoke('game_sizes').catch(() => null);
  if (!sizes) return;
  const total = sizes.maps + sizes.audio + sizes.covers + sizes.replays;
  if (total < 1024 ** 3) return;
  const note = h('div', { class: 'corner', role: 'status' },
    h('b', {}, 'No copy of your maps yet'),
    h('span', { class: 'small dim' }, `Maps and replays: ${gb(total)}.`),
    h('div', { class: 'btns' },
      button('Back up', async () => {
        note.remove();
        try {
          const made = await invoke('backup_now');
          toast(made ? 'Copy made.' : 'Made when you close Rhythia.');
        } catch (e) { toast(String(e), true); }
      }),
      button('Not now', async () => {
        note.remove();
        await invoke('backup_set', { settings: { ...s, nudge: false } }).catch(() => {});
        toast('Copies: Settings → Data.');
      }, 'btn ghost')));
  document.body.append(note);
}
listen('backup-done', e => {
  const { made, error } = e.payload;
  if (error) toast(`Copy failed: ${error}`, true);
  else if (made) toast(`Copy of your maps made (${gb(made.newBytes + made.databaseBytes)}).`);
});

// ---------- Terms ----------

// Shown after the version number while Atlas is an early release.
const STAGE = 'alpha';

// TERMS_TEXT start (made by npm run terms from TERMS.md and PRIVACY.md; do not edit here)
const TERMS_VERSION = "November 22, 2026";
const TERMS_TEXT = [ [ "h1", "Terms of use" ], [ "p", "Atlas is a free app by Shuriel for managing your Rhythia maps, scores and collections. It is not made by the Rhythia team. Contact: contact@shuriel.com." ], [ "h2", "Using Atlas" ], [ "p", "Your account follows Rhythia's rules. Atlas never touches the game program or your login file. It changes the game's files only when you ask, with the game closed." ], [ "p", "What you download, delete or change with Atlas is up to you. Keep your own backups." ], [ "h2", "Limits" ], [ "p", "Atlas downloads at most 3000 maps a day and sends at most 10 requests a second to Rhythia. Song previews of maps you don't have are limited to 300 a day." ], [ "p", "Atlas is open source (GPL-3.0). If you run a changed copy, for example one without these limits, it is at your own risk and you get no help; Rhythia or Shuriel's server can block your account or address. Shuriel's server and the Atlas name and logo are only for the official Atlas." ], [ "h2", "Signing in" ], [ "p", "Signing in is optional. Atlas never asks for or keeps your password." ], [ "h2", "No warranty" ], [ "p", "Atlas is an alpha: an early version with rough edges. It comes as it is. It can have bugs and wrong data, and features can change or stop. Shuriel is not liable for any damage or loss from using Atlas, such as lost data or actions Rhythia takes on your account, as far as the law allows." ], [ "h1", "Privacy" ], [ "p", "Atlas has no ads, no tracking and no accounts of its own. Nothing about you is sold or shared. Questions: contact@shuriel.com." ], [ "h2", "On your computer" ], [ "p", "Your library, collections, copies and settings stay on your computer. If you sign in to Rhythia, the sign-in is kept in the Windows password store." ], [ "h2", "What goes out" ], [ "ul", [ "**Rhythia:** maps, collections and your sign-in, as on the website.", "**Shuriel's server:** once a day, a random key and the version of the map types list. For Played near your rank, Near your rank and Your types, your public Rhythia player number; it answers with totals of players ranked near you, never their names. For At which speed and For you, the same number and the Rhythia number, stars and notes of the maps on screen; it answers from your public Rhythia top scores. It sees your IP address to keep its limits and block misuse; its logs keep only part of it.", "**GitHub:** once an hour, whether a new version is out." ] ], [ "p", "Reimagined and Atlas talk only on your computer." ], [ "h2", "Removing it" ], [ "p", "In Settings → Data, press Uninstall Atlas and leave **Also delete my Atlas data** ticked: Atlas, its data and your Rhythia sign-in are removed. From Windows Settings → Apps, tick **Delete the application data** for the same." ] ];
// TERMS_TEXT end

// Terms and privacy: the text of TERMS.md and PRIVACY.md (tools/terms.mjs). **bold** shows as bold.
function termsText() {
  const rich = t => t.split(/\*\*(.+?)\*\*/).map((part, i) => (i % 2 ? h('b', {}, part) : part));
  return h('div', { class: 'terms' },
    TERMS_TEXT.map(([tag, body]) => tag === 'ul' ? h('ul', {}, body.map(li => h('li', {}, rich(li))))
      : tag === 'h1' ? h('h2', { class: 'terms-part' }, body)
      : h(tag, {}, tag === 'p' ? rich(body) : body)));
}

function viewTerms(root) {
  root.append(head('Terms', `Version of ${TERMS_VERSION}`), termsText());
}

// At the first start, and when the Terms change: accept them or close Atlas.
async function termsGate() {
  if (await invoke('terms_accepted').catch(() => null) === TERMS_VERSION) return;
  const gate = h('dialog', { class: 'gate', oncancel: e => e.preventDefault() },
    h('h2', {}, 'Welcome to Atlas'),
    h('p', { class: 'dim' }, 'Please read and accept these first.'),
    h('div', { class: 'gate-text' }, termsText()),
    h('div', { class: 'btns' },
      button('Accept', async () => { await invoke('terms_accept', { version: TERMS_VERSION }).catch(() => {}); gate.close(); gate.remove(); }),
      button('Close Atlas', () => win.close(), 'btn ghost')));
  document.body.append(gate);
  gate.showModal();
  gate.querySelector('.gate-text').scrollTop = 0;
}

// ---------- Reimagined in the browser ----------

// The rail's Rhythia and Reimagined status open Settings on the Account tab, where both are.
for (const id of ['rhythia', 'bridge']) {
  document.getElementById(id).addEventListener('click', () => { settingsTab = 'account'; if (location.hash === '#settings') show('settings'); });
}

// One reading of the link with Reimagined, for the rail and Settings → Account, so both always say the same.
// key: start | port | on | asking | waiting | none.
function bridgeState(b = bridge) {
  if (!b) return { key: 'start', short: 'Starting', title: 'Starting…' };
  const list = b.paired || [];
  const on = list.find(p => p.connected);
  if (!b.port) return { key: 'port', short: 'Error', title: 'Atlas can’t accept connections', tone: 'bad' };
  if (on) return { key: 'on', short: 'Connected', title: `Connected to ${on.browser}`, tone: 'ok', browser: on.browser };
  if (b.asking) return { key: 'asking', short: 'Pairing', title: `${b.asking.browser} wants to connect` };
  if (list.length) return { key: 'waiting', short: 'Not connected', title: 'Not connected right now', browsers: list.map(p => p.browser) };
  return { key: 'none', short: 'Not set up', title: 'Not set up' };
}

function drawBridge(status) {
  bridge = status;
  const st = bridgeState();
  const el = document.getElementById('bridge');
  el.classList.toggle('on', st.tone === 'ok');
  el.classList.toggle('bad', st.tone === 'bad');
  el.title = `Reimagined for Rhythia: ${st.title}`;
  document.getElementById('bridge-text').textContent = st.short;
  drawAccountRail();
  onBridge?.();
}

// The rail's Rhythia status: the same reading as the top of Settings → Account.
function drawAccountRail() {
  const a = account;
  const el = document.getElementById('rhythia');
  const ended = a?.signedIn && (a.problem || rhythiaState().ended);
  const [short, title, tone] = !a ? ['…', 'Checking…', '']
    : !a.signedIn ? ['Not signed in', 'Not signed in', '']
    : ended ? ['Renew sign-in', 'Sign-in needs renewing', 'bad']
    : ['Signed in', a.player ? `Signed in as ${a.player.name}` : 'Signed in', 'ok'];
  el.classList.toggle('on', tone === 'ok');
  el.classList.toggle('bad', tone === 'bad');
  el.title = `Rhythia: ${title}`;
  document.getElementById('rhythia-text').textContent = short;
}

let pairing = false;
async function askPairing(req) {
  if (!req || pairing) return;
  pairing = true;
  const yes = await ask(`Connect Reimagined in ${req.browser}?`,
    `Allow only if you just clicked Connect in Reimagined and it shows the code ${req.code}.`, 'Connect');
  pairing = false;
  await invoke('bridge_answer', { yes });
}

// A state at the top of a Settings section: a colour bar and a short title, a line saying what it means, buttons.
const stateBox = (title, tone, text, ...btns) => h('div', { class: 'state' + (tone ? ' ' + tone : '') },
  h('div', { class: 'state-top' }, h('i'), h('b', {}, title)),
  text && h('p', { class: 'dim small' }, text),
  btns.flat().filter(Boolean).length > 0 && h('div', { class: 'btns' }, btns));
const sleep = ms => new Promise(r => setTimeout(r, ms));

// Settings → Rhythia: the player's Rhythia sign-in (account.rs). Atlas never sees a password; the token
// stays in the Windows password store and goes only to Rhythia.
let account = null; // { player, signedIn, expired, via, reimaginedWaiting, problem, checked }
let onAccount = null;
let signingIn = false; // the Rhythia window is open for a sign-in
let signInStuck = false; // that window has shown no page for a while (account.rs, account-window-stuck)
const loadAccount = () => invoke('account_status').then(a => { account = a; drawAccountRail(); onAccount?.(); }).catch(() => {});
listen('account-changed', () => { loadAccount(); });
// A sign-in can end on its own (Rhythia's lasts about an hour): read again now and then (local, no network).
setInterval(loadAccount, 60_000);
listen('account-window-closed', e => { signingIn = signInStuck = false; if (e.payload) toast(String(e.payload), true); onAccount?.(); });
listen('account-window-stuck', () => { signInStuck = true; onAccount?.(); });
listen('reimagined-no-session', () => toast('Open rhythia.com in your browser and sign in there, then try again.', true));
loadAccount();
// Rhythia's own website in a window of Atlas (account.rs); it closes by itself once signed in.
function signInWindow() {
  signingIn = true;
  signInStuck = false;
  onAccount?.();
  invoke('rhythia_sign_in');
}
// The sign-in Reimagined has in the browser (a rhythia.com tab where the player is signed in).
async function signInReimagined() {
  try { account = await invoke('rhythia_sign_in_reimagined'); } catch (e) { toast(String(e), true); return; }
  onAccount?.();
  toast('Asking Reimagined…');
  setTimeout(loadAccount, 121_000);
}

function accountPanel() {
  const box = h('div');
  let checking = false;
  let note = null; // the last check: { text, bad }
  async function check() {
    checking = true;
    note = null;
    draw();
    try {
      account = await invoke('account_check');
      note = { text: 'Rhythia confirmed it just now.' };
    } catch (e) {
      note = { text: String(e), bad: true };
      await loadAccount();
    }
    checking = false;
    draw();
  }
  const checkButton = label => {
    const b = button(checking ? 'Checking…' : label, check, 'btn mini');
    b.disabled = checking;
    return b;
  };
  const signOut = button('Sign out', async () => {
    await invoke('rhythia_sign_out');
    note = null;
    await loadAccount();
    toast('Signed out of Rhythia in Atlas.');
  }, 'btn ghost mini');
  function draw() {
    const a = account || {};
    const reimagined = bridgeState().key === 'on';
    const viaReimagined = button('Sign in with Reimagined', signInReimagined, 'btn ghost mini');
    if (!reimagined) {
      viaReimagined.disabled = true;
      viaReimagined.title = 'Reimagined is not connected right now (see below).';
    }
    if (a.signedIn) signingIn = false;
    const ended = a.signedIn && (a.problem || rhythiaState().ended);
    const state = !account ? stateBox('Checking…', '', '')
      : signingIn && signInStuck ? stateBox('The Rhythia window stays empty', 'bad', 'Rhythia\'s page did not load in it. Check that rhythia.com opens in your browser, then close the window and try again. You can also sign in with Reimagined.',
        button('Show the window', signInWindow, 'btn mini'), viaReimagined)
      : signingIn ? stateBox('Waiting for you to sign in', '', 'Sign in in the Rhythia window, as on rhythia.com. It closes by itself once you are in.',
        button('Show the window', signInWindow, 'btn mini'))
      : a.reimaginedWaiting && !a.signedIn ? stateBox('Waiting for Reimagined', '', 'Allow it in your browser. Reimagined takes the sign-in of a rhythia.com tab where you are signed in.',
        button('Sign in to Rhythia instead', signInWindow, 'btn ghost mini'))
      : !a.signedIn ? stateBox('Not signed in', '', 'Sign in to save collections to Rhythia and change your own collections there. Atlas never sees your password.',
        button('Sign in to Rhythia', signInWindow, 'btn mini'), viaReimagined)
      : ended ? stateBox('Sign-in needs renewing', 'bad', (a.problem || rhythiaState().text).replace(' (Settings → Account)', ''),
        checkButton('Try again'), button('Sign in again', signInWindow, 'btn ghost mini'), a.via === 'reimagined' && reimagined && viaReimagined, signOut)
      : stateBox(a.player ? `Signed in as ${a.player.name}` : 'Signed in', 'ok',
        (a.via === 'reimagined'
          ? 'Signed in with Reimagined. Atlas renews the sign-in through it, so it needs your browser connected now and then.'
          : 'Signed in with the Rhythia window. Atlas renews the sign-in by itself.')
          + (a.checked ? ` Last confirmed by Rhythia at ${ago(a.checked)}.` : ''),
        checkButton('Check now'), signOut);
    if (note && !checking) state.append(h('p', { class: (note.bad ? 'err' : 'dim') + ' small' }, note.text));
    fill(box, h('h2', { class: 'sec' }, 'Rhythia account'), state);
  }
  draw();
  onAccount = draw;
  loadAccount();
  return box;
}

function bridgePanel() {
  const box = h('div');
  let checking = false;
  let note = null; // after Refresh connection: { text, bad }
  // Atlas can only check its side and wait: Reimagined connects by itself (every 20 seconds while the browser
  // is awake, and whenever a rhythia.com tab opens), and nothing here can open or wake the browser.
  async function refresh() {
    checking = true;
    note = null;
    draw();
    const s = await invoke('bridge_check').catch(() => null);
    if (s) drawBridge(s);
    for (let i = 0; i < 25 && bridgeState().key === 'waiting'; i++) await sleep(1000);
    checking = false;
    const key = bridgeState().key;
    note = key === 'on' ? { text: 'Connected.' }
      : key === 'waiting' ? { text: 'Your browser did not connect. Open it with a rhythia.com tab, then wait a moment or refresh again.', bad: true }
      : key === 'port' ? { text: 'The ports are still taken by another program. Close it or restart the computer.', bad: true }
      : null;
    draw();
  }
  function draw() {
    const list = bridge?.paired || [];
    const log = bridge?.log || [];
    const st = bridgeState();
    const refreshButton = button(checking ? 'Checking… up to 25 s' : 'Refresh connection', refresh, 'btn mini');
    refreshButton.disabled = checking;
    const text = {
      start: '',
      port: 'Another program uses the ports Atlas listens on (47231 to 47233). Refresh connection tries again.',
      on: 'Install in Atlas, your scores from the game and Atlas collections work on rhythia.com now.',
      asking: 'Compare the code with the one in Reimagined and answer in Atlas.',
      waiting: `Atlas is paired with Reimagined in ${(st.browsers || []).join(' and ')}, but it is not connected now. Reimagined connects by itself while the browser is open. Atlas can’t open the browser for you.`,
      none: 'Pair once: in the Reimagined popup, Features → Extras → Atlas → Connect, then allow it here.',
    }[st.key];
    const state = stateBox(st.title, st.tone, text, st.key !== 'none' && st.key !== 'start' && refreshButton);
    if (note && !checking) state.append(h('p', { class: (note.bad ? 'err' : 'dim') + ' small' }, note.text));
    fill(box,
      h('h2', { class: 'sec' }, 'Reimagined in your browser'),
      state,
      ...list.map(p => h('div', { class: 'set' },
        h('span', {}, p.browser),
        h('span', { class: 'set-r' },
          h('span', { class: 'dim small' }, [p.version && `Reimagined ${p.version}`, p.connected ? 'connected now' : `last connected ${ago(p.lastSeen)}`].filter(Boolean).join(' · ')),
          h('button', { class: 'mini', type: 'button', title: 'Forget this browser. To use it again, press Connect in Reimagined.', onclick: () => invoke('bridge_forget', { id: p.id }) }, 'Remove')))),
      h('p', { class: 'dim small hint' }, 'For the Reimagined features, keep your browser open with the Reimagined extension (1.6.0 or newer) and a rhythia.com tab.'),
      log.length > 0 && h('details', { class: 'log' }, h('summary', { class: 'small dim' }, 'What happened lately'),
        h('ul', { class: 'small dim' }, log.map(l => h('li', {}, h('span', { class: 'mono' }, ago(l.at)), ' ', l.text)))));
  }
  draw();
  // The account above offers Sign in with Reimagined only while it is connected.
  onBridge = () => { draw(); onAccount?.(); };
  return box;
}

// ---------- links from Reimagined ----------

async function handleLink(link) {
  if (!link) return;
  if (link.kind === 'collection') {
    openCollection = { source: 'rhythia', id: link.id };
    if (location.hash === '#collections') show('collections'); else { openFromLink = true; location.hash = '#collections'; }
    return;
  }
  const ok = await ask(`Install map ${link.id}?`, 'A link asked Atlas to add this map to your game. Say yes only if you clicked it.', 'Install');
  if (ok) await install([link.id], 'map', false, { to: 'game' });
}

// ---------- Your game ----------
// From the game's own database, read only: what you played over time and maps worth another go. Near your
// rank uses the same Rhythia data as the Catalog (near.rs). Nothing here leaves Atlas.

const WHY = {
  fc: 'Close to full combo', low: 'Low RP for its stars', near: 'Behind players near you',
  slow: 'Played slower', faster: 'Could go faster', old: 'Old best', unpassed: 'Never passed',
};
const DAY_MS = 864e5;
const utcDay = d => new Date(d + 'T00:00:00Z').getTime();
const isoDay = t => new Date(t).toISOString().slice(0, 10);
// Monday of the week the day is in (UTC, as the game writes its dates).
const weekOf = d => { const t = utcDay(d); return isoDay(t - ((new Date(t).getUTCDay() + 6) % 7) * DAY_MS); };
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const monthName = d => `${MONTHS[Number(d.slice(5, 7)) - 1]} ${d.slice(2, 4)}`;
function svg(tag, attrs = {}, ...kids) {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  el.append(...kids);
  return el;
}

// Tries and passes a week, from the first week with a try (at least 8 weeks, at most a year).
function weekChart(days) {
  const now = weekOf(isoDay(Date.now()));
  const first = days.length ? weekOf(days[0].day) : now;
  const weeks = Math.min(52, Math.max(8, Math.round((utcDay(now) - utcDay(first)) / (7 * DAY_MS)) + 1));
  const keys = Array.from({ length: weeks }, (_, i) => isoDay(utcDay(now) - (weeks - 1 - i) * 7 * DAY_MS));
  const by = new Map(keys.map(k => [k, { tries: 0, passes: 0 }]));
  for (const d of days) { const w = by.get(weekOf(d.day)); if (w) { w.tries += d.tries; w.passes += d.passes; } }
  const top = Math.max(1, ...[...by.values()].map(w => w.tries));
  const W = 12, H = 90;
  const chart = svg('svg', { class: 'chart', viewBox: `0 0 ${weeks * W} ${H}`, preserveAspectRatio: 'none', role: 'img', 'aria-label': 'Tries and passes a week' });
  keys.forEach((k, i) => {
    const w = by.get(k);
    const th = (w.tries / top) * H, ph = (w.passes / top) * H;
    const g = svg('g', {}, svg('title', {}, `Week of ${k}: ${plural(w.tries, 'try', 'tries')}, ${plural(w.passes, 'pass', 'passes')}`));
    g.append(svg('rect', { x: i * W + 1.5, y: H - th, width: W - 3, height: Math.max(th, w.tries ? 1 : 0), class: 'c-try' }),
      svg('rect', { x: i * W + 1.5, y: H - ph, width: W - 3, height: Math.max(ph, w.passes ? 1 : 0), class: 'c-pass' }));
    chart.append(g);
  });
  // Month names under the first week of each month (HTML: the chart stretches, text should not).
  const labels = h('div', { class: 'chart-labels' }, keys.map((k, i) => (i === 0 || k.slice(5, 7) !== keys[i - 1].slice(5, 7))
    && h('span', { style: `left:${(100 * i) / weeks}%` }, MONTHS[Number(k.slice(5, 7)) - 1])));
  return h('div', { class: 'chart-box' }, chart, labels,
    h('div', { class: 'legend' }, h('span', { class: 'l-try' }, 'Tries'), h('span', { class: 'l-pass' }, 'Passes')));
}

// What types the maps you passed each month were (each map once a month).
function typeMonths(days, months = 12) {
  const by = new Map();
  for (const d of days) {
    if (!d.passes || !d.map) continue;
    const key = d.day.slice(0, 7);
    if (!by.has(key)) by.set(key, new Set());
    by.get(key).add(d.map);
  }
  const keys = [...by.keys()].sort().slice(-months);
  if (!keys.length) return empty('Nothing passed yet', '');
  const order = ['jump', 'stream', 'tech', 'varied'];
  const rows = keys.map(key => {
    const count = { jump: 0, stream: 0, tech: 0, varied: 0 };
    let known = 0;
    for (const id of by.get(key)) {
      const k = kindOf(lib?.byOnline.get(id) || {});
      if (k?.kind in count) { count[k.kind]++; known++; }
    }
    return h('div', { class: 'type-row' },
      h('span', { class: 'dim' }, monthName(key + '-01')),
      h('div', { class: 'type-bar', title: order.map(x => `${KIND_NAME[x]} ${count[x]}`).join(' · ') },
        known ? order.filter(x => count[x]).map(x => h('i', { class: 'k-' + x, style: `flex:${count[x]}` })) : h('i', { class: 'k-none', style: 'flex:1' })),
      h('span', { class: 'dim small' }, known ? plural(by.get(key).size, 'map') : 'types unknown'));
  });
  return h('div', {}, rows, h('div', { class: 'legend' }, order.map(x => h('span', { class: 'l-' + x }, KIND_NAME[x]))));
}

// Your types against the players near your rank: each type's share of your passed Ranked maps and of the maps in
// their tops (each map counted once per player), and how many of the Ranked maps of the type you tried you passed.
function typeCompare(days, group) {
  const order = ['jump', 'stream', 'tech', 'varied'];
  const tried = {}, passed = {};
  for (const d of days) {
    const m = d.map && lib?.byOnline.get(d.map);
    const k = m && isRanked(m) && kindOf(m)?.kind;
    if (!(k in KIND_NAME)) continue;
    (tried[k] ||= new Set()).add(d.map);
    if (d.passes) (passed[k] ||= new Set()).add(d.map);
  }
  const mine = Object.fromEntries(order.map(k => [k, passed[k]?.size || 0]));
  const myAll = order.reduce((n, k) => n + mine[k], 0);
  if (!myAll) return empty('Nothing passed yet', '');
  const theirs = Object.fromEntries(order.map(k => [k, 0]));
  for (const p of group?.plays || []) {
    const k = p.map && kindOf(p.map)?.kind;
    if (k in theirs) theirs[k] += p.all.players;
  }
  const theirAll = order.reduce((n, k) => n + theirs[k], 0);
  const share = (x, all) => (all ? x / all : null);
  const pct = x => (x == null ? none() : `${Math.round(100 * x)}%`);
  const rows = order.map(k => {
    const a = share(mine[k], myAll), b = share(theirs[k], theirAll);
    return { k, a, b, ratio: a != null && b ? a / b : null, rate: tried[k]?.size ? mine[k] / tried[k].size : null, tried: tried[k]?.size || 0 };
  });
  // The biggest differences, in a line.
  const more = rows.filter(r => r.ratio >= 1.3).sort((x, y) => y.ratio - x.ratio)[0];
  const less = rows.filter(r => r.ratio != null && r.ratio <= 1 / 1.3).sort((x, y) => x.ratio - y.ratio)[0];
  const bestRate = rows.filter(r => r.tried >= 5).sort((x, y) => y.rate - x.rate)[0];
  const says = [
    more && `${KIND_NAME[more.k]} ${more.ratio.toFixed(1)}× more than players near your rank`,
    less && `${KIND_NAME[less.k]} ${less.ratio.toFixed(1)}×`,
    bestRate && `you pass ${KIND_NAME[bestRate.k]} most often (${Math.round(100 * bestRate.rate)}%)`,
  ].filter(Boolean);
  return h('div', {},
    says.length ? h('p', { class: 'dim' }, says.join(' · ').replace(/^./, c => c.toUpperCase()) + '.') : null,
    h('div', { class: 'tbl' }, h('table', {},
      h('thead', {}, h('tr', {}, ['Type', 'Your passes', 'Near your rank', 'You vs them', 'Passed'].map((x, i) => h('th', { class: i ? 'n' : '' }, x)))),
      h('tbody', {}, rows.map(r => h('tr', {},
        h('td', {}, h('span', { class: 'l-' + r.k + ' key' }, KIND_NAME[r.k])),
        h('td', { class: 'n' }, pct(r.a)),
        h('td', { class: 'n' }, theirAll ? pct(r.b) : none()),
        h('td', { class: 'n' }, r.ratio != null ? `${r.ratio.toFixed(1)}×` : none()),
        h('td', { class: 'n', title: r.tried ? `${num(mine[r.k])} of ${plural(r.tried, 'map')}` : null }, r.rate != null ? pct(r.rate) : none())))))));
}

// Your counted plays on Rhythia (near.rs my_top), best RP first; null while unknown.
let myTop = null;
const loadMyTop = () => invoke('my_top').then(t => { myTop = t; }).catch(() => {});
// Rhythia's total RP: your 99 best plays, the n-th counts RP × 0.97^(n−1) (to the cent on players' profiles).
const COUNTED_PLAYS = 99;
const weighted = rps => rps.slice(0, COUNTED_PLAYS).reduce((s, p, i) => s + p * 0.97 ** i, 0);
// What a play of `rp` on the map would add to your total RP: it replaces your best there if higher.
function totalGain(m, rp) {
  if (!myTop || !(rp > 0)) return null;
  const old = myTop.find(t => (m.hash && t.hash === m.hash) || (m.legacyId && t.song === m.legacyId));
  if (old && old.rp >= rp) return 0;
  const rps = myTop.map(t => t.rp);
  const after = myTop.filter(t => t !== old).map(t => t.rp).concat(rp).sort((a, b) => b - a);
  return weighted(after) - weighted(rps);
}

// Maps worth another go, each with why.
function toImprove(bests) {
  const byId = new Map(bests.map(b => [b.id, b]));
  // What RP you usually get at a star level (half stars, your ranked passes; at least 4 of them).
  const levels = new Map();
  for (const b of bests) {
    const m = lib.byOnline.get(b.id);
    if (!m || !isRanked(m) || b.rp == null || m.stars == null) continue;
    const key = Math.round(m.stars * 2) / 2;
    if (!levels.has(key)) levels.set(key, []);
    levels.get(key).push(b.rp);
  }
  const usual = new Map([...levels].filter(([, l]) => l.length >= 4).map(([k, l]) => [k, median(l)]));
  const now = Date.now();
  const out = [];
  for (const m of lib.maps) {
    if (!m.plays || !m.onlineId) continue;
    const b = byId.get(m.onlineId);
    const check = speedCheck(m);
    const why = [];
    if (!b) { if (m.plays >= 3) why.push('unpassed'); }
    else {
      if (b.misses > 0 && b.misses <= 5) why.push('fc');
      const level = m.stars != null && usual.get(Math.round(m.stars * 2) / 2);
      if (isRanked(m) && level && b.rp != null && b.rp < level * 0.8) why.push('low');
      const play = nearGroup && m.legacyId && nearGroup.plays.find(p => p.song === m.legacyId);
      if (isRanked(m) && play && play.all.players >= 3 && b.rp != null && b.rp < play.all.rp) why.push('near');
      if (b.speed != null && b.speed < 1) why.push('slow');
      const bet = check?.bet;
      if (bet?.rp && b.rp != null && b.speed != null && bet.speed > b.speed + 0.01 && bet.rp[1] > b.rp * 1.1) why.push('faster');
      const at = b.at && new Date(b.at.replace(' ', 'T') + 'Z').getTime();
      if (isRanked(m) && at && now - at > 180 * DAY_MS) why.push('old');
    }
    // What the best bet's RP would add to your total RP.
    const gain = totalGain(m, check?.bet?.rp?.[1]);
    if (why.length) out.push({ ...m, why, check, gain: gain > 0 ? gain : null, bestMisses: b?.misses ?? null, bestSpeed: b?.speed ?? null });
  }
  return out.sort((a, b) => b.why.length - a.why.length || (b.stars ?? 0) - (a.stars ?? 0));
}

async function viewGame(root) {
  root.append(head('Your game'));
  const body = h('div', {}, loader());
  root.append(body);
  let pic;
  try { await getLibrary(true); pic = await invoke('game_picture'); } catch (e) { fill(body, empty('Could not read your game', String(e))); return; }
  loadNearGroup().then(g => { drawImprove(); drawCompare(); loadKinds(g.plays.map(p => p.map).filter(Boolean), drawCompare); }).catch(() => {});
  loadForYou(lib.maps.filter(m => m.plays && m.onlineId)).then(() => drawImprove());
  loadMyTop().then(() => drawImprove());

  const today = isoDay(Date.now());
  const within = n => pic.days.filter(d => utcDay(d.day) > utcDay(today) - n * DAY_MS);
  const sum = (list, key) => list.reduce((s, d) => s + d[key], 0);
  const firstPass = new Map();
  for (const d of pic.days) if (d.passes && d.map && !firstPass.has(d.map)) firstPass.set(d.map, d.day);
  const week = within(7), month = within(30);
  const newMaps = [...firstPass.values()].filter(d => utcDay(d) > utcDay(today) - 30 * DAY_MS).length;

  const s = pic.session;
  const sessionBox = s && h('div', { class: 'part' },
    h('h2', {}, 'Playing now'),
    h('p', {}, `Rhythia is open since ${new Date(s.since * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}: `,
      `${plural(s.tries, 'try', 'tries')}, ${plural(s.passes, 'pass', 'passes')}.`),
    s.last && h('p', { class: 'dim' }, 'Last: ', h('b', {}, s.last.title || 'a map'),
      s.last.passed ? ` · passed${s.last.accuracy != null ? ` · ${s.last.accuracy.toFixed(2)}%` : ''}${s.last.rp != null ? ` · ${num(Math.round(s.last.rp))} RP` : ''}` : ' · failed'));

  const improve = toImprove(pic.bests);
  const f = { why: 'all' };
  const improveBox = h('div');
  const cols = [
    { label: 'Status', sort: 'status', cell: m => statusChip(m.status) },
    { label: 'Stars', sort: 'stars', cls: 'n', cell: m => stars(m.stars) },
    { label: 'Best RP', sort: 'bestRp', cls: 'n', cell: m => rp(m.bestRp) },
    { label: 'Accuracy', sort: 'bestAccuracy', cls: 'n', cell: m => acc(m.bestAccuracy) },
    { label: 'Misses', sort: 'bestMisses', cls: 'n', cell: m => (m.bestMisses == null ? none() : num(m.bestMisses)) },
    { label: 'Try at', cls: 'n', cell: m => betText(m.check) },
    { label: 'Gain', sort: 'gain', cls: 'n', cell: m => (m.gain == null ? none() : `+${m.gain < 10 ? m.gain.toFixed(1) : num(Math.round(m.gain))}`) },
    { label: 'Why', sort: 'why', cls: 'why', cell: m => m.why.map(w => WHY[w]).join(' · ') },
  ];
  const GAME_SORTS = [['why', 'Most reasons'], ['gain', 'RP gain'], ['stars', 'Stars'], ['bestRp', 'Best RP'], ['bestAccuracy', 'Accuracy'],
    ['bestMisses', 'Misses'], ['lastPlayed', 'Last played'], ['title', 'Title']];
  if (!GAME_SORTS.some(([k]) => k === prefs.sort.game.key)) prefs.sort.game = { ...SORT_OF.game };
  const setSort = s => { prefs.sort.game = s; savePrefs(); drawImprove(); };
  function drawImprove() {
    const list = toImprove(pic.bests);
    const counts = Object.fromEntries(Object.keys(WHY).map(k => [k, list.filter(m => m.why.includes(k)).length]));
    const sort = prefs.sort.game;
    const shown = sortMaps(f.why === 'all' ? list : list.filter(m => m.why.includes(f.why)), sort);
    fill(improveBox,
      h('div', { class: 'bar' },
        select([['all', `Any reason (${num(list.length)})`], ...Object.entries(WHY).filter(([k]) => counts[k]).map(([k, t]) => [k, `${t} (${num(counts[k])})`])], f.why, v => { f.why = v; drawImprove(); }, 'Why'),
        tools(sortControls(GAME_SORTS, sort, setSort), perPagePick('perPage', drawImprove))),
      shown.length ? mapList({ mode: 'compact', maps: shown, cols, onOpen: openMap, perPage: prefs.perPage, page: 'game',
        sort, titleSort: 'title', onSort: key => setSort(nextSort(sort, key, SORT_OF.game)) }).el
        : empty('Nothing here', 'No maps for this reason.'));
  }
  const typesBox = h('div');
  const drawTypes = () => fill(typesBox, typeMonths(pic.days));
  const compareBox = h('div');
  const drawCompare = () => fill(compareBox, typeCompare(pic.days, nearGroup));

  fill(body,
    h('div', { class: 'stats' },
      stat(num(sum(week, 'tries')), 'tries this week'),
      stat(num(sum(week, 'passes')), 'passes this week'),
      stat(num(new Set(month.map(d => d.map)).size), 'maps played in 30 days'),
      stat(num(newMaps), 'new maps passed in 30 days'),
      stat(num(pic.bests.length), 'maps passed')),
    sessionBox,
    h('div', { class: 'part' }, h('h2', {}, 'Every week'), weekChart(pic.days)),
    h('div', { class: 'game-grid' },
      h('div', { class: 'part' }, h('h2', {}, 'Types you pass'), typesBox),
      h('div', { class: 'part' }, h('h2', {}, 'Your types'), compareBox)),
    h('div', { class: 'part' }, h('h2', {}, 'Worth another go'),
      improveBox));
  drawTypes();
  drawCompare();
  drawImprove();
  loadKinds(lib.maps, () => { drawTypes(); drawCompare(); });
  onGame = () => show('game');
}

// ---------- router ----------

const VIEWS = { library: viewLibrary, game: viewGame, catalog: viewCatalog, collections: viewCollections, downloads: viewDownloads, settings: viewSettings, terms: viewTerms };

async function show(name) {
  if (!VIEWS[name]) name = 'library';
  for (const a of document.querySelectorAll('.menu a')) {
    if (a.dataset.view === name) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  }
  const root = h('div', { class: 'view-in' });
  fill(view, root);
  view.scrollTop = 0;
  onJob = null;
  onBridge = null;
  onGame = null;
  try {
    const r = await VIEWS[name](root);
    if (name === 'downloads') onJob = r;
  } catch (e) {
    root.append(empty('Something went wrong', String(e)));
  }
}

let openFromLink = false; // the hash changes because a link opened a collection: keep it open
window.addEventListener('hashchange', () => {
  if (location.hash === '#collections' && !openFromLink) openCollection = null;
  openFromLink = false;
  show(location.hash.slice(1));
});

// Window buttons in our own bar (index.html .top). Dragging the bar and double-clicking it are Tauri's
// data-tauri-drag-region.
const win = window.__TAURI__.window.getCurrentWindow();
document.getElementById('win-min').addEventListener('click', () => win.minimize());
document.getElementById('win-max').addEventListener('click', () => win.toggleMaximize());
document.getElementById('win-close').addEventListener('click', () => win.close());
const syncMaximized = async () => {
  const maxed = await win.isMaximized();
  document.body.classList.toggle('maxed', maxed);
  const label = maxed ? 'Restore' : 'Maximize';
  const btn = document.getElementById('win-max');
  btn.setAttribute('aria-label', label);
  btn.title = label;
};
win.onResized(syncMaximized);
syncMaximized();

// A download to the computer, in a corner (not on Downloads, which shows it all): how far, then done.
let dlCorner = null;
function downloadCorner(j) {
  if (location.hash === '#downloads') { dlCorner?.remove(); dlCorner = null; return; }
  const total = j.items.length;
  const finished = j.items.filter(i => i.state === 'done' || i.state === 'failed' || i.state === 'cancelled').length;
  if (!dlCorner) {
    dlCorner = h('div', { class: 'corner dl', role: 'status' });
    dlCorner.bar = loader();
    document.body.append(dlCorner);
  }
  const close = () => { dlCorner?.remove(); dlCorner = null; };
  if (!j.done) {
    dlCorner.bar.set(finished, total);
    fill(dlCorner, h('b', {}, `Downloading ${num(finished)} of ${num(total)}`), dlCorner.bar,
      h('div', { class: 'btns' }, button('Cancel', () => invoke('cancel_install'), 'btn ghost')));
    return;
  }
  const done = j.items.filter(i => i.state === 'done').length;
  const failed = j.items.filter(i => i.state === 'failed').length;
  dlCorner.bar.set(1, 1);
  fill(dlCorner, h('b', {}, j.error ? 'Download stopped' : `Downloaded ${plural(done, 'map')}`),
    (failed > 0 || j.error) && h('span', { class: 'small dim' }, j.error || `${num(failed)} failed`),
    dlCorner.bar,
    h('div', { class: 'btns' },
      j.folder && button('Open folder', () => invoke('open_folder', { path: j.folder }).catch(e => toast(String(e), true))),
      button('Close', close, 'btn ghost')));
  const mine = dlCorner;
  setTimeout(() => { if (dlCorner === mine && !mine.matches(':hover')) close(); }, 15000);
}
listen('install-progress', e => {
  job = e.payload;
  onJob?.();
  if (job.to) downloadCorner(job);
  if (job.done) {
    refreshQuota();
    lib = null; // the game has new maps
    const done = job.items.filter(i => i.state === 'done').length;
    const failed = job.items.filter(i => i.state === 'failed').length;
    if (job.to) return; // the corner says it
    if (job.error) toast(job.error, true);
    else toast(`${plural(done, 'map')} added to the game.` + (failed ? ` ${num(failed)} not added.` : ''), failed > 0);
  }
});
listen('atlas-link', () => invoke('take_pending_link').then(handleLink));
// "Install missing" on a map list in Reimagined (bridge.rs): asked here first, always to the game.
listen('install-ask', e => {
  const ids = (e.payload?.ids || []).filter(n => Number.isInteger(n) && n > 0).slice(0, 100);
  if (ids.length) install(ids, `${plural(ids.length, 'map')} from Reimagined`, false, { to: 'game', ask: true });
});
listen('bridge-changed', e => drawBridge(e.payload));
// Reimagined added a map to an Atlas collection: an open Collections page shows it.
listen('game-collection-written', e => {
  const w = e.payload || {};
  if (w.error) toast(`“${w.name}” is not in the game: ${w.error}`, true);
  else toast(`“${w.name}” is in the game now, with ${plural(w.maps, 'map')}.` + (w.missing ? ` ${num(w.missing)} not installed, left out.` : ''));
  if (location.hash === '#collections') show('collections');
});
listen('collections-changed', () => { if (location.hash === '#collections') show('collections'); });
listen('bridge-pair', e => askPairing(e.payload));
// The game wrote to its database (a new attempt, a new map): the library is read again.
listen('game-changed', async e => {
  if (!lib) return;
  try { await getLibrary(true); } catch { return; }
  const tries = e.payload.tries || [];
  const t = tries[tries.length - 1];
  if (t) {
    const how = t.passed ? `passed, ${t.accuracy?.toFixed(2) ?? '?'}%${t.rp != null ? `, ${num(Math.round(t.rp))} RP` : ''}` : 'failed';
    toast((tries.length > 1 ? `${num(tries.length)} new tries. Last: ` : '') + `${t.title || 'A map'}: ${how}`);
  }
  onGame?.();
});
invoke('bridge_status').then(s => { drawBridge(s); askPairing(s.asking); });

invoke('app_info').then(i => (document.getElementById('version').textContent = `v${i.version} ${STAGE}`));
refreshQuota();

// Start: the box in index.html (#splash) stays until the first page is drawn (at least a moment, so
// it doesn't flash; at most a few seconds, so a slow game database never keeps the window hidden).
// Then it grows to the whole window, the page shows under it and the window gets its shadow back
// (off while the window is see-through, or it would outline the empty window around the box).
const started = performance.now();
function openSplash() {
  const el = document.getElementById('splash');
  if (!el || el.classList.contains('open')) return;
  el.classList.add('open');
  setTimeout(() => {
    document.documentElement.classList.remove('boot');
    win.setShadow(true).catch(() => {});
    el.classList.add('gone');
    setTimeout(() => { el.remove(); termsGate(); }, 300);
  }, 560);
}
const splashCap = setTimeout(openSplash, 4000);
show(location.hash.slice(1)).finally(() => {
  clearTimeout(splashCap);
  setTimeout(openSplash, Math.max(0, 900 - (performance.now() - started)));
});
invoke('take_pending_link').then(handleLink);
setTimeout(offerFirstCopy, 8000); // after the first page, never in the way of the start
