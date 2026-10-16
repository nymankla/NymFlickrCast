# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust bridge from Flickr to Chromecast. It is a single binary that serves a web app for browsing a Flickr user's public albums (newest first), previewing them in the browser, and casting an album as a slideshow to a Google Cast device. Original requirements are in `readme.md`.

## Commands

```
cargo build
cargo test                          # unit tests (no network needed)
cargo test cache_hit_and_expiry     # single test
cargo run --release -- [--port N] [user]   # web app port (default 8080); optional Flickr username or NSID prefills the UI (otherwise the browser's last user, else `DEFAULT_USER` Sigtuna_Nym)
git tag vX.Y.Z && git push origin vX.Y.Z   # .github/workflows/release.yml: tests, builds and publishes a GitHub release (Windows zip without the API key); the tag must match Cargo.toml's version
powershell -ExecutionPolicy Bypass -File .\release.ps1   # local-only (gitignored): release build → dist\NymFlickrCast.zip (exe, readme, secrets\ incl. the API key)
```

The app needs the Flickr API key in `secrets/flickrkey.txt`. `parse_key` takes the first 32-hex-char token, so the file may also hold labels or the secret. Don't print the contents of this file. `secrets/` is gitignored.

The app runs until killed. It shouldn't exit because something is missing at startup; it asks in the web app instead. If the key is missing or Flickr rejects it, `KEY` stays `None`, `/api/config` reports `needKey`, and `ui.html` shows a key form that posts to `/api/key`, which checks the key with Flickr and saves it to `KEY_FILE`. If the web app port is already in use, the app assumes it is already running, opens the browser on it and exits.

`--service` runs the app under the Windows service manager (`service` module, `windows-service` crate). `install-service.ps1` (admin; `-Port`, `-Uninstall`) registers the service with that flag and adds the firewall rule. In this mode the working directory is changed to the exe's folder (services start in System32, and `KEY_FILE` is relative), no browser is opened, and there's no console. `main` and service mode share `start()`, which starts the cast worker and both servers. The service reports Running before `start()`, because the key check with Flickr can take longer than the service manager waits.

## Architecture

All Rust code is in `src/main.rs`. The UI is `src/ui.html` (markup and CSS) plus `src/ui.js` (vanilla JS, served at `/ui.js`). Both are embedded with `include_str!`, so a rebuild is needed after editing them. There's no build step: `ui.js` uses `// @ts-check` with JSDoc types, which VS Code checks. The `@typedef`s at its top mirror the `json!` shapes in `main.rs`, so keep them in sync when an API response changes.

Two `tiny_http` servers, both bound to `0.0.0.0` so phones and iPads on the LAN can reach them:
- **Web app port** (`--port`, default 8080): the page `/`, the JSON API `/api/*`, and `/img/<photo id>` for the in-app preview.
- **Photo server** (`MEDIA_PORT` 8765, or any free port if 8765 is taken, kept in `MEDIA_PORT_USED`): only `/img/<photo id>`. The Chromecast fetches photos from here. The host in that URL is the local address of a probe connection to the Cast device, not `local_ip()`: the default route can be a VPN the TV can't reach. The UI server binds first, so `--port 8765` works too.

Data flow and global state (statics in `main.rs`):
- `/api/albums?user=` resolves the username to an NSID, then calls `flickr.photosets.getList`.
- `/api/photos?user=<nsid>&album=` pages through `flickr.photosets.getPhotos` and fills two maps:
  - `PHOTOS` (photo id → large Flickr URL). `/img` only serves ids in this map, so the server is not an open proxy.
  - `ALBUMS` (album id → ordered photo ids). `/api/cast` reads this, so an album must have been opened in the UI before it can be cast.
- All Flickr requests go through the shared `HTTP` ureq agent, which reuses connections and has a 30 s timeout. A bare `ureq::get` creates a new agent each call and has no read timeout.
- `fetch` keeps an in-memory `CACHE` of photo bytes for 2 hours (`CACHE_TTL`). `albums` caches each user's album list for 1 minute (`ALBUM_CACHE_TTL`), kept short so new Flickr albums show up quickly. Expired entries are pruned when new ones are inserted.

Casting:
- `/api/devices` runs an mDNS browse for `_googlecast._tcp` for 3 seconds. `has_video` drops devices whose `ca` TXT bitmask lacks bit 0 (video out), which removes speakers and speaker groups.
- One long-lived thread, `cast_worker`, does all casting; it is the only code that talks to Cast devices. `POST /api/cast?device=ip:port&name=&album=&start=` and `POST /api/stop` call `post_cast_cmd`, which puts a `CastCmd` (`Play` or `Stop`) in the `CAST_CMD` mailbox and shows a status right away. The mailbox holds one command and the latest wins, so quick album clicks collapse into the last one.
- `play` keeps the `CastSession` (connection plus Default Media Receiver app) when the device is unchanged, so an album switch is just the next `media.load` with no reconnect or app relaunch. Switching to another device or `Stop` stops the app on the old device.
- `play` calls `media.load` for each photo every `SLIDE_SECS` seconds. This is an atomic, default 4 and not saved, set by the web app's Delay field through `POST /api/settings`. It is read again for every slide, so a running cast and the browser preview pick up changes.
- The worker must answer heartbeat `Ping`s with `pong()`, or the device drops the connection. rust_cast's `receive()` blocks with no timeout and holds the connection lock, and its socket isn't reachable to set a read timeout. So the worker sees new commands only when the device sends something (pings every ~5 s), and switches and Stop can lag up to that long.
- Back/Home on the TV remote must end the cast, or the next `media.load` pulls the TV back into the slideshow. `cancelled_on_device` (media idle reason `Cancelled`, i.e. Back) stops right away. `app_maybe_closed` (a connection `Close`, or a receiver status without our app) is only a hint, because rust_cast hands over stale messages it held back during launch; `play` confirms it with `receiver.get_status()` first. `play` then sets "Stopped on <TV>" and stops the app. Messages that end a cast are logged as `cast: stopped on ...`.
- A device that goes silent (e.g. powered off) hangs the worker in `receive()`. `cast_status()` handles this: if nothing has been heard for `CAST_SILENCE_LIMIT` (`CAST_LAST_SEEN`), it reports "Lost connection" and `spawn_cast_worker` starts a new worker under a new `CAST_WORKER` id. The old worker exits without touching its device if it ever wakes.
- Two TLS backends are compiled in: ureq uses rustls with ring, and rust_cast uses aws-lc-rs. `main` must install the ring provider before the first TLS connection, or rustls panics inside the cast worker. A panic in `play` is caught and reported in `CAST_STATUS`, and the worker keeps running.
- The worker updates the status only through `set_cast_status`. It drops the update while a newer command waits in the mailbox (that command's status is already shown) or if the worker has been replaced. Lock order: `CAST_CMD`, then `CAST_STATUS`.
- The media server logs each request (`media: <ip> GET /img/..`). If a cast's photos never show and there's no log line from the TV's IP, the firewall is blocking it.
- `CAST_STATUS` holds the JSON that the UI polls at `/api/status`. While a cast is active it also has `device`, `addr` and `album`. `openAlbum` in `ui.html` uses them to restart the cast with the newly opened album on the same device, so casting continues across album changes.

Flickr access uses an API key only, with no OAuth. Only public albums are visible, and only the first 500 albums are listed. Flickr returns some numbers as strings, so read numeric fields with `num()`.

## Conventions

- Known limitations are marked with `// shortcut: <limit>, <when to upgrade>` comments.
- Errors use the `Res<T>` alias (boxed `Send + Sync` error). In `serve`, handler errors become a JSON `{"error": ...}` 500 response, which `api()` in `ui.html` shows to the user.
- The UI must keep working on phones and iPads (touch swipe, wrapping header, 16px inputs).
