// node client/stamp.mjs <site folder> <build id>
// Makes index.html the one file to trust: it pins every script, the style sheet and the WebAssembly by hash, so a page whose
// index.html matches the signed release can't load anything else. Run on a copy that is about to be published.
//
// Scripts are pinned with an import map's "integrity" entries, which cover static and dynamic imports alike. The import map is an
// inline script, so the CSP allows it by its own hash and nothing else inline. The WebAssembly is checked by app.js against the
// hash in a meta tag, before it is instantiated. Browsers without import-map integrity still run the page, unchecked.
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const [site, build] = process.argv.slice(2);
if (!site || !build) {
  console.error('usage: node client/stamp.mjs <site folder> <build id>');
  process.exit(2);
}

const hash = (algo, data, enc = 'base64') => createHash(algo).update(data).digest(enc);
const files = (dir) => readdirSync(dir).flatMap((name) => {
  const full = path.join(dir, name);
  return statSync(full).isDirectory() ? files(full) : [full];
});

const page = path.join(site, 'index.html');
let html = readFileSync(page, 'utf8');
if (!html.includes('__BUILD__') || html.includes('type="importmap"')) {
  console.error('index.html is already stamped, or not this page');
  process.exit(1);
}
html = html.replaceAll('__BUILD__', build);

// Each file is asked for at an address carrying its own hash, so a browser can never pair this index.html with a copy of a file it
// cached from an earlier build: GitHub Pages lets browsers keep every file for 10 minutes, each on its own. A file that didn't
// change keeps its address, and stays cached. The import map sends every module, imported or dynamic, to its versioned address.
const scripts = files(site).filter((f) => f.endsWith('.js')).sort();
const rel = (f) => `./${path.relative(site, f).split(path.sep).join('/')}`;
const version = (data) => hash('sha256', data, 'hex').slice(0, 16);
const imports = Object.fromEntries(scripts.map((f) => [rel(f), `${rel(f)}?v=${version(readFileSync(f))}`]));
const integrity = Object.fromEntries(scripts.map((f) => [imports[rel(f)], `sha384-${hash('sha384', readFileSync(f))}`]));
const importMap = JSON.stringify({ imports, integrity });
const wasm = readFileSync(path.join(site, 'wasm', 'liq2sol_client_wasm_bg.wasm'));

const swap = (from, to) => {
  if (!html.includes(from)) {
    console.error(`index.html has no ${from}`);
    process.exit(1);
  }
  html = html.replace(from, to);
};
swap("script-src 'self'", `script-src 'self' 'sha256-${hash('sha256', importMap)}'`);
const css = readFileSync(path.join(site, 'style.css'));
swap('<link rel="stylesheet" href="style.css">',
  `<link rel="stylesheet" href="style.css?v=${version(css)}" integrity="sha384-${hash('sha384', css)}">`);
// The entry script's own tag isn't mapped, so it names its versioned address itself.
swap('<script type="module" src="js/app.js"></script>',
  `<meta name="wasm-sha256" content="${hash('sha256', wasm, 'hex')}">\n<script type="importmap">${importMap}</script>\n`
  + `<script type="module" src="${imports['./js/app.js']}" integrity="${integrity[imports['./js/app.js']]}"></script>`);
writeFileSync(page, html);
console.log(`stamped ${scripts.length} scripts, the style sheet and the WebAssembly; index.html sha256 ${hash('sha256', html, 'hex')}`);
