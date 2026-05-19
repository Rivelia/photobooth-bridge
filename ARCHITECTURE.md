# Architecture

This document is for someone implementing, porting, or extending
`photobooth-bridge`. Read [`README.md`](README.md) first for the user-facing
description.

## Two-piece architecture

This proxy alone is **not enough** to import a foreign-region share. Two
things must happen, and only the second is this project:

1. **QR re-wrap** (web tool at `https://naraka.wiki/photo-booth`):
   take a `(shareCode_foreign, URL_foreign)` and produce
   `(shareCode_foreign, URL_local)`. The shareCode stays foreign; only the
   URL wrapper changes to the local region's `https://www.{narakathegame.com|yjwujian.cn}/h5/20260401/yingpengfx/?shareCode=…`
   form. Without this, the game's pre-check rejects the foreign URL and
   never makes an API call.
2. **API rewrite** (this project): when the game calls its own region's
   `studio_share/public_detail` with the foreign shareCode and gets
   `code=30003`, the proxy fetches from the other region's API and patches
   the response so the game thinks the share is local.

If you skip step 1 the proxy gets no traffic. If you skip step 2 the game
hits a real 30003 from its own backend and shows "share not found." Both
have to be in place.

## The problem in one paragraph

Naraka: Bladepoint runs two completely separate Photo Booth backends —
`api.narakathegame.com` for the Global client and `api.yjwujian.cn` for the
CN (国服) client. When you take a Photo Booth photo in-game, the client
uploads to its own region's backend and shows a QR pointing at a region-local
share URL. The shareCode in that URL is a 21-char base64url identifier that
exists **only on that region's backend** — querying the opposite region's
backend with the same shareCode returns `code=30003 Share record not found`.
So there is no way for a CN player and a Global player to scan each other's
QRs in-game: the local game's lookup will always 404. This tool fixes that by
sitting between the game and its backend and faking a successful lookup using
data fetched from the *other* region.

## Rewrite logic

Implemented in [`src/rewrite.rs`](src/rewrite.rs).

```
when {request.host ∈ {api.narakathegame.com, api.yjwujian.cn}
     and request.path startswith "/yjwj/studio_share/{game,public}_detail"}:

    response = forward_upstream(request)
    if response.json.code == 0:
        return response                          # success — passthrough

    opposite_host = swap_region(request.host)
    opposite_json = upstream_get(
        "https://{opposite_host}/yjwj/studio_share/public_detail?shareCode={shareCode}"
    )

    if opposite_json.code != 0:
        return response                          # not on either side; passthrough

    # Splice opposite-region data into our response, rewriting shareUrl so
    # the local game thinks the share originated locally.
    response.json = {
        "code": 0,
        "message": "Success",
        "data": {
            ...opposite_json.data,
            "shareUrl": build_local_share_url(request.host, shareCode),
        }
    }
    response.status = 200
    response.headers["content-type"] = "application/json; charset=utf-8"
    drop response.headers["content-length"]      # body length changed
    drop response.headers["content-encoding"]    # body is fresh JSON
    return response
```

Fields **not** touched (passed through from opposite region):
`shareCode`, `playerName`, `playerId`, `uid`, `source`, `reviewStatus`,
`shareImageUrl`, `createTime`. Everything except `shareUrl` is identical to
the foreign region's record.

`shareImageUrl` is the field that points at the foreign-region CDN — the
game's image loader follows it without complaint because the CDN serves the
photo publicly. **Do not rewrite this URL.** Attempting to mirror the image
to a local server defeats the point and adds bandwidth + storage burden.

## TLS interception

Implemented in [`src/proxy.rs`](src/proxy.rs) + [`src/ca.rs`](src/ca.rs).

Wire flow:

```
[game] --(TLS, original SNI=api.narakathegame.com)--> 127.0.0.1:443 [proxy]
   1. proxy peeks ClientHello via rustls' LazyConfigAcceptor
   2. proxy extracts SNI hostname
   3. proxy generates a leaf cert for that SNI, signed by the locally-trusted
      CA from src/ca.rs
   4. proxy completes the TLS handshake with the game
   5. proxy serves HTTP/1.1 over the decrypted stream
   6. proxy forwards each HTTP request to the real upstream via tokio-rustls
      (with webpki-roots, so upstream TLS is real)
   7. proxy applies rewrite rules and ships the response back
[real upstream] <--(real TLS, webpki-roots verified)-- [proxy]
```

The CA's private key lives at `~/.local/share/photobooth-bridge/ca-key.pem`
(mode 600). The cert at `~/.local/share/photobooth-bridge/ca.pem`. They
persist across restarts — generating a new CA on every run would require the
user to re-install the CA into the trust store each time.

Leaf certs are signed lazily on first connection for a given SNI and cached
in `CertificateAuthority::leaf_cache`. Keypair generation + signing dominate
the TLS hot path; the cache is a single `Mutex<HashMap>` with read-then-insert
under one lock so concurrent first-connections for the same SNI don't both
sign.

## Traffic redirection

Implemented in [`src/redirect.rs`](src/redirect.rs). Same approach on both
OSes — the only difference is which file we rewrite.

| OS      | Hosts file                                         |
|---------|----------------------------------------------------|
| Linux   | `/etc/hosts`                                       |
| Windows | `C:\Windows\System32\drivers\etc\hosts`            |

At `start`, we add two lines like:

```
127.0.0.1	api.narakathegame.com  # photobooth-bridge
127.0.0.1	api.yjwujian.cn  # photobooth-bridge
```

— and bind the proxy to `127.0.0.1:443`. The game's DNS lookup hits the
hosts file first, gets `127.0.0.1`, and connects to the proxy. Because
hosts-file redirection forges only the destination IP and not the port,
the proxy is forced to take 443; this is why `start` requires root /
Administrator.

The hosts are also resolved at startup via the real system resolver
(bypassing our rewrite, since at that point we haven't written it yet).
That `host → IP` map is stashed in the upstream client's `OverrideResolver`
(in [`src/upstream.rs`](src/upstream.rs)) so the proxy's own outbound HTTPS
calls to those same hostnames go to the real IPs instead of looping back
into 127.0.0.1. We don't poll DNS for refresh — if Cloudflare / AWS NLB
rotates an IP mid-session and the cached IP stops working, restarting the
proxy re-resolves.

Every line we add carries the `# photobooth-bridge` marker. Two entry
points use it:

1. **`install`** strips any pre-existing marker lines before writing
   fresh ones, so a crashed previous run gets cleaned up on the next
   `start`.
2. **`scrub`** is called by `cmd_start` on SIGINT / SIGTERM / Ctrl-C and
   by `uninstall`, so even without ever running `start` again the user can
   wipe leftovers.

### Why not iptables (Linux) or packet interception (Windows)?

Earlier versions of this project used `iptables -t nat OUTPUT REDIRECT` on
Linux. NDIS-layer packet intercept is the equivalent on Windows. Both work
but cost complexity:

- iptables needs `! --uid-owner root` to exempt the proxy's own outbound,
  plus crash-leaves-rules cleanup.
- NDIS packet intercept needs a signed `.sys` kernel driver shipped
  alongside the binary.

The hosts-file approach trades one downside — the proxy port is locked to
443 — for a single code path that runs on both OSes with no kernel
component and no privileged networking syscalls beyond editing a text
file.

## Trust store install

Implemented in [`src/trust_store.rs`](src/trust_store.rs) (Windows only).

On Windows the trust store is managed via `certutil.exe -addstore -f Root
<pem>` and `certutil.exe -delstore Root "<CN>"`. Both require admin / UAC
elevation.

There is no Linux equivalent. Naraka is a Windows binary running under
Proton/Wine, and Wine's SChannel cert store is per-prefix (registry-backed)
— it doesn't read `/etc/ca-certificates`. So the only install path that
matters on Linux is the Proton prefix one below.

## Wine / Proton on Linux

Naraka is Windows-only. Linux users run it via Steam Play (Proton) or
Wine. Wine implements Windows' SChannel API, and SChannel consults the
**Wine prefix's** registry-based cert store. The Windows binary's TLS
handshake to our proxy would otherwise fail with `TLS access_denied`
because the prefix doesn't know our CA — so this is the one and only place
on Linux where we install the CA.

`src/wine.rs` makes `install-ca` detect a Steam Proton prefix and inject
the CA directly into the prefix registry via `regedit.exe /S <file.reg>`.

The earlier implementation shelled out to `certutil.exe -addstore -f Root
<pem>`, but that command silently no-ops on Proton-Experimental: it returns
exit 0 and creates the `HKCU\…\Root\Certificates` parent key, but never
writes the per-fingerprint subkey. We bypass it by generating a `.reg`
file containing the cert in Wine's serialized property-blob format and
applying it directly:

```bash
STEAM_COMPAT_CLIENT_INSTALL_PATH=~/.steam/steam \
STEAM_COMPAT_DATA_PATH=~/.steam/steam/steamapps/compatdata/1203220 \
~/.steam/steam/steamapps/common/Proton\ Experimental/proton run \
  regedit.exe /S 'Z:\tmp\photobooth-bridge-<pid>.reg'
```

The `.reg` file contents:

```
REGEDIT4

[HKEY_CURRENT_USER\Software\Microsoft\SystemCertificates\Root\Certificates\<SHA1_UPPER_HEX>]
"Blob"=hex:<serialized property blob>
```

The blob is a concatenation of `{u32 prop_id, u32 encoding=1, u32 data_len,
[data...]}` records — minimally `CERT_SHA1_HASH_PROP_ID (0x03)` (20-byte
sha1 of the DER) and `CERT_CERT_PROP_ID (0x20)` (the cert DER itself).
SChannel reads this byte stream when validating chains. Uninstall emits the
same `.reg` shape with the key prefixed by `-` for deletion.

### CA cert shape (the rcgen / Wine gotcha)

`src/ca.rs` deliberately does **not** set a `KeyUsage` extension on the
root cert. rcgen 0.13 encodes the `KeyUsage` BIT STRING non-canonically
(e.g. `03 03 07 86 00` — a 3-byte value with the active bits in the first
byte and a `00` padding byte trailing) and Wine's `CRYPT_KeyUsageValid`
inspects only `pbData[cbData - 1]`. So Wine reads the trailing `00`, sees
`keyCertSign` unset, marks the chain `CERT_TRUST_IS_NOT_VALID_FOR_USAGE`,
and `CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL)` returns
`CERT_E_WRONG_USAGE (0x800B0110)` — the TLS handshake to the proxy fails
with `access_denied`. Both Wine and real Windows accept V3 CA certs with
no `KeyUsage` extension at all (Wine's `dlls/crypt32/chain.c` even
documents the precedent), so we omit it entirely. The root still carries
`BasicConstraints: CA:TRUE` (required) and a `serverAuth` EKU (cheap
belt-and-suspenders). Don't add KeyUsage back without first verifying
rcgen has switched to canonical DER for that extension.

Prefix detection: `~/.steam/steam/steamapps/compatdata/<appid>/pfx/` (with
fallback to `~/.local/share/Steam` and `~/.steam/debian-installation`).
Proton ranking: Experimental > GE-Proton{N-M} > Proton {major.minor}; the
highest-ranked one wins (overridable by reading the prefix's `version`
file if we ever need exact-match selection — not done yet).

`install-ca` on Linux does NOT need root: it only writes to
`~/.local/share/photobooth-bridge/` and to the user-owned Proton prefix.
If a user still invokes it via `sudo`, `paths.rs` falls back to
`$SUDO_USER`'s home for the CA file (so it lands at
`/home/<user>/.local/share/…` reachable to the real user, not
`/root/.local/share/…`), and `wine::install_ca` re-elevates back to that
user via `sudo -u <user>` before invoking Proton — Proton refuses to run
as root and would litter the prefix with root-owned files.

### Other launchers

Bottles / Lutris / vanilla Wine prefixes are not auto-detected yet. The
manual workaround is to set `$WINEPREFIX` to the prefix path and run:

```bash
WINEPREFIX=/path/to/prefix wine certutil.exe -addstore -f Root <ca.pem>
```

A future change could add `--wine-prefix <path>` to `install-ca` for these.

## Per-OS glue summary

`proxy.rs`, `ca.rs`, `rewrite.rs`, `redirect.rs`, `upstream.rs` are
platform-agnostic at the public-API level. The remaining per-OS bits:

- **`trust_store.rs`** — Windows only: shells out to `certutil.exe
  -addstore Root` / `-delstore Root`. Requires admin. No Linux equivalent
  (the only store Naraka consults under Proton is the prefix's, handled by
  `wine.rs`).
- **`platform.rs`** — `IsUserAnAdmin` on Windows, `geteuid()` on Unix; for
  shutdown, `tokio::signal::ctrl_c()` on Windows, SIGINT/SIGTERM on Unix.
- **`paths.rs`** — `%LOCALAPPDATA%\photobooth-bridge\` on Windows;
  `~/.local/share/photobooth-bridge/` (with `$SUDO_USER` fallback) on
  Linux.
- **`wine.rs`** — Linux-only. Naraka under Proton has its own
  SChannel-backed cert store; `install-ca` generates a `.reg` file with
  the CA in serialized property-blob form and applies it via
  `regedit.exe /S` *inside the Wine prefix* using Steam's bundled Proton
  script. See "Wine / Proton on Linux" above for why we don't use
  `certutil.exe`.

## What's intentionally out of scope

- **Hot DNS refresh.** The redirect resolves target hostnames once at
  startup. If Cloudflare / AWS NLB rotates an IP mid-session, that
  connection misses the redirect and the user sees a "share not found"
  error. Acceptable tradeoff for MVP — restarting the proxy re-resolves.
- **Scripting / user-defined rewrite rules.** The proxy hardcodes the
  photo-booth rewrite. We don't expose a Lua / JS / config-file extension
  point because the only rule anyone wants is the one already baked in,
  and a scripting runtime would only add attack surface.
- **System-tray / notification-area UI.** The app has a single-window
  egui/eframe GUI, not a system-tray resident. It's on-screen while running
  and cleans up on close.
- **Multiple games on the same machine.** The redirect rule is global to
  the user; running this while doing something else with
  `api.narakathegame.com` / `api.yjwujian.cn` will also rewrite that
  traffic. Restart only when you actually need it.

