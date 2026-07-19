'use strict';

const { EventEmitter } = require('node:events');
const { spawn } = require('node:child_process');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { buildLuaScript } = require('./lua');

const APP_ROOT = path.resolve(__dirname, '..');
const PANEL_MARKER = '@panel:';
const MAX_LOG_LINES = 1000;

class RuntimeManager extends EventEmitter {
  constructor(options = {}) {
    super();
    this.child = null;
    this.tempDir = null;
    this.startedAt = null;
    this.stopping = false;
    this.accounts = new Map();
    this.logs = [];
    this.logDir = null;
    this.logFiles = null;
    this.logWriteQueue = Promise.resolve();
    this.logRoot = path.resolve(options.logRoot || path.join(APP_ROOT, 'logs'));
  }

  getSnapshot() {
    const accounts = Array.from(this.accounts.values());
    const counts = accounts.reduce((result, account) => {
      result.total += 1;
      if (account.status === 'online') result.online += 1;
      else if (['starting', 'connecting', 'connected', 'authenticating', 'preparing', 'selecting_mode', 'registering'].includes(account.status)) result.connecting += 1;
      else if (['error', 'kicked', 'needs_password', 'needs_registration', 'verification_required'].includes(account.status)) result.attention += 1;
      else result.offline += 1;
      return result;
    }, { total: 0, online: 0, connecting: 0, attention: 0, offline: 0 });

    return {
      running: Boolean(this.child),
      stopping: this.stopping,
      pid: this.child?.pid || null,
      startedAt: this.startedAt,
      logDirectory: this.logDir,
      counts,
      accounts,
      logs: this.logs.slice(-250)
    };
  }

  async start(config, requestedUsernames = null) {
    if (this.child) throw new Error('Runtime MineRider już działa.');

    const filter = Array.isArray(requestedUsernames) && requestedUsernames.length > 0
      ? new Set(requestedUsernames.map((value) => String(value).toLowerCase()))
      : null;
    const accounts = config.accounts.filter((account) => account.enabled && (!filter || filter.has(account.username.toLowerCase())));
    if (accounts.length === 0) throw new Error('Nie wybrano żadnego aktywnego konta.');

    const password = String(process.env.MINERIDER_BOT_PASSWORD || '');
    if (!password) {
      throw new Error('Ustaw MINERIDER_BOT_PASSWORD przed uruchomieniem botów. Hasło nie jest zapisywane w panelu.');
    }

    const script = await buildLuaScript(config, accounts, password);
    this.tempDir = await fs.mkdtemp(path.join(os.tmpdir(), 'minerider-panel-'));
    const scriptPath = path.join(this.tempDir, 'swarm.lua');
    await fs.writeFile(scriptPath, script, { mode: 0o600 });

    const binary = await resolveExecutable(path.resolve(APP_ROOT, config.runtime.binary));
    this.accounts.clear();
    for (const account of accounts) {
      this.accounts.set(account.username, {
        username: account.username,
        mode: account.mode,
        status: 'starting',
        message: 'Oczekuje na start procesu',
        updatedAt: new Date().toISOString(),
        workerId: null,
        reconnectAttempt: null
      });
    }
    this.logs = [];
    this.startedAt = new Date().toISOString();
    this.stopping = false;
    await this.initializeLogs(config, accounts, binary);

    const args = [
      scriptPath,
      '--lua-workers', String(Math.min(config.runtime.workers, accounts.length)),
      '--log-level', config.runtime.logLevel,
      '--shutdown-timeout-secs', '10'
    ];

    const child = spawn(binary, args, {
      cwd: path.resolve(APP_ROOT, '../..'),
      env: { ...process.env },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true
    });
    this.child = child;
    this.emitSnapshot();

    attachLineReader(child.stdout, (line) => this.handleLine('stdout', line));
    attachLineReader(child.stderr, (line) => this.handleLine('stderr', line));

    child.once('error', (error) => {
      this.pushLog('error', `Nie udało się uruchomić MineRider: ${error.message}`);
      this.markAllRunning('error', error.message);
    });
    child.once('exit', (code, signal) => this.handleExit(child, code, signal));
    return this.getSnapshot();
  }

  async stop() {
    const child = this.child;
    if (!child) {
      await this.flushLogs();
      return this.getSnapshot();
    }
    this.stopping = true;
    this.pushLog('info', 'Zatrzymywanie runtime MineRider…');
    this.emitSnapshot();

    try {
      child.kill('SIGINT');
    } catch (error) {
      this.pushLog('warn', `Nie udało się wysłać SIGINT: ${error.message}`);
    }

    await Promise.race([
      new Promise((resolve) => child.once('exit', resolve)),
      new Promise((resolve) => setTimeout(resolve, 12000))
    ]);

    if (this.child === child) {
      this.pushLog('warn', 'Przekroczono czas łagodnego zamknięcia; kończę proces.');
      child.kill('SIGKILL');
    }
    return this.getSnapshot();
  }

  handleLine(source, rawLine) {
    const line = stripAnsi(String(rawLine || '')).trim();
    if (!line) return;
    this.persistRawLine(source, line);
    const markerIndex = line.indexOf(PANEL_MARKER);
    if (markerIndex >= 0) {
      const payload = line.slice(markerIndex + PANEL_MARKER.length).trim();
      try {
        this.handlePanelEvent(JSON.parse(payload));
        return;
      } catch (error) {
        this.pushLog('warn', `Nieprawidłowe zdarzenie panelu: ${error.message}`);
      }
    }
    const level = source === 'stderr' || /\b(error|failed|panic)\b/i.test(line) ? 'error' : /\bwarn\b/i.test(line) ? 'warn' : 'info';
    this.pushLog(level, redact(line));
  }

  handlePanelEvent(event) {
    if (!event || typeof event !== 'object') return;
    const username = String(event.username || '');
    const current = this.accounts.get(username);
    if (!current) return;
    const next = {
      ...current,
      status: String(event.status || current.status),
      message: String(event.message || ''),
      updatedAt: new Date().toISOString(),
      workerId: Number.isInteger(event.worker_id) ? event.worker_id : current.workerId,
      reconnectAttempt: Number.isInteger(event.attempt) ? event.attempt : null
    };
    this.accounts.set(username, next);
    this.persistBotEvent(username, next, event);
    this.pushLog(event.level || 'info', `[${username}] ${next.message || next.status}`, false);
    this.emitSnapshot();
  }

  async handleExit(child, code, signal) {
    if (this.child !== child) return;
    this.child = null;
    const expected = this.stopping;
    this.stopping = false;
    this.pushLog(expected ? 'info' : 'error', `Runtime zakończył pracę (kod=${code ?? '-'}, sygnał=${signal || '-'}).`);
    this.markAllRunning(expected ? 'stopped' : 'error', expected ? 'Zatrzymano przez operatora' : 'Proces zakończył się nieoczekiwanie');
    const tempDir = this.tempDir;
    this.tempDir = null;
    if (tempDir) await fs.rm(tempDir, { recursive: true, force: true }).catch(() => {});
    await this.flushLogs();
    this.emitSnapshot();
  }

  markAllRunning(status, message) {
    for (const [username, account] of this.accounts) {
      if (!['stopped', 'error'].includes(account.status) || status === 'error') {
        this.accounts.set(username, { ...account, status, message, updatedAt: new Date().toISOString() });
      }
    }
    this.emitSnapshot();
  }

  pushLog(level, message, emit = true) {
    const entry = { at: new Date().toISOString(), level, message: redact(String(message || '')) };
    this.logs.push(entry);
    if (this.logs.length > MAX_LOG_LINES) this.logs.splice(0, this.logs.length - MAX_LOG_LINES);
    if (this.logFiles) {
      this.queueLog(this.logFiles.panel, JSON.stringify(entry) + '\n');
      this.queueLog(this.logFiles.diagnostics, formatDiagnostic(entry.at, 'panel/' + level, entry.message));
    }
    if (emit) this.emit('log', entry);
  }

  async initializeLogs(config, accounts, binary) {
    const stamp = this.startedAt.replace(/[:.]/g, '-');
    this.logDir = path.join(this.logRoot, 'run-' + stamp);
    const botsDir = path.join(this.logDir, 'bots');
    await fs.mkdir(botsDir, { recursive: true });
    this.logFiles = {
      diagnostics: path.join(this.logDir, 'diagnostics.log'),
      panel: path.join(this.logDir, 'panel.jsonl'),
      stdout: path.join(this.logDir, 'minerider.stdout.log'),
      stderr: path.join(this.logDir, 'minerider.stderr.log'),
      botsDir
    };
    const session = {
      startedAt: this.startedAt,
      binary,
      server: config.server,
      runtime: config.runtime,
      accounts: accounts.map(({ username, mode, enabled }) => ({ username, mode, enabled }))
    };
    await fs.writeFile(path.join(this.logDir, 'session.json'), JSON.stringify(session, null, 2) + '\n');
    await fs.writeFile(path.join(this.logRoot, 'LATEST_RUN.txt'), this.logDir + '\n');
    await fs.writeFile(this.logFiles.diagnostics, formatDiagnostic(this.startedAt, 'panel/info', 'Rozpoczęto sesję logowania'));
  }

  persistRawLine(source, line) {
    if (!this.logFiles) return;
    const safeLine = redact(line);
    const at = new Date().toISOString();
    this.queueLog(source === 'stderr' ? this.logFiles.stderr : this.logFiles.stdout, `[${at}] ${safeLine}\n`);
    this.queueLog(this.logFiles.diagnostics, formatDiagnostic(at, 'minerider/' + source, safeLine));
  }

  persistBotEvent(username, state, event) {
    if (!this.logFiles) return;
    const filename = safeLogName(username) + '.jsonl';
    const record = redact(JSON.stringify({ at: state.updatedAt, ...event, username }));
    this.queueLog(path.join(this.logFiles.botsDir, filename), record + '\n');
  }

  queueLog(filePath, contents) {
    this.logWriteQueue = this.logWriteQueue
      .then(() => fs.appendFile(filePath, contents))
      .catch((error) => {
        console.error('Nie udało się zapisać logu:', error.message);
      });
  }

  async flushLogs() {
    await this.logWriteQueue;
  }

  getDownloadLogPath() {
    return this.logFiles?.diagnostics || null;
  }

  emitSnapshot() {
    this.emit('snapshot', this.getSnapshot());
  }
}

function attachLineReader(stream, callback) {
  let pending = '';
  stream.setEncoding('utf8');
  stream.on('data', (chunk) => {
    pending += chunk;
    const lines = pending.split(/\r?\n/);
    pending = lines.pop() || '';
    for (const line of lines) callback(line);
  });
  stream.on('end', () => {
    if (pending) callback(pending);
  });
}

async function resolveExecutable(binary) {
  const candidates = process.platform === 'win32' && !binary.toLowerCase().endsWith('.exe')
    ? [binary + '.exe', binary]
    : [binary];
  for (const candidate of candidates) {
    try {
      await fs.access(candidate);
      return candidate;
    } catch {
      // Try the next platform-specific filename.
    }
  }
  throw new Error('Nie znaleziono binarki MineRider: ' + candidates.join(' lub ') + '. Najpierw wykonaj cargo build --release --features lua.');
}

function redact(value) {
  const password = process.env.MINERIDER_BOT_PASSWORD;
  return password ? value.split(password).join('[REDACTED]') : value;
}

function stripAnsi(value) {
  return value.replace(/\u001B\[[0-?]*[ -/]*[@-~]/g, '');
}

function safeLogName(value) {
  return String(value || 'unknown').replace(/[^A-Za-z0-9_-]/g, '_').slice(0, 64) || 'unknown';
}

function formatDiagnostic(at, source, message) {
  return `[${at}] [${source}] ${redact(String(message || ''))}\n`;
}

module.exports = { PANEL_MARKER, RuntimeManager, attachLineReader, redact, resolveExecutable, safeLogName, stripAnsi };
