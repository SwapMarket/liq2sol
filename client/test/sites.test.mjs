// node --test client/test/sites.test.mjs
// Links between SwapMarket's sites stay on the nsite gateway they were opened from, and go to GitHub Pages from anywhere else.
import test from 'node:test';
import assert from 'node:assert/strict';
import { KEY36, NPUB, NSITE_GATEWAY, nsiteUrl, siteUrl } from '../site/js/sites.js';

test('on GitHub Pages and elsewhere, links go to GitHub Pages', () => {
  for (const host of ['swapmarket.github.io', '127.0.0.1', 'localhost', 'example.com']) {
    assert.equal(siteUrl('sol2base', host), 'https://swapmarket.github.io/sol2base/');
    assert.equal(siteUrl('', host), 'https://swapmarket.github.io/');
  }
});

test('on an nsite under SwapMarket\'s key, links stay on that gateway', () => {
  for (const host of [`${KEY36}liq2sol.nsite.lol`, `${KEY36}eth2base.nsite.lol`, `${NPUB}.nsite.lol`]) {
    assert.equal(siteUrl('sol2base', host), `https://${KEY36}sol2base.nsite.lol/`);
    assert.equal(siteUrl('liq2sol', host), `https://${KEY36}liq2sol.nsite.lol/`);
    assert.equal(siteUrl('', host), `https://${NPUB}.nsite.lol/`);
  }
  assert.equal(siteUrl('eth2base', `${KEY36}liq2sol.nwb.tf`), `https://${KEY36}eth2base.nwb.tf/`);
});

test('another key, or a lookalike, does not count', () => {
  const other = '0'.repeat(50);
  for (const host of [`${other}liq2sol.nsite.lol`, `${KEY36.slice(0, 49)}xliq2sol.nsite.lol`, `${NPUB.slice(0, -1)}x.nsite.lol`,
    `${KEY36}liq2sol`, `${KEY36}UPPERCASE.nsite.lol`, `${KEY36}waytoolongname.nsite.lol`, `x${KEY36}liq2sol.nsite.lol`]) {
    assert.equal(siteUrl('sol2base', host), 'https://swapmarket.github.io/sol2base/', host);
  }
});

test('the copy on Nostr is on the chosen gateway, from anywhere', () => {
  assert.equal(NSITE_GATEWAY, 'nwb.tf');
  assert.equal(nsiteUrl('liq2sol'), `https://${KEY36}liq2sol.nwb.tf/`);
  assert.equal(nsiteUrl(''), `https://${NPUB}.nwb.tf/`);
  assert.equal(nsiteUrl('sol2tron', 'nsite.lol'), `https://${KEY36}sol2tron.nsite.lol/`);
  for (const name of ['liq2sol', 'sol2base', 'eth2base', 'sol2tron']) assert.ok(`${KEY36}${name}`.length <= 63, 'a DNS label');
});
