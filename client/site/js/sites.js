// SwapMarket's sites. They are on GitHub Pages, and also nsites (NIP-5A) under SwapMarket's key, where each is its own subdomain
// on a gateway: <key in base36><name>.<gateway> for a named site, <npub>.<gateway> for the root site, BTC Swaps. A link opened on
// one of those stays on the same gateway. Anywhere else, including a copy under another key, it goes to GitHub Pages.
export const KEY36 = '1cdzwdkc0uepk5ll8mhpcbyw3xh8gakldbfdq8fphhqnv503bc';
export const NPUB = 'npub1xhklzztd83v34zsf9da2gvfrctj5wcsvsx09er9v26jy7r0uq8yq4vme7d';

// The gateway the footers link to for the copy on Nostr. nsite.lol kept serving liq2sol's first manifest and never picked up a later
// one (2026-11-19), while nwb.tf showed each new manifest within minutes.
export const NSITE_GATEWAY = 'nwb.tf';

// A site's address on that gateway, wherever the page is opened.
export function nsiteUrl(name, gateway = NSITE_GATEWAY) {
  return `https://${name ? KEY36 + name : NPUB}.${gateway}/`;
}

// The gateway this page is served from, when it is one of SwapMarket's nsites; null anywhere else. A page on Nostr already needs no
// link to its copy on Nostr.
export function nsiteGateway(hostname = location.hostname) {
  const [label, ...gateway] = hostname.split('.');
  const ours = label === NPUB || (label.startsWith(KEY36) && /^[a-z0-9-]{1,13}$/.test(label.slice(KEY36.length)));
  return gateway.length && ours ? gateway.join('.') : null;
}

// name: 'liq2sol', 'sol2base', 'eth2base', 'sol2tron', or '' for BTC Swaps.
export function siteUrl(name, hostname = location.hostname) {
  const gateway = nsiteGateway(hostname);
  if (gateway) return `https://${name ? KEY36 + name : NPUB}.${gateway}/`;
  return `https://swapmarket.github.io/${name ? `${name}/` : ''}`;
}
