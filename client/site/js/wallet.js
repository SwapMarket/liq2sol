// The Solana wallet: extensions through the Wallet Standard, or a phone through WalletConnect. The app holds no Solana key.
// It only ever asks for a signature and sends the transaction itself, because it must control when a claim's secret goes out.
import { NETWORK } from './config.js';
import { b64ToBytes, bytesToB64 } from './chain.js';

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

export async function connectExtension(w) {
  const { accounts } = await w.features['standard:connect'].connect();
  const account = accounts?.[0] ?? w.accounts[0];
  if (!account) throw new Error(`${w.name} gave no account.`);
  return {
    name: w.name,
    address: account.address,
    sign: async (tx) => {
      const [{ signedTransaction }] = await w.features['solana:signTransaction'].signTransaction({
        account, transaction: tx, chain: NETWORK.solanaChain,
      });
      return new Uint8Array(signedTransaction);
    },
  };
}

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

// Loaded only when someone picks WalletConnect, because it is large and needs Reown's relay.
export async function connectWalletConnect(projectId, showUri) {
  const { SignClient } = await import('../vendor/walletconnect.js');
  const client = await SignClient.init({
    projectId,
    metadata: {
      name: 'liq2sol', description: 'Trustless swaps between USDt on Liquid and USDT on Solana',
      url: new URL('.', location.href).href, icons: [],
    },
  });
  const chain = NETWORK.walletConnectChain;
  const { uri, approval } = await client.connect({
    requiredNamespaces: { solana: { chains: [chain], methods: ['solana_signTransaction'], events: [] } },
  });
  if (uri) showUri(uri);
  const session = await approval();
  const address = session.namespaces.solana.accounts[0].split(':')[2];
  return {
    name: session.peer.metadata.name,
    address,
    sign: async (tx) => {
      const res = await client.request({
        topic: session.topic, chainId: chain,
        request: { method: 'solana_signTransaction', params: { transaction: bytesToB64(tx) } },
      });
      if (res.transaction) return b64ToBytes(res.transaction);
      const signed = tx.slice();
      signed.set(base58Decode(res.signature), 1);
      return signed;
    },
  };
}
