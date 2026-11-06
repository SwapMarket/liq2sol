// What this build talks to. Testnet only: this project never touches mainnet. Endpoints and relays can be changed in Settings.
// The program, mint, assets and genesis are pinned here, and the swap logic holds the maker to them.
export const NETWORK = {
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
};

const SOLANA_MAINNET_GENESIS = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const LIQUID_MAINNET_GENESIS = '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003';

export function refuseMainnet(liquidGenesis, solanaGenesis) {
  if (liquidGenesis === LIQUID_MAINNET_GENESIS || solanaGenesis === SOLANA_MAINNET_GENESIS) {
    throw new Error('Refusing to run: an endpoint is on mainnet. This build is for test networks only.');
  }
}
