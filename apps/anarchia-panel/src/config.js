'use strict';

const fs = require('node:fs/promises');
const path = require('node:path');

const USERNAME_RE = /^[A-Za-z0-9_]{3,16}$/;
const MODES = new Set(['afk', 'register']);

const DEFAULT_CONFIG = Object.freeze({
  server: {
    name: 'authorized-server',
    host: '127.0.0.1',
    port: 25565,
    viewDistance: 8,
    sharedChunks: true,
    acceptResourcePacks: true
  },
  accounts: [],
  runtime: {
    binary: '../../target/release/minerider-lua',
    workers: 1,
    logLevel: 'info',
    reconnect: {
      enabled: true,
      maxRetries: 'unlimited',
      initialDelayMs: 8000,
      maxDelayMs: 300000,
      multiplier: 1.5,
      stableSessionResetMs: 60000
    }
  },
  panel: {
    host: '127.0.0.1',
    port: 3000
  }
});

function cloneDefault() {
  return JSON.parse(JSON.stringify(DEFAULT_CONFIG));
}

async function loadConfig(filePath) {
  try {
    const raw = await fs.readFile(filePath, 'utf8');
    return validateConfig(JSON.parse(raw));
  } catch (error) {
    if (error.code === 'ENOENT') return cloneDefault();
    throw error;
  }
}

async function saveConfig(filePath, input) {
  const config = validateConfig(input);
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const temporary = `${filePath}.${process.pid}.tmp`;
  await fs.writeFile(temporary, `${JSON.stringify(config, null, 2)}\n`, { mode: 0o600 });
  await fs.rename(temporary, filePath);
  return config;
}

function validateConfig(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) {
    throw new Error('Konfiguracja musi być obiektem JSON.');
  }

  const base = cloneDefault();
  const server = { ...base.server, ...(input.server || {}) };
  const runtimeInput = input.runtime || {};
  const runtime = {
    ...base.runtime,
    ...runtimeInput,
    reconnect: { ...base.runtime.reconnect, ...(runtimeInput.reconnect || {}) }
  };
  const panel = { ...base.panel, ...(input.panel || {}) };

  server.name = requiredText(server.name, 'server.name', 64);
  server.host = requiredText(server.host, 'server.host', 255);
  server.port = integerInRange(server.port, 'server.port', 1, 65535);
  server.viewDistance = integerInRange(server.viewDistance, 'server.viewDistance', 2, 32);
  server.sharedChunks = Boolean(server.sharedChunks);
  server.acceptResourcePacks = server.acceptResourcePacks !== false;

  runtime.binary = requiredText(runtime.binary, 'runtime.binary', 1024);
  runtime.workers = integerInRange(runtime.workers, 'runtime.workers', 1, 32);
  runtime.logLevel = String(runtime.logLevel || 'info').toLowerCase();
  if (!['trace', 'debug', 'info', 'warn', 'error'].includes(runtime.logLevel)) {
    throw new Error('runtime.logLevel musi mieć wartość trace, debug, info, warn albo error.');
  }

  const reconnect = runtime.reconnect;
  reconnect.enabled = Boolean(reconnect.enabled);
  if (reconnect.maxRetries !== 'unlimited') {
    reconnect.maxRetries = integerInRange(reconnect.maxRetries, 'runtime.reconnect.maxRetries', 0, 1000000);
  }
  reconnect.initialDelayMs = integerInRange(reconnect.initialDelayMs, 'runtime.reconnect.initialDelayMs', 0, 3600000);
  reconnect.maxDelayMs = integerInRange(reconnect.maxDelayMs, 'runtime.reconnect.maxDelayMs', reconnect.initialDelayMs, 86400000);
  reconnect.multiplier = finiteInRange(reconnect.multiplier, 'runtime.reconnect.multiplier', 1, 10);
  reconnect.stableSessionResetMs = integerInRange(reconnect.stableSessionResetMs, 'runtime.reconnect.stableSessionResetMs', 0, 86400000);

  panel.host = requiredText(panel.host, 'panel.host', 255);
  panel.port = integerInRange(panel.port, 'panel.port', 1, 65535);

  const sourceAccounts = Array.isArray(input.accounts) ? input.accounts : [];
  const usernames = new Set();
  const accounts = sourceAccounts.map((entry, index) => {
    const account = typeof entry === 'string' ? { username: entry } : { ...(entry || {}) };
    const username = requiredText(account.username, `accounts[${index}].username`, 16);
    if (!USERNAME_RE.test(username)) {
      throw new Error(`Nieprawidłowy nick Minecraft: ${username}`);
    }
    if (usernames.has(username.toLowerCase())) {
      throw new Error(`Powtórzony nick: ${username}`);
    }
    usernames.add(username.toLowerCase());
    const mode = String(account.mode || 'afk').toLowerCase();
    if (!MODES.has(mode)) throw new Error(`Nieprawidłowy tryb dla ${username}: ${mode}`);
    return { username, mode, enabled: account.enabled !== false };
  });

  return { server, accounts, runtime, panel };
}

function requiredText(value, field, maxLength) {
  const text = String(value ?? '').trim();
  if (!text) throw new Error(`${field} nie może być puste.`);
  if (text.length > maxLength) throw new Error(`${field} jest za długie.`);
  return text;
}

function integerInRange(value, field, min, max) {
  const number = Number(value);
  if (!Number.isInteger(number) || number < min || number > max) {
    throw new Error(`${field} musi być liczbą całkowitą od ${min} do ${max}.`);
  }
  return number;
}

function finiteInRange(value, field, min, max) {
  const number = Number(value);
  if (!Number.isFinite(number) || number < min || number > max) {
    throw new Error(`${field} musi być liczbą od ${min} do ${max}.`);
  }
  return number;
}

module.exports = { DEFAULT_CONFIG, loadConfig, saveConfig, validateConfig };
