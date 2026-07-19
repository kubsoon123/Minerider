'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { RuntimeManager, redact, resolveExecutable, safeLogName } = require('../src/runtime');

test('parser zdarzeń aktualizuje status konta', () => {
  const runtime = new RuntimeManager();
  runtime.accounts.set('Bot01', {
    username: 'Bot01',
    mode: 'afk',
    status: 'starting',
    message: '',
    updatedAt: null,
    workerId: null,
    reconnectAttempt: null
  });
  runtime.handleLine('stdout', 'INFO worker @panel:{"username":"Bot01","status":"online","message":"gotowy","worker_id":2}');
  assert.equal(runtime.accounts.get('Bot01').status, 'online');
  assert.equal(runtime.accounts.get('Bot01').workerId, 2);
});

test('parser zdarzeń ignoruje pola tracing po JSON-ie', () => {
  const runtime = new RuntimeManager();
  runtime.accounts.set('Bot01', {
    username: 'Bot01',
    mode: 'afk',
    status: 'starting',
    message: '',
    updatedAt: null,
    workerId: null,
    reconnectAttempt: null
  });
  runtime.handleLine(
    'stdout',
    'INFO [lua] @panel:{"username":"Bot01","status":"preparing","message":"tekst z { klamrą } i \\\"cytatem\\\"","worker_id":0} worker=0'
  );
  assert.equal(runtime.accounts.get('Bot01').status, 'preparing');
  assert.equal(runtime.accounts.get('Bot01').message, 'tekst z { klamrą } i "cytatem"');
  assert.equal(runtime.accounts.get('Bot01').workerId, 0);
  assert.equal(runtime.logs.some((entry) => entry.message.includes('Nieprawidłowe zdarzenie panelu')), false);
});

test('redakcja usuwa hasło z logu', () => {
  const previous = process.env.MINERIDER_BOT_PASSWORD;
  process.env.MINERIDER_BOT_PASSWORD = 'tajnehaslo';
  assert.equal(redact('login tajnehaslo'), 'login [REDACTED]');
  if (previous === undefined) delete process.env.MINERIDER_BOT_PASSWORD;
  else process.env.MINERIDER_BOT_PASSWORD = previous;
});

test('nazwa pliku logu bota jest bezpieczna', () => {
  assert.equal(safeLogName('../Bot 01'), '___Bot_01');
});

test('trwały log zapisuje diagnostykę bez hasła', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'minerider-log-test-'));
  const previous = process.env.MINERIDER_BOT_PASSWORD;
  process.env.MINERIDER_BOT_PASSWORD = 'sekret-do-redakcji';
  try {
    const runtime = new RuntimeManager({ logRoot: directory });
    runtime.startedAt = new Date().toISOString();
    await runtime.initializeLogs(
      { server: { host: '127.0.0.1' }, runtime: { workers: 1 } },
      [{ username: 'Bot01', mode: 'afk', enabled: true }],
      'minerider-lua'
    );
    runtime.handleLine('stderr', 'login sekret-do-redakcji failed');
    await runtime.flushLogs();
    const diagnostics = await fs.readFile(runtime.getDownloadLogPath(), 'utf8');
    assert.match(diagnostics, /\[REDACTED\]/);
    assert.equal(diagnostics.includes('sekret-do-redakcji'), false);
  } finally {
    if (previous === undefined) delete process.env.MINERIDER_BOT_PASSWORD;
    else process.env.MINERIDER_BOT_PASSWORD = previous;
    await fs.rm(directory, { recursive: true, force: true });
  }
});

test('resolver znajduje istniejącą binarkę', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'minerider-bin-test-'));
  const binary = path.join(directory, 'minerider-lua');
  await fs.writeFile(binary, '');
  assert.equal(await resolveExecutable(binary), binary);
  await fs.rm(directory, { recursive: true, force: true });
});
