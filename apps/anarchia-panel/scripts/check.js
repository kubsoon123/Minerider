'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const luaparse = require('luaparse');

const root = path.resolve(__dirname, '..');
const jsFiles = walk(root).filter((file) => file.endsWith('.js') && !file.includes(path.sep + 'node_modules' + path.sep));

for (const file of jsFiles) {
  const result = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
  if (result.status !== 0) {
    process.stderr.write(result.stderr);
    process.exit(result.status || 1);
  }
}

const template = fs.readFileSync(path.join(root, 'runtime/anarchia.lua.tpl'), 'utf8');
const lua = template.replace('__PANEL_CONFIG__', '{accounts={},server={},reconnect={},password=""}');
luaparse.parse(lua, { luaVersion: '5.3' });
console.log('OK: ' + jsFiles.length + ' plików JS i szablon Lua.');

function walk(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const file = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(file) : [file];
  });
}
