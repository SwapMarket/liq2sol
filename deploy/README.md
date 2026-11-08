# Running a test maker on Debian

For Debian 12 or later, on x86-64, or on arm64 such as a Raspberry Pi 4 or 5 with Raspberry Pi OS 64-bit (Bookworm). A 32-bit OS won't do. This release is for Liquid testnet and Solana devnet only, and both programs refuse to start on mainnet.

## Install

```sh
# Check the download first: SHA256SUMS is signed by SwapMarket, and lists the archive's checksum.
gpgv --keyring ./release-key.gpg SHA256SUMS.asc SHA256SUMS   # release-key.gpg from the repo's deploy/ folder
sha256sum -c --ignore-missing SHA256SUMS
tar xzf liq2sol-maker-*-linux.tar.gz
cd liq2sol-maker-*/

sudo useradd --system --home /var/lib/liq2sol --shell /usr/sbin/nologin liq2sol
sudo install -d /opt/liq2sol
sudo install -m 755 liq2sol-signer liq2sol-maker update.sh /opt/liq2sol/
sudo install -m 644 release-key.gpg /opt/liq2sol/
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

Each maker is a folder under `/var/lib/liq2sol`, with its own keys, configs and databases. Paths in a config are relative to that config, so the example configs work unchanged in any folder. To add `second`, repeat the folder steps with that name, set its prices, and give it its own `solana.json`. Its other keys are generated. Then start `liq2sol-signer@second` and `liq2sol-maker@second`.

Two makers must never share keys or a database. Their prices and limits are independent, but they share this machine's IP, and so its API rate limits.

## Earnings

```sh
sudo -u liq2sol /opt/liq2sol/liq2sol-maker report /var/lib/liq2sol/first/maker.json
```

It prints, per day and direction, swaps, volume and USDT spread earned, then L-BTC and SOL spent, then what the wallets hold now. It is safe while the maker runs. An escrow's rent counts as spent until closing it brings it back. Costs are recorded when a transaction is sent, so one that never lands still counts. Treat them as close estimates, and the balances, which come from the running signer, as the final word.

## Update

```sh
sudo /opt/liq2sol/update.sh          # the newest release
sudo /opt/liq2sol/update.sh v0.06    # or a given one
sudo /opt/liq2sol/update.sh --rollback
```

It downloads the release, checks it against its `SHA256SUMS`, stops every running maker and its signer, installs the binaries and service files, and starts them again, then waits for each to say `maker running`. Configs, keys and databases are left alone. The old binaries are kept in `/opt/liq2sol/previous` for `--rollback`, and the new example configs go to `/opt/liq2sol/examples`, to show any new settings. A swap in progress carries on after the restart.

It installs nothing unless the release's `SHA256SUMS` is signed by the key in `/opt/liq2sol/release-key.gpg`, SwapMarket's `E7947F8DBC700926D8A8175101151A25AAE22F55`. Releases are built on GitHub and signed by hand afterwards, so a new one can be unsigned for a while, and the script says so. Neither a GitHub account nor its build machines hold the key, so neither can ship something this accepts.

A machine with v0.05 or earlier has neither the script nor the key. Get both, check the key file (`sha256sum release-key.gpg` must print `28846ef1fcfac9b8b1b5c7b34bf14844db01488436f98928f3a70b2507dbd8b1`, or `gpg --show-keys release-key.gpg` the fingerprint above), and run it once:

```sh
curl -fsSLO https://raw.githubusercontent.com/SwapMarket/liq2sol/main/deploy/update.sh
curl -fsSLO https://raw.githubusercontent.com/SwapMarket/liq2sol/main/deploy/release-key.gpg
sudo bash update.sh
```

## Back up

`keys/` and both `.db` files. Prices, limits and API endpoints are in `maker.json`. Restart both services after changing a config.

Liquid's public APIs rate-limit by IP. If the logs show `status code 429`, change the endpoints in both configs.
