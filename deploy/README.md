# Running a maker on Debian

For Debian 12 or later, on x86-64, or on arm64 such as a Raspberry Pi 4 or 5 with Raspberry Pi OS 64-bit (Bookworm). A 32-bit OS won't do. A maker runs on mainnet or on the test networks (Liquid testnet and Solana devnet), as its configs say. For mainnet, read "A mainnet maker" below first.

## Install with one script

`install.sh` does everything below for one maker: it downloads the newest release, installs it only if SwapMarket's signature checks out, sets up the maker's folder from the example configs, makes its Solana wallet on this machine, starts it, and prints the addresses to fund and its Nostr key.

```sh
sudo apt install curl gpgv openssl
wget https://raw.githubusercontent.com/SwapMarket/liq2sol/main/deploy/install.sh
sudo bash install.sh main --helius YOUR_HELIUS_KEY --page-to npub1... --page-relay wss://your.dm.relay
```

`main` is the maker's folder name. Add `testnet` after it for a test maker. `--page-to` and `--page-relay` are optional (see "Pages on your phone"). With your own Elements node on the same machine, add `--elements-rpc USER:PASSWORD` (its `rpcuser` and `rpcpassword`), and `--elements-cli PATH` or `--elements-port N` if they aren't `/usr/local/bin/elements-cli` and the default port. The script checks the node answers on the right chain, points both configs at it, keeps a public API as the maker's second source, and turns on `fresh_addresses`. If the node isn't answering yet, the maker is set up but not started. Then back up `/var/lib/liq2sol/main/keys`, fund the addresses, set prices in `maker.json`, and restart the maker. It never touches a maker folder that exists. Updates are `sudo /opt/liq2sol/update.sh`.

## Your own Elements node

A maker reads Liquid better from its own node: public APIs rate-limit, and `fresh_addresses` needs one. `install-elements.sh`
downloads Elements, installs it only if the release's signature by Elements' release key checks out, and runs it as
`elementsd.service`, as user `elements`. Run it before `install.sh`, which then takes the password it prints:

```sh
sudo apt install curl gpgv
wget https://raw.githubusercontent.com/SwapMarket/liq2sol/main/deploy/install-elements.sh
wget https://raw.githubusercontent.com/SwapMarket/liq2sol/main/deploy/elements-release-key.gpg
sudo bash install-elements.sh mainnet --datadir /mnt/ssd/elements
```

Leave out `--datadir` for `/var/lib/elements`. A first sync takes hours, or days on a Raspberry Pi. Install the maker meanwhile
with `--no-start`, and start it once the node has caught up: a node still syncing answers with an old chain. The script never
replaces a node already set up.

A node you set up yourself needs `txindex=1`, because the maker looks up confirmed transactions by id, and `validatepegin=0` unless
it has a bitcoind. Without `txindex`, adding it later rebuilds the index, which takes a while.

## Install by hand

```sh
# Download a release: the archive for this machine, the checksums, SwapMarket's signature of them, and SwapMarket's key.
# The latest release's tag, as update.sh finds it. Or set one yourself, such as TAG=v0.13.
TAG=$(wget -qO- "https://api.github.com/repos/SwapMarket/liq2sol/releases?per_page=1" | sed -n '/"tag_name"/{s/.*"tag_name": *"\([^"]*\)".*/\1/p;q;}')
echo "$TAG"
ARCH=$(uname -m)   # x86_64, or aarch64 on a Raspberry Pi
mkdir -p ~/liq2sol-download && cd ~/liq2sol-download
wget "https://github.com/SwapMarket/liq2sol/releases/download/$TAG/liq2sol-maker-$TAG-$ARCH-linux.tar.gz" \
     "https://github.com/SwapMarket/liq2sol/releases/download/$TAG/SHA256SUMS" \
     "https://github.com/SwapMarket/liq2sol/releases/download/$TAG/SHA256SUMS.asc" \
     "https://raw.githubusercontent.com/SwapMarket/liq2sol/$TAG/deploy/release-key.gpg"
sha256sum release-key.gpg   # must be 28846ef1fcfac9b8b1b5c7b34bf14844db01488436f98928f3a70b2507dbd8b1

# Check the download first: SHA256SUMS is signed by SwapMarket, and lists the archive's checksum.
gpgv --keyring ./release-key.gpg SHA256SUMS.asc SHA256SUMS   # sudo apt install gpgv, if it is missing
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

Missing Liquid, seed and Nostr keys are generated on first start. `solana.json` must be supplied: make it with `solana-keygen new -o solana.json`. Never use the same keys on mainnet and on a test network.

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

## Pages on your phone

Pages can also reach you as Nostr private messages (NIP-17), which apps such as Amethyst, 0xchat and Primal show. Add this to the top level of `maker.json`, with your own key and the DM relays your app reads (its kind 10050 list, in the app's relay settings):

```json
"page_to": { "nostr": "npub1…", "relays": ["wss://relay.example"] },
```

Then check it, and restart the maker:

```sh
sudo -u liq2sol /opt/liq2sol/liq2sol-maker test-page /var/lib/liq2sol/first/maker.json
sudo systemctl restart liq2sol-maker@first
```

Messages come from the maker's own Nostr key, so name it in your app. `pages.log` stays the record: a message no relay takes is lost.

The maker also pages when its wallets run low: SOL-USDT or L-USDt below the largest trade it offers, or SOL or L-BTC for fewer than 10 swaps. It pages once when that starts, then every 6 hours while it lasts. To change these, add any of:

```json
"balance_alarms": { "gas_swaps": 10, "lamports_per_swap": 3000000, "lbtc_sats_per_swap": 200, "every_secs": 600, "repeat_secs": 21600 },
```

One swap ties up about 0.003 SOL, mostly escrow rent that comes back when the escrow closes, and spends up to about 150 sats of L-BTC.

## A mainnet maker

Mainnet swaps move real funds. The release caps one swap at 100 USDT, and both programs refuse to start with a higher limit.

1. **A folder of its own, with new keys.** Never copy a test maker's keys or databases. Repeat the folder steps under Install with a new name, for example `main`, then use the mainnet examples instead:
   ```sh
   sudo install -m 600 -o liq2sol -g liq2sol signer.mainnet.example.json /var/lib/liq2sol/main/signer.json
   sudo install -m 600 -o liq2sol -g liq2sol maker.mainnet.example.json /var/lib/liq2sol/main/maker.json
   ```
   Make `solana.json` on the machine itself, with `solana-keygen new -o solana.json`, and write its recovery phrase down offline. Back up the whole `keys/` folder somewhere safe: these keys hold real funds.
2. **Endpoints.** Put your Helius key in `solana_rpc` in both files, replacing `YOUR_HELIUS_KEY`. Public Solana endpoints rate-limit and keep little history, and the maker reads history after a deadline. For Liquid, your own node is better than the public APIs, and is needed for `fresh_addresses`. The maker calls `elements-cli` with the arguments you give, for example:
   ```json
   "liquid": { "Node": { "cli": "/usr/local/bin/elements-cli", "args": ["-chain=liquidv1", "-datadir=/var/lib/liq2sol/main/elements-cli", "-rpcuser=you", "-rpcpassword=secret"] } },
   ```
   Make that `-datadir` folder first, with `sudo -u liq2sol mkdir /var/lib/liq2sol/main/elements-cli`. The services may write only inside the maker's folder, and without it `elements-cli` stops trying to create `~/.elements`. Use the RPC user and password, not the node's cookie file, which the services can't read.
3. **Prices and limits.** Set `l2s_prices` and `s2l_prices` in `maker.json`. `max_per_swap`, `max_sol_units` and the signer's `max_per_swap` must stay at or under `100000000` (100 USDT). `max_open_l2s` and `max_open_s2l` limit what is locked at once.
4. **Pages.** Set `page_to` and check it with `test-page` (see "Pages on your phone"). Also have something on another machine notice if the maker stops.
5. **Start it**, as `liq2sol-signer@main` and `liq2sol-maker@main`. Both check that both chains are mainnet before loading a key. The signer's log gives the addresses to fund: `journalctl -u liq2sol-signer@main | grep "signer ready"`.
6. **Fund it** with USDt and L-BTC on Liquid, and USDT and SOL on Solana. Start small. SOL pays fees and each escrow's rent, which comes back when the escrow closes.
7. **Get listed.** Send SwapMarket the key from `maker answering quotes as <key>`. The page shows only vetted makers by default.

## Several makers on one machine

Each maker is a folder under `/var/lib/liq2sol`, with its own keys, configs and databases. Paths in a config are relative to that config, so the example configs work unchanged in any folder. To add `second`, repeat the folder steps with that name, set its prices, and give it its own `solana.json`. Its other keys are generated. Then start `liq2sol-signer@second` and `liq2sol-maker@second`.

Two makers must never share keys or a database. Their prices and limits are independent, but they share this machine's IP, and so its API rate limits.

## Earnings

```sh
sudo -u liq2sol /opt/liq2sol/liq2sol-maker report /var/lib/liq2sol/first/maker.json
```

It prints, per day and direction, swaps, volume and USDT spread earned, then L-BTC and SOL spent, then what the wallets hold now. It is safe while the maker runs. An escrow's rent counts as spent until closing it brings it back. Costs are recorded when a transaction is sent, so one that never lands still counts. Treat them as close estimates, and the balances, which come from the running signer, as the final word.

## Withdraw

The maker can send its funds out, but only to addresses you set in `signer.json`, so nothing that reaches the signer can choose
where they go. Use your own wallets, ideally cold ones: a confidential Liquid address (`lq1…`, or `tlq1…` on testnet) and a Solana
address.

```json
"withdraw_to": { "liquid": "lq1qq…", "solana": "YourSolanaAddress…" }
```

Restart the signer after setting them. The signer refuses to start with an unconfidential address or one for another network.
Then, as the user the services run as:

```sh
sudo -u liq2sol /opt/liq2sol/liq2sol-maker withdraw /var/lib/liq2sol/main/maker.json liquid usdt 50
sudo -u liq2sol /opt/liq2sol/liq2sol-maker withdraw /var/lib/liq2sol/main/maker.json solana gas all
```

The chain is `liquid` or `solana`, the asset `usdt` or `gas` (L-BTC or SOL), and the amount is in whole units, such as `12.5`,
or `all`. It refuses while any swap is open, since those may need the funds: try again once `report` shows none open. A Liquid
withdrawal that hasn't confirmed yet is sent again, not paid twice, if you ask again. With a vault, USDT on Solana is withdrawn
by the vault's cold owner instead. Taking all the gas leaves the maker unable to pay fees, so it can't take swaps until you
fund it again.

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

`keys/` and both `.db` files. With `fresh_addresses` on, `signer.db` is also how the signer finds the coins at its newer addresses. Prices, limits and API endpoints are in `maker.json`. Restart both services after changing a config.

Liquid's public APIs rate-limit by IP. From v0.14 the maker waits when one refuses (`status code 429` in the logs) and tries again later. If it keeps happening, use your own Elements node, or change the endpoints in both configs.
