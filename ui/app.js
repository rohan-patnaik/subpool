'use strict';

// ---------------------------------------------------------------- state

const $ = (sel, el = document) => el.querySelector(sel);
const view = $('#view');

const S = {
  key: localStorage.getItem('subpool.key') || '',
  locked: null, // null | 'key' | 'remote'
  route: 'overview',
  overview: null,
  accounts: null,
  requests: [],
  models: [],
  live: 'connecting',
  paused: false,
  filter: '',
  panel: null, // 'claude' | 'codex' | 'key'
  login: null, // { state, provider, url, callback, status, message }
  keyProvider: 'claude',
  snippet: localStorage.getItem('subpool.snippet') || 'claude',
  confirm: null,
  config: { text: null, saved: null, path: '', msg: null, busy: false },
};

const PROVIDER = { claude: 'Claude', codex: 'Codex', gemini: 'Gemini', 'openai-compat': 'Compatible' };
const CLIENT = { openai: 'OpenAI', responses: 'Responses', claude: 'Anthropic', gemini: 'Gemini' };

const ICON = {
  copy: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5V3.5A1 1 0 0 0 9.5 2.5h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2"/></svg>',
  check: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><path d="m3.5 8.5 3 3 6-7" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  refresh: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" stroke-linecap="round"/><path d="M13.5 2.5v3h-3" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  trash: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M2.5 4.5h11M6.5 4.5v-2h3v2M4 4.5l.7 9h6.6l.7-9" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  external: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="M9.5 2.5h4v4M13.5 2.5 7 9M11.5 9.5v3a1 1 0 0 1-1 1h-7a1 1 0 0 1-1-1v-7a1 1 0 0 1 1-1h3" stroke-linecap="round" stroke-linejoin="round"/></svg>',
};

// ---------------------------------------------------------------- helpers

const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

function fmt(n) {
  n = Number(n) || 0;
  if (n < 1000) return n.toLocaleString('en-US');
  if (n < 1e6) return (n / 1e3).toFixed(n < 1e5 ? 1 : 0).replace(/\.0$/, '') + 'k';
  if (n < 1e9) return (n / 1e6).toFixed(n < 1e7 ? 2 : n < 1e8 ? 1 : 0).replace(/(\.\d*?)0+$/, '$1').replace(/\.$/, '') + 'M';
  return (n / 1e9).toFixed(2).replace(/\.?0+$/, '') + 'B';
}

function ms(v) {
  if (v == null) return '—';
  if (v < 1000) return `${v}ms`;
  if (v < 60000) return `${(v / 1000).toFixed(v < 10000 ? 1 : 0)}s`;
  return `${Math.floor(v / 60000)}m ${Math.round((v % 60000) / 1000)}s`;
}

function ago(iso) {
  if (!iso) return 'never';
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 10) return 'just now';
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function until(iso) {
  const s = Math.max(0, Math.round((Date.parse(iso) - Date.now()) / 1000));
  if (s < 3600) return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

function span(secs) {
  if (secs < 60) return `${Math.floor(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
}

const clock = (iso) => new Date(iso).toLocaleTimeString('en-GB', { hour12: false });

class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

async function api(path, opts = {}) {
  const headers = { 'content-type': 'application/json' };
  if (S.key) headers.authorization = `Bearer ${S.key}`;
  const res = await fetch(`/api${path}`, { ...opts, headers });
  if (res.status === 401 || res.status === 403) {
    const lock = res.status === 401 ? 'key' : 'remote';
    if (S.locked !== lock) { S.locked = lock; render(); }
    throw new ApiError(res.status, 'locked');
  }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new ApiError(res.status, data.error || res.statusText);
  return data;
}

// ---------------------------------------------------------------- data

async function loadAll() {
  const [overview, accounts, requests, models] = await Promise.all([
    api('/overview'), api('/accounts'), api('/requests'), api('/models'),
  ]);
  Object.assign(S, { overview, accounts, requests, models, locked: null });
}

let accountsTimer = 0;
function refreshAccounts() {
  clearTimeout(accountsTimer);
  accountsTimer = setTimeout(async () => {
    try {
      const [accounts, overview, models] = await Promise.all([api('/accounts'), api('/overview'), api('/models')]);
      Object.assign(S, { accounts, overview, models });
      patch('ov-accounts', ovAccountsHTML);
      patch('acct-list', accountListHTML);
      patch('acct-head', accountHeadHTML);
      patch('connect', connectHTML);
    } catch {}
  }, 250);
}

// ---------------------------------------------------------------- live

let ws = null;
let wsDelay = 1000;

function connectLive() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const q = S.key ? `?key=${encodeURIComponent(S.key)}` : '';
  ws = new WebSocket(`${proto}://${location.host}/api/live${q}`);
  ws.onopen = () => { wsDelay = 1000; setLive('live'); };
  ws.onclose = () => {
    setLive('offline');
    if (!S.locked) setTimeout(connectLive, wsDelay);
    wsDelay = Math.min(wsDelay * 2, 15000);
  };
  ws.onmessage = (e) => {
    try { onLive(JSON.parse(e.data)); } catch {}
  };
}

function setLive(state) {
  S.live = state;
  renderStatus();
}

function onLive(msg) {
  if (msg.type === 'request') return onRequest(msg.data);
  if (msg.type === 'accounts') return refreshAccounts();
  if (msg.type === 'login') return pollLogin();
  if (msg.type === 'tick' && S.overview) {
    S.overview.totals = msg.data.totals;
    S.overview.active = msg.data.active;
    patch('figures', figuresHTML);
  }
}

function onRequest(log) {
  S.requests.unshift(log);
  if (S.requests.length > 300) S.requests.length = 300;
  const o = S.overview;
  if (o) {
    const t = o.totals;
    t.requests += 1;
    if (log.status < 400) t.ok += 1; else t.failed += 1;
    t.input_tokens += log.input_tokens;
    t.output_tokens += log.output_tokens;
    t.cache_tokens += log.cache_tokens;
    const minute = Math.floor(Date.parse(log.ts) / 60000);
    let b = o.series[o.series.length - 1];
    if (!b || b.minute !== minute) {
      o.series.push((b = { minute, requests: 0, failed: 0, tokens: 0 }));
      if (o.series.length > 60) o.series.shift();
    }
    b.requests += 1;
    if (log.status >= 400) b.failed += 1;
    b.tokens += log.input_tokens + log.output_tokens + log.cache_tokens;
  }
  if (S.route === 'overview') {
    patch('figures', figuresHTML);
    patch('bars', barsHTML);
    patch('recent', recentHTML, true);
  } else if (S.route === 'requests' && !S.paused && matches(log)) {
    const body = $('#req-body');
    if (body) {
      $('#req-empty')?.remove();
      body.insertAdjacentHTML('afterbegin', requestRowHTML(log, true));
      while (body.children.length > 300) body.lastElementChild.remove();
      patch('req-count', reqCountHTML);
    }
  }
}

// ---------------------------------------------------------------- render

function patch(id, fn, lit = false) {
  const el = document.getElementById(id);
  if (!el) return;
  el.innerHTML = fn(lit);
}

function renderStatus() {
  const el = $('#live-status');
  if (!el) return;
  const up = S.overview ? span((Date.now() - Date.parse(S.overview.started_at)) / 1000) : '';
  const [cls, text] = {
    live: ['ok', 'Live'],
    connecting: ['dim', 'Connecting'],
    offline: ['err', 'Reconnecting'],
  }[S.live];
  const dot = { ok: 'var(--ok)', dim: 'var(--fg-3)', err: 'var(--err)' }[cls];
  el.innerHTML = `<span class="dot" style="background:${dot}" title="${text}"></span><span class="live-word">${text}</span>${up ? `<span class="uptime dim">· up <span data-uptime>${up}</span></span>` : ''}`;
}

function render() {
  for (const a of document.querySelectorAll('.tabs a')) {
    if (a.dataset.tab === S.route && !S.locked) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
  renderStatus();
  if (S.locked) { view.innerHTML = lockHTML(); bindLock(); return; }
  if (!S.overview) { view.innerHTML = skeletonHTML(); return; }
  const pages = { overview: overviewHTML, accounts: accountsHTML, requests: requestsHTML, config: configHTML };
  view.innerHTML = (pages[S.route] || overviewHTML)();
  if (S.route === 'config') bindConfig();
  if (S.route === 'requests') bindRequests();
}

function skeletonHTML() {
  return `<div class="skel-rows" aria-busy="true" aria-label="Loading">${'<div class="skel"></div>'.repeat(6)}</div>`;
}

// overview -------------------------------------------------------------

function overviewHTML() {
  return `
    <section class="section">
      <div class="traffic-head">
        <div class="section-head" style="margin:0"><h2>Traffic</h2><span class="meta">since start</span></div>
        <dl class="figures" id="figures">${figuresHTML()}</dl>
      </div>
      <div class="chart" id="bars">${barsHTML()}</div>
      <div class="axis"><span>60 min ago</span><span>30 min</span><span>now</span></div>
      <div class="legend"><span><i></i>Succeeded</span><span><i class="f"></i>Failed</span></div>
    </section>
    <div class="section split">
      <section id="ov-accounts">${ovAccountsHTML()}</section>
      <section id="connect" class="connect">${connectHTML()}</section>
    </div>
    <section class="section">
      <div class="section-head"><h2>Latest requests</h2><a class="link" href="#/requests">All requests</a></div>
      <div id="recent">${recentHTML()}</div>
    </section>`;
}

function figuresHTML() {
  const o = S.overview;
  const t = o.totals;
  const rate = t.requests ? `${((t.ok / t.requests) * 100).toFixed(t.ok === t.requests ? 0 : 1)}%` : '—';
  const items = [
    ['Requests', fmt(t.requests)],
    ['Success', rate],
    ['Tokens in', fmt(t.input_tokens)],
    ['Tokens out', fmt(t.output_tokens)],
    ['Cached', fmt(t.cache_tokens)],
    ['In flight', fmt(o.active)],
  ];
  return items.map(([k, v]) => `<div><dt>${k}</dt><dd>${v}</dd></div>`).join('');
}

function niceMax(n) {
  const steps = [4, 8, 12, 20, 40, 60, 100, 200, 400, 600, 1000];
  return steps.find((x) => x >= n) || Math.ceil(n / 1000) * 1000;
}

function barsHTML() {
  const series = S.overview.series;
  const max = niceMax(Math.max(1, ...series.map((b) => b.requests)));
  const now = Math.floor(Date.now() / 60000);
  const total = series.reduce((a, b) => a + b.requests, 0);
  const n = Math.max(series.length - 1, 1);
  const x = (i) => (i / n) * 100;
  const y = (v) => 100 - (v / max) * 100;
  const line = (key) => series.map((b, i) => `${i ? 'L' : 'M'}${x(i).toFixed(2)},${y(key(b)).toFixed(2)}`).join('');
  const ok = line((b) => b.requests - b.failed);
  const hasFailed = series.some((b) => b.failed);
  const grid = [0, 0.25, 0.5, 0.75, 1].map((f) => `<line x1="0" x2="100" y1="${y(max * f)}" y2="${y(max * f)}"/>`).join('');
  const labels = [0, 0.5, 1].map((f) => `<span class="yl" style="top:${y(max * f)}%">${fmt(Math.round(max * f))}</span>`).join('');
  const pts = series.map((b, i) => {
    if (!b.requests) return '';
    const label = `${new Date(b.minute * 60000).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' })} · ${b.requests} request${b.requests === 1 ? '' : 's'}${b.failed ? `, ${b.failed} failed` : ''} · ${fmt(b.tokens)} tokens`;
    const cls = b.minute === now ? ' now' : '';
    return `<i class="pt${cls}" style="left:${x(i)}%;top:${y(b.requests - b.failed)}%" title="${esc(label)}"></i>${b.failed ? `<i class="pt f" style="left:${x(i)}%;top:${y(b.failed)}%" title="${esc(label)}"></i>` : ''}`;
  }).join('');
  return `<span class="sr-only">${total} requests in the last hour</span>
    <div class="plot">
      <svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
        <defs><linearGradient id="area-fill" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#c5ac98" stop-opacity=".16"/><stop offset="1" stop-color="#c5ac98" stop-opacity="0"/></linearGradient></defs>
        <g class="grid">${grid}</g>
        <path class="area" d="${ok}L100,100L0,100Z"/>
        <path class="ln" d="${ok}"/>
        ${hasFailed ? `<path class="ln f" d="${line((b) => b.failed)}"/>` : ''}
      </svg>
      ${pts}
    </div>
    <div class="ylabels" aria-hidden="true">${labels}</div>`;
}

function acctStatus(a, withScope = true) {
  if (a.disabled) return { cls: 'disabled', html: 'Disabled' };
  const cds = Object.entries(a.cooldowns || {}).sort((x, y) => Date.parse(y[1]) - Date.parse(x[1]));
  if (cds.length) {
    const [model, t] = cds[0];
    const scope = model === '*' || !withScope ? '' : ` <span class="dim">${esc(model)}</span>`;
    return { cls: 'cooling', html: `Cooling <span data-until="${esc(t)}">${until(t)}</span>${scope}`, scope: model === '*' ? 'all models' : model };
  }
  if (a.last_error) return { cls: 'error', html: 'Error' };
  return { cls: 'ready', html: 'Ready' };
}

function acctSub(a) {
  const parts = [a.provider === 'openai-compat' ? (a.group || 'Compatible') : PROVIDER[a.provider]];
  if (a.kind === 'oauth') {
    parts.push('OAuth');
    if (a.expires_at) {
      const left = (Date.parse(a.expires_at) - Date.now()) / 1000;
      parts.push(left > 0 ? `token valid ${span(left)}` : 'token expired');
    }
  } else {
    parts.push('API key');
  }
  return parts.join(' · ');
}

function statusHTML(a, withScope = true) {
  const st = acctStatus(a, withScope);
  return `<span class="status ${st.cls}"><span class="dot"></span><span>${st.html}</span></span>`;
}

function ovAccountsHTML() {
  const list = S.accounts || [];
  const head = `<div class="section-head"><h2>Accounts</h2><a class="link" href="#/accounts">Manage</a></div>`;
  if (!list.length) {
    return `${head}<div class="empty list">
      <h3>No accounts connected</h3>
      <p>Sign in with a subscription or add an API key. From a terminal you can also run <code>subpool login claude</code>.</p>
      <div class="actions">
        <button class="btn" data-act="start-login" data-provider="claude"><span class="pdot" style="background:var(--claude)"></span>Sign in with Claude</button>
        <button class="btn" data-act="start-login" data-provider="codex"><span class="pdot" style="background:var(--codex)"></span>Sign in with ChatGPT</button>
        <button class="btn" data-act="open-panel" data-panel="key">Add API key</button>
      </div></div>`;
  }
  const shown = list.slice(0, 7);
  const rows = shown.map((a) => `
    <div class="row acct-row">
      <div class="acct-name"><span class="dot ${esc(a.provider)}"></span><span class="who"><span class="label">${esc(a.label)}</span><span class="sub">${esc(acctSub(a))}</span></span></div>
      ${statusHTML(a)}
      <span class="num hide-sm"><b>${fmt(a.counters.requests)}</b> req</span>
    </div>`).join('');
  const more = list.length > shown.length ? `<p class="note"><a class="link" href="#/accounts">${list.length - shown.length} more</a></p>` : '';
  return `${head}<div class="list">${rows}</div>${more}`;
}

function snippet(kind) {
  const origin = location.origin;
  const key = S.overview.client_keys[0];
  const token = key || 'subpool';
  const pick = (prefix, fallback) => (S.models.find((m) => m.id.startsWith(prefix)) || {}).id || fallback;
  const any = (S.models[0] || {}).id || 'claude-sonnet-5-5';
  const k = (s) => `<span class="k">${esc(s)}</span>`;
  const v = (s) => `<span class="v">${esc(s)}</span>`;
  switch (kind) {
    case 'codex':
      return {
        text: `# ~/.codex/config.toml\nmodel = "${pick('gpt-', 'gpt-6-astra')}"\nmodel_provider = "subpool"\n\n[model_providers.subpool]\nname = "Subpool"\nbase_url = "${origin}/v1"\nwire_api = "responses"${key ? '\nenv_key = "SUBPOOL_KEY"' : ''}`,
        html: `${k('# ~/.codex/config.toml')}\nmodel = ${v(`"${pick('gpt-', 'gpt-6-astra')}"`)}\nmodel_provider = ${v('"subpool"')}\n\n[model_providers.subpool]\nname = ${v('"Subpool"')}\nbase_url = ${v(`"${origin}/v1"`)}\nwire_api = ${v('"responses"')}${key ? `\nenv_key = ${v('"SUBPOOL_KEY"')}` : ''}`,
        note: `${key ? 'Then export SUBPOOL_KEY with your key. ' : ''}Both HTTP and websocket transports work, and any model your accounts serve can be used.`,
      };
    case 'sdk':
      return {
        text: `from openai import OpenAI\n\nclient = OpenAI(base_url="${origin}/v1", api_key="${token}")\nreply = client.chat.completions.create(\n    model="${any}",\n    messages=[{"role": "user", "content": "Hello"}],\n)`,
        html: `from openai import OpenAI\n\nclient = OpenAI(base_url=${v(`"${origin}/v1"`)}, api_key=${v(`"${token}"`)})\nreply = client.chat.completions.create(\n    model=${v(`"${any}"`)},\n    messages=[{"role": "user", "content": "Hello"}],\n)`,
        note: 'Any model works with any client format; Subpool translates between OpenAI, Anthropic and Gemini.',
      };
    case 'curl':
      return {
        text: `curl ${origin}/v1/chat/completions \\\n  -H "Authorization: Bearer ${token}" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model": "${any}", "messages": [{"role": "user", "content": "Hello"}]}'`,
        html: `curl ${v(`${origin}/v1/chat/completions`)} \\\n  -H ${v(`"Authorization: Bearer ${token}"`)} \\\n  -H "Content-Type: application/json" \\\n  -d '{"model": ${v(`"${any}"`)}, "messages": [{"role": "user", "content": "Hello"}]}'`,
        note: 'Also available: /v1/messages, /v1/responses (HTTP and websocket) and /v1beta/models.',
      };
    default:
      return {
        text: `export ANTHROPIC_BASE_URL=${origin}\nexport ANTHROPIC_AUTH_TOKEN=${token}\nclaude`,
        html: `export ANTHROPIC_BASE_URL=${v(origin)}\nexport ANTHROPIC_AUTH_TOKEN=${v(token)}\nclaude`,
        note: 'Claude Code requests pass through untouched. Set ANTHROPIC_MODEL to use a GPT or Gemini model instead.',
      };
  }
}

function connectHTML() {
  const o = S.overview;
  const key = o.client_keys[0];
  const tabs = [['claude', 'Claude Code'], ['codex', 'Codex'], ['sdk', 'OpenAI SDK'], ['curl', 'curl']];
  const sn = snippet(S.snippet);
  return `
    <div class="section-head"><h2>Connect</h2><span class="meta">${o.models} models</span></div>
    <dl>
      <dt>Base URL</dt><dd class="mono" title="${esc(location.origin)}">${esc(location.origin)}</dd>
      <dd><button class="btn ghost small" data-act="copy" data-text="${esc(location.origin)}" aria-label="Copy base URL">${ICON.copy}</button></dd>
      <dt>API key</dt>${key
        ? `<dd class="mono">${esc(key)}</dd><dd><button class="btn ghost small" data-act="copy" data-text="${esc(key)}" aria-label="Copy API key">${ICON.copy}</button></dd>`
        : `<dd>Not required <span class="dim">· set api-keys to require one</span></dd><dd></dd>`}
    </dl>
    <div class="snip-head">
      <div class="seg" role="group" aria-label="Client">${tabs.map(([id, label]) => `<button data-act="snippet" data-id="${id}" aria-pressed="${S.snippet === id}">${label}</button>`).join('')}</div>
      <button class="btn ghost small" data-act="copy" data-text="${esc(sn.text)}" aria-label="Copy snippet">${ICON.copy}<span>Copy</span></button>
    </div>
    <pre class="code">${sn.html}</pre>
    <p class="note">${esc(sn.note)}</p>`;
}

function routeHTML(r, tags = false) {
  const provider = PROVIDER[r.provider] || (r.provider ? r.provider : '—');
  const extra = tags ? [r.transport === 'ws' ? 'ws' : null, r.attempts > 1 ? `${r.attempts} tries` : null].filter(Boolean) : [];
  return `<span class="route"><span>${esc(CLIENT[r.client] || r.client)}</span><span class="arrow">→</span><span class="dot ${esc(r.provider)}"></span><span>${esc(provider)}</span>${extra.map((t) => `<span class="tag">${t}</span>`).join('')}</span>`;
}

function codeClass(s) {
  if (s === 499) return 'code-499';
  if (s >= 500) return 'code-5xx';
  if (s >= 400) return 'code-4xx';
  return 'code-200';
}

function requestRowHTML(r, lit = false, full = true) {
  const status = r.status === 499 ? 'closed' : r.status;
  const err = r.error && r.status >= 400 && r.status !== 499 ? `<span class="errline" title="${esc(r.error)}">${esc(r.error)}</span>` : '';
  return `<tr class="${lit ? 'lit' : ''}">
    <td class="mono" title="${esc(r.ts)}">${clock(r.ts)}</td>
    <td>${routeHTML(r, full)}</td>
    <td><span class="model mono">${esc(r.model)}</span>${err}</td>
    <td class="hide-sm">${esc(r.account || '—')}</td>
    <td class="mono ${codeClass(r.status)}">${status}</td>
    ${full ? `<td class="r mono hide-sm">${ms(r.ttft_ms)}</td>` : ''}
    <td class="r mono">${ms(r.latency_ms)}</td>
    <td class="r mono">${fmt(r.input_tokens)}</td>
    <td class="r mono">${fmt(r.output_tokens)}</td>
    ${full ? `<td class="r mono hide-sm">${fmt(r.cache_tokens)}</td>` : ''}
  </tr>`;
}

function recentHTML(lit = false) {
  const rows = S.requests.slice(0, 8);
  if (!rows.length) {
    return `<div class="empty"><h3>No requests yet</h3><p>Point a client at the base URL above and requests will show up here as they happen.</p></div>`;
  }
  return `<div class="table-wrap"><table>
    <thead><tr><th>Time</th><th>Route</th><th>Model</th><th class="hide-sm">Account</th><th>Status</th><th class="r">Latency</th><th class="r">In</th><th class="r">Out</th></tr></thead>
    <tbody>${rows.map((r, i) => requestRowHTML(r, lit && i === 0, false)).join('')}</tbody></table></div>`;
}

// accounts --------------------------------------------------------------

function accountsHTML() {
  return `
    <div id="acct-head">${accountHeadHTML()}</div>
    <div id="acct-panel">${panelHTML()}</div>
    <div id="acct-list">${accountListHTML()}</div>`;
}

function accountHeadHTML() {
  const n = (S.accounts || []).length;
  const btn = (p, label, color) => `<button class="btn" data-act="start-login" data-provider="${p}" aria-expanded="${S.panel === p}"><span class="pdot" style="background:var(--${color})"></span>${label}</button>`;
  return `<div class="page-head">
    <div><h1>Accounts</h1><p>${n ? `${n} connected · stored in <span class="mono">${esc(S.overview.auth_dir)}</span> and config.yaml` : 'Nothing connected yet'}</p></div>
    <div class="actions">
      ${btn('claude', 'Sign in with Claude', 'claude')}
      ${btn('codex', 'Sign in with ChatGPT', 'codex')}
      <button class="btn" data-act="open-panel" data-panel="key" aria-expanded="${S.panel === 'key'}">Add API key</button>
    </div></div>`;
}

function panelHTML() {
  if (S.panel === 'key') return keyPanelHTML();
  if (S.panel === 'claude' || S.panel === 'codex') return loginPanelHTML();
  return '';
}

function loginPanelHTML() {
  const L = S.login;
  const isClaude = S.panel === 'claude';
  const name = isClaude ? 'Claude' : 'ChatGPT';
  const port = isClaude ? 54545 : 1455;
  const example = isClaude ? 'http://localhost:54545/callback?code=…&state=…' : 'http://localhost:1455/auth/callback?code=…&state=…';
  const intro = isClaude ? 'Connect a Claude Pro or Max subscription.' : 'Connect a ChatGPT Plus, Pro or Team subscription for Codex models.';
  let status;
  if (!L || L.status === 'starting') status = `<span class="wait"><span class="pulse"></span>Opening the sign-in page…</span>`;
  else if (L.status === 'done') status = `<span class="ok">Connected ${esc(L.message || '')}</span>`;
  else if (L.status === 'error') status = `<span class="err">${esc(L.message || 'Sign-in failed')}</span>`;
  else if (L.callback) status = `<span class="wait"><span class="pulse"></span>Waiting for you to approve in the browser…</span>`;
  else status = `<span class="warn">This server can't receive the redirect (port ${port} is busy). Paste the URL below.</span>`;

  if (L && L.status === 'done') {
    return `<div class="panel" role="region" aria-label="Sign in with ${name}">
      <h3>Signed in</h3><p>${esc(L.message || '')} is ready to serve requests.</p>
      <div class="actions"><button class="btn" data-act="close-panel">Done</button></div></div>`;
  }
  return `<div class="panel" role="region" aria-label="Sign in with ${name}">
    <h3>Sign in with ${name}</h3>
    <p>${intro} Credentials stay on this machine.</p>
    <ol class="steps">
      <li><span class="n">1</span><div class="t"><b>Approve access</b> in the tab that opened.${L && L.url ? ` <a class="link" href="${esc(L.url)}" target="_blank" rel="noopener">Open sign-in page again ${ICON.external.replace('<svg', '<svg style="width:12px;height:12px;vertical-align:-1px"')}</a>` : ''}</div></li>
      <li><span class="n">2</span><div class="t" aria-live="polite">${status}</div></li>
    </ol>
    <div class="divider"></div>
    <form class="field" data-form="paste">
      <label for="paste-url"><span class="dim" style="font-size:12.5px;font-weight:500">Signed in from another device? Paste the address the browser was sent to</span></label>
      <div class="inline">
        <input id="paste-url" class="mono" type="text" name="input" placeholder="${esc(example)}" autocomplete="off" spellcheck="false" ${L && L.state ? '' : 'disabled'}>
        <button class="btn" type="submit" ${L && L.state ? '' : 'disabled'}>Connect</button>
      </div>
      <small>After you approve, that localhost page won't load when the browser runs elsewhere. Copy its full address from the address bar.</small>
      ${L && L.error ? `<p class="msg err" role="alert">${esc(L.error)}</p>` : ''}
    </form>
    <div class="actions" style="margin-top:18px"><button class="btn ghost" data-act="close-panel">Cancel</button></div>
  </div>`;
}

function keyPanelHTML() {
  const p = S.keyProvider;
  const opts = [['claude', 'Claude'], ['codex', 'OpenAI'], ['gemini', 'Gemini'], ['compat', 'OpenAI-compatible']];
  const base = { claude: 'https://api.anthropic.com', codex: 'https://api.openai.com/v1', gemini: 'https://generativelanguage.googleapis.com', compat: 'https://openrouter.ai/api/v1' }[p];
  const compat = p === 'compat';
  return `<form class="panel" data-form="key" aria-label="Add an API key">
    <h3>Add an API key</h3>
    <p>Keys are saved to <span class="mono">config.yaml</span> and used alongside your signed-in accounts.</p>
    <div class="seg" role="group" aria-label="Provider">${opts.map(([id, label]) => `<button type="button" data-act="key-provider" data-id="${id}" aria-pressed="${p === id}">${label}</button>`).join('')}</div>
    <div class="grid">
      <label class="field wide"><span>API key${compat ? ' (optional for local servers)' : ''}</span><input class="mono" type="password" name="api_key" autocomplete="off" spellcheck="false" ${compat ? '' : 'required'}></label>
      <label class="field ${compat ? '' : 'wide'}"><span>Base URL${compat ? '' : ' (optional)'}</span><input class="mono" type="url" name="base_url" placeholder="${esc(base)}" ${compat ? 'required' : ''}></label>
      ${compat ? `<label class="field"><span>Name</span><input type="text" name="name" placeholder="openrouter"></label>
      <label class="field wide"><span>Models</span><input class="mono" type="text" name="models" placeholder="moonshotai/kimi-k3, kimi=moonshotai/kimi-k3" required><small>Comma separated. Write alias=upstream-name to expose a model under a shorter name.</small></label>` : ''}
    </div>
    <div class="actions"><button class="btn primary" type="submit">Add key</button><button class="btn ghost" type="button" data-act="close-panel">Cancel</button></div>
    <p class="msg" id="key-msg" aria-live="polite"></p>
  </form>`;
}

function accountListHTML() {
  const list = S.accounts || [];
  if (!list.length) {
    return `<div class="empty" style="border-top:1px solid var(--line)"><h3>No accounts yet</h3>
      <p>Sign in with Claude or ChatGPT above, add an API key, or run <code>subpool login claude</code> on the server. Point <code>auth-dir</code> at <code>~/.cli-proxy-api</code> to reuse existing CLIProxyAPI logins.</p></div>`;
  }
  const head = `<div class="row" style="min-height:36px;color:var(--fg-3);font-size:12px;font-weight:500"><span>Account</span><span>Status</span><span class="hide-md">Usage</span><span class="hide-md">Last used</span><span></span></div>`;
  const rows = list.map((a) => {
    const confirming = S.confirm === a.id;
    const cooling = Object.keys(a.cooldowns || {}).length > 0;
    const actions = confirming
      ? `<button class="btn small danger" data-act="delete" data-id="${esc(a.id)}" aria-label="Confirm removing ${esc(a.label)}">Remove</button><button class="btn ghost small" data-act="cancel-delete">Keep</button>`
      : `${a.kind === 'oauth' ? `<button class="btn ghost small" data-act="refresh" data-id="${esc(a.id)}" aria-label="Refresh token for ${esc(a.label)}" title="Refresh token">${ICON.refresh}</button>` : ''}
         <button class="switch" role="switch" aria-checked="${!a.disabled}" aria-label="${a.disabled ? 'Enable' : 'Disable'} ${esc(a.label)}" title="${a.disabled ? 'Disabled' : 'Enabled'}" data-act="toggle" data-id="${esc(a.id)}"></button>
         <button class="btn ghost small" data-act="confirm-delete" data-id="${esc(a.id)}" aria-label="Remove ${esc(a.label)}" title="Remove">${ICON.trash}</button>`;
    const c = a.counters;
    return `<div class="row">
      <div class="acct-name"><span class="dot ${esc(a.provider)}"></span><span class="who"><span class="label">${esc(a.label)}</span><span class="sub">${esc(acctSub(a))}</span></span></div>
      <div class="stack">${statusHTML(a, false)}${cooling ? `<span class="sub">${esc(acctStatus(a).scope)} · <button class="linkbtn" data-act="reset" data-id="${esc(a.id)}" title="Make this account available again now">Clear</button></span>` : ''}</div>
      <div class="stack hide-md"><span class="main"><b>${fmt(c.requests)}</b> requests</span><span class="sub">${fmt(c.input_tokens)} in · ${fmt(c.output_tokens)} out${c.failures ? ` · <span class="err">${fmt(c.failures)} failed</span>` : ''}</span></div>
      <span class="num hide-md" style="text-align:left" data-ago="${esc(a.last_used || '')}">${ago(a.last_used)}</span>
      <div class="row-actions">${actions}</div>
      ${a.last_error ? `<div class="acct-err">${esc(a.last_error)}</div>` : ''}
    </div>`;
  }).join('');
  return `<div class="acct-table list">${head}${rows}</div>`;
}

// requests --------------------------------------------------------------

function matches(r) {
  const q = S.filter.trim().toLowerCase();
  if (!q) return true;
  return [r.model, r.account, r.provider, r.client, String(r.status), r.error || ''].some((s) => String(s).toLowerCase().includes(q));
}

function reqCountHTML() {
  const n = S.requests.filter(matches).length;
  return `${fmt(n)} shown`;
}

function requestsHTML() {
  const rows = S.requests.filter(matches);
  return `
    <div class="page-head">
      <div><h1>Requests</h1><p>The last 300 requests since the server started.</p></div>
      <div class="toolbar">
        <label class="sr-only" for="req-filter">Filter requests</label>
        <input id="req-filter" type="text" placeholder="Filter by model, account, status…" value="${esc(S.filter)}" autocomplete="off" spellcheck="false">
        <button class="btn" data-act="pause" aria-pressed="${S.paused}">${S.paused ? 'Resume' : 'Pause'}</button>
      </div>
    </div>
    <p class="note" style="margin:-8px 0 12px" id="req-count">${reqCountHTML()}</p>
    <div class="table-wrap"><table>
      <thead><tr><th>Time</th><th>Route</th><th>Model</th><th class="hide-sm">Account</th><th>Status</th><th class="r hide-sm">First token</th><th class="r">Total</th><th class="r">In</th><th class="r">Out</th><th class="r hide-sm">Cached</th></tr></thead>
      <tbody id="req-body">${rows.map((r) => requestRowHTML(r)).join('')}</tbody>
    </table></div>
    ${rows.length ? '' : `<div class="empty" id="req-empty"><h3>${S.filter ? 'Nothing matches that filter' : 'No requests yet'}</h3><p>${S.filter ? 'Try a model name, an account or a status code.' : 'Requests appear here the moment a client sends one.'}</p></div>`}`;
}

function bindRequests() {
  const input = $('#req-filter');
  input?.addEventListener('input', () => {
    S.filter = input.value;
    const rows = S.requests.filter(matches);
    $('#req-body').innerHTML = rows.map((r) => requestRowHTML(r)).join('');
    patch('req-count', reqCountHTML);
  });
}

// config ----------------------------------------------------------------

function configHTML() {
  const c = S.config;
  if (c.text == null) {
    loadConfig();
    return skeletonHTML();
  }
  const dirty = c.text !== c.saved;
  return `
    <div class="page-head"><div><h1>Configuration</h1><p class="mono">${esc(c.path)}</p></div></div>
    <label class="sr-only" for="cfg">config.yaml</label>
    <textarea id="cfg" class="editor" spellcheck="false" autocapitalize="off" autocomplete="off">${esc(c.text)}</textarea>
    <div class="editor-foot">
      <button class="btn primary" data-act="save-config" ${dirty && !c.busy ? '' : 'disabled'}>${c.busy ? 'Saving…' : 'Save changes'}</button>
      <button class="btn ghost" data-act="revert-config" ${dirty ? '' : 'disabled'}>Revert</button>
      <p class="msg ${c.msg ? c.msg.kind : ''}" id="cfg-msg" aria-live="polite">${c.msg ? esc(c.msg.text) : '<span class="dim">Saved changes apply immediately. Changing host or port needs a restart.</span>'}</p>
    </div>`;
}

async function loadConfig() {
  try {
    const r = await api('/config');
    Object.assign(S.config, { text: r.text, saved: r.text, path: r.path, msg: null });
    if (S.route === 'config') render();
  } catch {}
}

function bindConfig() {
  const ta = $('#cfg');
  if (!ta) return;
  const sync = () => {
    S.config.text = ta.value;
    const dirty = S.config.text !== S.config.saved;
    $('[data-act="save-config"]').disabled = !dirty || S.config.busy;
    $('[data-act="revert-config"]').disabled = !dirty;
  };
  ta.addEventListener('input', sync);
  ta.addEventListener('keydown', (e) => {
    if (e.key === 'Tab' && !e.shiftKey) {
      e.preventDefault();
      const { selectionStart: s, selectionEnd: en } = ta;
      ta.setRangeText('  ', s, en, 'end');
      sync();
    }
    if ((e.metaKey || e.ctrlKey) && e.key === 's') {
      e.preventDefault();
      saveConfig();
    }
  });
}

async function saveConfig() {
  const c = S.config;
  if (c.busy || c.text === c.saved) return;
  c.busy = true;
  const ta = $('#cfg');
  const pos = ta ? [ta.selectionStart, ta.scrollTop] : null;
  render();
  try {
    const r = await api('/config', { method: 'PUT', body: JSON.stringify({ text: c.text }) });
    c.saved = c.text;
    c.msg = { kind: 'ok', text: r.restart_required ? 'Saved. Restart Subpool to apply the new host or port.' : 'Saved and applied.' };
    refreshAccounts();
  } catch (e) {
    c.msg = { kind: 'err', text: e.message };
  }
  c.busy = false;
  render();
  const ta2 = $('#cfg');
  if (ta2 && pos) { ta2.focus(); ta2.selectionStart = ta2.selectionEnd = pos[0]; ta2.scrollTop = pos[1]; }
}

// lock ------------------------------------------------------------------

function lockHTML() {
  if (S.locked === 'remote') {
    return `<div class="lock"><h1>Dashboard is local-only</h1>
      <p>Without a management key the dashboard only answers on localhost. Set <span class="mono">management-key</span> in config.yaml on the server, then reload this page.</p></div>`;
  }
  return `<div class="lock"><h1>Dashboard locked</h1>
    <p>Enter the <span class="mono">management-key</span> from config.yaml.</p>
    <form data-form="unlock"><label class="sr-only" for="mk">Management key</label>
      <input id="mk" class="mono" type="password" name="key" autocomplete="current-password" required autofocus>
      <button class="btn primary" type="submit">Unlock</button>
      <p class="msg err" id="lock-msg" aria-live="polite"></p></form></div>`;
}

function bindLock() {
  $('#mk')?.focus();
}

// ---------------------------------------------------------------- actions

async function startLogin(provider) {
  S.panel = provider;
  S.login = { provider, status: 'starting' };
  if (S.route !== 'accounts') location.hash = '#/accounts';
  else { patch('acct-panel', panelHTML); patch('acct-head', accountHeadHTML); }
  // Open the tab synchronously so popup blockers allow it.
  const tab = window.open('about:blank', '_blank');
  try {
    const r = await api(`/login/${provider}`, { method: 'POST' });
    S.login = { provider, state: r.state, url: r.url, callback: r.callback, status: 'pending' };
    if (tab) { tab.opener = null; tab.location.href = r.url; }
  } catch (e) {
    tab?.close();
    S.login = { provider, status: 'error', message: e.message };
  }
  patch('acct-panel', panelHTML);
  schedulePoll();
}

let pollTimer = 0;
function schedulePoll() {
  clearTimeout(pollTimer);
  if (S.login && S.login.status === 'pending') pollTimer = setTimeout(pollLogin, 1500);
}

async function pollLogin() {
  const L = S.login;
  if (!L || !L.state || L.status !== 'pending') return;
  try {
    const r = await api(`/login/${encodeURIComponent(L.state)}`);
    if (r.status !== L.status) {
      Object.assign(L, { status: r.status, message: r.message });
      const input = $('#paste-url');
      const keep = input ? input.value : '';
      patch('acct-panel', panelHTML);
      if ($('#paste-url') && keep) $('#paste-url').value = keep;
      if (r.status === 'done') refreshAccounts();
    }
  } catch {}
  schedulePoll();
}

async function submitPaste(form) {
  const L = S.login;
  const input = form.elements.input.value.trim();
  if (!input || !L) return;
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  btn.textContent = 'Connecting…';
  try {
    const r = await api(`/login/${encodeURIComponent(L.state)}/code`, { method: 'POST', body: JSON.stringify({ input }) });
    Object.assign(L, { status: 'done', message: r.label });
    refreshAccounts();
  } catch (e) {
    L.error = e.message;
  }
  patch('acct-panel', panelHTML);
  schedulePoll();
}

async function submitKey(form) {
  const data = Object.fromEntries(new FormData(form).entries());
  data.provider = S.keyProvider;
  const msg = $('#key-msg');
  const btn = form.querySelector('button[type="submit"]');
  btn.disabled = true;
  try {
    await api('/keys', { method: 'POST', body: JSON.stringify(data) });
    S.panel = null;
    patch('acct-panel', panelHTML);
    patch('acct-head', accountHeadHTML);
    refreshAccounts();
  } catch (e) {
    msg.className = 'msg err';
    msg.textContent = e.message;
    btn.disabled = false;
  }
}

async function accountAction(act, id) {
  const a = (S.accounts || []).find((x) => x.id === id);
  try {
    if (act === 'toggle') {
      a.disabled = !a.disabled;
      patch('acct-list', accountListHTML);
      await api(`/accounts/${encodeURIComponent(id)}/toggle`, { method: 'POST', body: JSON.stringify({ disabled: a.disabled }) });
    } else if (act === 'refresh') {
      const btn = document.querySelector(`[data-act="refresh"][data-id="${CSS.escape(id)}"]`);
      if (btn) { btn.disabled = true; btn.style.opacity = 1; btn.firstElementChild.style.animation = 'pulse 1s infinite'; }
      await api(`/accounts/${encodeURIComponent(id)}/refresh`, { method: 'POST' });
    } else if (act === 'reset') {
      await api(`/accounts/${encodeURIComponent(id)}/reset`, { method: 'POST' });
    } else if (act === 'delete') {
      S.confirm = null;
      await api(`/accounts/${encodeURIComponent(id)}`, { method: 'DELETE' });
    }
  } catch (e) {
    if (a) a.last_error = e.message;
  }
  refreshAccounts();
}

async function copy(btn) {
  const text = btn.dataset.text;
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement('textarea');
    ta.value = text;
    document.body.append(ta);
    ta.select();
    document.execCommand('copy');
    ta.remove();
  }
  const prev = btn.innerHTML;
  btn.innerHTML = btn.querySelector('span') ? `${ICON.check}<span>Copied</span>` : ICON.check;
  btn.classList.add('copied');
  setTimeout(() => { btn.innerHTML = prev; btn.classList.remove('copied'); }, 1400);
}

document.addEventListener('click', (e) => {
  const el = e.target.closest('[data-act]');
  if (!el) return;
  const { act, id } = el.dataset;
  switch (act) {
    case 'copy': return copy(el);
    case 'snippet':
      S.snippet = id;
      localStorage.setItem('subpool.snippet', id);
      return patch('connect', connectHTML);
    case 'start-login':
      if (S.panel === el.dataset.provider && S.login && S.login.status === 'pending') return;
      return startLogin(el.dataset.provider);
    case 'open-panel':
      S.panel = el.dataset.panel;
      if (S.route !== 'accounts') { location.hash = '#/accounts'; return; }
      patch('acct-panel', panelHTML);
      patch('acct-head', accountHeadHTML);
      return $('#acct-panel input')?.focus();
    case 'close-panel':
      S.panel = null;
      S.login = null;
      clearTimeout(pollTimer);
      patch('acct-panel', panelHTML);
      return patch('acct-head', accountHeadHTML);
    case 'key-provider':
      S.keyProvider = id;
      patch('acct-panel', panelHTML);
      return $(`[data-act="key-provider"][data-id="${id}"]`)?.focus();
    case 'confirm-delete':
      S.confirm = id;
      patch('acct-list', accountListHTML);
      return $('[data-act="cancel-delete"]')?.focus();
    case 'cancel-delete':
      S.confirm = null;
      return patch('acct-list', accountListHTML);
    case 'toggle': case 'refresh': case 'reset': case 'delete':
      return accountAction(act, id);
    case 'pause':
      S.paused = !S.paused;
      return render();
    case 'save-config': return saveConfig();
    case 'revert-config':
      S.config.text = S.config.saved;
      S.config.msg = null;
      return render();
  }
});

document.addEventListener('submit', async (e) => {
  const form = e.target.closest('form[data-form]');
  if (!form) return;
  e.preventDefault();
  const kind = form.dataset.form;
  if (kind === 'paste') return submitPaste(form);
  if (kind === 'key') return submitKey(form);
  if (kind === 'unlock') {
    S.key = form.elements.key.value.trim();
    try {
      await api('/overview');
      localStorage.setItem('subpool.key', S.key);
      S.locked = null;
      await boot();
    } catch {
      const m = $('#lock-msg');
      if (m) m.textContent = 'That key was not accepted.';
    }
  }
});

// ---------------------------------------------------------------- routing + timers

function onRoute() {
  const r = (location.hash.replace(/^#\/?/, '') || 'overview').split('/')[0];
  S.route = ['overview', 'accounts', 'requests', 'config'].includes(r) ? r : 'overview';
  if (S.route !== 'config') S.config.msg = null;
  render();
  if (S.route === 'accounts' && S.panel === 'key') $('#acct-panel input')?.focus();
  view.focus({ preventScroll: true });
  window.scrollTo(0, 0);
}

setInterval(() => {
  let expired = false;
  for (const el of document.querySelectorAll('[data-until]')) {
    if (Date.parse(el.dataset.until) <= Date.now()) expired = true;
    el.textContent = until(el.dataset.until);
  }
  for (const el of document.querySelectorAll('[data-ago]')) el.textContent = ago(el.dataset.ago);
  const up = $('[data-uptime]');
  if (up && S.overview) up.textContent = span((Date.now() - Date.parse(S.overview.started_at)) / 1000);
  if (expired) refreshAccounts();
}, 1000);

// Resync the hour of traffic once a minute (rolls the window forward).
setInterval(async () => {
  if (S.locked || !S.overview) return;
  try {
    S.overview = await api('/overview');
    if (S.route === 'overview') { patch('figures', figuresHTML); patch('bars', barsHTML); }
  } catch {}
}, 60000);

window.addEventListener('beforeunload', (e) => {
  if (S.config.text != null && S.config.text !== S.config.saved) e.preventDefault();
});

async function boot() {
  render();
  try {
    await loadAll();
  } catch (e) {
    if (!(e instanceof ApiError && (e.status === 401 || e.status === 403))) {
      view.innerHTML = `<div class="lock"><h1>Can't reach Subpool</h1><p>${esc(e.message)}. Check that the server is running, then reload.</p></div>`;
    }
    return;
  }
  render();
  if (!ws || ws.readyState > 1) connectLive();
}

window.addEventListener('hashchange', onRoute);
const initial = (location.hash.replace(/^#\/?/, '') || 'overview').split('/')[0];
S.route = ['overview', 'accounts', 'requests', 'config'].includes(initial) ? initial : 'overview';
boot();
