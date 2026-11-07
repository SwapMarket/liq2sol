// Links that open this page in a wallet app's own browser, which gives the page a wallet to talk to. Pure, so it can be tested.
const WALLETS = {
  Phantom: { link: (url, ref) => `phantom.app/ul/browse/${url}?ref=${ref}`, android: 'app.phantom' },
  Solflare: { link: (url, ref) => `solflare.com/ul/v1/browse/${url}?ref=${ref}`, android: 'com.solflare.mobile' },
  Trust: { link: (url) => `link.trustwallet.com/open_url?coin_id=501&url=${url}` },
};

// Opera on Android sent Solflare's normal link to Google Play, with the app installed. Chrome opened the app. So only Opera names
// the app outright, with an Android intent link (github.com/GoogleChrome/multi-device, android/intents.html).
export const isOperaOnAndroid = (userAgent) => /Android/.test(userAgent) && /\bOP[RTX]\//.test(userAgent);

export function walletLink(name, url, ref, userAgent) {
  const wallet = WALLETS[name];
  const target = wallet.link(url, ref);
  if (isOperaOnAndroid(userAgent) && wallet.android) return `intent://${target}#Intent;scheme=https;package=${wallet.android};end`;
  return `https://${target}`;
}
