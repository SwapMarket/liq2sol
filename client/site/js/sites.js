// SwapMarket's sites. They are on GitHub Pages, and also nsites (NIP-5A) under SwapMarket's key, where each is its own subdomain
// on a gateway: <key in base36><name>.<gateway> for a named site, <npub>.<gateway> for the root site, BTC Swaps. A link opened on
// one of those stays on the same gateway. Anywhere else, including a copy under another key, it goes to GitHub Pages.
export const KEY36 = '1cdzwdkc0uepk5ll8mhpcbyw3xh8gakldbfdq8fphhqnv503bc';
export const NPUB = 'npub1xhklzztd83v34zsf9da2gvfrctj5wcsvsx09er9v26jy7r0uq8yq4vme7d';

// name: 'liq2sol', 'sol2base', 'eth2base', or '' for BTC Swaps.
export function siteUrl(name, hostname = location.hostname) {
  const [label, ...gateway] = hostname.split('.');
  if (gateway.length && (label === NPUB || (label.startsWith(KEY36) && /^[a-z0-9-]{1,13}$/.test(label.slice(KEY36.length))))) {
    return `https://${name ? KEY36 + name : NPUB}.${gateway.join('.')}/`;
  }
  return `https://swapmarket.github.io/${name ? `${name}/` : ''}`;
}
