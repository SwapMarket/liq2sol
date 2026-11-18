# Running the liq2sol page yourself

This folder is the taker page at one release, built reproducibly. If https://swapmarket.github.io/liq2sol/ is ever gone or
changed, it runs the same on your own machine, and its Rescue tab picks up any swap from its recovery file.

## Check it

From the release, download `SHA256SUMS`, `SHA256SUMS.asc` and this archive, and SwapMarket's key, `deploy/release-key.gpg` in
https://github.com/SwapMarket/liq2sol. Then:

```sh
gpgv --keyring ./release-key.gpg SHA256SUMS.asc SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
```

Both must succeed. Anyone with Docker can also rebuild the same bytes from the tag: `scripts/build-page.sh <tag> <folder>`.

## Run it

A browser won't run it from a file, so serve it on your own machine, from inside this folder:

```sh
python3 -m http.server 8000 --bind 127.0.0.1
```

Then open http://localhost:8000/. It still talks to the public Liquid and Solana endpoints, and to Nostr relays, as the site does.

- **Swaps and settings stay with the address.** What the site stored in your browser isn't here, since a browser keeps each address's
  storage apart. Bring your recovery files.
- **Rescue** takes a recovery file, reads both chains and offers what the swap still needs: a claim, or a refund once its time has come.
  A Liquid refund needs no wallet. A Solana refund needs an extension wallet, such as Phantom, for its fee.
- **WalletConnect** allows `localhost`, so open it at that name. From any other address it may refuse to connect. Extension wallets work either way.
