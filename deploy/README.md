# Running a test maker on Debian

For an x86-64 machine with Debian 12 or later. This release is for Liquid testnet and Solana devnet only, and both programs refuse to start on mainnet.

## Install

```sh
# Check the download first, against SHA256SUMS from the same release.
sha256sum -c SHA256SUMS
tar xzf liq2sol-maker-*-x86_64-linux.tar.gz
cd liq2sol-maker-*/

sudo useradd --system --home /var/lib/liq2sol --shell /usr/sbin/nologin liq2sol
sudo install -d /opt/liq2sol
sudo install -m 755 liq2sol-signer liq2sol-maker /opt/liq2sol/
sudo install -m 644 liq2sol-signer@.service liq2sol-maker@.service /etc/systemd/system/

# One folder per maker. `first` can be any name.
sudo install -d -m 700 -o liq2sol -g liq2sol /var/lib/liq2sol /var/lib/liq2sol/first /var/lib/liq2sol/first/keys /var/lib/liq2sol/first/run
sudo install -m 600 -o liq2sol -g liq2sol signer.example.json /var/lib/liq2sol/first/signer.json
sudo install -m 600 -o liq2sol -g liq2sol maker.example.json /var/lib/liq2sol/first/maker.json
```

## Keys

Put these in `/var/lib/liq2sol/first/keys`, owned by `liq2sol`, mode 600:

| File | What it is |
|---|---|
| `liquid-wallet.key`, `liquid-blinding.key` | The Liquid wallet |
| `solana.json` | The Solana wallet |
| `swap-seed.key` | Derives each swap's key. Without it, open swaps can't be claimed |
| `nostr.key` | The maker's public identity. Keep it to stay on the vetted list |

Missing Liquid, seed and Nostr keys are generated on first start. `solana.json` must be supplied: make it with `solana-keygen new -o solana.json`, on any machine. Only ever use test keys here.

## Starting with new keys

1. Start the services as below. The signer's log gives the addresses to fund: `journalctl -u liq2sol-signer@first | grep "signer ready"`.
2. Fund them: USDt and L-BTC on Liquid, and stand-in USDT and devnet SOL on Solana. Quotes in a direction wait until the funds that pay it are there.
3. Find its public key in the maker's log, `maker answering quotes as <key>`. A new key isn't on the page's vetted list, so its offers stay hidden unless a taker turns off "Only vetted by SwapMarket", until the key is added to `client/site/js/vetted.js`.

## Moving a running maker

Run one copy at a time. Two copies with the same keys could sign conflicting transactions.

1. Stop the old maker and signer.
2. Copy `keys/`, `signer.db` and `maker.db` to `/var/lib/liq2sol/first`, including any `-wal` and `-shm` files beside them. The signer's ledger is what stops it reusing coins and locking twice, so never start from an empty one with keys that have been used.
3. Set ownership: `sudo chown -R liq2sol:liq2sol /var/lib/liq2sol`.

## Start

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now liq2sol-signer@first liq2sol-maker@first
journalctl -u liq2sol-maker@first -f
```

It is running when the log says `maker answering quotes as <key>`, followed by `maker running`. `/var/lib/liq2sol/first/pages.log` holds what an operator needs to see, such as a rollback alarm. Watch it.

## Several makers on one machine

Each maker is a folder under `/var/lib/liq2sol`, with its own keys, configs and databases. To add `second`, repeat the folder steps with that name, change `first` to `second` in both of its configs, set its prices, and give it its own keys, including its own `nostr.key` and `solana.json`. Then start `liq2sol-signer@second` and `liq2sol-maker@second`.

Two makers must never share keys or a database. Their prices and limits are independent, but they share this machine's IP, and so its API rate limits.

## Back up

`keys/` and both `.db` files. Prices, limits and API endpoints are in `maker.json`. Restart both services after changing a config.

Liquid's public APIs rate-limit by IP. If the logs show `status code 429`, change the endpoints in both configs.
