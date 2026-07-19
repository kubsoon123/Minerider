'use strict';

const socket = io();
const elements = Object.fromEntries([
  'connectionBadge', 'serverLabel', 'startSelectedButton', 'startAllButton', 'stopButton',
  'settingsButton', 'settingsDialog', 'settingsForm', 'closeSettingsButton',
  'cancelSettingsButton', 'serverHost', 'serverPort', 'viewDistance', 'runtimeBinary',
  'runtimeWorkers', 'sharedChunks', 'acceptResourcePacks', 'accountsInput', 'searchInput', 'selectAllButton',
  'masterCheckbox', 'accountsBody', 'emptyAccounts', 'logOutput', 'clearLogsButton', 'downloadLogsButton',
  'metricOnline', 'metricConnecting', 'metricAttention', 'metricOffline', 'toast'
].map((id) => [id, document.getElementById(id)]));

let config = null;
let snapshot = {
  running: false,
  stopping: false,
  accounts: [],
  logs: [],
  counts: { online: 0, connecting: 0, attention: 0, offline: 0, total: 0 }
};
let selected = new Set();
let toastTimer = null;

socket.on('connect', () => {
  elements.connectionBadge.classList.add('online');
  elements.connectionBadge.lastChild.textContent = ' Panel połączony';
});
socket.on('disconnect', () => {
  elements.connectionBadge.classList.remove('online');
  elements.connectionBadge.lastChild.textContent = ' Panel offline';
});
socket.on('config', (value) => {
  const firstLoad = !config;
  config = value;
  if (firstLoad) selected = new Set(config.accounts.filter((account) => account.enabled).map((account) => account.username));
  render();
});
socket.on('snapshot', (value) => {
  snapshot = value;
  render();
});
socket.on('log', (entry) => appendLog(entry));

elements.startSelectedButton.addEventListener('click', async () => {
  if (selected.size === 0) return showToast('Zaznacz co najmniej jedno konto.', true);
  await requestStart(Array.from(selected));
});
elements.startAllButton.addEventListener('click', () => requestStart(null));
elements.stopButton.addEventListener('click', async () => {
  await runAction('/api/swarm/stop', {}, 'Runtime zatrzymany.');
});
elements.searchInput.addEventListener('input', renderAccounts);
elements.clearLogsButton.addEventListener('click', () => {
  elements.logOutput.replaceChildren();
});
elements.selectAllButton.addEventListener('click', () => {
  for (const account of visibleAccounts()) selected.add(account.username);
  renderAccounts();
});
elements.masterCheckbox.addEventListener('change', () => {
  for (const account of visibleAccounts()) {
    if (elements.masterCheckbox.checked) selected.add(account.username);
    else selected.delete(account.username);
  }
  renderAccounts();
});
elements.settingsButton.addEventListener('click', openSettings);
elements.closeSettingsButton.addEventListener('click', () => elements.settingsDialog.close());
elements.cancelSettingsButton.addEventListener('click', () => elements.settingsDialog.close());
elements.settingsForm.addEventListener('submit', saveSettings);

async function requestStart(usernames) {
  await runAction('/api/swarm/start', usernames ? { usernames } : {}, 'Uruchamianie runtime MineRider…');
}

async function runAction(url, body, successMessage) {
  setBusy(true);
  try {
    const response = await fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body)
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Operacja nie powiodła się.');
    snapshot = result;
    render();
    showToast(successMessage);
  } catch (error) {
    showToast(error.message, true);
  } finally {
    setBusy(false);
  }
}

function render() {
  if (config) {
    elements.serverLabel.textContent = config.server.host + ':' + config.server.port +
      ' · ' + config.accounts.length + ' kont · ' + config.runtime.workers + ' workery Lua';
  }
  const counts = snapshot.counts || {};
  elements.metricOnline.textContent = counts.online || 0;
  elements.metricConnecting.textContent = counts.connecting || 0;
  elements.metricAttention.textContent = counts.attention || 0;
  elements.metricOffline.textContent = snapshot.running
    ? counts.offline || 0
    : config ? config.accounts.filter((account) => account.enabled).length : 0;
  elements.startSelectedButton.disabled = snapshot.running || snapshot.stopping;
  elements.startAllButton.disabled = snapshot.running || snapshot.stopping;
  elements.stopButton.disabled = !snapshot.running || snapshot.stopping;
  elements.settingsButton.disabled = snapshot.running;
  renderAccounts();
  renderLogs();
}

function mergedAccounts() {
  if (!config) return [];
  const runtimeByName = new Map((snapshot.accounts || []).map((account) => [account.username, account]));
  return config.accounts.map((account) => ({
    ...account,
    status: runtimeByName.get(account.username)?.status || (account.enabled ? 'offline' : 'disabled'),
    message: runtimeByName.get(account.username)?.message || (account.enabled ? 'Gotowy do uruchomienia' : 'Konto wyłączone'),
    updatedAt: runtimeByName.get(account.username)?.updatedAt || null,
    workerId: runtimeByName.get(account.username)?.workerId ?? null
  }));
}

function visibleAccounts() {
  const query = elements.searchInput.value.trim().toLowerCase();
  return mergedAccounts().filter((account) => !query || account.username.toLowerCase().includes(query));
}

function renderAccounts() {
  const accounts = visibleAccounts();
  elements.accountsBody.replaceChildren();
  elements.emptyAccounts.hidden = accounts.length > 0;
  for (const account of accounts) {
    const row = document.createElement('tr');
    row.append(
      cellWithCheckbox(account),
      textCell(account.username, account.workerId === null ? (account.enabled ? 'aktywny' : 'wyłączony') : 'worker ' + account.workerId, 'account-name'),
      badgeCell(account.mode, 'mode-badge'),
      badgeCell(statusLabel(account.status), 'status-badge status-' + safeClass(account.status)),
      textCell(account.message, account.updatedAt ? formatTime(account.updatedAt) : '—')
    );
    elements.accountsBody.append(row);
  }
  const enabledVisible = accounts.filter((account) => account.enabled);
  elements.masterCheckbox.checked = enabledVisible.length > 0 && enabledVisible.every((account) => selected.has(account.username));
  elements.masterCheckbox.indeterminate = enabledVisible.some((account) => selected.has(account.username)) && !elements.masterCheckbox.checked;
}

function cellWithCheckbox(account) {
  const cell = document.createElement('td');
  cell.className = 'check-col';
  const input = document.createElement('input');
  input.type = 'checkbox';
  input.checked = selected.has(account.username);
  input.disabled = !account.enabled || snapshot.running;
  input.setAttribute('aria-label', 'Wybierz ' + account.username);
  input.addEventListener('change', () => {
    if (input.checked) selected.add(account.username);
    else selected.delete(account.username);
    renderAccounts();
  });
  cell.append(input);
  return cell;
}

function textCell(primary, secondary, className) {
  const cell = document.createElement('td');
  const main = document.createElement('span');
  main.className = className || '';
  main.textContent = primary || '—';
  const meta = document.createElement('span');
  meta.className = className === 'account-name' ? 'account-meta' : 'event-time';
  meta.textContent = secondary || '—';
  cell.append(main, meta);
  return cell;
}

function badgeCell(text, className) {
  const cell = document.createElement('td');
  const badge = document.createElement('span');
  badge.className = className;
  badge.textContent = text;
  cell.append(badge);
  return cell;
}

function renderLogs() {
  if (elements.logOutput.childElementCount > 0) return;
  for (const entry of snapshot.logs || []) appendLog(entry, false);
}

function appendLog(entry, scroll = true) {
  const row = document.createElement('div');
  row.className = 'log-line ' + safeClass(entry.level || 'info');
  const time = document.createElement('time');
  time.textContent = formatTime(entry.at);
  const level = document.createElement('span');
  level.className = 'log-level';
  level.textContent = String(entry.level || 'info').toUpperCase();
  const message = document.createElement('span');
  message.className = 'log-message';
  message.textContent = entry.message || '';
  row.append(time, level, message);
  elements.logOutput.append(row);
  while (elements.logOutput.childElementCount > 400) elements.logOutput.firstElementChild.remove();
  if (scroll) elements.logOutput.scrollTop = elements.logOutput.scrollHeight;
}

function openSettings() {
  if (!config || snapshot.running) return;
  elements.serverHost.value = config.server.host;
  elements.serverPort.value = config.server.port;
  elements.viewDistance.value = config.server.viewDistance;
  elements.runtimeBinary.value = config.runtime.binary;
  elements.runtimeWorkers.value = config.runtime.workers;
  elements.sharedChunks.checked = config.server.sharedChunks;
  elements.acceptResourcePacks.checked = config.server.acceptResourcePacks !== false;
  elements.accountsInput.value = config.accounts
    .map((account) => account.username + ':' + account.mode + (account.enabled ? '' : ':off'))
    .join('\n');
  elements.settingsDialog.showModal();
}

async function saveSettings(event) {
  event.preventDefault();
  try {
    const accounts = parseAccounts(elements.accountsInput.value);
    const next = {
      ...config,
      server: {
        ...config.server,
        host: elements.serverHost.value.trim(),
        port: Number(elements.serverPort.value),
        viewDistance: Number(elements.viewDistance.value),
        sharedChunks: elements.sharedChunks.checked,
        acceptResourcePacks: elements.acceptResourcePacks.checked
      },
      accounts,
      runtime: {
        ...config.runtime,
        binary: elements.runtimeBinary.value.trim(),
        workers: Number(elements.runtimeWorkers.value)
      }
    };
    const response = await fetch('/api/config', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(next)
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Nie udało się zapisać konfiguracji.');
    config = result;
    selected = new Set(config.accounts.filter((account) => account.enabled).map((account) => account.username));
    elements.settingsDialog.close();
    render();
    showToast('Konfiguracja zapisana.');
  } catch (error) {
    showToast(error.message, true);
  }
}

function parseAccounts(value) {
  return value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean).map((line) => {
    const [username, mode = 'afk', state = 'on'] = line.split(':').map((part) => part.trim());
    return { username, mode: mode.toLowerCase(), enabled: state.toLowerCase() !== 'off' };
  });
}

function statusLabel(status) {
  return ({
    offline: 'offline',
    disabled: 'wyłączony',
    starting: 'start',
    connecting: 'łączenie',
    connected: 'połączony',
    authenticating: 'logowanie',
    preparing: 'przygotowanie',
    selecting_mode: 'wybór trybu',
    registering: 'rejestracja',
    registered: 'zarejestrowany',
    online: 'online',
    disconnected: 'rozłączony',
    stopped: 'zatrzymany',
    kicked: 'wyrzucony',
    error: 'błąd',
    needs_registration: 'wymaga rejestracji',
    needs_password: 'brak hasła',
    verification_required: 'weryfikacja'
  })[status] || status;
}

function safeClass(value) {
  return String(value || '').replace(/[^a-z0-9_-]/gi, '_');
}

function formatTime(value) {
  if (!value) return '—';
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? '—' : date.toLocaleTimeString('pl-PL', { hour12: false });
}

function setBusy(value) {
  if (value) {
    elements.startSelectedButton.disabled = true;
    elements.startAllButton.disabled = true;
    elements.stopButton.disabled = true;
  } else render();
}

function showToast(message, error = false) {
  clearTimeout(toastTimer);
  elements.toast.textContent = message;
  elements.toast.classList.toggle('error', error);
  elements.toast.hidden = false;
  toastTimer = setTimeout(() => { elements.toast.hidden = true; }, 5000);
}
