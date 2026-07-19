'use strict';

const fs = require('node:fs/promises');
const path = require('node:path');

const TEMPLATE_PATH = path.resolve(__dirname, '../runtime/anarchia.lua.tpl');

async function buildLuaScript(config, accounts, password) {
  const template = await fs.readFile(TEMPLATE_PATH, 'utf8');
  const payload = {
    server: config.server,
    accounts,
    password,
    reconnect: config.runtime.reconnect
  };
  return template.replace('__PANEL_CONFIG__', toLua(payload));
}

function toLua(value, depth = 0) {
  if (depth > 24) throw new Error('Konfiguracja ma zbyt głęboką strukturę.');
  if (value === null || value === undefined) return 'nil';
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw new Error('Lua nie obsługuje niefinitywnej liczby w konfiguracji.');
    return String(value);
  }
  if (typeof value === 'string') return quoteLua(value);
  if (Array.isArray(value)) return `{${value.map((item) => toLua(item, depth + 1)).join(',')}}`;
  if (typeof value === 'object') {
    const fields = Object.entries(value).map(([key, item]) => `[${quoteLua(key)}]=${toLua(item, depth + 1)}`);
    return `{${fields.join(',')}}`;
  }
  throw new Error(`Nieobsługiwany typ konfiguracji: ${typeof value}`);
}

function quoteLua(value) {
  return `"${String(value)
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\r/g, '\\r')
    .replace(/\n/g, '\\n')
    .replace(/\0/g, '\\0')}"`;
}

module.exports = { buildLuaScript, quoteLua, toLua };
