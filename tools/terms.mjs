// npm run terms: puts TERMS.md and PRIVACY.md into the app's Terms page (src/app.js, between the
// TERMS_TEXT markers), so the app shows exactly the text of the files. Run it after changing either file.
// Kept: ## headings, paragraphs, "- " list items, **bold**; links show their text (a mailto link its address).
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const read = name => readFileSync(root + name, 'utf8').replace(/\r\n/g, '\n');

function blocks(md, title) {
  const out = [['h1', title]];
  // The first lines are the file's own title and a note for whoever edits it.
  const body = md.split('\n').slice(1).join('\n').replace(/^\s*Version of [^\n]*\n/, '');
  for (const part of body.split(/\n{2,}/).map(p => p.trim()).filter(Boolean)) {
    if (part.startsWith('## ')) out.push(['h2', part.slice(3)]);
    else if (part.startsWith('- ')) out.push(['ul', part.split(/\n(?=- )/).map(l => inline(l.slice(2).replace(/\n/g, ' ')))]);
    else out.push(['p', inline(part.replace(/\n/g, ' '))]);
  }
  return out;
}

function inline(text) {
  const plain = text
    .replace(/\[([^\]]+)\]\(mailto:[^)]+\)/g, '$1')
    .replace(/\[([^\]]+)\]\([^)]+\)/g, '$1')
    .replace(/`([^`]+)`/g, '$1');
  return plain;
}

const version = read('TERMS.md').match(/Version of ([^.]+)\./)?.[1] || '';
const text = [...blocks(read('TERMS.md'), 'Terms of use'), ...blocks(read('PRIVACY.md'), 'Privacy')];
const file = root + 'src/app.js';
const app = readFileSync(file, 'utf8');
const nl = app.includes('\r\n') ? '\r\n' : '\n';
const start = '// TERMS_TEXT start (made by npm run terms from TERMS.md and PRIVACY.md; do not edit here)';
const end = '// TERMS_TEXT end';
const a = app.indexOf(start), b = app.indexOf(end);
if (a < 0 || b < a) throw new Error('TERMS_TEXT markers not found in src/app.js');
const block = [start, `const TERMS_VERSION = ${JSON.stringify(version)};`, `const TERMS_TEXT = ${JSON.stringify(text, null, 1).replace(/\n\s*/g, ' ')};`, ''].join(nl);
writeFileSync(file, app.slice(0, a) + block + app.slice(b));
console.log(`Terms page updated (${text.length} blocks, version of ${version}).`);
