'use strict';

const $ = (id) => document.getElementById(id);

// contextBridge installs `hub` as a non-configurable global, so a top-level
// `const hub = window.hub` collides with it and kills this whole script.
const api = window.hub;

let instances = [];
let lastTunnelSig = '';
let regionReady = false;

const REGIONS = [
  'eu-west-1', 'eu-west-2', 'eu-west-3', 'eu-central-1', 'eu-north-1',
  'us-east-1', 'us-east-2', 'us-west-1', 'us-west-2',
  'ap-southeast-1', 'ap-southeast-2', 'ap-south-1', 'ap-northeast-1',
  'ca-central-1', 'sa-east-1',
];

// Keeps the current region selectable even when it isn't in the list above.
function syncRegion(current) {
  const sel = $('region');
  const names = REGIONS.includes(current) ? REGIONS : [current, ...REGIONS];
  if (!regionReady || sel.dataset.names !== names.join()) {
    sel.innerHTML = '';
    for (const r of names) sel.appendChild(new Option(r, r));
    sel.dataset.names = names.join();
    regionReady = true;
  }
  if (sel.value !== current) sel.value = current;
}

$('region').addEventListener('change', async () => {
  const res = await api.setRegion($('region').value);
  if (!res.ok) {
    $('instanceNote').textContent = res.error;
    return;
  }
  instances = [];
  renderInstances();
  refreshInstances();
});

// ---------- status messages ----------

const ICON = { busy: '⋯', ok: '✓', err: '✕', warn: '!' };

// Every click has to say something: what is happening, or what went wrong and
// what to do about it. Silence is what made this confusing.
function status(id, kind, text) {
  const el = $(id);
  if (!kind) {
    el.classList.add('hidden');
    el.textContent = '';
    return;
  }
  el.className = `status status-${kind}`;
  el.innerHTML = '';
  const icon = document.createElement('span');
  icon.className = 'status-icon';
  icon.textContent = ICON[kind] || '';
  const msg = document.createElement('span');
  msg.textContent = text;
  el.append(icon, msg);
}

// ---------- profiles from ~/.aws ----------

let profiles = [];

async function loadProfiles() {
  const note = $('profileNote');
  const sel = $('profile');
  const res = await api.listProfiles();

  if (!res.ok) {
    profiles = [];
    sel.innerHTML = '';
    sel.appendChild(new Option('Unavailable', ''));
    sel.disabled = true;
    $('profileBtn').disabled = true;
    note.textContent = res.error;
    return;
  }

  profiles = res.data.profiles;
  sel.innerHTML = '';

  if (!profiles.length) {
    sel.appendChild(new Option('No profiles found', ''));
    sel.disabled = true;
    $('profileBtn').disabled = true;
    note.textContent = `No profiles in ${res.data.configPath}. Paste credentials below instead.`;
    return;
  }

  for (const p of profiles) {
    const bits = [p.region, p.sourceLabel].filter(Boolean).join(' · ');
    sel.appendChild(new Option(`${p.name} — ${bits}`, p.name));
  }
  sel.disabled = false;

  // Reselect whatever was used last, so the common case is one click.
  const last = res.data.lastUsed;
  if (last && profiles.some((p) => p.name === last)) sel.value = last;
  describeProfile();
}

// Warn about an unusable profile before the user clicks, not after it fails.
function describeProfile() {
  const p = profiles.find((x) => x.name === $('profile').value);
  const note = $('profileNote');
  // Don't offer a button that is certain to fail.
  $('profileBtn').disabled = !p || !p.usable;
  if (!p) {
    note.textContent = '';
    return;
  }
  if (!p.usable) {
    note.textContent = `"${p.name}" has no credentials configured — it only sets a region. Paste credentials below instead.`;
    note.style.color = 'var(--warn)';
    return;
  }
  note.style.color = '';
  note.textContent =
    p.source === 'sso'
      ? 'SSO profile — the AWS CLI refreshes this for you, and reports a real expiry.'
      : p.source === 'role'
        ? 'Assumed-role profile — the CLI assumes the role for each call.'
        : '';
}

$('profile').addEventListener('change', describeProfile);
$('profileRefresh').addEventListener('click', loadProfiles);

$('profileBtn').addEventListener('click', async () => {
  const name = $('profile').value;
  if (!name) return;

  $('profileBtn').disabled = true;
  $('profileBtn').textContent = 'Checking…';
  status('credsStatus', 'busy', `Verifying profile "${name}" with AWS…`);

  const res = await api.useProfile(name);

  $('profileBtn').disabled = false;
  $('profileBtn').textContent = 'Use profile';

  if (!res.ok) {
    status('credsStatus', 'err', res.error);
    return;
  }
  const c = res.data;
  status('credsStatus', 'ok', `Using profile "${name}" — ${c.username} in account ${c.account}.`);
  refreshInstances();
});

// ---------- credentials ----------

$('loadBtn').addEventListener('click', async () => {
  const text = $('credsInput').value;

  if (!text.trim()) {
    status('credsStatus', 'warn', 'Nothing pasted yet. Paste your credentials into the box above first.');
    return;
  }

  $('loadBtn').disabled = true;
  $('loadBtn').textContent = 'Verifying…';
  status('credsStatus', 'busy', 'Checking these credentials with AWS…');

  const res = await api.loadCreds(text);

  $('loadBtn').disabled = false;
  $('loadBtn').textContent = 'Load & verify';

  if (!res.ok) {
    status('credsStatus', 'err', res.error);
    return;
  }

  // Consumed — wipe the pasted secret out of the DOM immediately.
  $('credsInput').value = '';
  const c = res.data;
  status('credsStatus', 'ok', `Verified as ${c.username} in account ${c.account}.`);
  refreshInstances();
});

$('clearBtn').addEventListener('click', async () => {
  await api.clearCreds();
  instances = [];
  renderInstances();
  status('credsStatus', null);
  status('startStatus', null);
});

function fmtDuration(s) {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return h > 0 ? `${h}h ${m}m` : m > 0 ? `${m}m ${sec}s` : `${sec}s`;
}

// AWS exposes no way to look up a token's expiry, so this is deliberately explicit
// about which of the three situations we are in rather than showing a fake clock.
function fmtExpiry(c) {
  if (c.expirySource === 'observed') {
    return { text: 'expired (AWS rejected it)', level: 'err' };
  }
  if (c.expirySource === 'unknown') {
    const age = c.loadedAt ? fmtDuration(Math.round((Date.now() - c.loadedAt) / 1000)) : null;
    // In profile mode the CLI was asked directly and had nothing to report, which
    // usually means long-lived keys rather than a missing field.
    // Always say when AWS last confirmed them: with no expiry to show, that is the
    // only real evidence the session is still good.
    const checked = c.verifiedAgoSeconds !== null ? ` · checked ${fmtDuration(c.verifiedAgoSeconds)} ago` : '';
    if (c.mode === 'profile') {
      return {
        text: `not reported by the CLI${age ? ` · in use ${age}` : ''}${checked}`,
        level: 'warn',
      };
    }
    return {
      text: `not stated by AWS${age ? ` · loaded ${age} ago` : ''}${checked}`,
      level: 'warn',
      hint: true,
    };
  }
  const s = c.expiresInSeconds;
  if (s <= 0) return { text: 'expired', level: 'err' };
  const at = new Date(c.expiration).toLocaleTimeString();
  return { text: `${fmtDuration(s)} left · ${at}`, level: s < 300 ? 'warn' : 'ok' };
}

// "Online" is a claim about right now, so it requires a recent successful remote
// check — not merely the absence of bad news. `c.online` is computed in the main
// process so this and the tray always agree.
function credState(c) {
  if (c.expired) {
    return { text: 'Expired', level: 'err', title: 'AWS has rejected these credentials.' };
  }
  if (!c.online) {
    const ago = c.verifiedAgoSeconds;
    return {
      text: 'Offline',
      level: 'warn',
      title:
        (c.offlineReason || 'AWS cannot be reached.') +
        (ago !== null ? ` AWS last answered ${fmtDuration(ago)} ago.` : ''),
    };
  }
  const ago = c.verifiedAgoSeconds;
  const soon = c.expiresInSeconds !== null && c.expiresInSeconds < 300;
  return {
    text: soon ? 'Expiring' : 'Online',
    level: soon ? 'warn' : 'ok',
    title: `AWS answered and accepted these credentials ${fmtDuration(ago)} ago.`,
  };
}

function renderCreds(c) {
  const badge = $('credsBadge');
  if (!c.loaded) {
    $('credsForm').classList.remove('hidden');
    $('credsLoaded').classList.add('hidden');
    badge.textContent = 'Not loaded';
    badge.className = 'badge badge-idle';
    $('identity').textContent = '';
    // Show the remembered region, but it can only be changed once credentials are
    // loaded, since changing it re-queries AWS.
    if (c.region) syncRegion(c.region);
    $('region').disabled = true;
    $('region').title = 'Load credentials to change region';
    return;
  }

  $('region').disabled = false;
  $('region').title = 'Region — remembered for next launch';
  syncRegion(c.region);

  $('credsForm').classList.add('hidden');
  $('credsLoaded').classList.remove('hidden');

  const exp = fmtExpiry(c);
  const state = credState(c);
  badge.textContent = state.text;
  badge.className = `badge badge-${state.level}`;
  badge.title = state.title;

  // Profile mode has no key to show — the CLI holds the credentials, not this app.
  if (c.mode === 'profile') {
    $('fKeyLabel').textContent = 'Profile';
    $('fKey').textContent = c.profile;
    $('fKey').title = 'Credentials are resolved by the AWS CLI from ~/.aws';
  } else {
    $('fKeyLabel').textContent = 'Access key';
    $('fKey').textContent = c.accessKeyIdRedacted;
    $('fKey').title = '';
  }
  $('fAccount').textContent = c.account || '—';
  $('fUser').textContent = c.username || '—';
  $('fRegion').textContent = c.regionSupplied ? c.region : `${c.region} (default — not in paste)`;
  $('fRegion').style.color = c.regionSupplied ? '' : 'var(--warn)';
  $('fExpiry').textContent = exp.text;
  $('fExpiry').style.color = exp.level === 'ok' ? '' : `var(--${exp.level})`;
  $('fExpiry').title = exp.hint
    ? 'No AWS API reports a token’s expiry. To see a real countdown, paste output that ' +
      'includes it: `aws configure export-credentials --format env`, or the JSON from ' +
      '`aws sts assume-role`.'
    : '';
  $('fArn').textContent = c.arn || '—';

  renderAssume(c);

  // While a role is held it is the identity every tunnel runs as, so it is what the
  // header should name — the base user is no longer who AWS sees.
  $('identity').textContent = c.assumed
    ? `${c.assumed.roleName} @ ${c.account || '—'} · ${c.region}`
    : `${c.username || '—'} @ ${c.account || '—'} · ${c.region}`;
  $('startBtn').disabled = c.expired || !$('instance').value;
}

// The assume-role row. Deliberately inert about eligibility: the field validates the
// ARN's shape only, because no AWS call can say in advance which roles you may assume.
function renderAssume(c) {
  const holding = !!c.assumed;
  $('revertBtn').classList.toggle('hidden', !holding);
  $('assumeBtn').textContent = holding ? 'Switch role' : 'Assume';

  const note = $('assumeNote');
  if (holding) {
    note.textContent = `Acting as ${c.assumed.roleName} (session ${c.assumed.sessionName}). ` +
      'Renewed automatically before it expires.';
    note.className = 'hint ok-text';
  } else {
    note.textContent = 'Optional. The role must trust this identity — that is only confirmed by assuming it.';
    note.className = 'hint';
  }

  if (!roleFieldTouched && holding) $('roleArn').value = c.assumed.roleArn;
}

// Set once the user edits the field, so a 1s broadcast repaint cannot overwrite what
// they are halfway through typing.
let roleFieldTouched = false;

async function loadRecentRoles() {
  const res = await api.recentRoles();
  if (!res.ok) return;
  const list = $('roleRecent');
  list.innerHTML = '';
  for (const arn of res.data.roles || []) {
    const opt = document.createElement('option');
    opt.value = arn;
    list.appendChild(opt);
  }
}

async function doAssume() {
  const btn = $('assumeBtn');
  const arn = $('roleArn').value.trim();
  if (!arn) {
    status('assumeStatus', 'err', 'Enter the ARN of the role to assume.');
    return;
  }
  btn.disabled = true;
  status('assumeStatus', 'busy', 'Assuming role…');
  const res = await api.assumeRole({
    roleArn: arn,
    mfaSerial: $('mfaSerial').value.trim() || undefined,
    tokenCode: $('mfaCode').value.trim() || undefined,
  });
  btn.disabled = false;
  if (!res.ok) {
    status('assumeStatus', 'err', res.error);
    // Surface the MFA inputs exactly when AWS says they are needed, rather than
    // showing two more fields to everyone who will never use them.
    if (/MFA/i.test(res.error)) $('mfaRow').classList.remove('hidden');
    return;
  }
  $('mfaCode').value = '';
  roleFieldTouched = false;
  status('assumeStatus', 'ok', `Now acting as ${res.data.assumed.roleName}.`);
  loadRecentRoles();
}

async function doRevert() {
  const btn = $('revertBtn');
  btn.disabled = true;
  status('assumeStatus', 'busy', 'Returning to base credentials…');
  const res = await api.revertRole();
  btn.disabled = false;
  if (!res.ok) {
    status('assumeStatus', 'err', res.error);
    return;
  }
  roleFieldTouched = false;
  status('assumeStatus', 'ok', 'Back to your base credentials.');
}

// ---------- instances ----------

$('refreshBtn').addEventListener('click', refreshInstances);

async function refreshInstances() {
  const sel = $('instance');
  const note = $('instanceNote');
  const state = await api.getState();
  if (!state.ok || !state.data.creds.loaded) {
    instances = [];
    renderInstances();
    return;
  }

  const region = state.data.creds.region;
  note.textContent = `Looking for instances in ${region}…`;
  note.style.color = '';
  sel.disabled = true;

  const res = await api.listInstances();
  if (!res.ok) {
    instances = [];
    note.textContent = res.error;
    note.style.color = 'var(--err)';
    renderInstances();
    return;
  }

  instances = res.data;
  if (instances.length) {
    note.textContent = `${instances.length} instance${instances.length === 1 ? '' : 's'} online in ${region}`;
  } else {
    // Almost always the wrong region rather than a genuinely empty account.
    note.textContent = `No SSM-managed instances online in ${region}. Try another region.`;
    note.style.color = 'var(--warn)';
  }
  renderInstances();
}

function renderInstances() {
  const sel = $('instance');
  const current = sel.value;
  sel.innerHTML = '';

  if (instances.length === 0) {
    sel.appendChild(new Option('No instances', ''));
    sel.disabled = true;
    $('startBtn').disabled = true;
    return;
  }

  sel.appendChild(new Option('Select an instance…', ''));
  for (const i of instances) {
    const bits = [i.id, i.privateIp, i.platform].filter(Boolean).join(' · ');
    sel.appendChild(new Option(`${i.name} — ${bits}`, i.id));
  }
  sel.disabled = false;
  if (current && instances.some((i) => i.id === current)) sel.value = current;
  $('startBtn').disabled = !sel.value;
}

$('instance').addEventListener('change', () => {
  const inst = instances.find((i) => i.id === $('instance').value);
  $('startBtn').disabled = !inst;
  // Windows hosts almost always mean RDP; Linux means SSH. Nudge, don't force.
  if (inst && !$('localPort').value) {
    const guess = inst.windows ? '3389' : '22';
    $('preset').value = guess;
    $('remotePort').value = guess;
  }
});

// ---------- ports ----------

$('preset').addEventListener('change', () => {
  const v = $('preset').value;
  if (v === 'custom') {
    $('remotePort').value = '';
    $('remotePort').focus();
  } else {
    $('remotePort').value = v;
  }
});

$('remotePort').addEventListener('input', () => {
  const v = $('remotePort').value;
  const opt = [...$('preset').options].find((o) => o.value === v);
  $('preset').value = opt ? v : 'custom';
});

// A port already listed in Tunnels belongs to that card. Say so as it is typed, rather
// than letting the user click Start and be refused.
function checkLocalPortClaim() {
  const raw = $('localPort').value.trim();
  const note = $('localPortNote');
  const port = Number(raw);

  // Only ever *disables* the button. Whether it should be enabled depends on
  // credentials and instance selection, which renderCreds already decides — and it
  // runs before this on every broadcast, so a claim still wins.
  if (!raw || !Number.isInteger(port)) {
    note.textContent = '';
    return;
  }

  const held = lastTunnelList.find((t) => t.localPort === port);
  if (!held) {
    note.textContent = '';
    return;
  }

  const live = held.status === 'open' || held.status === 'starting' || held.status === 'reconnecting';
  note.textContent = live
    ? `In use by ${held.instanceName} below — stop it, or pick another port.`
    : `Reserved by ${held.instanceName} below — reconnect or dismiss it to free this port.`;
  $('startBtn').disabled = true;
}

$('localPort').addEventListener('input', checkLocalPortClaim);

// ---------- tunnels ----------

function clearStartConfirm() {
  $('startConfirm').innerHTML = '';
}

async function startTunnel({ allowForeignKill = false } = {}) {
  const inst = instances.find((i) => i.id === $('instance').value);
  if (!inst) {
    status('startStatus', 'warn', 'Select an instance first.');
    return;
  }
  if (!$('remotePort').value.trim()) {
    status('startStatus', 'warn', 'Enter a remote port, or pick one of the presets.');
    return;
  }

  $('startBtn').disabled = true;
  status('startStatus', 'busy', `Opening a tunnel to ${inst.name}…`);

  const res = await api.startTunnel({
    instanceId: inst.id,
    instanceName: inst.name,
    remotePort: $('remotePort').value,
    remoteHost: $('remoteHost').value.trim() || null,
    localPort: $('localPort').value.trim() || null,
    allowForeignKill,
  });
  $('startBtn').disabled = false;

  if (!res.ok) {
    clearStartConfirm();
    status('startStatus', 'err', res.error);
    return;
  }

  // The requested local port belongs to something we did not start — ask before
  // taking it, exactly as a reconnect does.
  if (res.data.needsConfirmation) {
    const conflict = res.data.conflict;
    status('startStatus', null);
    clearStartConfirm();
    $('startConfirm').appendChild(
      buildConfirmPrompt({
        conflict,
        verb: 'start the tunnel',
        onApprove: async () => {
          clearStartConfirm();
          await startTunnel({ allowForeignKill: true });
        },
        onCancel: () => {
          clearStartConfirm();
          status('startStatus', 'warn', `Cancelled — local port ${conflict.port} is still in use.`);
        },
      })
    );
    return;
  }

  clearStartConfirm();
  const t = res.data.tunnel;
  // If something had to be stopped to free the port, say so rather than doing it
  // invisibly.
  const note = t.cleared
    ? ` Stopped ${t.cleared.label}${t.cleared.ours ? ' (a leftover from an earlier tunnel)' : ''} to free the port.`
    : '';
  status(
    'startStatus',
    'ok',
    `Tunnel starting — connect to localhost:${t.localPort} (see Tunnels below).${note}`
  );
  // Free the local port field so the next tunnel auto-allocates again.
  $('localPort').value = '';
}

$('startBtn').addEventListener('click', () => startTunnel());

const STATUS_TEXT = {
  starting: 'Starting',
  open: 'Open',
  reconnecting: 'Reconnecting',
  stopping: 'Stopping',
  stopped: 'Stopped',
  error: 'Error',
};

// Per-card messages, keyed by tunnel id, so a failed reconnect reports against the
// tunnel it belongs to rather than the shared status line at the top.
const cardMessages = new Map();

// Tunnels awaiting a yes/no on killing a process we did not start.
const pendingKill = new Map();

// The last list painted, so a card message can be shown immediately rather than
// waiting for the next state broadcast to come round.
let lastTunnelList = [];

function repaintTunnels() {
  lastTunnelSig = '';
  renderTunnels(lastTunnelList);
}

// Whether AWS is currently reachable and the credentials still good. A tunnel cannot
// be working when this is false — its SSM session runs over that same connection — so
// no tunnel is shown as Open while we are offline.
let credsOnline = false;
let offlineReasonText = null;

function tunnelLabel(t) {
  const live = t.status === 'open' || t.status === 'starting';
  if (!credsOnline && live) return 'Offline';
  if (t.status === 'reconnecting') {
    const secs = t.retryAt ? Math.max(0, Math.ceil((t.retryAt - Date.now()) / 1000)) : 0;
    return `Reconnecting in ${secs}s — attempt ${t.attempts}`;
  }
  if (t.dropped && t.status === 'error') return 'Disconnected unexpectedly';
  return STATUS_TEXT[t.status] || t.status;
}

// Asking before killing something the app did not start. Names the process and the
// pid so the choice is informed, and states plainly when we cannot stop it at all.
// Shared by the New tunnel section and the tunnel cards so both read the same.
function buildConfirmPrompt({ conflict, verb, onApprove, onCancel }) {
  const box = document.createElement('div');
  box.className = 'tunnel-confirm';

  const text = document.createElement('div');
  text.textContent = conflict.canTerminate
    ? `${conflict.message} It was not started by Tunnel Keeper. Stop it and ${verb} on the same port?`
    : `${conflict.message} It was not started by Tunnel Keeper and belongs to another user, so this app cannot stop it.`;
  box.appendChild(text);

  const row = document.createElement('div');
  row.className = 'row';

  if (conflict.canTerminate) {
    const kill = document.createElement('button');
    kill.className = 'small danger-solid';
    kill.textContent = `Stop ${conflict.holder.name || 'it'} and ${verb}`;
    kill.addEventListener('click', async () => {
      kill.disabled = true;
      kill.textContent = 'Stopping…';
      await onApprove();
    });
    row.appendChild(kill);
  }

  const cancel = document.createElement('button');
  cancel.className = 'small';
  cancel.textContent = conflict.canTerminate ? 'Cancel' : 'Dismiss';
  cancel.addEventListener('click', onCancel);
  row.appendChild(cancel);

  box.appendChild(row);
  return box;
}

function buildKillPrompt(t, conflict) {
  return buildConfirmPrompt({
    conflict,
    verb: 'reconnect',
    onApprove: async () => {
      const res = await api.reconnectTunnel(t.id, true);
      pendingKill.delete(t.id);
      if (!res.ok) cardMessages.set(t.id, res.error);
      else if (res.data.needsConfirmation) cardMessages.set(t.id, res.data.conflict.message);
      repaintTunnels();
    },
    onCancel: () => {
      pendingKill.delete(t.id);
      repaintTunnels();
    },
  });
}

function renderTunnels(list) {
  const box = $('tunnels');
  // Normally repaint only on real change, so open <details> survive the 1s tick.
  // While a retry countdown is showing there is something to animate, so allow a
  // once-per-second repaint for exactly that case.
  const counting = list.some((t) => t.status === 'reconnecting');
  const sig =
    JSON.stringify(list) + `|${credsOnline}` + (counting ? `|${Math.floor(Date.now() / 1000)}` : '');
  lastTunnelList = list;
  // Dismissing a card frees its port, so the New tunnel hint must follow the list.
  if (typeof checkLocalPortClaim === 'function') checkLocalPortClaim();
  if (sig === lastTunnelSig) return;
  lastTunnelSig = sig;

  const active = list.filter((t) => t.status === 'open' || t.status === 'starting').length;
  $('tunnelCount').textContent = list.length
    ? credsOnline
      ? `${active} online · ${list.length} total`
      : `offline · ${list.length} total`
    : '';

  box.innerHTML = '';

  // Say why every tunnel reads Offline, rather than leaving the user to guess.
  if (list.length && !credsOnline) {
    const banner = document.createElement('div');
    banner.className = 'status status-warn';
    const icon = document.createElement('span');
    icon.className = 'status-icon';
    icon.textContent = '!';
    const msg = document.createElement('span');
    msg.textContent =
      `${offlineReasonText || 'AWS cannot be reached.'} These tunnels are not carrying ` +
      'traffic, and will show their real state once the connection is back.';
    banner.append(icon, msg);
    box.appendChild(banner);
  }
  if (list.length === 0) {
    const p = document.createElement('p');
    p.className = 'empty';
    p.textContent = 'No tunnels yet.';
    box.appendChild(p);
    return;
  }

  for (const t of list) {
    const card = document.createElement('div');
    card.className = 'tunnel';

    const head = document.createElement('div');
    head.className = 'tunnel-head';

    const left = document.createElement('div');
    const title = document.createElement('div');
    title.className = 'tunnel-title';
    const dot = document.createElement('span');
    const liveNow = t.status === 'open' || t.status === 'starting';
    dot.className = `dot dot-${!credsOnline && liveNow ? 'offline' : t.status}`;
    title.appendChild(dot);
    title.appendChild(document.createTextNode(`${t.instanceName} — ${tunnelLabel(t)}`));
    const sub = document.createElement('div');
    sub.className = 'tunnel-sub';
    sub.textContent = t.remoteHost
      ? `localhost:${t.localPort} → ${t.remoteHost}:${t.remotePort} via ${t.instanceId}`
      : `localhost:${t.localPort} → ${t.instanceId}:${t.remotePort}`;
    left.append(title, sub);

    const actions = document.createElement('div');
    actions.className = 'tunnel-actions';
    const live = t.status === 'open' || t.status === 'starting' || t.status === 'reconnecting';
    // A tunnel that is up again has outlived any earlier failure message.
    if (live) cardMessages.delete(t.id);

    if (live) {
      const copy = document.createElement('button');
      copy.className = 'small';
      copy.textContent = 'Copy address';
      copy.addEventListener('click', () => {
        navigator.clipboard.writeText(`localhost:${t.localPort}`);
        copy.textContent = 'Copied';
        setTimeout(() => (copy.textContent = 'Copy address'), 1200);
      });
      const stop = document.createElement('button');
      stop.className = 'small danger';
      stop.textContent = 'Stop';
      stop.addEventListener('click', () => api.stopTunnel(t.id));
      actions.append(copy, stop);
    } else {
      // Reconnect keeps the same local port, so whatever is configured to use it
      // keeps working — that is the whole point of offering it.
      const again = document.createElement('button');
      again.className = 'small primary';
      again.textContent = 'Reconnect';
      again.title = `Reopen on the same port, localhost:${t.localPort}`;
      // Opened with credentials that are no longer loaded: it cannot reach the same
      // instance, and main.js has already offered to clear it. Dismiss is the only
      // useful action left.
      if (t.stale) {
        again.disabled = true;
        again.title = 'These credentials are for a different AWS identity.';
      }
      again.addEventListener('click', async () => {
        again.disabled = true;
        again.textContent = 'Reconnecting…';
        const res = await api.reconnectTunnel(t.id, false);
        if (!res.ok) cardMessages.set(t.id, res.error);
        else if (res.data.needsConfirmation) {
          // Something we did not start owns the port — ask before killing it.
          pendingKill.set(t.id, res.data.conflict);
          cardMessages.delete(t.id);
        } else {
          cardMessages.delete(t.id);
          pendingKill.delete(t.id);
        }
        repaintTunnels();
      });
      const del = document.createElement('button');
      del.className = 'small';
      del.textContent = 'Dismiss';
      del.addEventListener('click', () => {
        cardMessages.delete(t.id);
        api.removeTunnel(t.id);
      });
      actions.append(again, del);
    }

    head.append(left, actions);
    card.appendChild(head);

    // Auto-reconnect is per tunnel and off by default: reopening a session to a
    // production host unattended should always be a deliberate choice.
    const auto = document.createElement('label');
    auto.className = 'toggle';
    const box2 = document.createElement('input');
    box2.type = 'checkbox';
    box2.checked = !!t.autoReconnect;
    box2.addEventListener('change', () => api.setAutoReconnect(t.id, box2.checked));
    const autoText = document.createElement('span');
    autoText.textContent = 'Auto-reconnect if it drops';
    auto.append(box2, autoText);
    card.appendChild(auto);

    const ask = pendingKill.get(t.id);
    if (ask) {
      card.appendChild(buildKillPrompt(t, ask));
    } else {
      // A stale tunnel shows its reason whatever its status: one that is still
      // running under the previous credentials is the case most worth explaining.
      const msg = cardMessages.get(t.id) || ((t.status === 'error' || t.stale) && t.reason) || null;
      if (msg) {
        const err = document.createElement('div');
        err.className = 'tunnel-error';
        err.textContent = msg;
        card.appendChild(err);
      }
    }

    if (t.log && t.log.length) {
      const det = document.createElement('details');
      det.className = 'log';
      const sum = document.createElement('summary');
      sum.textContent = 'Session output';
      const pre = document.createElement('pre');
      pre.textContent = t.log.join('\n');
      det.append(sum, pre);
      card.appendChild(det);
    }

    box.appendChild(card);
  }
}

// ---------- theme ----------

// Day (07:00–18:59 local) gets the light palette, night the dark one. The clock
// is the only input: this is deliberately not the OS appearance setting, and
// there is no manual switch. Re-checked every minute so a session left open
// across dusk or dawn flips without a restart.
const DAY_START_H = 7;
const DAY_END_H = 19;

function isDaytime(now) {
  const h = now.getHours();
  return h >= DAY_START_H && h < DAY_END_H;
}

function applyTheme() {
  const theme = isDaytime(new Date()) ? 'light' : 'dark';
  if (document.documentElement.dataset.theme !== theme) {
    document.documentElement.dataset.theme = theme;
  }
}

applyTheme();
setInterval(applyTheme, 60_000);

// ---------- wiring ----------

api.onState((state) => {
  credsOnline = !!state.creds.online;
  offlineReasonText = state.creds.offlineReason || null;
  renderCreds(state.creds);
  renderTunnels(state.tunnels);
});

$('assumeBtn').addEventListener('click', doAssume);
$('revertBtn').addEventListener('click', doRevert);
$('roleArn').addEventListener('input', () => {
  roleFieldTouched = true;
  status('assumeStatus', null);
});
$('roleArn').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') doAssume();
});

loadProfiles();

api.getState().then((res) => {
  if (res.ok) {
    credsOnline = !!res.data.creds.online;
    offlineReasonText = res.data.creds.offlineReason || null;
    renderCreds(res.data.creds);
    renderTunnels(res.data.tunnels);
    if (res.data.creds.loaded) loadRecentRoles();
  }
});
