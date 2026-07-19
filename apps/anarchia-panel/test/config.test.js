'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { validateConfig } = require('../src/config');

test('normalizuje konta i uzupełnia bezpieczne wartości domyślne', () => {
  const config = validateConfig({
    server: { host: 'localhost', port: 25565 },
    accounts: ['Bot_One', { username: 'Bot_Two', mode: 'register' }]
  });
  assert.equal(config.server.viewDistance, 8);
  assert.equal(config.server.acceptResourcePacks, true);
  assert.deepEqual(config.accounts, [
    { username: 'Bot_One', mode: 'afk', enabled: true },
    { username: 'Bot_Two', mode: 'register', enabled: true }
  ]);
  assert.equal(config.runtime.reconnect.maxRetries, 'unlimited');
});

test('odrzuca powtórzone i nieprawidłowe nicki', () => {
  assert.throws(() => validateConfig({
    accounts: ['SameBot', 'samebot']
  }), /Powtórzony nick/);
  assert.throws(() => validateConfig({
    accounts: ['nick ze spacją']
  }), /Nieprawidłowy nick/);
});

test('odrzuca nieznany tryb', () => {
  assert.throws(() => validateConfig({
    accounts: [{ username: 'ValidBot', mode: 'farm' }]
  }), /Nieprawidłowy tryb/);
});
