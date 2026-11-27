// npm run terms: puts TERMS.md and PRIVACY.md into the app's Terms page and CHANGELOG.md into Settings →
// Changelog (src/app.js, between the TERMS_TEXT markers), so the app shows exactly the text of the files.
// Run it after changing any of them.
// Terms and privacy keep: ## headings, paragraphs, "- " list items, **bold**; links show their text (a mailto
// link its address). The changelog is read as versions (its own note says how it is written).
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

// CHANGELOG.md: [{ version, title, date, sections: [{ cat: 'added'|'changed'|'fixed', items: [{ title, body }] }] }],
// newest first. A mistake in the file stops the script rather than showing a broken changelog.
function versions(md) {
  const out = [];
  let section = null;
  for (const [n, raw] of md.split('\n').entries()) {
    const line = raw.trim();
    const fail = why => { throw new Error(`CHANGELOG.md line ${n + 1}: ${why}`); };
    if (line.startsWith('## ')) {
      const [version, title, date = ''] = line.slice(3).split(' · ').map(x => x.trim());
      if (!version || !title || (version !== 'Next' && !/^\d{4}-\d{2}-\d{2}$/.test(date))) fail('expected "## version · title · YYYY-MM-DD"');
      out.push({ version, title, date, sections: [] });
      section = null;
    } else if (line.startsWith('### ')) {
      const cat = line.slice(4).toLowerCase();
      if (!['added', 'changed', 'fixed'].includes(cat) || !out.length) fail('expected ### Added, Changed or Fixed under a version');
      section = { cat, items: [] };
      out.at(-1).sections.push(section);
    } else if (line.startsWith('- ')) {
      const m = line.slice(2).match(/^([^:]+):\s+(.+)$/);
      if (!m || !section) fail('expected "- Title: what changed" under a ### section');
      section.items.push({ title: inline(m[1].trim()), body: inline(m[2].trim()) });
    }
  }
  return out;
}
const changes = versions(read('CHANGELOG.md'));
const file = root + 'src/app.js';
const app = readFileSync(file, 'utf8');
const nl = app.includes('\r\n') ? '\r\n' : '\n';
const start = '// TERMS_TEXT start (made by npm run terms from TERMS.md, PRIVACY.md and CHANGELOG.md; do not edit here)';
const end = '// TERMS_TEXT end';
const a = app.indexOf('// TERMS_TEXT start'), b = app.indexOf(end);
if (a < 0 || b < a) throw new Error('TERMS_TEXT markers not found in src/app.js');
const one = list => JSON.stringify(list, null, 1).replace(/\n\s*/g, ' ');
const block = [start, `const TERMS_VERSION = ${JSON.stringify(version)};`, `const TERMS_TEXT = ${one(text)};`, `const CHANGELOG = ${one(changes)};`, ''].join(nl);
writeFileSync(file, app.slice(0, a) + block + app.slice(b));
console.log(`Terms page updated (${text.length} blocks, version of ${version}); changelog ${changes.length} versions.`);
