// node --test client/test/openin.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { isOperaOnAndroid, walletLink } from '../site/js/openin.js';

const CHROME = 'Mozilla/5.0 (Linux; Android 14; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36';
const OPERA = `${CHROME} OPR/85.0.0.0`;
const DESKTOP_OPERA = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36 OPR/139.0.0.0';
const URL_ = encodeURIComponent('https://example.org/a');
const REF = encodeURIComponent('https://example.org');

test('only Opera on Android is treated as Opera on Android', () => {
  assert.equal(isOperaOnAndroid(OPERA), true);
  assert.equal(isOperaOnAndroid(CHROME), false, 'Chrome’s normal link works, so it keeps it');
  assert.equal(isOperaOnAndroid(DESKTOP_OPERA), false);
});

test('normal browsers keep the wallets’ own links', () => {
  assert.equal(walletLink('Solflare', URL_, REF, CHROME), `https://solflare.com/ul/v1/browse/${URL_}?ref=${REF}`);
  assert.equal(walletLink('Phantom', URL_, REF, CHROME), `https://phantom.app/ul/browse/${URL_}?ref=${REF}`);
});

test('Opera on Android names the app, keeping the same path and encoded page', () => {
  assert.equal(walletLink('Solflare', URL_, REF, OPERA), `intent://solflare.com/ul/v1/browse/${URL_}?ref=${REF}#Intent;scheme=https;package=com.solflare.mobile;end`);
  assert.equal(walletLink('Phantom', URL_, REF, OPERA), `intent://phantom.app/ul/browse/${URL_}?ref=${REF}#Intent;scheme=https;package=app.phantom;end`);
});

test('a wallet with no known app id keeps its normal link', () => {
  assert.ok(walletLink('Trust', URL_, REF, OPERA).startsWith('https://link.trustwallet.com/'));
});
