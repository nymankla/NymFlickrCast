# NymFlickrCast

Show your Flickr albums on a TV. NymFlickrCast is a small Rust app that runs on your computer. It serves a web page where you browse a Flickr user's public albums, preview the photos, and cast an album as a slideshow to a Chromecast or other Google Cast device with a screen.

The web page also works on phones and iPads on the same Wi-Fi, so you can use one as a remote control.

## Features

- Albums of any Flickr user, newest first
- In-browser preview: photo grid, full-screen viewer, swipe and arrow-key navigation, slideshow
- Casting to Chromecast / Google TV / Nest Hub. Speakers and speaker groups are hidden.
- Adjustable slideshow delay, shared by the preview and the cast (default 4 s)
- Opening another album while casting switches the TV to that album
- Photos up to 2048 px, cached in memory for 2 hours. The album list is cached for 1 minute.
- A single binary with no other installs. If the API key is missing, the web app asks for it.

## How it works

```
[ Flickr API ] ──(1. photo URLs)──> [ NymFlickrCast ] ──(3. "show this URL")──> [ Google Cast device ]
                                         │                                      (Default Media Receiver)
                                    (2. local HTTP server) <──(4. fetches photo)──────────┘
```

A Cast device doesn't receive a video stream. It is given a URL and downloads the photo itself. NymFlickrCast therefore runs two small HTTP servers:

- **Web app** (port 8080): the page you use, plus a JSON API.
- **Photo server** (port 8765, or a free port if that one is taken): the Cast device fetches photos from here. The app downloads each photo from Flickr, caches it, and passes it on.

## Requirements

- A Flickr API key. Get one for free at <https://www.flickr.com/services/apps/create/>. The secret isn't needed.
- To build: Rust 1.85 or newer (edition 2024).
- To cast: the computer and the Cast device must be on the same network. Your firewall must let other devices on the network reach the app's ports.

## Download

Windows builds are on the [Releases page](https://github.com/nymankla/NymFlickrCast/releases/latest). Unzip and run `nym_flickr_cast.exe`. The web app asks for your Flickr API key on first start.

## Build from source

1. Put your API key in `secrets/flickrkey.txt`. The file can contain just the key, or the key and secret copied from Flickr. See [secrets/README.md](secrets/README.md).
   If you skip this step, the web app asks for the key on first start and saves it there.
2. Build and run:

   ```
   cargo run --release -- [--port N] [flickr-username-or-NSID]
   ```

   - `--port N` / `-p N`: web app port (default 8080).
   - The username is optional. Without it, the web app opens the user you last viewed, or the built-in default user (`DEFAULT_USER` in `src/main.rs`).

The browser opens automatically. The console also prints the address to use from a phone or iPad, for example `http://192.168.1.10:8080/`.

On Windows, allow the app through Windows Firewall when asked, on private networks. Otherwise phones can't open the page and the Cast device can't fetch the photos.

## Usage

- **Albums:** click an album to see its photos. Click a photo to open the viewer: swipe or use the arrow keys, ▶ for a slideshow, ⛶ for full screen, Esc to close.
- **Delay:** the **Delay** field in the album bar sets seconds per photo.
- **Cast:** the Cast icon (top right) lists the Cast devices it finds. Pick one to cast the open album, starting from the photo you're viewing. Click the icon again to stop.

## Run as a Windows service

To have NymFlickrCast start with Windows and run with no one logged in:

1. Move the unzipped folder somewhere permanent, for example `C:\Program Files\NymFlickrCast`. The service runs the exe from there and keeps the API key in its `secrets` folder.
2. In PowerShell **as administrator**, in that folder:

   ```
   powershell -ExecutionPolicy Bypass -File .\install-service.ps1 [-Port 8080]
   ```

   This registers the service (delayed automatic start, restarts if it crashes), allows the app through Windows Firewall on private networks, and starts it.
3. Open `http://localhost:8080/`, or the PC's address from a phone.

To remove it: `.\install-service.ps1 -Uninstall`. A service has no console window, so the log lines aren't shown.

## Limitations

- Only **public** photos and albums are shown. The app uses an API key, not a Flickr login. Only the first 500 albums of a user are listed.
- Anyone on your local network who can open the page can control casting. There is no login. Other websites you visit can't control it.
- The photo cache lives in memory, up to about 768 MB (roughly 1000 photos at 2048 px), and is empty again after a restart.
- Slides on the TV can run a few seconds longer than the set delay.

## Development

```
cargo build
cargo test
```

All server code is in `src/main.rs`. The web page is `src/ui.html` and `src/ui.js` (type-checked JavaScript via JSDoc, no build step), compiled into the binary. [CLAUDE.md](CLAUDE.md) describes the architecture in more detail.

## License

[MIT](LICENSE)
