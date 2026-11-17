// node --test client/test/network.test.mjs
// Choosing a network: test networks unless mainnet is chosen and available, separate storage, and both chains held to the choice.
import test from 'node:test';
import assert from 'node:assert/strict';

const LIQUID_MAIN = '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003';
const SOLANA_MAIN = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const DEVNET = 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG';

// A fresh copy of the module for each stored state, as a page load gives.
let loads = 0;
async function load(stored, swaps) {
  const items = new Map(stored === undefined ? [] : [['liq2sol.network', stored]]);
  if (swaps) items.set('liq2sol.swaps', JSON.stringify(swaps));
  globalThis.localStorage = { getItem: (k) => items.get(k) ?? null, setItem: (k, v) => items.set(k, v) };
  let reloaded = 0;
  globalThis.location = { reload: () => { reloaded += 1; } };
  const config = await import(`../site/js/config.js?load=${loads += 1}`);
  return { ...config, items, reloaded: () => reloaded };
}

test('mainnet is the default, and only an exact stored choice gives test networks', async () => {
  for (const stored of [undefined, 'mainnet', 'Testnet', 'testnet ', '', 'null']) {
    const c = await load(stored);
    assert.equal(c.NETWORK.name, 'mainnet', String(stored));
    assert.equal(c.storageKey('liq2sol.swaps'), 'liq2sol.swaps.mainnet', 'mainnet has storage of its own');
  }
  const t = await load('testnet');
  assert.equal(t.NETWORK.name, 'testnet');
  assert.equal(t.storageKey('liq2sol.swaps'), 'liq2sol.swaps', 'testnet keeps its old storage');
  t.chooseNetwork('mainnet');
  assert.equal(t.items.get('liq2sol.network'), 'mainnet');
  assert.equal(t.reloaded(), 1, 'switching reloads the page');
  assert.throws(() => t.chooseNetwork('regtest'), /not available/);
});

test('a browser with test swaps from before mainnet stays on test networks, and remembers it', async () => {
  const c = await load(undefined, { abc: { id: 'abc', state: 'fund' } });
  assert.equal(c.NETWORK.name, 'testnet');
  assert.equal(c.items.get('liq2sol.network'), 'testnet');
  // A stored choice wins over that.
  assert.equal((await load('mainnet', { abc: {} })).NETWORK.name, 'mainnet');
  // An empty or broken store of swaps doesn't count.
  assert.equal((await load(undefined, {})).NETWORK.name, 'mainnet');
});

test('a browser with no storage gets mainnet, the default', async () => {
  globalThis.localStorage = { getItem: () => { throw new Error('blocked'); } };
  const c = await import(`../site/js/config.js?load=${loads += 1}`);
  assert.equal(c.NETWORK.name, 'mainnet');
});

test('mainnet pins the real ids, has no test tools, and caps a swap at 100 USDT', async () => {
  const { NETWORKS } = await load();
  const { mainnet, testnet } = NETWORKS;
  assert.equal(mainnet.genesis, LIQUID_MAIN);
  assert.equal(mainnet.usdt, 'ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2');
  assert.equal(mainnet.lbtc, '6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d');
  assert.equal(mainnet.mint, 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB');
  assert.equal(mainnet.walletConnectChain, `solana:${SOLANA_MAIN.slice(0, 32)}`, 'CAIP-2: the first 32 characters of the genesis');
  assert.equal(mainnet.faucet, null);
  assert.equal(mainnet.regtest, false);
  assert.equal(mainnet.maxSwapUnits, 100000000n);
  assert.equal(testnet.maxSwapUnits, null);
  // Nothing of testnet's may leak through the shared defaults.
  for (const key of ['genesis', 'usdt', 'lbtc', 'mint', 'programId', 'solana', 'heliusTemplate', 'explorer', 'walletConnectChain', 'solanaChain', 'walletNetwork', 'label', 'name']) {
    assert.notEqual(mainnet[key], testnet[key], key);
  }
  assert.ok(mainnet.esplora.every((url) => !/testnet/.test(url)) && !/devnet/.test(mainnet.solana + mainnet.heliusTemplate + mainnet.solanaExplorer('x')));
});

test('both chains must agree with the network, in both directions', async () => {
  const { NETWORKS, checkGenesis } = await load();
  const main = { ...NETWORKS.mainnet, programId: 'deployed' };
  assert.doesNotThrow(() => checkGenesis(NETWORKS.testnet, NETWORKS.testnet.genesis, DEVNET));
  assert.doesNotThrow(() => checkGenesis(main, LIQUID_MAIN, SOLANA_MAIN));
  assert.throws(() => checkGenesis(NETWORKS.testnet, LIQUID_MAIN, DEVNET), /mainnet/);
  assert.throws(() => checkGenesis(NETWORKS.testnet, NETWORKS.testnet.genesis, SOLANA_MAIN), /mainnet/);
  assert.throws(() => checkGenesis(main, LIQUID_MAIN, DEVNET), /another network/);
  assert.throws(() => checkGenesis(main, NETWORKS.testnet.genesis, SOLANA_MAIN), /another network/);
  assert.throws(() => checkGenesis({ ...main, programId: null }, LIQUID_MAIN, SOLANA_MAIN), /not on mainnet yet/);
});
