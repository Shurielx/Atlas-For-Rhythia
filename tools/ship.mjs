// npm run ship: builds the installer and puts it over the Atlas installed on this computer, without
// clicking through it (NSIS /S), then starts Atlas. Needs no network. Atlas' data (collections,
// pairings, limits) is in its own folder and stays.
import { execSync, spawn, spawnSync } from 'node:child_process';
import { existsSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const bundle = join(root, 'src-tauri', 'target', 'release', 'bundle', 'nsis');
const installed = join(process.env.LOCALAPPDATA || '', 'Atlas for Rhythia', 'atlas-for-rhythia.exe');

// Update files (signed installer and .sig) need the updater key; without it on this computer the local
// build leaves them out. The key never goes into the repo (GitHub Actions has it as a secret).
const key = join(process.env.USERPROFILE || '', '.tauri', 'atlas-for-rhythia.key');
const env = { ...process.env };
let extra = '';
if (existsSync(key)) {
  env.TAURI_SIGNING_PRIVATE_KEY = key;
  env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ??= '';
} else {
  extra = ` --config "{\\"bundle\\":{\\"createUpdaterArtifacts\\":false}}"`;
}
execSync('npx tauri build' + extra, { cwd: root, stdio: 'inherit', env });

const newest = readdirSync(bundle)
  .filter(f => f.endsWith('-setup.exe'))
  .map(f => join(bundle, f))
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!newest) throw new Error('No installer was built.');

// A running Atlas keeps its exe locked.
spawnSync('taskkill', ['/F', '/IM', 'atlas-for-rhythia.exe'], { stdio: 'ignore' });
console.log(`Installing ${newest}`);
const run = spawnSync(newest, ['/S'], { stdio: 'inherit' });
if (run.status !== 0) throw new Error(`The installer stopped with code ${run.status}.`);

// The build takes minutes: Rhythia may have started meanwhile, and no window goes over the game.
const playing = spawnSync('tasklist', ['/FI', 'IMAGENAME eq rhythia.exe', '/NH'], { encoding: 'utf8' }).stdout?.toLowerCase().includes('rhythia.exe');
if (playing) {
  console.log('Installed. Rhythia is running, so Atlas was not started.');
} else if (existsSync(installed)) {
  spawn(installed, [], { detached: true, stdio: 'ignore' }).unref();
  console.log('Atlas is installed and started.');
} else {
  console.log(`Installed. Atlas was not found at ${installed}; start it from the Start menu.`);
}
