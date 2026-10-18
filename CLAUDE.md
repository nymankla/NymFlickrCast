# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust bridge from Flickr to Chromecast. It is a single binary that serves a web app for browsing a Flickr user's public albums (newest first), previewing them in the browser, and casting an album as a slideshow to a Google Cast device. Original requirements are in `readme.md`.

## Commands

```
cargo build
cargo test                          # unit tests (no network needed)
cargo test cache_hit_and_expiry     # single test
cargo fmt && cargo clippy --all-targets -- -D warnings   # lints: [lints] in Cargo.toml (clippy pedantic + extras, unsafe_code forbidden), rustfmt.toml (140 cols)
npx -p typescript@5.9.2 tsc            # strict type check of src/ui.ts and compile to src/ui.js (tsconfig.json); run after every ui.ts edit
# CI (.github/workflows/ci.yml) runs all of the above plus cargo audit on Windows and Linux for every push/PR.
cargo run --release -- [--port N] [user]   # web app port (default 8080); optional Flickr username or NSID prefills the UI (otherwise the browser's last user, else `DEFAULT_USER` Sigtuna_Nym)
git tag vX.Y.Z && git push origin vX.Y.Z   # .github/workflows/release.yml: tests, builds and publishes a GitHub release (Windows zip without the API key); the tag must match Cargo.toml's version
powershell -ExecutionPolicy Bypass -File .\release.ps1   # local-only (gitignored): release build → dist\NymFlickrCast.zip (exe, readme, secrets\ incl. the API key)
```

The app needs the Flickr API key in `secrets/flickrkey.txt`. `parse_key` takes the first 32-hex-char token, so the file may also hold labels or the secret. Don't print the contents of this file. `secrets/` is gitignored.

The app runs until killed. It shouldn't exit because something is missing at startup; it asks in the web app instead. At startup a key from the file is used right away and checked with Flickr in the background, so a slow or offline Flickr doesn't delay startup. If the key is missing or Flickr rejects it, `KEY` is `None`, `/api/config` reports `needKey`, and `ui.html` shows a key form that posts to `/api/key`, which checks the key with Flickr and saves it to `KEY_FILE`. When the background check rejects a key after the page has loaded, the next album request fails with `KEY_MISSING`, and `ui.ts` shows the key form again. If the web app port is already in use, the app assumes it is already running, opens the browser on it and exits.

`--service` runs the app under the Windows service manager (`service` module, `windows-service` crate). `install-service.ps1` (admin; `-Port`, `-Uninstall`) registers the service with that flag and adds the firewall rule. In this mode the working directory is changed to the exe's folder (services start in System32, and `KEY_FILE` is relative), no browser is opened, and there's no console. `main` and service mode share `start()`, which starts the cast worker and both servers. The service reports Running once `start()` has bound the ports.

## Architecture

All Rust code is in `src/main.rs`. The UI is `src/ui.html` (markup and CSS) plus `src/ui.ts` (vanilla TypeScript, no framework). `tsc` compiles it to `src/ui.js`, which is committed so `cargo build` needs no Node; CI fails if it is stale. Never edit `ui.js` by hand. `ui.html` and `ui.js` are embedded with `include_str!` and served at `/` and `/ui.js`, so a rebuild is needed after editing them. The types at the top of `ui.ts` mirror `main.rs`: the response interfaces match its `json!` shapes, `Routes` lists every endpoint with its response (and `POST_ROUTES` the POST ones), and `Els` lists the element ids in `ui.html` with their types. Keep them in sync when an endpoint, response or element id changes.

Two `tiny_http` servers, both bound to `0.0.0.0` so phones and iPads on the LAN can reach them. Each has a fixed pool of `HTTP_WORKERS` threads (`serve`); a handler panic is answered with a 500 and the worker keeps going:
- **Web app port** (`--port`, default 8080): the page `/`, the JSON API `/api/*`, and `/img/<photo id>` for the in-app preview. Two checks protect it from other web pages, since there's no login:
  - `host_allowed` rejects a `Host` that isn't an IP address, a single-label name (localhost, the PC's name) or a local suffix such as `.local`. This blocks DNS rebinding.
  - POSTs must carry the `X-NymFlickrCast` header (`API_HEADER`), which `api()` in `ui.ts` always sends. A cross-site page can't add it without CORS permission, which the app never gives, so this blocks CSRF.
- **Photo server** (`MEDIA_PORT` 8765, or any free port if 8765 is taken, kept in `MEDIA_PORT_USED`): only `/img/<photo id>`. The Chromecast fetches photos from here. The host in that URL is the local address of a probe connection to the Cast device, not `local_ip()`: the default route can be a VPN the TV can't reach. The UI server binds first, so `--port 8765` works too.

Data flow and global state (statics in `main.rs`):
- `/api/albums?user=` resolves the username to an NSID, then calls `flickr.photosets.getList`.
- `/api/photos?user=<nsid>&album=` pages through `flickr.photosets.getPhotos` and fills two maps:
  - `PHOTOS` (photo id → large Flickr URL). `/img` only serves ids in this map, so the server is not an open proxy.
  - `ALBUMS` (album id → ordered photo ids). `/api/cast` reads this, so an album must have been opened in the UI before it can be cast.
- All Flickr requests go through the shared `HTTP` ureq 3 agent, which reuses connections and has a 30 s timeout. A bare `ureq::get` creates a new agent each call and has no timeout.
- `fetch` keeps an in-memory `CACHE` (`PhotoCache`) of photo bytes for 2 hours (`CACHE_TTL`), up to `CACHE_MAX_BYTES`, dropping the oldest photos first. A URL being downloaded is in `loading`, and other requests for it wait on the cache's `Condvar` instead of downloading it again (e.g. the next-slide preload and the TV). The `Loading` guard clears the mark even on error. `/img` responses carry `Cache-Control: private, max-age=7200`, since a photo id always shows the same photo. `albums` caches each user's album list for 1 minute (`ALBUM_CACHE_TTL`), kept short so new Flickr albums show up quickly. Expired entries are pruned when new ones are inserted.

Casting:
- `watch_cast_devices` runs one mDNS browse for `_googlecast._tcp` for the app's lifetime and keeps `DEVICES` current, so `/api/devices` answers at once (it waits up to 3 s only while the list is still empty after startup). `has_video` drops devices whose `ca` TXT bitmask lacks bit 0 (video out), which removes speakers and speaker groups.
- One long-lived thread, `cast_worker`, does all casting; it is the only code that talks to Cast devices. `POST /api/cast?device=ip:port&album=&start=` and `POST /api/stop` call `post_cast_cmd`, which puts a `CastCmd` (`Play` or `Stop`) in the `CAST_CMD` mailbox and shows a status right away. The mailbox holds one command and the latest wins, so quick album clicks collapse into the last one. `/api/cast` only accepts a device found by mDNS (`known_device`, which also supplies the name), so the app connects to nothing else.
- `play` keeps the `CastSession` (connection plus Default Media Receiver app) when the device is unchanged, so an album switch is just the next `media.load` with no reconnect or app relaunch. Switching to another device or `Stop` stops the app on the old device.
- `play` calls `media.load` for each photo every `SLIDE_SECS` seconds. This is an atomic, default 4, set by the web app's Delay field through `POST /api/settings` and saved in `SETTINGS_FILE` (`settings.json` in the working directory, next to `secrets/`; gitignored), which `start()` loads. It is read again for every slide, so a running cast and the browser preview pick up changes.
- The worker must answer heartbeat `Ping`s with `pong()`, or the device drops the connection. rust_cast's `receive()` blocks with no timeout and holds the connection lock, and its socket isn't reachable to set a read timeout. So the worker sees new commands only when the device sends something (pings every ~5 s), and switches and Stop can lag up to that long.
- Back/Home on the TV remote must end the cast, or the next `media.load` pulls the TV back into the slideshow. `cancelled_on_device` (media idle reason `Cancelled`, i.e. Back) stops right away. `app_maybe_closed` (a connection `Close`, or a receiver status without our app) is only a hint, because rust_cast hands over stale messages it held back during launch; `play` confirms it with `receiver.get_status()` first. `play` then sets "Stopped on <TV>" and stops the app. Messages that end a cast are logged as `cast: stopped on ...`.
- A device that goes silent (e.g. powered off) hangs the worker in `receive()`. `cast_status()` handles this; a background thread calls it every 5 s, so it also runs with no browser open. If nothing has been heard for `CAST_SILENCE_LIMIT` (`CAST_LAST_SEEN`), it reports "Lost connection" and `spawn_cast_worker` starts a new worker under a new `CAST_WORKER` id. A command left in the mailbox for `CAST_PICKUP_LIMIT` (e.g. switching away from a TV that was turned off) also gets a new worker, which picks it up. The old worker exits without touching its device if it ever wakes. `CastSession::connect` pushes `CAST_LAST_SEEN` ahead by `CAST_LAUNCH_GRACE` while the device launches the app, and `CastSession::stop` gives up after 5 s (rust_cast's `thread_safe` feature makes the session `Send` for that), so neither a slow TV nor a dead one blocks the worker.
- TLS uses rustls with aws-lc-rs only: rust_cast needs it, and ureq is built with `rustls-no-provider`, so it uses the process default provider that `main` installs before the first TLS connection. Don't enable ureq's default `rustls` feature: it adds ring, and with two providers compiled in rustls can't pick one by itself and panics. A panic in `play` is caught and reported in `CAST_STATUS`, and the worker keeps running.
- The worker updates the status only through `set_cast_status`. It drops the update while a newer command waits in the mailbox (that command's status is already shown) or if the worker has been replaced. Lock order: `CAST_CMD`, then `CAST_STATUS`.
- The media server logs each request (`media: <ip> GET /img/..`). If a cast's photos never show and there's no log line from the TV's IP, the firewall is blocking it.
- `CAST_STATUS` holds the JSON that the UI polls at `/api/status`. While a cast is active it also has `device`, `addr` and `album`. `openAlbum` in `ui.html` uses them to restart the cast with the newly opened album on the same device, so casting continues across album changes.

Flickr access uses an API key only, with no OAuth. Only public albums are visible, and only the first 500 albums are listed. Flickr returns some numbers as strings, so read numeric fields with `num()`.

## Conventions

- Known limitations are marked with `// shortcut: <limit>, <when to upgrade>` comments.
- Lock mutexes with `lock(&M)`, never `.lock().unwrap()`: it ignores poisoning, so one panic doesn't make every later request fail.
- Errors use the `Res<T>` alias (boxed `Send + Sync` error). In `serve`, handler errors become a JSON `{"error": ...}` 500 response, which `api()` in `ui.html` shows to the user.
- The UI must keep working on phones and iPads (touch swipe, wrapping header, 16px inputs).
