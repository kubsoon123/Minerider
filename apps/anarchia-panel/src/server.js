'use strict';

const http = require('node:http');
const path = require('node:path');
const express = require('express');
const { Server } = require('socket.io');
const { loadConfig, saveConfig, validateConfig } = require('./config');
const { RuntimeManager } = require('./runtime');

const APP_ROOT = path.resolve(__dirname, '..');
const CONFIG_PATH = path.resolve(process.env.MINERIDER_PANEL_CONFIG || path.join(APP_ROOT, 'config.local.json'));

async function createPanel() {
  let config = await loadConfig(CONFIG_PATH);
  const runtime = new RuntimeManager();
  const app = express();
  const server = http.createServer(app);
  const io = new Server(server, { serveClient: true, maxHttpBufferSize: 1024 * 1024 });

  app.disable('x-powered-by');
  app.use(express.json({ limit: '256kb' }));
  app.use(express.static(path.join(APP_ROOT, 'public'), { index: 'index.html' }));

  app.get('/api/health', (_request, response) => {
    response.json({ ok: true, runtime: runtime.getSnapshot(), configPath: CONFIG_PATH });
  });
  app.get('/api/config', (_request, response) => response.json(config));
  app.put('/api/config', asyncHandler(async (request, response) => {
    if (runtime.getSnapshot().running) throw httpError(409, 'Zatrzymaj runtime przed zmianą konfiguracji.');
    config = await saveConfig(CONFIG_PATH, request.body);
    io.emit('config', config);
    response.json(config);
  }));
  app.post('/api/swarm/start', asyncHandler(async (request, response) => {
    const usernames = Array.isArray(request.body?.usernames) ? request.body.usernames : null;
    const snapshot = await runtime.start(validateConfig(config), usernames);
    response.status(202).json(snapshot);
  }));
  app.post('/api/swarm/stop', asyncHandler(async (_request, response) => {
    response.json(await runtime.stop());
  }));
  app.get('/api/swarm', (_request, response) => response.json(runtime.getSnapshot()));
  app.get('/api/logs/download', asyncHandler(async (_request, response) => {
    await runtime.flushLogs();
    const logPath = runtime.getDownloadLogPath();
    if (!logPath) throw httpError(404, 'Brak logów. Najpierw uruchom boty.');
    response.download(logPath, 'minerider-diagnostics.log');
  }));

  app.use((error, _request, response, _next) => {
    const status = Number(error.statusCode) || 400;
    response.status(status).json({ error: error.message || 'Nieznany błąd panelu.' });
  });

  runtime.on('snapshot', (snapshot) => io.emit('snapshot', snapshot));
  runtime.on('log', (entry) => io.emit('log', entry));
  io.on('connection', (socket) => {
    socket.emit('config', config);
    socket.emit('snapshot', runtime.getSnapshot());
  });

  return { app, config, io, runtime, server };
}

async function main() {
  const panel = await createPanel();
  const { host, port } = panel.config.panel;
  await new Promise((resolve, reject) => {
    panel.server.once('error', reject);
    panel.server.listen(port, host, resolve);
  });
  console.log(`MineRider Control: http://${host}:${port}`);

  const shutdown = async () => {
    await panel.runtime.stop();
    panel.server.close(() => process.exit(0));
  };
  process.once('SIGINT', shutdown);
  process.once('SIGTERM', shutdown);
}

function asyncHandler(handler) {
  return (request, response, next) => Promise.resolve(handler(request, response, next)).catch(next);
}

function httpError(statusCode, message) {
  const error = new Error(message);
  error.statusCode = statusCode;
  return error;
}

if (require.main === module) {
  main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
}

module.exports = { createPanel };
