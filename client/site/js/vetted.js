// Makers vetted by SwapMarket, by network. To change this list, change this file: its history is the record of who was added and when.
// A key is the maker's full Nostr public key in lowercase hex, as "Maker answering quotes as ..." prints it. Offers are signed
// by that key, so a maker can't take on a listed key without holding it. Being listed says a maker is expected to follow through.
// It doesn't protect funds, because a swap's safety never depends on the maker.
export const VETTED_MAKERS = {
  testnet: [
    { key: 'de5ca52f51bc239c35d4e51344e5b9f0d375349b81bad250ffb8c4079a2cd4c7', name: 'SwapMarket test maker' },
    { key: 'fb14836a5f084c36c2ec88f07c6016400ed23422b1fc5b77c0973df2e0207c84', name: 'SwapMarket VPS maker' },
    { key: '5d9a02ba557fc7cad0df76416302e4e6d0348a766fcbf21ce8254f3af3322e11', name: 'SwapMarket VPS maker 2' },
  ],
};

export const vettedMaker = (network, key) => (VETTED_MAKERS[network] ?? []).find((m) => m.key === key) ?? null;
