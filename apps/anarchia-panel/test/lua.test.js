'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const luaparse = require('luaparse');
const { buildLuaScript, toLua } = require('../src/lua');
const { validateConfig } = require('../src/config');

test('serializator Lua bezpiecznie cytuje dane', () => {
  assert.equal(toLua('a"b\\nc'), '"a\\"b\\\\nc"');
  assert.equal(toLua({ enabled: true, count: 2 }), '{["enabled"]=true,["count"]=2}');
});

test('generator tworzy poprawny składniowo skrypt wrappera', async () => {
  const config = validateConfig({
    server: { name: 'test', host: '127.0.0.1', port: 25565 },
    accounts: [{ username: 'LuaBot01', mode: 'afk' }]
  });
  const script = await buildLuaScript(config, config.accounts, 'sekret"test');
  assert.equal(script.includes('__PANEL_CONFIG__'), false);
  assert.match(script, /swarm:add_bot/);
  assert.match(script, /bot:click_gui/);
  assert.match(script, /verification_required/);
  assert.match(script, /accept_resource_packs\s*=\s*CONFIG\.server\.acceptResourcePacks/);
  assert.match(script, /emit_fatal/);
  assert.match(script, /scope\s*=\s*"swarm"/);
  luaparse.parse(script, { luaVersion: '5.3' });
});
