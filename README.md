# clipd

An HTTP bridge to the macOS pasteboard. Runs on the Mac; a phone on the same
LAN pushes **text or images** into the Mac's clipboard with one share-sheet tap.

Replaces the usual `set_clipboard?text=` one-liner, with the sharp edges filed
off: image support, HEIC from the iPhone camera, a size limit, and a token.

## Install

```bash
brew install wesleyel/tap/clipd
brew services start clipd
```

Do not start the service with `sudo` — the pasteboard is per-user. Logs land
in `$(brew --prefix)/var/log/clipd.log`.

To require a token, run with `--token` or set `CLIPD_TOKEN`.

## Build

```sh
cargo build --release
# binary at target/release/clipd
```

## Run

```sh
clipd                                  # 0.0.0.0:14756, no auth
clipd --token s3cret --notify          # require a token, show a banner on each write
clipd --bind 127.0.0.1 --port 8080     # loopback only
```

| Flag | Default | Meaning |
|---|---|---|
| `--bind` | `0.0.0.0` | Listen address |
| `--port` | `14756` | Listen port |
| `--token` | *(unset)* | Shared secret; also read from `$CLIPD_TOKEN` |
| `--max-body-mb` | `32` | Reject larger bodies with 413 |
| `--notify` | off | Post a macOS notification per write |

Logging is `clipd=info`; override with `RUST_LOG=clipd=debug`.

## API

| Route | Purpose |
|---|---|
| `GET /set_clipboard?text=...` | Set text. Kept for compatibility with existing shortcuts. |
| `POST /set_clipboard` | Set text **or** an image, in any body format below. |
| `GET /get_clipboard` | Current clipboard as `text/plain`. |
| `GET /get_clipboard/image` | Current clipboard as `image/png`. |
| `GET /get_clipboard/auto` | Text if there is any, otherwise PNG. One route for clients that can't branch on a 404 — Shortcuts, for one. |
| `GET /health` | Liveness. Also requires the token when one is set. |

`POST /set_clipboard` dispatches on `Content-Type`:

- `image/*` — decoded and placed on the pasteboard as an image
- `text/*`, `application/json` — body is the clipboard text
- `application/x-www-form-urlencoded` — reads the `text` field
- `multipart/form-data` — the first file part becomes an image, otherwise the
  first non-file part becomes text (field names don't matter)
- anything else, including `application/octet-stream` — sniffed as an image,
  and treated as UTF-8 text if that fails

Formats: PNG, JPEG, GIF, BMP, TIFF and WebP decode in-process. Anything else —
**HEIC/HEIF from the iPhone camera included** — falls back to `/usr/bin/sips`,
which ships with macOS and reads every format the OS knows.

```sh
curl "http://mac.local:14756/set_clipboard?text=hello"
curl -X POST --data-binary @shot.png -H 'Content-Type: image/png' \
     http://mac.local:14756/set_clipboard
curl -X POST -F "file=@photo.heic" http://mac.local:14756/set_clipboard
curl http://mac.local:14756/get_clipboard
```

With a token, send `X-Clipd-Token: s3cret` or append `&token=s3cret`.

## Shortcuts setup

**Text.** Receive Text/URLs from the share sheet → `URL Encode` the shortcut
input → `Get Contents of URL` on
`http://mac.local:14756/set_clipboard?text=[encoded]`.

Shortcuts' `URL Encode` uses the URL *query* character set, which leaves `&`,
`+` and `=` untouched and truncates your text server-side. Either add three
`Replace Text` steps (`&`→`%26`, `+`→`%2B`, `=`→`%3D`), or skip the encoding
entirely and POST:

**Text or images, one shortcut.** Receive Text, URLs, Images and Files → `Get
Contents of URL`, Method `POST`, URL `http://mac.local:14756/set_clipboard`,
Request Body `File`, and pass the shortcut input straight through. No encoding
step, no URL length ceiling, and images work.

Images arrive as HEIC from the camera roll, which the `sips` fallback handles.
Adding a `Convert Image` → PNG step before the request is a little faster since
it skips the transcode.

## Autostart

```sh
brew services start clipd
```

## Notes

- The pasteboard survives the process exiting — macOS's pasteboard server owns
  the data, unlike X11.
- Set a `--token` if you bind to `0.0.0.0`. Without one, anyone who can reach
  the port can read and write your clipboard, and it's plain HTTP, so treat it
  as a trusted-LAN tool only.
- iOS asks for Local Network permission on the first run; denying it makes
  requests fail silently.
- Use an mDNS name (`mac.local`) rather than a DHCP address that will change.
