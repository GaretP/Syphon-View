'use strict';
/* The Syphon app's own pages: finding your Syphon, "Who's watching?", signing in, and the app settings.
   Everything else on screen comes from the node itself. Works with a mouse, a keyboard, a remote or a game controller. */

const $ = (s, r = document) => r.querySelector(s);
function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (['value', 'checked', 'disabled', 'type', 'placeholder', 'href', 'src', 'autocomplete', 'name', 'id', 'title', 'inputMode', 'maxLength'].includes(k)) el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

// One request to the app's helper. Errors always come back as a sentence that says what happened (never a code alone);
// a request that takes far too long is given up instead of leaving the screen waiting forever.
async function call(path, body, opts = {}) {
  const ctl = new AbortController();
  const limit = setTimeout(() => ctl.abort(), (opts.seconds || 180) * 1000);
  let r;
  try {
    r = await fetch(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body || {}), signal: ctl.signal });
  } catch (e) {
    throw new Error(e && e.name === 'AbortError' ? 'This took too long and was stopped. Check the connection and try again.' : 'The app\'s helper is not answering. Close the app and open it again.');
  } finally {
    clearTimeout(limit);
  }
  let d = {};
  try { d = await r.json(); } catch { /* not JSON */ }
  if (!r.ok) throw new Error(d.error || (r.status === 403 ? 'This window lost its connection to the app. Close the app and open it again.' : r.status >= 500 ? 'Something went wrong in the app (' + r.status + '). Try again.' : 'That did not work (' + r.status + ').'));
  return d;
}
async function getState() {
  try {
    const r = await fetch('/__app/api/state');
    if (r.ok) return await r.json();
  } catch { /* falls through */ }
  throw new Error('The app\'s helper is not answering. Close the app and open it again.');
}

// Short notes at the bottom of the screen, instead of pop-up boxes (which a TV remote can't always close).
function toast(text, kind) {
  let box = $('#toasts');
  if (!box) { box = h('div', { id: 'toasts', 'aria-live': 'polite' }); document.body.append(box); }
  const t = h('div', { class: 'toast ' + (kind || '') }, text);
  box.append(t);
  setTimeout(() => t.remove(), kind === 'bad' ? 9000 : 3500);
}

// A question with two answers, in the app's own style, that works with a remote: arrows, OK, Back.
function ask(question, okLabel, opts = {}) {
  return new Promise((resolve) => {
    const prevBack = back;
    const prev = document.activeElement;
    const done = (v) => { wrap.remove(); back = prevBack; if (prev && document.contains(prev)) prev.focus(); resolve(v); };
    const yes = h('button', { class: 'btn ' + (opts.danger ? 'danger' : 'primary'), type: 'button', 'data-nav': '' }, okLabel || 'OK');
    const no = h('button', { class: 'btn ghost', type: 'button', 'data-nav': '' }, opts.cancel || 'Cancel');
    yes.addEventListener('click', () => done(true));
    no.addEventListener('click', () => done(false));
    const wrap = h('div', { class: 'dialog', role: 'dialog', 'aria-modal': 'true', 'aria-label': question },
      h('div', { class: 'dialog-box' }, h('p', null, question), opts.note ? h('small', null, opts.note) : null, h('div', { class: 'row end' }, no, yes)));
    wrap.addEventListener('click', (e) => { if (e.target === wrap) done(false); });
    document.body.append(wrap);
    back = () => done(false);
    no.focus();
  });
}

// Anything that slips through still ends up as a readable note, not a silent failure.
window.addEventListener('unhandledrejection', (e) => { e.preventDefault(); toast((e.reason && e.reason.message) || 'Something went wrong. Try again.', 'bad'); });
window.addEventListener('error', (e) => { if (e.message) toast('Something went wrong: ' + e.message, 'bad'); });

// While a slow step runs, show what the helper says it is doing ("Reaching your VPN box…").
function follow(el, promise) {
  let live = true;
  (async () => {
    while (live) {
      await new Promise((r) => setTimeout(r, 500));
      if (!live) break;
      try { const st = await getState(); if (live && st.status) el.textContent = st.status; } catch { /* keep the last line */ }
    }
  })();
  return promise.finally(() => { live = false; });
}

let ST = null;
const target = () => (ST.settings.tv_mode ? '/#/tv' : '/');
// the same page in the normal view or the TV view ("#/tv/show/x" <-> "#/show/x"; anything else: the library / TV home)
const toNormal = (hash) => { const m = (hash || '').match(/^#\/tv\/(show|watch)\/(.+)$/); return m ? '#/' + m[1] + '/' + m[2] : ((hash || '').startsWith('#/tv') || !hash ? '#/library' : hash); };
const toTv = (hash) => { const m = (hash || '').match(/^#\/(show|watch)\/(.+)$/); return m ? '#/tv/' + m[1] + '/' + m[2] : ((hash || '').startsWith('#/tv') ? hash : '#/tv'); };
const go = () => { location.href = target(); };
const show = (...kids) => { $('#app').replaceChildren(h('div', { class: 'screen' + (ST && ST.settings.tv_mode ? ' tv' : '') }, ...kids)); focusFirst(); };
const logo = () => h('img', { class: 'logo', src: '/__app/logo.svg', alt: '' });
const msg = (text, kind) => h('p', { class: 'msg ' + (kind || ''), role: kind === 'bad' ? 'alert' : null }, text || '');
function hue(str) {
  let x = 7;
  for (const c of String(str).toLowerCase()) x = (x * 31 + c.charCodeAt(0)) >>> 0;
  return 250 + (x % 95);
}
// A button whose action may take a while: it shows it is working (a small spinner) and can't be pressed twice.
// An error the action doesn't handle itself becomes a note on screen.
function button(label, run, opts = {}) {
  const b = h('button', { class: 'btn' + (opts.primary ? ' primary' : '') + (opts.danger ? ' danger' : '') + (opts.ghost ? ' ghost' : ''), type: opts.submit ? 'submit' : 'button', 'data-nav': '', disabled: opts.disabled }, label);
  if (run) b.addEventListener('click', async (e) => {
    if (opts.submit) e.preventDefault();
    if (b.classList.contains('busy')) return;
    b.disabled = true;
    const slow = setTimeout(() => b.classList.add('busy'), 150);
    try { await run(e); } catch (err) { toast(err.message, 'bad'); } finally {
      clearTimeout(slow);
      if (document.contains(b)) { b.disabled = false; b.classList.remove('busy'); }
    }
  });
  return b;
}
const spinner = () => h('span', { class: 'spinner', 'aria-hidden': 'true' });
const field = (label, input, note) => h('label', { class: 'field' }, h('span', null, label), input, note ? h('small', null, note) : null);
const input = (props) => h('input', Object.assign({ 'data-nav': '' }, props));
function toggle(label, on, note, change) {
  const box = h('input', { type: 'checkbox', checked: on, 'data-nav': '' });
  box.addEventListener('change', () => change(box.checked, box));
  return h('label', { class: 'toggle' }, box, h('span', { class: 'track' }), h('span', { class: 'toggle-text' }, h('b', null, label), note ? h('small', null, note) : null));
}

// ------------------------------------------------------------------ arrows, OK and Back

function focusFirst() {
  setTimeout(() => {
    if (document.activeElement && document.activeElement !== document.body && document.contains(document.activeElement)) return;
    const f = $('[autofocus]') || $('.primary[data-nav]') || $('[data-nav]');
    if (f) f.focus();
  }, 30);
}
function move(dir) {
  const all = [...document.querySelectorAll('[data-nav]')].filter((el) => { const r = el.getBoundingClientRect(); return r.width > 0 && !el.disabled; });
  const cur = document.activeElement;
  if (!all.includes(cur)) return all[0] && all[0].focus();
  const r = cur.getBoundingClientRect(), cx = r.left + r.width / 2, cy = r.top + r.height / 2;
  let best = null, bk = null;
  for (const el of all) {
    if (el === cur) continue;
    const b = el.getBoundingClientRect(), x = b.left + b.width / 2, y = b.top + b.height / 2;
    let main, side;
    if (dir === 'left' || dir === 'right') { if (Math.abs(y - cy) > Math.max(r.height, b.height) / 2) continue; main = dir === 'left' ? cx - x : x - cx; side = Math.abs(y - cy); }
    else { main = Math.round((dir === 'up' ? cy - y : y - cy) / 16); side = Math.abs(x - cx); }
    if (main <= 0) continue;
    if (!bk || main < bk[0] || (main === bk[0] && side < bk[1])) { best = el; bk = [main, side]; }
  }
  if (best) { best.focus(); best.scrollIntoView({ block: 'nearest', behavior: 'smooth' }); }
}
let back = null;
document.addEventListener('keydown', (e) => {
  const t = e.target;
  const typing = t && (t.tagName === 'INPUT' && !['checkbox', 'radio'].includes(t.type) || t.tagName === 'TEXTAREA');
  const dirs = { ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right' };
  if (dirs[e.key] && !(typing && (e.key === 'ArrowLeft' || e.key === 'ArrowRight'))) { e.preventDefault(); move(dirs[e.key]); return; }
  if ((e.key === 'Escape' || e.key === 'BrowserBack' || e.keyCode === 461 || (e.key === 'Backspace' && !typing)) && back) { e.preventDefault(); back(); }
});
// game controllers: D-pad / stick to move, A = OK, B = back
let padHeld = {};
setInterval(() => {
  if (!navigator.getGamepads) return;
  const now = {};
  for (const gp of navigator.getGamepads()) {
    if (!gp) continue;
    if (gp.buttons[12] && gp.buttons[12].pressed || gp.axes[1] < -0.6) now.up = 1;
    if (gp.buttons[13] && gp.buttons[13].pressed || gp.axes[1] > 0.6) now.down = 1;
    if (gp.buttons[14] && gp.buttons[14].pressed || gp.axes[0] < -0.6) now.left = 1;
    if (gp.buttons[15] && gp.buttons[15].pressed || gp.axes[0] > 0.6) now.right = 1;
    if (gp.buttons[0] && gp.buttons[0].pressed) now.ok = 1;
    if (gp.buttons[1] && gp.buttons[1].pressed) now.back = 1;
  }
  for (const k of Object.keys(now)) if (!padHeld[k]) {
    if (k === 'ok') { const a = document.activeElement; if (a && a.click) a.click(); }
    else if (k === 'back') { if (back) back(); }
    else move(k);
  }
  padHeld = now;
}, 60);
document.addEventListener('mousemove', (e) => { const t = e.target.closest && e.target.closest('[data-nav]'); if (t && t !== document.activeElement && t.tagName === 'BUTTON') t.focus({ preventScroll: true }); });

// ------------------------------------------------------------------ start

async function main() {
  ST = await getState();
  const path = location.pathname.replace(/\/+$/, '');
  const q = new URLSearchParams(location.search);
  if (!ST.node || path === '/__app/setup') return viewSetup();
  if (path === '/__app/settings') return viewSettings(q.get('from') || '');
  if (path === '/__app/leave-tv') return viewLeaveTv(q.get('to') || '#/library');
  if (!(await connect(q.get('offline')))) return;
  ST = await getState();
  if (path === '/__app/signin') return viewSignin(q.get('user') || '');
  if (path === '/__app/profiles') return viewProfiles();
  if (ST.active) return go();
  if (!ST.profiles.length) return viewSignin('');
  // open straight into the library as the last person, unless the app is asked to show "Who's watching?"
  const ask = ST.settings.tv_mode || ST.settings.ask_profile;
  const last = ST.profiles.find((p) => p.user === ST.last);
  if (!ask && last && last.remembered) {
    try { const r = await call('/__app/api/use', { user: last.user }); if (r.ok) return go(); } catch { /* show the picker */ }
  }
  return viewProfiles();
}

// The VPN test: each step with what it found, so a problem says where it is
function vpnTest() {
  const list = h('ol', { class: 'checks' });
  const run = button('Test the VPN connection', async () => {
    list.replaceChildren(h('li', { class: 'wait' }, h('b', null, 'Testing…'), h('small', null, 'This takes up to 20 seconds.')));
    try {
      const r = await call('/__app/api/vpn/test');
      list.replaceChildren(...r.steps.map((x) => h('li', { class: x.ok ? 'ok' : 'bad' }, h('b', null, (x.ok ? '\u2713 ' : '\u2717 ') + x.step), h('small', null, x.detail))));
    } catch (e) { list.replaceChildren(h('li', { class: 'bad' }, h('b', null, '\u2717 Test'), h('small', null, e.message))); }
  }, { ghost: true });
  return h('div', { class: 'vpntest' }, run, list);
}

// "Connecting to Living Room…", then on, or a clear message and what to do about it
async function connect(lost) {
  const name = ST.node.name;
  const status = msg('Looking for ' + name + '…');
  const bar = h('div', { class: 'progress moving', role: 'progressbar', 'aria-label': 'Connecting' }, h('i'));
  const took = msg('', 'faint');
  const started = Date.now();
  show(h('div', { class: 'center' }, logo(), h('h1', null, name), status, bar, took, lost ? msg(lost, 'warn') : null));
  const tick = setInterval(async () => {
    const secs = Math.round((Date.now() - started) / 1000);
    if (secs >= 5) took.textContent = secs + ' s' + (secs >= 20 ? ' (a sleeping Syphon takes up to a minute to wake)' : '');
    try {
      const s = await getState();
      if (s.status) status.textContent = s.status;
    } catch { /* keep going */ }
  }, 500);
  try {
    await call('/__app/api/connect');
    return true;
  } catch (e) {
    show(h('div', { class: 'center' }, logo(), h('h1', null, 'Can\'t reach ' + name), msg(e.message, 'bad'),
      h('div', { class: 'row' }, button('Try again', () => main(), { primary: true }), button('App settings', () => { location.href = '/__app/settings'; }),
        button('Choose another Syphon', () => { location.href = '/__app/setup'; }, { ghost: true })),
      ST.node && ST.node.vpn ? vpnTest() : null));
    return false;
  } finally {
    clearInterval(tick);
  }
}

// ------------------------------------------------------------------ finding the node

async function viewSetup(mode) {
  const err = msg('', 'bad');
  const choose = async (body) => {
    err.className = 'msg'; err.textContent = body.code ? 'Reading the code…' : 'Looking for your Syphon…';
    try {
      await follow(err, call('/__app/api/node', body));
      ST = await getState();
      location.href = '/__app/signin';
    } catch (e) { err.className = 'msg bad'; err.textContent = e.message; }
  };
  const tabs = h('div', { class: 'tabs', role: 'tablist' },
    button('I\'m at home', () => viewSetup('home'), { ghost: mode === 'away' }),
    button('I\'m away from home', () => viewSetup('away'), { ghost: mode !== 'away' }));
  tabs.children[mode === 'away' ? 1 : 0].setAttribute('aria-selected', 'true');
  const head = h('div', { class: 'head' }, logo(), h('h1', null, 'Connect to your Syphon'));
  back = ST.node ? () => { location.href = '/__app/'; } : null;
  const cancel = ST.node ? button('Back', () => { location.href = '/__app/'; }, { ghost: true }) : null;

  if (mode === 'away') {
    // away from home: a VPN code from the owner's VPN box
    const code = input({ type: 'text', placeholder: 'SYPHON-VPN-1:…', autocomplete: 'off', autofocus: true });
    const go = async (body) => { if (!body.code.trim()) { err.className = 'msg bad'; err.textContent = 'Paste the VPN code first.'; code.focus(); return; } await choose(body); };
    const vpnBtn = button('Connect', () => go({ code: code.value }), { primary: true, submit: true });
    show(h('div', { class: 'narrow' }, head, tabs,
      h('p', { class: 'lead' }, 'Away from home the app connects through the VPN box at your home, built into the app: nothing else to install. Ask the owner for a VPN code (in Syphon: Settings > Node > Outside access, or on the box: sudo syphon-vpn add "your device") and paste it here. It works once, on one device.'),
      h('form', { class: 'card', onsubmit: (e) => { e.preventDefault(); vpnBtn.click(); } },
        field('VPN code', code), vpnBtn),
      err,
      h('p', { class: 'faint' }, 'Signing in from outside for the first time needs one extra step: someone at home approves it in Syphon, or you type the code from your authenticator app if two-step sign-in is on. After that this computer is trusted.'),
      cancel));
    return;
  }

  const list = h('div', { class: 'found' }, msg('Looking on your network…'));
  const addr = input({ type: 'text', placeholder: 'for example 192.168.1.20', autocomplete: 'off' });
  show(h('div', { class: 'narrow' }, head, tabs,
    h('p', { class: 'lead' }, 'Pick your Syphon, sign in once, and next time the app opens straight into your library.'),
    list,
    h('form', { class: 'row', onsubmit: (e) => { e.preventDefault(); choose({ url: addr.value }); } }, field('Or type its address', addr), button('Connect', () => choose({ url: addr.value }), { primary: true, submit: true })),
    err, cancel));
  let found = [];
  try { found = (await call('/__app/api/find')).found; } catch { /* nothing found */ }
  list.replaceChildren(...(found.length
    ? found.map((f) => button(h('span', null, h('b', null, f.name), h('small', null, f.url.replace('http://', ''))), () => choose({ url: f.url, name: f.name, id: f.id }), { primary: found.length === 1 }))
    : [msg('Nothing found on this network. A node on another part of your network (a different VLAN) does not show up: type its address. Not at home? Choose "I\'m away from home".', 'faint')]));
  if (found.length) list.querySelector('button').focus();
}

// ------------------------------------------------------------------ who's watching

function avatar(name, big) {
  const a = h('span', { class: 'avatar' + (big ? ' big' : '') }, name.slice(0, 1).toUpperCase());
  a.style.setProperty('--h', hue(name));
  return a;
}

function viewProfiles(note) {
  back = null;
  const tiles = ST.profiles.map((p) => {
    const b = h('button', { class: 'person', type: 'button', 'data-nav': '' }, avatar(p.user, true), h('span', { class: 'name' }, p.user), p.remembered ? null : h('small', null, 'needs password'));
    b.addEventListener('click', async () => {
      if (b.disabled) return;
      b.disabled = true;
      b.classList.add('busy');
      for (const o of document.querySelectorAll('.person')) if (o !== b) o.disabled = true;
      try {
        const r = await call('/__app/api/use', { user: p.user });
        if (r.ok) return go();
        viewSignin(p.user, r.message || 'Sign in with your password.');
      } catch (e) { viewProfiles(e.message); } finally { b.disabled = false; b.classList.remove('busy'); }
    });
    return b;
  });
  const add = h('button', { class: 'person add', type: 'button', 'data-nav': '', onclick: () => viewSignin('') }, h('span', { class: 'avatar big plus' }, '+'), h('span', { class: 'name' }, 'Add someone'));
  show(h('div', { class: 'center wide' }, h('p', { class: 'node' }, ST.node.name), h('h1', null, 'Who\'s watching?'),
    note ? msg(note, 'bad') : null,
    h('div', { class: 'people' }, tiles, add),
    h('div', { class: 'row' }, button('App settings', () => { location.href = '/__app/settings'; }, { ghost: true }))));
  const last = [...document.querySelectorAll('.person')].find((x) => x.textContent.startsWith((ST.last || '\u0000')));
  setTimeout(() => (last || $('.person')).focus(), 40);
}

// ------------------------------------------------------------------ signing in

function viewSignin(user, note) {
  const name = input({ type: 'text', value: user, autocomplete: 'username', placeholder: 'Your name' });
  const pw = input({ type: 'password', autocomplete: 'current-password', placeholder: 'Your password' });
  const code = input({ type: 'text', inputMode: 'numeric', autocomplete: 'one-time-code', placeholder: '6-digit code' });
  const codeField = field('Code from your authenticator app', code);
  codeField.classList.add('hidden');
  const remember = h('input', { type: 'checkbox', checked: true, 'data-nav': '' });
  const err = msg(note || '', note ? 'warn' : 'bad');
  let poll = null;
  const done = () => { clearInterval(poll); go(); };
  const submit = async () => {
    err.textContent = ''; err.className = 'msg bad';
    if (!name.value.trim() || !pw.value) { err.textContent = 'Type your name and password.'; (name.value.trim() ? pw : name).focus(); return; }
    err.className = 'msg'; err.textContent = 'Signing in…';
    try {
      const r = await call('/__app/api/signin', { user: name.value.trim(), password: pw.value, totp: code.value.trim() || null, remember: remember.checked });
      if (r.ok) return done();
      if (r.need === 'totp') { codeField.classList.remove('hidden'); code.focus(); err.className = 'msg warn'; err.textContent = 'Two-step sign-in is on: type the code from your authenticator app.'; return; }
      if (r.pending) return waitForApproval(r.pending);
      err.textContent = '';
    } catch (e) { err.className = 'msg bad'; err.textContent = e.message; pw.select(); }
  };
  const waitForApproval = (p) => {
    show(h('div', { class: 'center' }, logo(), h('h1', null, 'Approve this sign-in at home'),
      h('p', { class: 'lead' }, 'You are signing in from outside your home network. Open Syphon on a device at home, find the notice about this sign-in, check the code and press Approve.'),
      h('p', { class: 'code' }, p.code), h('div', { class: 'progress moving' }, h('i')), msg('Waiting… the request expires in 10 minutes.', 'faint'),
      button('Cancel', () => { clearInterval(poll); viewSignin(name.value); }, { ghost: true })));
    poll = setInterval(async () => {
      try {
        const r = await call('/__app/api/signin', { user: name.value.trim(), id: p.id, secret: p.secret, remember: remember.checked });
        if (r.ok) return done();
        if (r.state === 'denied' || r.state === 'expired') { clearInterval(poll); viewSignin(name.value, r.state === 'denied' ? 'That sign-in was declined at home.' : 'The request expired. Sign in again.'); }
      } catch { /* keep waiting */ }
    }, 3000);
  };
  const form = h('form', { class: 'card', onsubmit: (e) => { e.preventDefault(); submit(); } },
    field('Name', name), field('Password', pw), codeField,
    h('label', { class: 'check' }, remember, h('span', null, h('b', null, 'Remember me on this computer'), h('small', null, 'Next time, pick your name and you are in, no password. Anyone using this computer can do the same, so leave it off on a shared one.'))),
    err, h('div', { class: 'row' }, button('Sign in', submit, { primary: true, submit: true }), ST.profiles.length ? button('Back', () => viewProfiles(), { ghost: true }) : null));
  show(h('div', { class: 'narrow' }, h('div', { class: 'head' }, logo(), h('div', null, h('p', { class: 'node' }, ST.node.name), h('h1', null, user ? 'Hello, ' + user : 'Sign in'))), form));
  back = ST.profiles.length ? () => viewProfiles() : null;
  setTimeout(() => (user ? pw : name).focus(), 40);
}

// ------------------------------------------------------------------ app settings

// "Leave the TV view" from the TV menu: TV mode off (with the PIN when one is set), then the same page, normal view
async function viewLeaveTv(to) {
  const done = () => { location.href = '/' + toNormal(to); };
  if (!ST.settings.tv_mode) return done();
  const off = async (pin) => { await call('/__app/api/settings', { tv_mode: false, pin_check: pin }); done(); };
  if (!ST.settings.pin_set) {
    show(h('div', { class: 'center' }, logo(), msg('Leaving TV mode…'), h('div', { class: 'progress moving' }, h('i'))));
    try { await off(''); } catch (e) { toast(e.message, 'bad'); setTimeout(() => history.back(), 1500); }
    return;
  }
  const pin = input({ type: 'password', inputMode: 'numeric', placeholder: 'PIN', autocomplete: 'off', maxLength: 12 });
  const err = msg('', 'bad');
  const ok = async () => { try { await off(pin.value); } catch (e) { err.textContent = e.message; pin.value = ''; pin.focus(); } };
  show(h('div', { class: 'center' }, logo(), h('h1', null, 'Leave TV mode'), h('form', { class: 'card', onsubmit: (e) => { e.preventDefault(); ok(); } }, field('PIN', pin), err,
    h('div', { class: 'row' }, button('Leave TV mode', ok, { primary: true, submit: true }), button('Back', () => history.back(), { ghost: true })))));
  back = () => history.back();
  setTimeout(() => pin.focus(), 40);
}

async function viewSettings(from) {
  from = from || viewSettings.from || '';
  viewSettings.from = from;
  // in TV mode a PIN can guard the settings (and leaving TV mode)
  if (ST.settings.pin_set && ST.settings.tv_mode && !viewSettings.pin) {
    const pin = input({ type: 'password', inputMode: 'numeric', placeholder: 'PIN', autocomplete: 'off', maxLength: 12 });
    const err = msg('', 'bad');
    const ok = async () => { try { await call('/__app/api/pin', { pin: pin.value }); viewSettings.pin = pin.value; viewSettings(); } catch (e) { err.textContent = e.message; pin.value = ''; } };
    show(h('div', { class: 'center' }, logo(), h('h1', null, 'App settings'), h('form', { class: 'card', onsubmit: (e) => { e.preventDefault(); ok(); } }, field('PIN', pin), err,
      h('div', { class: 'row' }, button('Open', ok, { primary: true, submit: true }), button('Back', () => history.back(), { ghost: true })))));
    back = () => history.back();
    setTimeout(() => pin.focus(), 40);
    return;
  }
  const pinNow = viewSettings.pin || '';
  const save = async (body, el) => {
    try { await call('/__app/api/settings', Object.assign({ pin_check: pinNow }, body)); ST = await getState(); return true; }
    catch (e) { toast(e.message, 'bad'); if (el && 'checked' in el) el.checked = !el.checked; return false; }
  };
  const s = ST.settings;
  const n = ST.node || {};
  const pin = input({ type: 'password', inputMode: 'numeric', placeholder: s.pin_set ? 'new PIN (4–12 digits)' : '4–12 digits', autocomplete: 'off', maxLength: 12 });
  const vpnCode = input({ type: 'text', placeholder: 'SYPHON-VPN-1:…', autocomplete: 'off' });
  const vpnMsg = msg('');
  const people = h('div', { class: 'list' }, ST.profiles.length ? ST.profiles.map((p) => h('div', { class: 'line' }, avatar(p.user), h('span', { class: 'grow' }, h('b', null, p.user), h('small', null, p.remembered ? 'signs in without a password here' : 'needs the password here')),
    button('Remove from this computer', async () => { if (!await ask('Remove ' + p.user + ' from this computer?', 'Remove', { danger: true, note: 'They can sign in again any time.' })) return; await call('/__app/api/forget', { user: p.user }); ST = await getState(); viewSettings(); }, { danger: true }))) : msg('Nobody has signed in on this computer yet.', 'faint'));
  const leave = () => { location.href = !ST.active ? '/__app/profiles' : from ? '/' + (ST.settings.tv_mode ? toTv(from) : toNormal(from)) : target(); };
  show(h('div', { class: 'narrow settings' }, h('div', { class: 'head' }, logo(), h('h1', null, 'App settings')),
    h('section', null, h('h2', null, 'This computer'),
      toggle('TV mode', s.tv_mode, 'Full screen with the big-screen layout, made for a computer plugged into a TV. Everything works with arrows, OK and Back: a keyboard, a remote, an air mouse or a game controller. Hold Back for the menu.', async (v, el) => {
        // switching takes you back to the page you came from, in the new view
        if (await save({ tv_mode: v }, el) && ST.active) location.href = '/' + (v ? toTv(from) : toNormal(from));
      }),
      toggle('Ask "Who\'s watching?" when the app opens', s.ask_profile || s.tv_mode, s.tv_mode ? 'Always on in TV mode.' : 'Otherwise it opens straight into the library as the last person.', (v, el) => save({ ask_profile: v }, el)),
      toggle('Start with this computer', s.autostart, 'Useful for a TV box: switch it on and Syphon is there.', (v, el) => save({ autostart: v }, el))),
    h('section', null, h('h2', null, 'PIN for TV mode'),
      h('p', { class: 'faint' }, 'With a PIN, leaving TV mode, quitting and these settings need it, so nobody wanders off the TV by accident.'),
      h('div', { class: 'row' }, pin, button(s.pin_set ? 'Change PIN' : 'Set PIN', async () => { if (await save({ pin: pin.value })) { viewSettings.pin = pin.value; viewSettings(); } }),
        s.pin_set ? button('Remove PIN', async () => { if (await save({ pin: '' })) { viewSettings.pin = ''; viewSettings(); } }, { ghost: true }) : null)),
    h('section', null, h('h2', null, 'People on this computer'), people),
    h('section', null, h('h2', null, 'Your Syphon'),
      h('p', null, h('b', null, n.name || 'none chosen'), ' ', h('small', { class: 'faint' }, (n.lan || []).join(', '))),
      h('p', { class: 'faint' }, ST.route === 'home' ? 'Connected over your home network.' : ST.route === 'vpn' ? 'Connected through your VPN box.' : 'Not connected right now.'),
      button('Choose another Syphon', () => { location.href = '/__app/setup'; }, { ghost: true })),
    h('section', null, h('h2', null, 'Away from home'),
      h('p', { class: 'faint' }, n.vpn ? 'Away from home the app connects through your VPN box (' + n.vpn + '). At home it talks to your Syphon directly.' : 'Away from home the app connects through the VPN box at your home. Paste the VPN code from the owner (on the VPN box: sudo syphon-vpn add "this computer").'),
      field(n.vpn ? 'New VPN code' : 'VPN code', vpnCode, n.vpn ? 'Only needed when the owner gave this computer a new one.' : 'It works once: the app makes a key of its own, which never leaves this computer.'),
      h('div', { class: 'row' }, button(n.vpn ? 'Use this code' : 'Connect', async () => {
        if (!vpnCode.value.trim()) { vpnMsg.className = 'msg bad'; vpnMsg.textContent = 'Paste the VPN code first.'; vpnCode.focus(); return; }
        vpnMsg.className = 'msg'; vpnMsg.textContent = 'Reading the code…';
        try { await follow(vpnMsg, call('/__app/api/node', { code: vpnCode.value })); ST = await getState(); toast('Connected through your VPN box', 'ok'); viewSettings(); } catch (e) { vpnMsg.className = 'msg bad'; vpnMsg.textContent = e.message; }
      }),
        n.vpn ? button('Stop using the VPN box', async () => { if (!await ask('Stop using the VPN box on this computer?', 'Stop using it', { danger: true, note: 'You need a new code from the owner to use it again.' })) return; await call('/__app/api/node', { vpn_off: true }); ST = await getState(); viewSettings(); }, { danger: true }) : null), vpnMsg,
      n.vpn ? vpnTest() : null),
    h('section', null, h('h2', null, 'About'), h('p', { class: 'faint' }, 'Syphon app ' + ST.version + '. It only talks to your own Syphon: no accounts elsewhere, no tracking.'),
      h('div', { class: 'row' }, ST.active ? button('Sign out', async () => { await call('/__app/api/signout'); location.href = '/__app/profiles'; }, { ghost: true }) : null,
        button('Quit the app', () => call('/__app/api/quit', { pin: pinNow }), { danger: true }))),
    h('div', { class: 'row end' }, button('Done', leave, { primary: true }))));
  back = leave;
  setTimeout(() => { window.scrollTo(0, 0); const t = $('.toggle input'); if (t) t.focus({ preventScroll: true }); }, 50);
}

main().catch((e) => {
  $('#app').replaceChildren(h('div', { class: 'screen' }, h('div', { class: 'center' }, logo(), h('h1', null, 'Something went wrong'), msg(e.message, 'bad'),
    h('div', { class: 'row' }, button('Try again', () => location.reload(), { primary: true }), button('App settings', () => { location.href = '/__app/settings'; })))));
  focusFirst();
});
