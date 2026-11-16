// node --test client/test/network.test.mjs
// Choosing a network: test networks unless mainnet is chosen and available, separate storage, and both chains held to the choice.
import test from 'node:test';
import assert from 'node:assert/strict';

const LIQUID_MAIN = '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003';
const SOLANA_MAIN = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const DEVNET = 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG';

// A fresh copy of the module for each stored choice, as a page load gives.
let loads = 0;
async function load(stored) {
  const items = new Map(stored === undefined ? [] : [['liq2sol.network', stored]]);
  globalThis.localStorage = { getItem: (k) => items.get(k) ?? null, setItem: (k, v) => items.set(k, v) };
  let reloaded = 0;
  globalThis.location = { reload: () => { reloaded += 1; } };
  const config = await import(`../site/js/config.js?load=${loads += 1}`);
  return { ...config, items, reloaded: () => reloaded };
}

test('test networks are the default, and stay it for anything but an exact, available choice', async () => {
  for (const stored of [undefined, 'testnet', 'Mainnet', 'mainnet ', '', 'null']) {
    const c = await load(stored);
    assert.equal(c.NETWORK.name, 'testnet', String(stored));
    assert.equal(c.storageKey('liq2sol.swaps'), 'liq2sol.swaps', 'testnet keeps its old storage');
  }
  // Mainnet has no escrow program yet, so even a stored choice of it gives test networks.
  const c = await load('mainnet');
  assert.equal(c.mainnetAvailable(), !!c.NETWORKS.mainnet.programId);
  if (!c.mainnetAvailable()) {
    assert.equal(c.NETWORK.name, 'testnet');
    assert.throws(() => c.chooseNetwork('mainnet'), /not available/);
    assert.equal(c.reloaded(), 0);
  }
  assert.throws(() => c.chooseNetwork('regtest'), /not available/);
});

test('a browser with no storage still gets test networks', async () => {
  globalThis.localStorage = { getItem: () => { throw new Error('blocked'); } };
  const c = await import(`../site/js/config.js?load=${loads += 1}`);
  assert.equal(c.NETWORK.name, 'testnet');
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
