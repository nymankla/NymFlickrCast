# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust bridge from Flickr to Chromecast. It is a single binary that serves a web app for browsing a Flickr user's public albums (newest first), previewing them in the browser, and casting an album as a slideshow to a Google Cast device. Original requirements are in `readme.md`.

## Commands

```
cargo build
cargo test                          # unit tests (no network needed)
cargo test cache_hit_and_expiry     # single test
cargo run --release -- [user]       # optional Flickr username or NSID (e.g. 35067687@N04) prefills the UI
```

The app needs the Flickr API key in `secrets/flickrkey.txt`. `parse_key` takes the first 32-hex-char token, so the file may also hold labels or the secret. Don't print the contents of this file. `secrets/` is gitignored.

The app runs until killed. A running instance holds both ports, so a second launch fails with `AddrInUse` (os error 10048).

## Architecture

All Rust code is in `src/main.rs`. The whole UI is in `src/ui.html` (vanilla JS, no build step), embedded with `include_str!`, so a rebuild is needed after editing it.

Two `tiny_http` servers, both bound to `0.0.0.0` so phones and iPads on the LAN can reach them:
- **UI_PORT 8080**: the page `/`, the JSON API `/api/*`, and `/img/<photo id>` for the in-app preview.
- **MEDIA_PORT 8765**: only `/img/<photo id>`. The Chromecast fetches photos from here, at `http://<local_ip()>:8765/img/<id>`.

Data flow and global state (statics in `main.rs`):
- `/api/albums?user=` resolves the username to an NSID, then calls `flickr.photosets.getList`.
- `/api/photos?user=<nsid>&album=` pages through `flickr.photosets.getPhotos` and fills two maps:
  - `PHOTOS` (photo id → large Flickr URL). `/img` only serves ids in this map, so the server is not an open proxy.
  - `ALBUMS` (album id → ordered photo ids). `/api/cast` reads this, so an album must have been opened in the UI before it can be cast.
- `fetch` keeps an in-memory `CACHE` of photo bytes for 2 hours (`CACHE_TTL`). Expired entries are pruned when new ones are inserted.

Casting:
- `/api/devices` runs an mDNS browse for `_googlecast._tcp` for 3 seconds.
- `POST /api/cast?device=ip:port&name=&album=&start=` starts `cast_slideshow` on a background thread. It uses `rust_cast` to launch the Default Media Receiver and calls `media.load` for each photo every `SLIDE_SECS` (8 s).
- The `CAST_GEN` counter controls start and stop: a new cast or `POST /api/stop` bumps it, and the running cast loop exits when its generation no longer matches.
- The cast loop must answer heartbeat `Ping`s with `pong()`, or the device drops the connection.
- `CAST_STATUS` holds the JSON that the UI polls at `/api/status`.

Flickr access uses an API key only, with no OAuth. Only public albums are visible, and only the first 500 albums are listed. Flickr returns some numbers as strings, so read numeric fields with `num()`.

## Conventions

- Known limitations are marked with `// shortcut: <limit>, <when to upgrade>` comments.
- Errors use the `Res<T>` alias (boxed `Send + Sync` error). In `serve`, handler errors become a JSON `{"error": ...}` 500 response, which `api()` in `ui.html` shows to the user.
- The UI must keep working on phones and iPads (touch swipe, wrapping header, 16px inputs).
