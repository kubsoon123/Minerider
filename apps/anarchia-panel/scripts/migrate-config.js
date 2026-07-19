'use strict';

const fs = require('node:fs/promises');
const path = require('node:path');
const { validateConfig } = require('../src/config');

async function main() {
  const sourcePath = process.argv[2];
  if (!sourcePath) throw new Error('Użycie: npm run migrate -- "/ścieżka/do/starego/config.json"');
  const source = JSON.parse(await fs.readFile(path.resolve(sourcePath), 'utf8'));
  const target = validateConfig({
    server: {
      name: 'anarchia',
      host: source.host,
      port: source.port,
      viewDistance: source.viewDistance === 'tiny' ? 2 : 8,
      sharedChunks: true
    },
    accounts: (source.accounts || []).map((entry) => ({
      username: typeof entry === 'string' ? entry : entry.username,
      mode: 'afk',
      enabled: true
    }))
  });
  const outputPath = path.resolve(__dirname, '../config.local.json');
  await fs.writeFile(outputPath, JSON.stringify(target, null, 2) + '\n', { mode: 0o600 });
  console.log('Zapisano ' + outputPath);
  console.log('Hasło ze starego pliku celowo nie zostało skopiowane. Ustaw MINERIDER_BOT_PASSWORD.');
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
