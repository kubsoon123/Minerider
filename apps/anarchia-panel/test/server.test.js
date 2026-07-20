'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');

test('panel udostępnia konfigurację, snapshot i nowy interfejs', async (context) => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'minerider-panel-test-'));
  const configPath = path.join(directory, 'config.json');
  await fs.writeFile(configPath, JSON.stringify({
    server: { name: 'test', host: '127.0.0.1', port: 25565 },
    accounts: [{ username: 'WebBot01', mode: 'afk' }],
    panel: { host: '127.0.0.1', port: 3000 }
  }));
  process.env.MINERIDER_PANEL_CONFIG = configPath;
  const modulePath = require.resolve('../src/server');
  delete require.cache[modulePath];
  const { createPanel } = require('../src/server');
  const panel = await createPanel();
  await new Promise((resolve) => panel.server.listen(0, '127.0.0.1', resolve));
  context.after(async () => {
    await new Promise((resolve) => panel.server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
    delete process.env.MINERIDER_PANEL_CONFIG;
  });

  const address = panel.server.address();
  const base = 'http://127.0.0.1:' + address.port;
  const configResponse = await fetch(base + '/api/config');
  const config = await configResponse.json();
  assert.equal(config.accounts[0].username, 'WebBot01');
  assert.equal(config.server.acceptResourcePacks, true);
  assert.equal(config.runtime.workers, 1);
  assert.equal(Object.hasOwn(config, 'password'), false);

  const snapshotResponse = await fetch(base + '/api/swarm');
  const snapshot = await snapshotResponse.json();
  assert.equal(snapshot.running, false);
  assert.equal(snapshot.lastError, null);

  const eventsResponse = await fetch(base + '/api/accounts/WebBot01/events');
  const events = await eventsResponse.json();
  assert.equal(events.username, 'WebBot01');
  assert.deepEqual(events.events, []);

  const htmlResponse = await fetch(base + '/');
  const html = await htmlResponse.text();
  assert.match(html, /MineRider <em>Control/);
  assert.match(html, /Start wybranych/);
  assert.match(html, /Akceptuj resource packi serwera/);
  assert.match(html, /Pobierz logi/);
});
