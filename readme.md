# Flickr to Chromcast bridge

* The bridge is a Rust app 
* If possible, when the app start, query avaible, let the user choos from a list of Albums. Sort newest first 
* To cast media, a Google Cast device doesn't actually receive a video stream from the app. Instead, it is given a URL pointing to a
media asset, which its internal web browser downloads and plays. 
* The app can also show the pictures on the computers own screen to be able to preview.


Architecture proposal look like this:
[ Flickr API ] ──(1. Get Image URLs)──> [ Rust App ] ──(3. Control Slideshow)──┐
                                               │                                  │
                                     (2. Host Local Web Server)                   ▼
                                               │                       [ Google Cast Device ]
                                               └───────(4. Streams)──────> (Default Receiver)
1. The Backend / Fetcher: Pulls photo data and image source links from Flickr.
2. The Local Media Server: Since Flickr's direct photo URLs can sometimes expire or contain restrictive CORS headers, the Rust app
will run a tiny local HTTP server to proxy or serve the image queue.
3. The Cast Controller: Discovers Cast devices on your local network and sends the image URLs sequentially to create a slideshow.

## Run

```
cargo run --release -- [flickr-username-or-NSID]
```

Needs the Flickr API key in `secrets/flickrkey.txt`. Opens the web app at `http://localhost:8080/`; the console also prints
the LAN address to use from a phone or iPad on the same Wi-Fi.

* Browse albums (newest first), tap an album for its photos, tap a photo to preview (swipe / arrow keys, ▶ for slideshow).
* The Cast icon (top right) lists Chromecasts; pick one to cast the open album from the current photo. Tap it again to stop.
* Ports: 8080 = web app, 8765 = photo URLs the Chromecast fetches. Windows Firewall must allow both on the private network.
