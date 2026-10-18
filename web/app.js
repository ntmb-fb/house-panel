'use strict';
// Floorplan views served from /floorplan; element ids are the same in all of them.
const VIEWS = [
  { svg: 'house', name: '2D', css: 'house.css' },
  { svg: 'house3d', name: '3D', css: 'house3d.css' },
  { svg: 'house3d_south', name: '3D syd', css: 'house3d.css' },
  { svg: 'house3d_portrait', name: '3D mobil', css: 'house3d.css' },
];
const plan = document.getElementById('plan');
const pop = document.getElementById('pop');
const tabs = document.getElementById('tabs');
const S = {}; // state id -> state
let byEl = {}, byId = {};
let open = null; // binding shown in the popup
let acts = []; // commands behind the popup's buttons

const store = (k, v) => {
  try { return v === undefined ? localStorage.getItem(k) : localStorage.setItem(k, v); } catch { return null; }
};
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const send = c => fetch('api/cmd', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(c) });

function cls(b, s) {
  switch (b.kind) {
    case 'light': return !s ? null : s.reachable === false ? 'unavailable' : s.on ? 'on' : 'off';
    case 'group': return s?.on ? 'on' : 'off';
    case 'bluos': return !s?.online ? 'unavailable' : ['play', 'stream', 'connecting'].includes(s.state) ? 'on' : 'off';
    case 'onkyo': return s?.online === false ? 'unavailable' : s?.power ? 'on' : 'off';
    case 'lgtv': return s?.power ? 'on' : 'off';
    case 'tado': return s?.heating ? 'on' : 'off';
    case 'unavailable': return 'unavailable';
  }
  return null;
}

function tadoText(s) {
  if (s?.temp == null) return '–';
  return `${s.temp.toFixed(1)}°` + (s.low_battery ? ' · Batteri lavt' : '');
}

function apply(b) {
  const el = document.getElementById(b.el);
  if (!el) return;
  if (b.kind === 'blank') { el.textContent = ''; return; }
  const s = S[b.id], c = cls(b, s);
  if (c) { el.classList.remove('on', 'off', 'unavailable', 'info'); el.classList.add(c); }
  if (b.val) { const v = document.getElementById(b.val); if (v) v.textContent = tadoText(s); }
}
const applyAll = () => Object.values(byEl).forEach(apply);

async function show(i) {
  const v = VIEWS[i];
  tabs.querySelectorAll('button').forEach((t, j) => t.setAttribute('aria-selected', i === j));
  document.getElementById('plan-css').href = `floorplan/${v.css}`;
  const res = await fetch(`floorplan/${v.svg}.svg`);
  plan.innerHTML = res.ok ? await res.text() : `<p>floorplan/${v.svg}.svg mangler</p>`;
  store('view', i);
  applyAll();
}

// --- taps: short = toggle light / open popup, long or right-click = popup (brightness for lights)
function find(t) {
  for (let el = t; el && el !== plan; el = el.parentElement) if (byEl[el.id]) return byEl[el.id];
  return null;
}
function tap(b) {
  if (b.kind === 'light' || b.kind === 'group') send({ dev: 'hue', target: b.target, on: !S[b.id]?.on });
  else if (['bluos', 'onkyo', 'lgtv', 'tado'].includes(b.kind)) openPop(b);
}
function long(b) {
  if (b.kind === 'group') return tap(b);
  if (b.kind !== 'blank' && b.kind !== 'unavailable') openPop(b);
}
let timer = null, down = null;
plan.addEventListener('pointerdown', e => {
  if (e.button !== 0) return;
  down = find(e.target);
  if (down) timer = setTimeout(() => { timer = null; long(down); }, 500);
});
plan.addEventListener('pointerup', () => { if (timer) { clearTimeout(timer); timer = null; tap(down); } });
['pointercancel', 'pointerleave'].forEach(t => plan.addEventListener(t, () => { clearTimeout(timer); timer = null; }));
plan.addEventListener('contextmenu', e => {
  const b = find(e.target);
  if (!b) return;
  e.preventDefault();
  if (e.pointerType !== 'touch' && !timer && e.button === 2) long(b); // touch long-press is handled by the timer
});

// --- popup
let tadoPending = null, tadoTimer = null;
function btn(label, cmd, on = false) {
  acts.push(cmd);
  return `<button class="${on ? 'on' : ''}" data-i="${acts.length - 1}">${label}</button>`;
}
function volRow(b, s, step = 1) {
  return `<div class="row">${btn('−', { dev: b.dev, action: 'vol', step: -step })}<span class="grow">Lydstyrke ${esc(s.volume ?? '–')}</span>` +
    btn('+', { dev: b.dev, action: 'vol', step }) + btn(s.mute ? '🔇' : '🔈', { dev: b.dev, action: 'mute' }, s.mute) + '</div>';
}
function body(b, s) {
  switch (b.kind) {
    case 'light':
      return `<h2>${esc(s.name)}</h2><p class="sub">${s.reachable === false ? 'Ikke forbundet' : s.on ? 'Tændt' : 'Slukket'}</p>` +
        `<input type="range" min="1" max="254" value="${s.bri ?? 254}" data-bri>` +
        `<div class="row">${btn(s.on ? 'Sluk' : 'Tænd', { dev: 'hue', target: b.target, on: !s.on }, s.on)}</div>`;
    case 'bluos': {
      const playing = ['play', 'stream'].includes(s.state);
      return `<h2>${esc(s.name)}</h2><p class="sub">${s.online ? esc([s.title, s.artist].filter(Boolean).join(' – ')) : 'Ikke forbundet'}</p>` +
        `<div class="row">${btn('⏮', { dev: b.dev, action: 'prev' })}${btn(playing ? '⏸' : '▶︎', { dev: b.dev, action: playing ? 'pause' : 'play' }, playing)}${btn('⏭', { dev: b.dev, action: 'next' })}</div>` +
        volRow(b, s, 3) +
        `<div class="row">${(s.presets || []).map(p => btn(esc(p.name), { dev: b.dev, action: 'preset', id: p.id })).join('')}</div>`;
    }
    case 'onkyo':
      return `<h2>Onkyo</h2><p class="sub">${s.online === false ? 'Ikke forbundet' : s.power ? 'Tændt' : 'Standby'}</p>` +
        `<div class="row">${btn(s.power ? 'Sluk' : 'Tænd', { dev: 'onkyo', action: 'power', on: !s.power }, s.power)}</div>` +
        volRow(b, s) +
        `<div class="row">${(s.inputs || []).map(i => btn(esc(i.name), { dev: 'onkyo', action: 'input', id: i.id }, i.id === s.input)).join('')}</div>`;
    case 'lgtv':
      return `<h2>LG TV</h2><p class="sub">${s.power ? esc(s.app || 'Tændt') : 'Slukket'}</p>` +
        `<div class="row">${btn(s.power ? 'Sluk' : 'Tænd', { dev: 'lgtv', action: 'power', on: !s.power }, s.power)}</div>` +
        (s.power ? volRow(b, s) + `<div class="row">` +
          btn('Hjem', { dev: 'lgtv', action: 'app', id: 'com.webos.app.home' }) +
          btn('Netflix', { dev: 'lgtv', action: 'app', id: 'netflix' }) +
          btn('YouTube', { dev: 'lgtv', action: 'app', id: 'youtube.leanback.v4' }) + '</div>' : '');
    case 'tado': {
      const t = tadoPending ?? s.target;
      return `<h2>${esc(b.name || 'tado')}</h2><p class="sub">${s.temp != null ? `${s.temp.toFixed(1)}° · ${Math.round(s.humidity ?? 0)}% fugt` : 'Ingen data endnu'}` +
        `${s.low_battery ? ' · Batteri lavt' : ''}</p>` +
        `<div class="row"><button data-tado="-0.5">−</button><span class="grow">Mål ${t != null ? t.toFixed(1) + '°' : 'slukket'}</span><button data-tado="0.5">+</button></div>` +
        (s.overlay ? `<div class="row">${btn('Følg plan', { dev: 'tado', zone: b.zone, resume: true })}</div>` : '');
    }
  }
  return '';
}
function render() {
  if (!open) return;
  if (pop.contains(document.activeElement) && document.activeElement.matches('input')) return; // don't fight a slider drag
  acts = [];
  pop.innerHTML = `<button class="close" aria-label="Luk">×</button>` + body(open, S[open.id] || {});
}
function openPop(b) {
  open = b; tadoPending = null;
  render();
  if (!pop.open) pop.showModal();
}
pop.addEventListener('close', () => { open = null; });
pop.addEventListener('click', e => {
  if (e.target === pop || e.target.closest('.close')) return pop.close();
  const el = e.target.closest('button');
  if (!el) return;
  if (el.dataset.i) send(acts[+el.dataset.i]);
  if (el.dataset.tado) {
    // Collect taps for 1.5 s before sending: each send costs tado API requests.
    const s = S[open.id] || {};
    tadoPending = Math.min(25, Math.max(5, (tadoPending ?? s.target ?? 20) + +el.dataset.tado));
    render();
    clearTimeout(tadoTimer);
    const zone = open.zone, target = tadoPending;
    tadoTimer = setTimeout(() => send({ dev: 'tado', zone, target }), 1500);
  }
});
pop.addEventListener('change', e => {
  if (e.target.matches('[data-bri]')) { send({ dev: 'hue', target: open.target, on: true, bri: +e.target.value }); e.target.blur(); }
});

// --- start
(async () => {
  const bindings = await (await fetch('api/config')).json();
  for (const b of bindings) {
    byEl[b.el] = b;
    if (b.id) (byId[b.id] ||= []).push(b);
  }
  tabs.innerHTML = VIEWS.map(v => `<button role="tab">${v.name}</button>`).join('');
  tabs.querySelectorAll('button').forEach((t, i) => t.addEventListener('click', () => show(i)));
  const saved = store('view');
  await show(saved != null && VIEWS[+saved] ? +saved : matchMedia('(max-width: 700px)').matches ? 3 : 2);

  new EventSource('api/events').onmessage = e => {
    const d = JSON.parse(e.data);
    Object.assign(S, d);
    for (const id in d) (byId[id] || []).forEach(apply);
    if (open && open.id in d) render();
  };
})();
