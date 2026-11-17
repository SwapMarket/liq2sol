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

test('Trust is named outright on every Android browser, and keeps its normal link elsewhere', () => {
  const intent = `intent://link.trustwallet.com/open_url?coin_id=501&url=${URL_}#Intent;scheme=https;package=com.wallet.crypto.trustapp;end`;
  assert.equal(walletLink('Trust', URL_, REF, OPERA), intent);
  assert.equal(walletLink('Trust', URL_, REF, CHROME), intent, 'Chrome on Android too');
  const IPHONE = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1';
  assert.equal(walletLink('Trust', URL_, REF, IPHONE), `https://link.trustwallet.com/open_url?coin_id=501&url=${URL_}`);
  assert.equal(walletLink('Trust', URL_, REF, DESKTOP_OPERA), `https://link.trustwallet.com/open_url?coin_id=501&url=${URL_}`);
});
