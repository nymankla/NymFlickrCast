//! Flickr → Chromecast bridge. A web app (`--port`, default 8080) to browse albums, preview them
//! and cast them; a media server (MEDIA_PORT) the Chromecast fetches photos from.

use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{Media, StreamType};
use rust_cast::channels::receiver::CastDeviceApp;
use rust_cast::{CastDevice, ChannelMessage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const API: &str = "https://api.flickr.com/services/rest/";
const DEFAULT_UI_PORT: u16 = 8080;
/// Preferred photo server port; a free one is picked if it's taken (the TV gets the URL anyway).
const MEDIA_PORT: u16 = 8765;
const KEY_FILE: &str = "secrets/flickrkey.txt";
/// Flickr user shown when none is given (URL/CLI or remembered in the browser).
const DEFAULT_USER: &str = "Sigtuna_Nym";
// shortcut: in-memory cache (~0.5 GB for a 500-photo album at 2048 px), lost on restart; move to disk if that matters.
const CACHE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
// Short, so albums added on Flickr show up quickly.
const ALBUM_CACHE_TTL: Duration = Duration::from_secs(60);

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Resp = tiny_http::ResponseBox;

/// Seconds per photo, for the cast and the browser preview; set from the web app.
// shortcut: not saved, back to 4 s on restart; persist it next to the key if that annoys.
static SLIDE_SECS: AtomicU64 = AtomicU64::new(4);
/// Flickr API key; None until a valid one is loaded from KEY_FILE or entered in the web app.
static KEY: Mutex<Option<String>> = Mutex::new(None);
/// Flickr photo id → large photo URL, filled as albums are opened. Only these are served.
static PHOTOS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
/// Album id → photo ids in order, for casting a whole album.
static ALBUMS: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(Default::default);
/// Bumped to start or stop a cast; a running cast stops when it no longer matches.
static CAST_GEN: AtomicU64 = AtomicU64::new(0);
static CAST_STATUS: LazyLock<Mutex<Value>> = LazyLock::new(|| Mutex::new(json!({"active": false, "text": ""})));
/// Port the photo server actually got.
static MEDIA_PORT_USED: AtomicU16 = AtomicU16::new(MEDIA_PORT);
/// When the running cast last heard from its device. Pings arrive every ~5 s, so a long silence
/// means the device is gone (e.g. powered off) and the cast thread's receive() will never return.
static CAST_LAST_SEEN: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
const CAST_SILENCE_LIMIT: Duration = Duration::from_secs(20);
/// One agent for all Flickr requests: reuses connections, and the timeout stops a stalled download hanging forever.
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build());

fn main() -> Res<()> {
    // ureq brings rustls with ring and rust_cast with aws-lc-rs; with both compiled in, rustls
    // can't pick a provider by itself and panics on the first TLS connection to a Cast device.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ui_port, user) = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}
Usage: nym_flickr_cast [--port N] [flickr-username-or-NSID]");
            std::process::exit(2);
        }
    };
    let mut url = format!("http://localhost:{ui_port}/");
    if let Some(user) = user {
        url += &format!("?user={}", url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>());
    }
    if std::net::TcpListener::bind(("0.0.0.0", ui_port)).is_err() {
        println!("Port {ui_port} is in use, so NymFlickrCast is probably already running. Opening {url}");
        open_browser(&url);
        return Ok(());
    }

    // A missing or rejected key doesn't stop the app; the web app asks for one instead.
    let key = std::fs::read_to_string(KEY_FILE).ok().and_then(|t| parse_key(&t));
    match &key {
        None => println!("No Flickr API key in {KEY_FILE}; the web app will ask for one."),
        Some(k) if matches!(key_valid(k), Ok(false)) => println!("Flickr rejected the key in {KEY_FILE}; the web app will ask for a new one."),
        Some(k) => *KEY.lock().unwrap() = Some(k.clone()),
    }

    serve(ui_port, Arc::new(ui_route))?;
    let media: Arc<dyn Fn(&tiny_http::Request) -> Res<Resp> + Send + Sync> = Arc::new(|req| {
        // Logged so you can see whether the Cast device reaches us (firewall issues show up as silence).
        println!("media: {} {} {}", req.remote_addr().map(|a| a.to_string()).unwrap_or_default(), req.method(), req.url());
        match req.url().strip_prefix("/img/") {
            Some(id) => photo(id),
            None => Ok(text(404, "not found")),
        }
    });
    let media_port = serve(MEDIA_PORT, media.clone()).or_else(|_| serve(0, media))?;
    MEDIA_PORT_USED.store(media_port, Ordering::SeqCst);
    if media_port != MEDIA_PORT {
        println!("Port {MEDIA_PORT} is busy; the Chromecast photo server uses port {media_port} instead.");
    }

    println!("NymFlickrCast {} running at {url} (Ctrl+C to quit)", env!("CARGO_PKG_VERSION"));
    if let Ok(ip) = local_ip() {
        println!("On a phone/iPad on the same Wi-Fi: http://{ip}:{ui_port}/");
    }
    open_browser(&url);
    loop {
        std::thread::park();
    }
}

/// Run an HTTP server on its own thread, one thread per request. Returns the port it got (`port` 0 = any free one).
fn serve(port: u16, handler: Arc<dyn Fn(&tiny_http::Request) -> Res<Resp> + Send + Sync>) -> Res<u16> {
    let server = tiny_http::Server::http(("0.0.0.0", port))?;
    let port = server.server_addr().to_ip().map_or(port, |a| a.port());
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let resp = handler(&req).unwrap_or_else(|e| {
                    tiny_http::Response::from_string(json!({"error": e.to_string()}).to_string())
                        .with_status_code(500)
                        .with_header(header("application/json"))
                        .boxed()
                });
                let _ = req.respond(resp);
            });
        }
    });
    Ok(port)
}

fn ui_route(req: &tiny_http::Request) -> Res<Resp> {
    let (path, query) = req.url().split_once('?').unwrap_or((req.url(), ""));
    let q: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let arg = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let post = *req.method() == tiny_http::Method::Post;
    let key = || KEY.lock().unwrap().clone().ok_or("Flickr API key missing");
    Ok(match path {
        "/" => tiny_http::Response::from_string(include_str!("ui.html")).with_header(header("text/html; charset=utf-8")).boxed(),
        "/ui.js" => tiny_http::Response::from_string(include_str!("ui.js")).with_header(header("text/javascript; charset=utf-8")).boxed(),
        _ if path.starts_with("/img/") => photo(&path[5..])?,
        "/api/config" => json_resp(json!({"version": env!("CARGO_PKG_VERSION"), "defaultUser": DEFAULT_USER, "needKey": KEY.lock().unwrap().is_none(), "slideSecs": SLIDE_SECS.load(Ordering::SeqCst)})),
        "/api/settings" if post => {
            let secs: u64 = arg("slideSecs").parse().map_err(|_| "delay must be a whole number of seconds")?;
            SLIDE_SECS.store(secs.clamp(2, 3600), Ordering::SeqCst);
            json_resp(json!({"slideSecs": SLIDE_SECS.load(Ordering::SeqCst)}))
        }
        "/api/key" if post => json_resp(set_key(arg("key"))?),
        "/api/albums" => json_resp(albums(&key()?, arg("user"))?),
        "/api/photos" => json_resp(album_photos(&key()?, arg("user"), arg("album"))?),
        "/api/devices" => json_resp(json!(discover_cast_devices(Duration::from_secs(3)))),
        "/api/status" => json_resp(cast_status()),
        "/api/cast" if post => {
            let ids = ALBUMS.lock().unwrap().get(arg("album")).cloned().ok_or("open the album first")?;
            let addr: std::net::SocketAddr = arg("device").parse()?;
            start_cast(addr, arg("name").to_string(), arg("album").to_string(), ids, arg("start").parse().unwrap_or(0));
            json_resp(json!({"ok": true}))
        }
        "/api/stop" if post => {
            CAST_GEN.fetch_add(1, Ordering::SeqCst);
            *CAST_STATUS.lock().unwrap() = json!({"active": false, "text": "Stopped"});
            json_resp(json!({"ok": true}))
        }
        _ => text(404, "not found"),
    })
}

/// `[--port N | -p N] [user]` → (web app port, optional Flickr user to open).
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<(u16, Option<String>), String> {
    let (mut port, mut user) = (DEFAULT_UI_PORT, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" | "-p" => {
                let v = args.next().ok_or("--port needs a number")?;
                port = v.parse().ok().filter(|p| *p != 0).ok_or(format!("invalid port: {v}"))?;
            }
            _ if a.starts_with('-') => return Err(format!("unknown option: {a}")),
            _ if user.is_none() => user = Some(a),
            _ => return Err(format!("unexpected argument: {a}")),
        }
    }
    Ok((port, user))
}

/// The key file may hold labels/secret too; the API key is the 32-hex-char token.
fn parse_key(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|t| t.len() == 32)
        .map(str::to_string)
}

/// Ok(false) only when Flickr says the key is invalid; network trouble is an Err.
fn key_valid(key: &str) -> Res<bool> {
    Ok(flickr_raw(key, "flickr.test.echo", &[])?["stat"] == "ok")
}

/// Key entered in the web app: accepted only while none is set, checked with Flickr, then saved.
fn set_key(input: &str) -> Res<Value> {
    if KEY.lock().unwrap().is_some() {
        return Err("an API key is already set".into());
    }
    let key = parse_key(input).ok_or("that doesn't look like a Flickr API key (32 letters/digits, 0-9 a-f)")?;
    if !key_valid(&key)? {
        return Err("Flickr rejected that key".into());
    }
    std::fs::create_dir_all("secrets")?;
    std::fs::write(KEY_FILE, &key)?;
    *KEY.lock().unwrap() = Some(key);
    Ok(json!({"ok": true}))
}

/// Flickr's answer, failing only on network/HTTP errors.
fn flickr_raw(key: &str, method: &str, params: &[(&str, &str)]) -> Res<Value> {
    let mut req = HTTP.get(API)
        .query("method", method)
        .query("api_key", key)
        .query("format", "json")
        .query("nojsoncallback", "1");
    for (k, v) in params {
        req = req.query(k, v);
    }
    Ok(req.call()?.into_json()?)
}

/// Flickr's answer, also failing when Flickr reports an error.
fn flickr(key: &str, method: &str, params: &[(&str, &str)]) -> Res<Value> {
    let v = flickr_raw(key, method, params)?;
    if v["stat"] != "ok" {
        return Err(format!("{method}: {}", v["message"].as_str().unwrap_or("error")).into());
    }
    Ok(v)
}

/// Flickr returns numbers as either strings or ints depending on the field.
fn num(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str()?.parse().ok()).unwrap_or(0)
}

/// Album list per user, kept for ALBUM_CACHE_TTL.
static ALBUM_CACHE: LazyLock<Mutex<HashMap<String, (Instant, Value)>>> = LazyLock::new(Default::default);

fn albums(key: &str, user: &str) -> Res<Value> {
    if let Some((t, v)) = ALBUM_CACHE.lock().unwrap().get(user) {
        if t.elapsed() < ALBUM_CACHE_TTL {
            return Ok(v.clone());
        }
    }
    let v = fetch_albums(key, user)?;
    let mut cache = ALBUM_CACHE.lock().unwrap();
    cache.retain(|_, (t, _)| t.elapsed() < ALBUM_CACHE_TTL);
    cache.insert(user.to_string(), (Instant::now(), v.clone()));
    Ok(v)
}

fn fetch_albums(key: &str, user: &str) -> Res<Value> {
    let nsid = if user.contains('@') {
        user.to_string()
    } else {
        let r = flickr(key, "flickr.people.findByUsername", &[("username", user)])?;
        r["user"]["nsid"].as_str().ok_or("user not found")?.to_string()
    };
    // shortcut: public albums only (API key, no OAuth), first 500 albums; add OAuth / paging if needed.
    let r = flickr(key, "flickr.photosets.getList", &[
        ("user_id", &nsid), ("per_page", "500"), ("primary_photo_extras", "url_q"),
    ])?;
    let mut list: Vec<Value> = r["photosets"]["photoset"].as_array().into_iter().flatten().map(|a| json!({
        "id": a["id"],
        "title": a["title"]["_content"],
        "count": num(&a["photos"]),
        "created": num(&a["date_create"]),
        "thumb": a["primary_photo_extras"]["url_q"],
    })).collect();
    list.sort_by_key(|a| std::cmp::Reverse(a["created"].as_u64()));
    Ok(json!({"nsid": nsid, "albums": list}))
}

fn album_photos(key: &str, nsid: &str, album: &str) -> Res<Value> {
    let mut photos = Vec::new();
    let mut page = 1;
    loop {
        let p = page.to_string();
        let r = flickr(key, "flickr.photosets.getPhotos", &[
            ("photoset_id", album), ("user_id", nsid), ("extras", "url_q,url_k,url_h,url_l,url_c,url_z"),
            ("per_page", "500"), ("page", &p), ("media", "photos"),
        ])?;
        for ph in r["photoset"]["photo"].as_array().into_iter().flatten() {
            if let (Some(id), Some(u)) = (ph["id"].as_str(), best_url(ph)) {
                PHOTOS.lock().unwrap().insert(id.to_string(), u.to_string());
                photos.push(json!({"id": id, "title": ph["title"], "thumb": ph["url_q"]}));
            }
        }
        if page >= num(&r["photoset"]["pages"]) {
            break;
        }
        page += 1;
    }
    let ids = photos.iter().filter_map(|p| p["id"].as_str().map(str::to_string)).collect();
    ALBUMS.lock().unwrap().insert(album.to_string(), ids);
    Ok(json!(photos))
}

/// Largest photo URL up to 2048 px: a Chromecast shows at most 1920x1080, and bigger files only slow it down.
/// Sizes above 1024 px are missing when the owner disallows downloads, so fall back.
fn best_url(ph: &Value) -> Option<&str> {
    ["url_k", "url_h", "url_l", "url_c", "url_z"].iter().find_map(|k| ph[*k].as_str())
}

/// Serve a known photo by Flickr id, through the cache.
fn photo(id: &str) -> Res<Resp> {
    let Some(url) = PHOTOS.lock().unwrap().get(id).cloned() else { return Ok(text(404, "unknown photo")) };
    let bytes = fetch(&url)?;
    let len = bytes.len();
    // Send Content-Length instead of tiny_http's default chunking above 32 KB; simpler for Cast receivers.
    Ok(tiny_http::Response::new(200.into(), vec![header("image/jpeg")], std::io::Cursor::new(bytes), Some(len), None)
        .with_chunked_threshold(usize::MAX)
        .boxed())
}

/// Photo bytes by Flickr URL, kept for CACHE_TTL. Shared (Arc) so a cache hit doesn't copy the photo.
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Arc<[u8]>)>>> = LazyLock::new(Default::default);

fn fetch(url: &str) -> Res<Arc<[u8]>> {
    if let Some((t, bytes)) = CACHE.lock().unwrap().get(url) {
        if t.elapsed() < CACHE_TTL {
            return Ok(bytes.clone());
        }
    }
    let mut buf = Vec::new();
    HTTP.get(url).call()?.into_reader().read_to_end(&mut buf)?;
    let bytes: Arc<[u8]> = buf.into();
    let mut cache = CACHE.lock().unwrap();
    cache.retain(|_, (t, _)| t.elapsed() < CACHE_TTL);
    cache.insert(url.to_string(), (Instant::now(), bytes.clone()));
    Ok(bytes)
}

fn header(content_type: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes("Content-Type", content_type).unwrap()
}

fn json_resp(v: Value) -> Resp {
    tiny_http::Response::from_string(v.to_string()).with_header(header("application/json")).boxed()
}

fn text(code: u16, s: &str) -> Resp {
    tiny_http::Response::from_string(s).with_status_code(code).boxed()
}

/// This PC's address on the network with the default route (no packet is actually sent); only for the startup hint.
fn local_ip() -> Res<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.connect("8.8.8.8:80")?;
    Ok(s.local_addr()?.ip())
}

fn discover_cast_devices(wait: Duration) -> Vec<Value> {
    let mut found: Vec<Value> = Vec::new();
    let Ok(mdns) = mdns_sd::ServiceDaemon::new() else { return found };
    let Ok(rx) = mdns.browse("_googlecast._tcp.local.") else { return found };
    let end = Instant::now() + wait;
    while let Ok(ev) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        if let mdns_sd::ServiceEvent::ServiceResolved(s) = ev {
            if !has_video(s.txt_properties.get_property_val_str("ca")) {
                continue;
            }
            let name = s.txt_properties.get_property_val_str("fn").unwrap_or(&s.fullname).to_string();
            if let Some(ip) = s.get_addresses_v4().into_iter().next() {
                if !found.iter().any(|d| d["name"] == name) {
                    found.push(json!({"name": name, "addr": format!("{ip}:{}", s.port)}));
                }
            }
        }
    }
    let _ = mdns.shutdown();
    found
}

/// Cast devices advertise capabilities in the `ca` TXT bitmask; bit 0 = video out.
/// Speakers and speaker groups lack it. A device that doesn't advertise `ca` is kept.
fn has_video(ca: Option<&str>) -> bool {
    ca.and_then(|c| c.parse::<u64>().ok()).is_none_or(|c| c & 1 != 0)
}

/// Start a slideshow on a Cast device in the background, replacing any running one.
/// While active, the status also names the device (`addr`, `device`) and `album`, so the web app can
/// switch a running cast to another album on the same device.
fn start_cast(addr: std::net::SocketAddr, name: String, album: String, ids: Vec<String>, start: usize) {
    let generation = CAST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    mark_cast_alive();
    *CAST_STATUS.lock().unwrap() = json!({"active": true, "text": format!("Connecting to {name}…"), "device": name, "addr": addr.to_string(), "album": album});
    std::thread::spawn(move || {
        // A panic would otherwise leave the UI on "Connecting…" forever.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cast_slideshow(addr, &name, &album, &ids, start, generation)))
            .unwrap_or_else(|_| Err("internal error, see the console".into()));
        let text = match result {
            Ok(()) => "Stopped".to_string(),
            Err(e) => format!("Cast error: {e}"),
        };
        set_cast_status(generation, json!({"active": false, "text": text}));
    });
}

/// Status update from a cast thread; dropped if that cast was replaced or stopped meanwhile.
/// Checked under the status lock, so it can't overwrite what Stop wrote (Stop bumps CAST_GEN first).
fn set_cast_status(generation: u64, status: Value) {
    let mut current = CAST_STATUS.lock().unwrap();
    if CAST_GEN.load(Ordering::SeqCst) == generation {
        *current = status;
    }
}

fn mark_cast_alive() {
    *CAST_LAST_SEEN.lock().unwrap() = Instant::now();
}

/// Current cast status. A cast whose device has been silent too long is ended here, because its
/// thread is stuck in receive() and can't report it (that thread is left behind).
fn cast_status() -> Value {
    let mut status = CAST_STATUS.lock().unwrap();
    if status["active"] == true && CAST_LAST_SEEN.lock().unwrap().elapsed() > CAST_SILENCE_LIMIT {
        CAST_GEN.fetch_add(1, Ordering::SeqCst);
        let device = status["device"].as_str().unwrap_or("the device").to_string();
        *status = json!({"active": false, "text": format!("Lost connection to {device}")});
    }
    status.clone()
}

fn cast_slideshow(addr: std::net::SocketAddr, name: &str, album: &str, ids: &[String], start: usize, generation: u64) -> Res<()> {
    // rust_cast has no connect timeout; fail fast with a clear message when the device is unreachable.
    let probe = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("can't reach {name} at {addr}: {e}"))?;
    // Our address on the network that reaches the device (not the default route, which may be a VPN).
    let base = format!("http://{}:{}", probe.local_addr()?.ip(), MEDIA_PORT_USED.load(Ordering::SeqCst));
    drop(probe);
    let dev = CastDevice::connect_without_host_verification(addr.ip().to_string(), addr.port())?;
    dev.connection.connect("receiver-0")?;
    dev.heartbeat.ping()?;
    let app = dev.receiver.launch_app(&CastDeviceApp::DefaultMediaReceiver)?;
    dev.connection.connect(app.transport_id.as_str())?;
    for i in (0..ids.len()).cycle().skip(start) {
        let media = Media {
            content_id: format!("{base}/img/{}", ids[i]),
            content_type: "image/jpeg".into(),
            stream_type: StreamType::None,
            metadata: None,
            duration: None,
        };
        dev.media.load(app.transport_id.as_str(), app.session_id.as_str(), &media)?;
        mark_cast_alive();
        // Warm the cache with the next photo, so the TV doesn't wait for Flickr on the next slide.
        if let Some(next) = PHOTOS.lock().unwrap().get(&ids[(i + 1) % ids.len()]).cloned() {
            std::thread::spawn(move || { let _ = fetch(&next); });
        }
        set_cast_status(generation, json!({"active": true, "text": format!("Casting to {name} · {}/{}", i + 1, ids.len()), "device": name, "addr": addr.to_string(), "album": album}));
        // Keep the connection alive while the photo is shown.
        // shortcut: receive() blocks until the next message (pings arrive every ~5s), so slides and Stop may lag a few seconds.
        let end = Instant::now() + Duration::from_secs(SLIDE_SECS.load(Ordering::SeqCst));
        while Instant::now() < end {
            if CAST_GEN.load(Ordering::SeqCst) != generation {
                let _ = dev.receiver.stop_app(app.session_id.as_str());
                return Ok(());
            }
            let msg = dev.receive()?;
            mark_cast_alive();
            if let ChannelMessage::Heartbeat(HeartbeatResponse::Ping) = msg {
                dev.heartbeat.pong()?;
            }
        }
    }
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    let _ = r;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_and_numbers() {
        let k = "0123456789abcdef0123456789abcdef";
        assert_eq!(parse_key(&format!("Key:\n{k}\n\nSecret:\n0123456789abcdef\n")).as_deref(), Some(k));
        assert_eq!(parse_key("nothing here"), None);
        assert_eq!(num(&serde_json::json!("42")), 42);
        assert_eq!(num(&serde_json::json!(7)), 7);
    }

    #[test]
    fn cli_args() {
        let p = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        assert_eq!(p(&[]), Ok((8080, None)));
        assert_eq!(p(&["--port", "9000", "bob"]), Ok((9000, Some("bob".into()))));
        assert_eq!(p(&["bob", "-p", "9001"]), Ok((9001, Some("bob".into()))));
        assert!(p(&["--port"]).is_err());
        assert!(p(&["--port", "abc"]).is_err());
        assert!(p(&["--port", "0"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["a", "b"]).is_err());
    }

    #[test]
    fn album_cache_hit() {
        let v = json!({"nsid": "x@N00", "albums": []});
        ALBUM_CACHE.lock().unwrap().insert("cached-user".into(), (Instant::now(), v.clone()));
        assert_eq!(albums("no-key", "cached-user").unwrap(), v); // no Flickr call needed
    }

    #[test]
    fn best_photo_size() {
        assert_eq!(best_url(&json!({"url_k": "k", "url_l": "l"})), Some("k"));
        assert_eq!(best_url(&json!({"url_l": "l", "url_z": "z"})), Some("l")); // owner blocks > 1024 px
        assert_eq!(best_url(&json!({"url_o": "o", "url_q": "q"})), None); // original and thumbnail aren't used
    }

    // One test for all cast status logic, since CAST_GEN/CAST_STATUS are globals shared between tests.
    #[test]
    fn cast_status_updates() {
        let generation = CAST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        mark_cast_alive();
        set_cast_status(generation, json!({"active": true, "text": "Casting", "device": "TV"}));
        assert_eq!(cast_status()["text"], "Casting");
        // Stop bumps the generation; a late update from the old cast must not overwrite it.
        CAST_GEN.fetch_add(1, Ordering::SeqCst);
        *CAST_STATUS.lock().unwrap() = json!({"active": false, "text": "Stopped"});
        set_cast_status(generation, json!({"active": true, "text": "Casting 6/30"}));
        assert_eq!(cast_status()["text"], "Stopped");
        // A device that has gone silent is reported as lost, and its late updates are dropped.
        let generation = CAST_GEN.load(Ordering::SeqCst);
        set_cast_status(generation, json!({"active": true, "text": "Casting", "device": "TV"}));
        *CAST_LAST_SEEN.lock().unwrap() = Instant::now().checked_sub(CAST_SILENCE_LIMIT + Duration::from_secs(1)).unwrap();
        assert_eq!(cast_status()["text"], "Lost connection to TV");
        set_cast_status(generation, json!({"active": true, "text": "Casting 7/30"}));
        assert_eq!(cast_status()["text"], "Lost connection to TV");
    }

    #[test]
    fn video_capable_devices() {
        assert!(has_video(Some("465413"))); // Chromecast
        assert!(has_video(Some("231941"))); // Nest Hub (has a screen)
        assert!(!has_video(Some("199172"))); // Nest Audio / Google Home
        assert!(!has_video(Some("198660"))); // Nest Mini
        assert!(!has_video(Some("199204"))); // speaker group
        assert!(has_video(None));
    }

    #[test]
    fn cache_hit_and_expiry() {
        let url = "http://invalid.test/x.jpg";
        CACHE.lock().unwrap().insert(url.into(), (Instant::now(), Arc::from([1u8, 2, 3])));
        assert_eq!(&*fetch(url).unwrap(), &[1, 2, 3]); // served without network
        let old = Instant::now().checked_sub(CACHE_TTL + Duration::from_secs(1)).unwrap();
        CACHE.lock().unwrap().insert(url.into(), (old, Arc::from([1u8, 2, 3])));
        assert!(fetch(url).is_err()); // expired, so it refetches (and the fake URL fails)
    }
}
