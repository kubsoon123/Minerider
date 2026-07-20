'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { RuntimeManager, classifyProcessError, redact, resolveExecutable, safeLogName } = require('../src/runtime');

function makeAccount(overrides = {}) {
  return {
    username: 'Bot01',
    mode: 'afk',
    status: 'starting',
    message: '',
    updatedAt: null,
    workerId: null,
    reconnectAttempt: null,
    lastError: null,
    ...overrides
  };
}

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

test('classifyProcessError rozpoznaje znane wzorce błędów procesu', () => {
  assert.equal(classifyProcessError('error: swarm failed to start: boom'), 'swarm_start_failed');
  assert.equal(classifyProcessError('error: could not read script `x`: boom'), 'script_read_failed');
  assert.equal(classifyProcessError('error: proxy profile configuration failed: boom'), 'proxy_profile_invalid');
  assert.equal(classifyProcessError('error: something unexpected'), 'process_error');
  assert.equal(classifyProcessError('info: not an error'), null);
});

test('linia stderr z prefiksem error: ustawia strukturalny lastError', () => {
  const runtime = new RuntimeManager();
  runtime.handleLine('stderr', 'error: swarm failed to start: proxy profile `x` is not configured');
  assert.equal(runtime.lastError.code, 'swarm_start_failed');
  assert.equal(runtime.lastError.scope, 'process');
  assert.equal(runtime.getSnapshot().lastError.code, 'swarm_start_failed');
});

test('zdarzenie panelu w zasięgu swarm trafia do lastError bez konta', () => {
  const runtime = new RuntimeManager();
  runtime.handleLine(
    'stdout',
    'INFO @panel:{"scope":"swarm","status":"fatal_error","code":"duplicate_id","message":"bot username `Bot01` is already in use"}'
  );
  assert.equal(runtime.lastError.scope, 'swarm');
  assert.equal(runtime.lastError.code, 'duplicate_id');
  assert.equal(runtime.accounts.size, 0);
});

test('błąd konta trafia do historii konta i do lastError z username', () => {
  const runtime = new RuntimeManager();
  runtime.accounts.set('Bot01', makeAccount());
  runtime.handleLine(
    'stdout',
    'INFO @panel:{"username":"Bot01","status":"error","message":"GUI wyboru trybu nie zawiera pozycji BOXPVP"}'
  );
  const account = runtime.accounts.get('Bot01');
  assert.equal(account.lastError.message, 'GUI wyboru trybu nie zawiera pozycji BOXPVP');
  assert.equal(runtime.lastError.scope, 'account');
  assert.equal(runtime.lastError.username, 'Bot01');
  const events = runtime.getAccountEvents('Bot01');
  assert.equal(events.length, 1);
  assert.equal(events[0].status, 'error');
  assert.equal(runtime.getAccountEvents('NieznanyBot').length, 0);
});

test('nieoczekiwane zakończenie procesu bez wcześniejszego błędu ustawia domyślny lastError', async () => {
  const runtime = new RuntimeManager();
  const fakeChild = { pid: 123 };
  runtime.child = fakeChild;
  runtime.stopping = false;
  await runtime.handleExit(fakeChild, 1, null);
  assert.equal(runtime.lastError.code, 'process_exit');
});

test('nieoczekiwane zakończenie procesu nie nadpisuje już ustawionego lastError', async () => {
  const runtime = new RuntimeManager();
  const fakeChild = { pid: 123 };
  runtime.child = fakeChild;
  runtime.stopping = false;
  runtime.handleLine('stderr', 'error: could not read script `x`: not found');
  await runtime.handleExit(fakeChild, 1, null);
  assert.equal(runtime.lastError.code, 'script_read_failed');
});

test('ogólny błąd procesu na stderr nie nadpisuje wcześniejszego szczegółowego błędu swarm/konta', () => {
  const runtime = new RuntimeManager();
  runtime.handleLine(
    'stdout',
    'ERROR @panel:{"scope":"swarm","status":"fatal_error","code":"duplicate_id","message":"Nie udało się dodać konta Bot2: bot id 7 is already in use"}'
  );
  runtime.handleLine('stderr', 'error: swarm failed to start: worker 0 failed during startup: bot id 7 is already in use');
  assert.equal(runtime.lastError.scope, 'swarm');
  assert.equal(runtime.lastError.code, 'duplicate_id');
});

test('zatrzymanie przez operatora nie ustawia lastError', async () => {
  const runtime = new RuntimeManager();
  const fakeChild = { pid: 123 };
  runtime.child = fakeChild;
  runtime.stopping = true;
  await runtime.handleExit(fakeChild, 0, null);
  assert.equal(runtime.lastError, null);
});
