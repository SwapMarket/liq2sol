// The Solana wallet: extensions through the Wallet Standard, or a phone through WalletConnect. The app holds no Solana key.
// It only ever asks for a signature and sends the transaction itself, because it must control when a claim's secret goes out.
import { NETWORK } from './config.js';
import { walletLink } from './openin.js';
import { b64ToBytes, bytesToB64 } from './chain.js';
import { asV0, walletConnectTransaction } from './txformat.js';

const found = new Set();

function register(...wallets) {
  for (const w of wallets) found.add(w);
  return () => wallets.forEach((w) => found.delete(w));
}

// The Wallet Standard's handshake: wallets answer either event, whichever side loads first.
window.addEventListener('wallet-standard:register-wallet', ({ detail }) => detail({ register }));
window.dispatchEvent(new CustomEvent('wallet-standard:app-ready', { detail: { register } }));

export function extensionWallets() {
  return [...found].filter((w) => w.features['standard:connect'] && w.features['solana:signTransaction']);
}

// Trust's injected wallet signs a legacy transaction through code that expects an old-style object, and fails with
// "e.serializeMessage is not a function" (trustwallet/trust-web3-provider, packages/solana/MobileAdapter.ts, legacySign). It
// handles v0 ones, so it gets those.
const wantsV0 = (w) => w.name === 'Trust' || Object.keys(w.features).some((f) => f.startsWith('trust:'));


// silent asks the wallet to reconnect only if it already trusts this page, with no prompt.
export async function connectExtension(w, silent = false) {
  const { accounts } = await w.features['standard:connect'].connect(silent ? { silent: true } : undefined);
  const account = accounts?.[0] ?? w.accounts[0];
  if (!account) throw new Error(`${w.name} gave no account.`);
  return {
    name: w.name,
    address: account.address,
    sign: async (tx) => {
      const [{ signedTransaction }] = await w.features['solana:signTransaction'].signTransaction({
        account, transaction: wantsV0(w) ? asV0(tx) : tx, chain: NETWORK.solanaChain,
      });
      return new Uint8Array(signedTransaction);
    },
  };
}

export const openInWallets = (url, ref) => NETWORK.openInWallets.map((name) => ({ name, href: walletLink(name, url, ref, navigator.userAgent) }));

const B58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';

function base58Decode(s) {
  const out = [0];
  for (const c of s) {
    let carry = B58.indexOf(c);
    if (carry < 0) throw new Error('Bad signature from the wallet.');
    for (let i = 0; i < out.length; i++) {
      carry += out[i] * 58;
      out[i] = carry & 0xff;
      carry >>= 8;
    }
    while (carry) {
      out.push(carry & 0xff);
      carry >>= 8;
    }
  }
  for (const c of s) {
    if (c !== '1') break;
    out.push(0);
  }
  return Uint8Array.from(out.reverse());
}

// Loaded only when someone picks WalletConnect, or used it last time, because it is large and needs Reown's relay.
// One client for the page, per project. Each SignClient.init starts its own WalletConnect core on the same storage and relay, so with
// two (one restoring the last session at load, one connecting) the wallet's answer could reach the one that hadn't asked. It dropped
// it ("No matching key. proposal"), and the page never saw the wallet connect. The promise is kept, so calls that overlap share it.
const clients = new Map();
function walletConnectClient(projectId) {
  if (!clients.has(projectId)) {
    const made = import('../vendor/walletconnect.js').then(({ SignClient }) => SignClient.init({
      projectId,
      metadata: {
        name: 'liq2sol', description: 'Trustless swaps between USDt on Liquid and USDT on Solana',
        url: new URL('.', location.href).href, icons: [],
      },
    }));
    // A failed start, such as the relay being unreachable, can be tried again.
    made.catch(() => clients.delete(projectId));
    clients.set(projectId, made);
  }
  return clients.get(projectId);
}

function walletConnectWallet(client, session) {
  const chain = NETWORK.walletConnectChain;
  const account = session.namespaces.solana?.accounts?.find((a) => a.startsWith(`${chain}:`));
  // Since 2.2x WalletConnect asks for every namespace as optional, so a wallet can approve without sharing a Solana account.
  if (!account) throw new Error(`${session.peer?.metadata?.name ?? 'The wallet'} connected without sharing a Solana account on this network. ${NETWORK.walletConnectHint}`);
  const address = account.split(':')[2];
  return {
    name: session.peer.metadata.name,
    address,
    sign: async (tx) => {
      // What the wallet signs, which a signature alone must go back into.
      const sent = walletConnectTransaction(session.peer?.metadata?.name, tx);
      const res = await client.request({
        topic: session.topic, chainId: chain,
        request: { method: 'solana_signTransaction', params: { transaction: bytesToB64(sent) } },
      });
      if (res.transaction) return b64ToBytes(res.transaction);
      const signed = sent.slice();
      signed.set(base58Decode(res.signature), 1);
      return signed;
    },
  };
}

export async function connectWalletConnect(projectId, showUri) {
  const client = await walletConnectClient(projectId);
  const { uri, approval } = await client.connect({
    optionalNamespaces: { solana: { chains: [NETWORK.walletConnectChain], methods: ['solana_signTransaction'], events: [] } },
  });
  if (uri) showUri(uri);
  const session = await approval().catch((e) => {
    throw new Error(`Your wallet refused the connection. ${NETWORK.walletConnectHint} (${e?.message ?? e})`);
  });
  return walletConnectWallet(client, session);
}

// A session the phone approved earlier and hasn't ended, with no QR code. Null if there is none.
export async function restoreWalletConnect(projectId) {
  const client = await walletConnectClient(projectId);
  const session = client.session.getAll()
    .filter((x) => x.expiry * 1000 > Date.now() && x.namespaces.solana?.accounts.some((a) => a.startsWith(`${NETWORK.walletConnectChain}:`)))
    .sort((a, b) => b.expiry - a.expiry)[0];
  return session ? walletConnectWallet(client, session) : null;
}
