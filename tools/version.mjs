// npm run set-version -- 0.2.0: sets Atlas' version in the three places that hold it (package.json,
// src-tauri/tauri.conf.json, src-tauri/Cargo.toml). The window, the installer and Settings → About
// read it from there. Cargo.lock follows on the next build.
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const next = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(next || '')) {
  console.error('Usage: npm run set-version -- <major.minor.patch>, for example 0.2.0');
  process.exit(1);
}
const root = fileURLToPath(new URL('..', import.meta.url));
const change = (file, from, to) => {
  const path = join(root, file);
  const text = readFileSync(path, 'utf8');
  if (!from.test(text)) throw new Error(`No version found in ${file}`);
  writeFileSync(path, text.replace(from, to));
};
change('package.json', /"version": "[^"]+"/, `"version": "${next}"`);
change('src-tauri/tauri.conf.json', /"version": "[^"]+"/, `"version": "${next}"`);
change('src-tauri/Cargo.toml', /^version = "[^"]+"/m, `version = "${next}"`);
console.log(`Atlas is now ${next}. Commit, then tag it: git tag v${next}`);
