// What this page talks to: test networks by default, or mainnet when it is chosen in Settings. Endpoints and relays can be changed
// there too. The program, mint, assets and genesis are pinned here per network, and the swap logic holds the maker to them.
const TESTNET = {
  name: 'testnet',
  label: 'Liquid testnet + Solana devnet',
  genesis: 'a771da8e52ee6ad581ed1e9a99825e5b3b7992225534eaa2ae23244fe26ab1c1',
  usdt: 'b612eb46313a2cd6ebabd8b7a8eed5696e29898b87a43bff41c94f51acef9d73',
  lbtc: '144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49',
  regtest: false,
  esplora: ['https://blockstream.info/liquidtestnet/api', 'https://liquid.network/liquidtestnet/api'],
  explorer: 'https://blockstream.info/liquidtestnet',
  solanaExplorer: (address) => `https://explorer.solana.com/address/${address}?cluster=devnet`,
  solana: 'https://api.devnet.solana.com',
  heliusTemplate: 'https://devnet.helius-rpc.com/?api-key=',
  solanaChain: 'solana:devnet',
  walletConnectChain: 'solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1',
  programId: 'EcK819kqJT6pHzQVLbzbUnK6ieaiS9JVGubPhC6Q2puU',
  mint: '8BXimW8mCEfhdD1fhv9aoCfQgSj3KxFc52jMAbLvaFwi',
  relays: ['wss://relay.primal.net', 'wss://relay.coinos.pro'],
  // Reown's project id is public by nature, because a static page can't hide it. It is shared with sol2base and eth2base.
  walletConnectProject: 'c001d9e3d0c6308a5ce7e5932d40064a',
  walletNetwork: 'devnet',
  // Only on test networks, so a mainnet build has no Faucet tab.
  faucet: {
    program: 'GAcP2avhVggTqeHSEoKGmGXw4wpJCTasRBjbUfrC1Xpu',
    drip: '100',
    solana: 'https://faucet.solana.com',
    liquid: 'https://liquidtestnet.com/faucet',
  },
  // Trust belongs here on mainnet. Reown's registry lists only Solana mainnet for it, so a devnet build can't use it.
  openInWallets: ['Phantom', 'Solflare'],
  walletConnectHint: 'This build needs Solana devnet: Phantom and Solflare list it, and Trust Wallet lists only mainnet.',
  pollMs: 15000,
  liquidConfirmations: 2,
  // docs/protocol.md section 3.
  rules: {
    l2s_min_blocks: 240, l2s_max_blocks: 360, s2l_min_blocks: 50, s2l_max_blocks: 150,
    max_deadline_slots: 200000,
  },
  claimCutoffSlots: 600,
  claimCutoffBlocks: 10,
  maxSpreadBps: 200,
  // A claim carries the secret, so its fee is set from what is landing now, with a floor, and capped so a bad reading can't drain the wallet.
  priorityFloor: 1000,
  priorityCap: 2000000,
  minSolLamports: 5000000n,
  // A refund's fee coin comes from the user's own wallet, and any amount from this up works, because all of it pays the fee.
  // A refund needs 29 sats at the relay floor. Wallets have their own minimums: Aqua's is 100.
  refundFeeCoinSats: 30,
  // No limit of the page's own on test networks.
  maxSwapUnits: null,
};


// Real funds. Everything here is checked against the chains by `checkGenesis` before the page does anything.
const MAINNET = {
  ...TESTNET,
  name: 'mainnet',
  label: 'Liquid + Solana mainnet',
  // blockstream.info/liquid/api/block-height/0
  genesis: '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003',
  // Tether's USDt, which the Liquid asset registry ties to tether.to, and L-BTC, the policy asset.
  usdt: 'ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2',
  lbtc: '6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d',
  esplora: ['https://blockstream.info/liquid/api', 'https://liquid.network/api'],
  explorer: 'https://blockstream.info/liquid',
  solanaExplorer: (address) => `https://explorer.solana.com/address/${address}`,
  // api.mainnet-beta.solana.com refuses browsers, so the default is the one sol2base uses.
  solana: 'https://solana-rpc.publicnode.com',
  heliusTemplate: 'https://mainnet.helius-rpc.com/?api-key=',
  solanaChain: 'solana:mainnet',
  walletConnectChain: 'solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp',
  // The escrow program, built with its `mainnet` feature from commit 297cd7c and deployed final, with no upgrade authority.
  programId: 'DsgtATJzzejhSZweUsna77kU5vYFqrSojoEYUNmufi6b',
  // Tether's USDT on Solana: the legacy Token program, 6 decimals.
  mint: 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB',
  walletNetwork: 'mainnet-beta',
  faucet: null,
  openInWallets: ['Phantom', 'Solflare', 'Trust'],
  walletConnectHint: '',
  // The most one swap may move, in USDT units of 10^-6: 100 USDT (user's decision, 2026-11-16). The maker's release has the same cap.
  maxSwapUnits: 100000000n,
};

export const NETWORKS = { testnet: TESTNET, mainnet: MAINNET };

// The choice lives outside both networks' storage, and only an exact, available name counts.
const CHOICE = 'liq2sol.network';

export const mainnetAvailable = () => !!MAINNET.programId;

function chosen() {
  try {
    return localStorage.getItem(CHOICE) === 'mainnet' && mainnetAvailable() ? 'mainnet' : 'testnet';
  } catch {
    return 'testnet';
  }
}

export const NETWORK = NETWORKS[chosen()];

// Switching reloads the page, so nothing built for one network is ever used on the other.
export function chooseNetwork(name) {
  if (!NETWORKS[name] || (name === 'mainnet' && !mainnetAvailable())) throw new Error('That network is not available.');
  localStorage.setItem(CHOICE, name);
  location.reload();
}

// Each network keeps its own swaps, settings and logs. Testnet keeps the names it always had.
export const storageKey = (base) => (NETWORK.name === 'mainnet' ? `${base}.mainnet` : base);

const SOLANA_MAINNET_GENESIS = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const LIQUID_MAINNET_GENESIS = '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003';

// Hold the page to its network, whatever the endpoints in Settings say: mainnet means both chains are mainnet, and a test
// network means neither is. `network` is the configuration in use.
export function checkGenesis(network, liquidGenesis, solanaGenesis) {
  const main = [liquidGenesis === LIQUID_MAINNET_GENESIS, solanaGenesis === SOLANA_MAINNET_GENESIS];
  if (network.name === 'mainnet') {
    if (!main[0] || !main[1]) throw new Error('Refusing to run: this is set to mainnet, and an endpoint is on another network.');
    if (!network.programId) throw new Error('Refusing to run: the escrow program is not on mainnet yet.');
  } else if (main[0] || main[1]) {
    throw new Error('Refusing to run: an endpoint is on mainnet, and this is set to a test network. Switch in Settings.');
  }
}
