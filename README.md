# photobooth-bridge

Cross-region Photo Booth QR support for **Naraka: Bladepoint**. Lets a Global
player scan a CN player's Photo Booth QR (or vice-versa) in-game and see the
photo, instead of the "Share not found" error you'd normally get because the
two regions have separate backends.

Free and open source. Runs on Windows and Linux.

## How it works (the 30-second version)

Importing a foreign-region QR takes two pieces:

1. **A converter.** Save the foreign-region QR as an image (a screenshot, a
   picture from a gallery site, an image shared in Discord — wherever you got
   it from) and upload it to the converter at
   [naraka.wiki/photo-booth](https://naraka.wiki/photo-booth). It re-wraps the
   share so the game's URL pre-check accepts it.
2. **This bridge.** Runs in the background while you scan the converted QR.
   When the game asks its own backend about the foreign share and gets
   "not found", the bridge fetches the photo from the other region's backend
   and hands it to the game.

Both pieces are required. The converter alone gets you a QR the game will
*try* to look up; without the bridge running, the lookup still fails.

---

## Windows — quick start

> Requires Windows 10 or 11.

1. Download `photobooth-bridge.exe` from the [Releases page](https://github.com/rivelia/photobooth-bridge/releases).
2. Double-click it. Windows will ask for Administrator — click Yes. (The bridge
   needs Admin to bind port 443 and edit the system hosts file.)
3. The first time you run it, click **Install certificate and start bridge**.
   This adds a local certificate to your trust store so the game trusts the
   bridge.
4. The status will turn green: *"Bridge is ON"*.
5. Upload the foreign-region QR image you want to import to
   [naraka.wiki/photo-booth](https://naraka.wiki/photo-booth), then scan the
   resulting QR in-game.
6. When you're done, close the window — the bridge stops itself cleanly.

To remove everything (certificate, hosts-file entries, CA files), click
**Remove certificate and clean up** before closing.

## Linux — quick start

> Requires Steam + Proton, and the Naraka prefix must have been launched at
> least once.

The graphical app exists on Linux too but launching it with the privileges it
needs is awkward, so the CLI is the recommended path:

```bash
# Build the binary (release mode):
cargo build --release

# One-time setup: generate the local CA and install it into Naraka's Proton
# prefix. Does NOT need sudo — Proton refuses to run as root anyway.
./target/release/photobooth-bridge install-ca

# Start the bridge. Needs sudo to bind 443 and write /etc/hosts. Leave this
# running while you scan QRs in-game. Ctrl-C to stop — the hosts file is
# cleaned up on exit.
sudo ./target/release/photobooth-bridge start

# When you don't need cross-region imports anymore, remove the certificate
# and clean up:
./target/release/photobooth-bridge uninstall
```

While the bridge is running, use
[naraka.wiki/photo-booth](https://naraka.wiki/photo-booth) to convert a
foreign-region QR image, then scan the resulting QR in-game.

---

## Common problems

**"The bridge won't start — port 443 is in use."** Something else on your
machine is bound to port 443 (IIS, Skype, Hamachi, another proxy). The bridge
can't share the port — hosts-file redirection can only forge the destination
IP, not the port. Stop whatever's on 443 and try again.

**"It says installed but the game still shows 'Share not found'."** Make sure
you've converted the QR through
[naraka.wiki/photo-booth](https://naraka.wiki/photo-booth) first. Scanning a
raw foreign QR won't work — the game's URL pre-check rejects it before any
network call happens.

**"My machine was force-rebooted while the bridge was running."** The bridge
tags every line it adds to the hosts file with a comment marker; the next time
you run it (or run `uninstall`), any leftover entries are scrubbed automatically.

**"I'm on Linux and the bridge can't find my Proton prefix."** The default app
id is `1203220` (Naraka: Bladepoint). If you're on a different shop entry,
pass `--steam-app-id <id>` to both `install-ca` and `uninstall`. Bottles /
Lutris / vanilla Wine prefixes aren't auto-detected — see
[ARCHITECTURE.md](ARCHITECTURE.md) for the manual workaround.

---

## CLI reference

The GUI is the default on Windows. Power users can still use these
subcommands on either OS:

```
photobooth-bridge                  # launch the GUI (same as `gui`)
photobooth-bridge gui              # launch the GUI explicitly
photobooth-bridge install-ca       # generate CA, install into trust store
photobooth-bridge start            # bind 443, install hosts redirect, run proxy
photobooth-bridge uninstall        # remove CA, scrub hosts entries, delete files
photobooth-bridge status           # show CA paths and existence
```

Useful flags:

- `start --port <N> --no-redirect` — listen on a non-443 port and skip
  hosts-file edits. Pair with `curl --resolve` for testing.
- `install-ca --no-wine` *(Linux)* — generate the local CA file only; don't
  install it into the Proton prefix.
- `install-ca --steam-app-id <id>` *(Linux)* — target a different game's
  Proton prefix.
- `uninstall --keep-files` — keep `ca.pem` and `ca-key.pem` after uninstalling.

---

## Important caveats

- **The bridge is a transparent MITM proxy on your own machine.** While it's
  running, the two API hostnames (`api.narakathegame.com`, `api.yjwujian.cn`)
  resolve to `127.0.0.1` and traffic to them is intercepted and re-signed by
  a local CA whose private key lives in your user data directory. **Run the
  bridge only when you actually need cross-region imports**, and run
  `uninstall` (or click *Remove certificate and clean up* in the GUI) when
  you're done.
- **Protect the CA private key.** It's at
  `%LOCALAPPDATA%\photobooth-bridge\ca-key.pem` (Windows) or
  `~/.local/share/photobooth-bridge/ca-key.pem` (Linux, mode 600). Anyone
  with that key could sign certificates for any hostname accepted by your
  trust store.
- **Don't run other things on port 443.** The bridge takes the whole port
  while it's running.
- **The bridge doesn't phone home.** No telemetry, no remote logging. The
  source is in this repo; the only outbound connections it makes are to the
  two Naraka API hosts to fulfil photo lookups.

---

## Build from source

```bash
# Linux:
cargo build --release
# → target/release/photobooth-bridge

# Windows cross-compile from Linux (needs podman):
./build-windows.sh
# → target-windows/x86_64-pc-windows-gnu/release/photobooth-bridge.exe
```

The Windows build embeds a UAC manifest, so the .exe auto-prompts for
Administrator when double-clicked.

---

## Technical details

For maintainers and contributors. End users don't need any of this.

### Mechanism

1. **Root CA.** Generated locally on first run. Installed into the Windows
   machine `Root` store (`certutil -addstore Root`) or — on Linux — into the
   Naraka Proton prefix's per-prefix SChannel store via a `.reg` file applied
   through `regedit.exe`.
2. **Hosts-file redirect.** Two lines are appended to the system hosts file
   (`C:\Windows\System32\drivers\etc\hosts` on Windows, `/etc/hosts` on
   Linux), pointing `api.narakathegame.com` and `api.yjwujian.cn` at
   `127.0.0.1`. Each line carries a `# photobooth-bridge` marker so they can
   be reliably scrubbed.
3. **TLS interception.** The proxy binds 127.0.0.1:443, peeks the SNI from
   each incoming ClientHello, signs a leaf cert for that SNI with the local
   CA (cached per-SNI), and serves HTTP/1.1 over the decrypted stream.
4. **Cross-region rewrite.** On `code=30003` ("Share not found") from the
   game's own region, the proxy fetches the same `shareCode` from the
   opposite region's backend and splices the foreign region's `data` into
   the response, rewriting `shareUrl` so the game thinks the share is local.
   `shareImageUrl` is passed through unchanged — Photo Booth CDN URLs are
   region-agnostic and publicly accessible.

### Why this design

- **Hosts-file redirection** instead of kernel-level packet interception
  (Windows) or iptables (Linux) keeps the codebase free of kernel drivers
  and netfilter rules. The trade-off is the proxy port is locked to 443.
- **In-process DNS override.** The proxy's own outbound HTTPS calls to those
  same two hostnames must NOT loop back through the hosts file or it would
  intercept itself. Targets are resolved at startup via the real system
  resolver, before the hosts file is rewritten, and the resolved IPs are
  stashed in the upstream client's `OverrideResolver`.
- **Lazy leaf-cert signing.** Leaf certs are minted on the fly when a new
  SNI shows up and cached in a `Mutex<HashMap>` for the lifetime of the
  process.

### Testing without modifying the hosts file

```bash
./target/release/photobooth-bridge install-ca --no-wine
./target/release/photobooth-bridge start --port 18443 --no-redirect &

CA=~/.local/share/photobooth-bridge/ca.pem

# Querying api.narakathegame.com with a CN shareCode — the proxy falls back
# to the CN backend and patches the response so the response looks like a
# Global share.
curl -s --resolve api.narakathegame.com:18443:127.0.0.1 --cacert "$CA" \
  "https://api.narakathegame.com:18443/yjwj/studio_share/public_detail?shareCode=AAFOe61_lWSyRnlE5NawI" \
  | jq
```

### Further reading

[`ARCHITECTURE.md`](ARCHITECTURE.md) — full API contract, rewrite logic,
per-OS implementation notes, and the Wine/Proton SChannel gotchas. Read it
before extending the project.

## License

MIT.
