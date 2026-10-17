//! Flickr → Chromecast bridge. A web app (`--port`, default 8080) to browse albums, preview them
//! and cast them; a media server (`MEDIA_PORT`) the Chromecast fetches photos from.

use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{IdleReason, Media, MediaResponse, StreamType};
use rust_cast::channels::receiver::{Application, CastDeviceApp, ReceiverResponse};
use rust_cast::{CastDevice, ChannelMessage};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const API: &str = "https://api.flickr.com/services/rest/";
const DEFAULT_UI_PORT: u16 = 8080;
/// Preferred photo server port; a free one is picked if it's taken (the TV gets the URL anyway).
const MEDIA_PORT: u16 = 8765;
const KEY_FILE: &str = "secrets/flickrkey.txt";
/// Flickr user shown when none is given (URL/CLI or remembered in the browser).
const DEFAULT_USER: &str = "Sigtuna_Nym";
// shortcut: in-memory photo cache, lost on restart; move to disk if that matters.
const CACHE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// Photo cache size limit (about 1000 photos at 2048 px); the oldest photos are dropped first.
const CACHE_MAX_BYTES: usize = 768 * 1024 * 1024;
/// Largest photo download accepted (a 2048 px JPEG is usually under 2 MB).
const MAX_PHOTO_BYTES: u64 = 32 * 1024 * 1024;
/// Threads per HTTP server: enough for the UI, the TV and preloading, without one thread per connection.
const HTTP_WORKERS: usize = 16;
/// Header the web app sends with every API call. A page on another site can't add it to a request
/// without CORS permission, which the server never gives, so it can't POST to the API (CSRF).
const API_HEADER: &str = "X-NymFlickrCast";
/// Error text the web app recognises to show the key form again (`KEY_MISSING` in ui.ts).
const KEY_MISSING: &str = "Flickr API key missing";
// Short, so albums added on Flickr show up quickly.
const ALBUM_CACHE_TTL: Duration = Duration::from_secs(60);

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Resp = tiny_http::ResponseBox;
type Handler = Arc<dyn Fn(&tiny_http::Request) -> Res<Resp> + Send + Sync>;

/// Seconds per photo, for the cast and the browser preview; set from the web app.
// shortcut: not saved, back to 4 s on restart; persist it next to the key if that annoys.
static SLIDE_SECS: AtomicU64 = AtomicU64::new(4);
/// Flickr API key; None until a valid one is loaded from `KEY_FILE` or entered in the web app.
static KEY: Mutex<Option<String>> = Mutex::new(None);
/// Flickr photo id → large photo URL, filled as albums are opened. Only these are served.
static PHOTOS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
/// Album id → photo ids in order, for casting a whole album.
static ALBUMS: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(Default::default);
/// Mailbox for the cast worker. Only the latest command counts, so quick album clicks collapse into the last one.
static CAST_CMD: LazyLock<(Mutex<Option<CastCmd>>, Condvar)> = LazyLock::new(Default::default);
/// Id of the current cast worker; bumped when a stuck one is replaced.
static CAST_WORKER: AtomicU64 = AtomicU64::new(0);
static CAST_STATUS: LazyLock<Mutex<Value>> = LazyLock::new(|| Mutex::new(json!({"active": false, "text": ""})));
/// Port the photo server actually got.
static MEDIA_PORT_USED: AtomicU16 = AtomicU16::new(MEDIA_PORT);
/// When the running cast last heard from its device. Pings arrive every ~5 s, so a long silence
/// means the device is gone (e.g. powered off) and the cast worker's `receive()` will never return.
static CAST_LAST_SEEN: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
const CAST_SILENCE_LIMIT: Duration = Duration::from_secs(20);
/// One agent for all Flickr requests: reuses connections, and the timeout stops a stalled download hanging forever.
static HTTP: LazyLock<ureq::Agent> =
    LazyLock::new(|| ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build().into());

/// Locks `m`, ignoring poisoning: one panicking request must not make every later request fail too.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn main() -> Res<()> {
    // ureq is built without a TLS provider of its own and uses the process default; rust_cast's rustls
    // also uses aws-lc-rs. Install it before the first TLS connection (Flickr or a Cast device).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Added by install-service.ps1: run under the Windows service manager instead of from a console.
    let service = args.iter().position(|a| a == "--service").map(|i| args.remove(i)).is_some();
    let (ui_port, user) = match parse_args(args.into_iter()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "{e}
Usage: nym_flickr_cast [--port N] [--service] [flickr-username-or-NSID]"
            );
            std::process::exit(2);
        }
    };
    if service {
        return run_service(ui_port);
    }
    let query = user.map(|u| format!("?user={}", form_urlencoded::byte_serialize(u.as_bytes()).collect::<String>()));
    let url = format!("http://localhost:{ui_port}/{}", query.unwrap_or_default());
    if std::net::TcpListener::bind(("0.0.0.0", ui_port)).is_err() {
        println!("Port {ui_port} is in use, so NymFlickrCast is probably already running. Opening {url}");
        open_browser(&url);
        return Ok(());
    }
    start(ui_port)?;
    println!("NymFlickrCast {} running at {url} (Ctrl+C to quit)", env!("CARGO_PKG_VERSION"));
    if let Ok(ip) = local_ip() {
        println!("On a phone/iPad on the same Wi-Fi: http://{ip}:{ui_port}/");
    }
    open_browser(&url);
    loop {
        std::thread::park();
    }
}

/// Loads the key and starts the cast worker and both servers, which run on background threads.
fn start(ui_port: u16) -> Res<()> {
    // A missing or rejected key doesn't stop the app; the web app asks for one instead.
    match std::fs::read_to_string(KEY_FILE).ok().and_then(|t| parse_key(&t)) {
        None => println!("No Flickr API key in {KEY_FILE}; the web app will ask for one."),
        Some(k) => {
            *lock(&KEY) = Some(k.clone());
            // Checked in the background: Flickr may be slow or unreachable (e.g. at boot), and the app starts anyway.
            std::thread::spawn(move || {
                if matches!(key_valid(&k), Ok(false)) {
                    println!("Flickr rejected the key in {KEY_FILE}; the web app will ask for a new one.");
                    let mut current = lock(&KEY);
                    if current.as_deref() == Some(k.as_str()) {
                        *current = None;
                    }
                }
            });
        }
    }

    spawn_cast_worker();
    // Notices a silent Cast device even when no browser is polling /api/status.
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            cast_status();
        }
    });
    if let Err(e) = watch_cast_devices() {
        println!("Can't search for Cast devices: {e}");
    }
    let ui: Handler = Arc::new(ui_route);
    serve(ui_port, &ui)?;
    let media: Handler = Arc::new(|req| {
        // Logged so you can see whether the Cast device reaches us (firewall issues show up as silence).
        println!("media: {} {} {}", req.remote_addr().map(ToString::to_string).unwrap_or_default(), req.method(), req.url());
        match req.url().strip_prefix("/img/") {
            Some(id) => photo(id),
            None => Ok(text(404, "not found")),
        }
    });
    let media_port = serve(MEDIA_PORT, &media).or_else(|_| serve(0, &media))?;
    MEDIA_PORT_USED.store(media_port, Ordering::SeqCst);
    if media_port != MEDIA_PORT {
        println!("Port {MEDIA_PORT} is busy; the Chromecast photo server uses port {media_port} instead.");
    }
    Ok(())
}

#[cfg(windows)]
fn run_service(ui_port: u16) -> Res<()> {
    // Services start in System32; secrets\ is next to the exe.
    let exe = std::env::current_exe()?;
    std::env::set_current_dir(exe.parent().ok_or("the exe has no folder")?)?;
    service::PORT.store(ui_port, Ordering::SeqCst);
    service::dispatch()?;
    Ok(())
}

#[cfg(not(windows))]
fn run_service(_ui_port: u16) -> Res<()> {
    Err("--service is only supported on Windows".into())
}

/// Windows service mode (`--service`), registered by install-service.ps1.
// shortcut: a service has no console, so log lines are lost; write them to a file or the event log if you need them.
#[cfg(windows)]
mod service {
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::time::Duration;
    use windows_service::service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType};
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    pub const NAME: &str = "NymFlickrCast";
    /// Web app port, from the command line (the service manager passes its own arguments to `service_main`).
    pub static PORT: AtomicU16 = AtomicU16::new(0);
    windows_service::define_windows_service!(ffi_service_main, service_main);

    /// Hands this thread to the service manager until the service stops; fails when not started by it.
    pub fn dispatch() -> windows_service::Result<()> {
        windows_service::service_dispatcher::start(NAME, ffi_service_main)
    }

    fn service_main(_args: Vec<std::ffi::OsString>) {
        let _ = run();
    }

    fn run() -> crate::Res<()> {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let handle = service_control_handler::register(NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        let status = |state: ServiceState, exit: u32| ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(exit),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        };
        // All state is in memory, so stopping is just reporting Stopped; the process exits after that.
        let exit = match crate::start(PORT.load(Ordering::SeqCst)) {
            Ok(()) => {
                handle.set_service_status(status(ServiceState::Running, 0))?;
                let _ = stop_rx.recv();
                0
            }
            Err(_) => 1, // e.g. the port is taken
        };
        handle.set_service_status(status(ServiceState::Stopped, exit))?;
        Ok(())
    }
}

/// Run an HTTP server on `HTTP_WORKERS` threads. Returns the port it got (`port` 0 = any free one).
fn serve(port: u16, handler: &Handler) -> Res<u16> {
    let server = Arc::new(tiny_http::Server::http(("0.0.0.0", port))?);
    let port = server.server_addr().to_ip().map_or(port, |a| a.port());
    for _ in 0..HTTP_WORKERS {
        let (server, handler) = (server.clone(), handler.clone());
        std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                // A panic becomes a 500 instead of killing this worker for good.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&req)))
                    .unwrap_or_else(|_| Err("internal error".into()));
                let resp = result.unwrap_or_else(|e| {
                    tiny_http::Response::from_string(json!({"error": e.to_string()}).to_string())
                        .with_status_code(500)
                        .with_header(header("application/json"))
                        .boxed()
                });
                let _ = req.respond(resp);
            }
        });
    }
    Ok(port)
}

/// Blocks DNS rebinding: a page on some public domain that resolves to this PC sends that domain as Host.
/// Allowed: IP addresses, single-label names (localhost, the PC's name) and local-network suffixes.
fn host_allowed(host: &str) -> bool {
    if host.starts_with('[') {
        return true; // IPv6 address
    }
    let name = host.split(':').next().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
    name.parse::<std::net::Ipv4Addr>().is_ok()
        || !name.contains('.')
        || [".local", ".lan", ".home", ".home.arpa", ".internal", ".localhost"].iter().any(|suffix| name.ends_with(suffix))
}

fn ui_route(req: &tiny_http::Request) -> Res<Resp> {
    let (path, query) = req.url().split_once('?').unwrap_or_else(|| (req.url(), ""));
    let q: HashMap<String, String> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let arg = |k: &str| q.get(k).map_or("", String::as_str);
    let post = *req.method() == tiny_http::Method::Post;
    let key = || lock(&KEY).clone().ok_or(KEY_MISSING);
    let header_value = |name: &'static str| req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str());
    if !header_value("Host").is_none_or(host_allowed) {
        return Ok(text(403, "unknown host name; open the app by IP address, localhost or the PC's name"));
    }
    if post && header_value(API_HEADER).is_none() {
        return Ok(text(403, "missing API header"));
    }
    Ok(match path {
        "/" => tiny_http::Response::from_string(include_str!("ui.html")).with_header(header("text/html; charset=utf-8")).boxed(),
        "/ui.js" => tiny_http::Response::from_string(include_str!("ui.js")).with_header(header("text/javascript; charset=utf-8")).boxed(),
        _ if path.starts_with("/img/") => photo(&path[5..])?,
        "/api/config" => json_resp(
            &json!({"version": env!("CARGO_PKG_VERSION"), "defaultUser": DEFAULT_USER, "needKey": lock(&KEY).is_none(), "slideSecs": SLIDE_SECS.load(Ordering::SeqCst)}),
        ),
        "/api/settings" if post => {
            let secs: u64 = arg("slideSecs").parse().map_err(|_| "delay must be a whole number of seconds")?;
            SLIDE_SECS.store(secs.clamp(2, 3600), Ordering::SeqCst);
            json_resp(&json!({"slideSecs": SLIDE_SECS.load(Ordering::SeqCst)}))
        }
        "/api/key" if post => json_resp(&set_key(arg("key"))?),
        "/api/albums" => json_resp(&albums(&key()?, arg("user"))?),
        "/api/photos" => json_resp(&album_photos(&key()?, arg("user"), arg("album"))?),
        "/api/devices" => json_resp(&json!(cast_devices(Duration::from_secs(3)))),
        "/api/status" => json_resp(&cast_status()),
        "/api/cast" if post => {
            let ids = lock(&ALBUMS).get(arg("album")).cloned().ok_or("open the album first")?;
            let addr: SocketAddr = arg("device").parse()?;
            let (name, album) = (arg("name").to_owned(), arg("album").to_owned());
            let status =
                json!({"active": true, "text": format!("Starting on {name}…"), "device": name, "addr": addr.to_string(), "album": album});
            post_cast_cmd(CastCmd::Play { addr, name, album, ids, start: arg("start").parse().unwrap_or(0) }, status);
            json_resp(&json!({"ok": true}))
        }
        "/api/stop" if post => {
            post_cast_cmd(CastCmd::Stop, json!({"active": false, "text": "Stopped"}));
            json_resp(&json!({"ok": true}))
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
                port = v.parse().ok().filter(|p| *p != 0).ok_or_else(|| format!("invalid port: {v}"))?;
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
    text.split(|c: char| !c.is_ascii_hexdigit()).find(|t| t.len() == 32).map(str::to_owned)
}

/// Ok(false) only when Flickr says the key is invalid; network trouble is an Err.
fn key_valid(key: &str) -> Res<bool> {
    Ok(flickr_raw(key, "flickr.test.echo", &[])?["stat"] == "ok")
}

/// Key entered in the web app: accepted only while none is set, checked with Flickr, then saved.
fn set_key(input: &str) -> Res<Value> {
    if lock(&KEY).is_some() {
        return Err("an API key is already set".into());
    }
    let key = parse_key(input).ok_or("that doesn't look like a Flickr API key (32 letters/digits, 0-9 a-f)")?;
    if !key_valid(&key)? {
        return Err("Flickr rejected that key".into());
    }
    std::fs::create_dir_all("secrets")?;
    std::fs::write(KEY_FILE, &key)?;
    *lock(&KEY) = Some(key);
    Ok(json!({"ok": true}))
}

/// Flickr's answer, failing only on network/HTTP errors.
fn flickr_raw(key: &str, method: &str, params: &[(&str, &str)]) -> Res<Value> {
    let mut req = HTTP.get(API).query("method", method).query("api_key", key).query("format", "json").query("nojsoncallback", "1");
    for (k, v) in params {
        req = req.query(k, v);
    }
    Ok(req.call()?.body_mut().read_json()?)
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

/// Album list per user, kept for `ALBUM_CACHE_TTL`.
static ALBUM_CACHE: LazyLock<Mutex<HashMap<String, (Instant, Value)>>> = LazyLock::new(Default::default);

fn albums(key: &str, user: &str) -> Res<Value> {
    if let Some((t, v)) = lock(&ALBUM_CACHE).get(user) {
        if t.elapsed() < ALBUM_CACHE_TTL {
            return Ok(v.clone());
        }
    }
    let v = fetch_albums(key, user)?;
    let mut cache = lock(&ALBUM_CACHE);
    cache.retain(|_, (t, _)| t.elapsed() < ALBUM_CACHE_TTL);
    cache.insert(user.to_owned(), (Instant::now(), v.clone()));
    Ok(v)
}

fn fetch_albums(key: &str, user: &str) -> Res<Value> {
    let nsid = if user.contains('@') {
        user.to_owned()
    } else {
        let r = flickr(key, "flickr.people.findByUsername", &[("username", user)])?;
        r["user"]["nsid"].as_str().ok_or("user not found")?.to_owned()
    };
    // shortcut: public albums only (API key, no OAuth), first 500 albums; add OAuth / paging if needed.
    let r = flickr(key, "flickr.photosets.getList", &[("user_id", &nsid), ("per_page", "500"), ("primary_photo_extras", "url_q")])?;
    let mut list: Vec<Value> = r["photosets"]["photoset"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            json!({
                "id": a["id"],
                "title": a["title"]["_content"],
                "count": num(&a["photos"]),
                "created": num(&a["date_create"]),
                "thumb": a["primary_photo_extras"]["url_q"],
            })
        })
        .collect();
    list.sort_by_key(|a| std::cmp::Reverse(a["created"].as_u64()));
    Ok(json!({"nsid": nsid, "albums": list}))
}

fn album_photos(key: &str, nsid: &str, album: &str) -> Res<Value> {
    let mut photos = Vec::new();
    let mut page = 1;
    loop {
        let p = page.to_string();
        let r = flickr(
            key,
            "flickr.photosets.getPhotos",
            &[
                ("photoset_id", album),
                ("user_id", nsid),
                ("extras", "url_q,url_k,url_h,url_l,url_c,url_z"),
                ("per_page", "500"),
                ("page", &p),
                ("media", "photos"),
            ],
        )?;
        for ph in r["photoset"]["photo"].as_array().into_iter().flatten() {
            if let (Some(id), Some(u)) = (ph["id"].as_str(), best_url(ph)) {
                lock(&PHOTOS).insert(id.to_owned(), u.to_owned());
                photos.push(json!({"id": id, "title": ph["title"], "thumb": ph["url_q"]}));
            }
        }
        if page >= num(&r["photoset"]["pages"]) {
            break;
        }
        page += 1;
    }
    let ids = photos.iter().filter_map(|p| p["id"].as_str().map(str::to_owned)).collect();
    lock(&ALBUMS).insert(album.to_owned(), ids);
    Ok(json!(photos))
}

/// Largest photo URL up to 2048 px: a Chromecast shows at most 1920x1080, and bigger files only slow it down.
/// Sizes above 1024 px are missing when the owner disallows downloads, so fall back.
fn best_url(ph: &Value) -> Option<&str> {
    ["url_k", "url_h", "url_l", "url_c", "url_z"].iter().find_map(|k| ph[*k].as_str())
}

/// Serve a known photo by Flickr id, through the cache.
fn photo(id: &str) -> Res<Resp> {
    let Some(url) = lock(&PHOTOS).get(id).cloned() else { return Ok(text(404, "unknown photo")) };
    let bytes = fetch(&url)?;
    let len = bytes.len();
    // Send Content-Length instead of tiny_http's default chunking above 32 KB; simpler for Cast receivers.
    // A photo id always shows the same photo, so browsers may keep it (swiping back needs no new download).
    let cache_control = tiny_http::Header::from_bytes("Cache-Control", "private, max-age=7200").unwrap();
    Ok(tiny_http::Response::new(200.into(), vec![header("image/jpeg"), cache_control], std::io::Cursor::new(bytes), Some(len), None)
        .with_chunked_threshold(usize::MAX)
        .boxed())
}

/// Photo bytes by Flickr URL, kept for `CACHE_TTL` and up to `CACHE_MAX_BYTES`.
/// Shared (Arc) so a cache hit doesn't copy the photo.
#[derive(Default)]
struct PhotoCache {
    photos: HashMap<String, (Instant, Arc<[u8]>)>,
    bytes: usize,
    /// URLs being downloaded; other requests for them wait instead of downloading the same photo again.
    loading: HashSet<String>,
}

impl PhotoCache {
    fn get(&self, url: &str) -> Option<Arc<[u8]>> {
        self.photos.get(url).filter(|(t, _)| t.elapsed() < CACHE_TTL).map(|(_, bytes)| bytes.clone())
    }

    /// Adds a photo, then drops expired photos and the oldest others until the total fits in `max_bytes`.
    fn insert(&mut self, url: &str, bytes: Arc<[u8]>, max_bytes: usize) {
        self.bytes += bytes.len();
        if let Some((_, old)) = self.photos.insert(url.to_owned(), (Instant::now(), bytes)) {
            self.bytes -= old.len();
        }
        let mut freed = 0;
        self.photos.retain(|_, (t, b)| {
            let keep = t.elapsed() < CACHE_TTL;
            if !keep {
                freed += b.len();
            }
            keep
        });
        self.bytes -= freed;
        while self.bytes > max_bytes {
            let Some(oldest) = self.photos.iter().filter(|(k, _)| *k != url).min_by_key(|(_, (t, _))| *t).map(|(k, _)| k.clone()) else {
                break;
            };
            if let Some((_, b)) = self.photos.remove(&oldest) {
                self.bytes -= b.len();
            }
        }
    }
}

static CACHE: LazyLock<(Mutex<PhotoCache>, Condvar)> = LazyLock::new(Default::default);

/// Clears a URL's loading mark when its download ends, even by error or panic, so waiting requests go on.
struct Loading<'a>(&'a str);

impl Drop for Loading<'_> {
    fn drop(&mut self) {
        lock(&CACHE.0).loading.remove(self.0);
        CACHE.1.notify_all();
    }
}

fn fetch(url: &str) -> Res<Arc<[u8]>> {
    let (cache, loaded) = &*CACHE;
    let mut c = lock(cache);
    loop {
        if let Some(bytes) = c.get(url) {
            return Ok(bytes);
        }
        if c.loading.insert(url.to_owned()) {
            break; // our turn to download
        }
        c = loaded.wait(c).unwrap_or_else(PoisonError::into_inner);
    }
    drop(c);
    let _loading = Loading(url);
    let bytes: Arc<[u8]> = HTTP.get(url).call()?.body_mut().with_config().limit(MAX_PHOTO_BYTES).read_to_vec()?.into();
    lock(cache).insert(url, bytes.clone(), CACHE_MAX_BYTES);
    Ok(bytes)
}

fn header(content_type: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes("Content-Type", content_type).unwrap()
}

fn json_resp(v: &Value) -> Resp {
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

/// Cast devices with a screen, by mDNS name, kept current by `watch_cast_devices`.
static DEVICES: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

/// Starts a long-running mDNS browse that keeps `DEVICES` current, so the cast menu needn't search each time.
fn watch_cast_devices() -> Res<()> {
    let mdns = mdns_sd::ServiceDaemon::new()?;
    let rx = mdns.browse("_googlecast._tcp.local.")?;
    std::thread::spawn(move || {
        let _daemon = mdns; // runs as long as this thread
        while let Ok(event) = rx.recv() {
            match event {
                mdns_sd::ServiceEvent::ServiceResolved(s) => {
                    let Some(ip) = s.get_addresses_v4().into_iter().next() else { continue };
                    if has_video(s.txt_properties.get_property_val_str("ca")) {
                        let name = s.txt_properties.get_property_val_str("fn").unwrap_or(&s.fullname);
                        lock(&DEVICES).insert(s.fullname.clone(), json!({"name": name, "addr": format!("{ip}:{}", s.port)}));
                    }
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                    lock(&DEVICES).remove(&fullname);
                }
                _ => {}
            }
        }
    });
    Ok(())
}

/// Known Cast devices by name; waits up to `wait` for the first ones right after startup.
fn cast_devices(wait: Duration) -> Vec<Value> {
    let end = Instant::now() + wait;
    loop {
        let mut list: Vec<Value> = lock(&DEVICES).values().cloned().collect();
        if !list.is_empty() || Instant::now() >= end {
            list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            return list;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Cast devices advertise capabilities in the `ca` TXT bitmask; bit 0 = video out.
/// Speakers and speaker groups lack it. A device that doesn't advertise `ca` is kept.
fn has_video(ca: Option<&str>) -> bool {
    ca.and_then(|c| c.parse::<u64>().ok()).is_none_or(|c| c & 1 != 0)
}

/// A command for the cast worker, posted by the web app.
enum CastCmd {
    Play { addr: SocketAddr, name: String, album: String, ids: Vec<String>, start: usize },
    Stop,
}

/// Posts `cmd` to the cast worker, replacing any command it hasn't picked up yet, and shows `status` right away.
fn post_cast_cmd(cmd: CastCmd, status: Value) {
    let mut slot = lock(&CAST_CMD.0);
    *slot = Some(cmd);
    *lock(&CAST_STATUS) = status;
    mark_cast_alive();
    CAST_CMD.1.notify_all();
}

/// Status update from worker `id`; dropped if a newer command is waiting (its status is already shown)
/// or the worker has been replaced. Lock order: `CAST_CMD`, then `CAST_STATUS`.
fn set_cast_status(id: u64, status: Value) {
    let pending = lock(&CAST_CMD.0);
    if pending.is_none() && CAST_WORKER.load(Ordering::SeqCst) == id {
        *lock(&CAST_STATUS) = status;
    }
}

fn mark_cast_alive() {
    *lock(&CAST_LAST_SEEN) = Instant::now();
}

/// Current cast status. A cast whose device has been silent too long is ended here: the worker is
/// stuck in `receive()` and can't report it, so it's left behind and a fresh worker takes over.
fn cast_status() -> Value {
    let mut status = lock(&CAST_STATUS);
    if status["active"] == true && lock(&CAST_LAST_SEEN).elapsed() > CAST_SILENCE_LIMIT {
        let device = status["device"].as_str().unwrap_or("the device").to_owned();
        *status = json!({"active": false, "text": format!("Lost connection to {device}")});
        spawn_cast_worker();
    }
    status.clone()
}

/// Starts a cast worker; an older one exits (without touching its device) the next time it wakes.
fn spawn_cast_worker() {
    let id = CAST_WORKER.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || cast_worker(id));
}

/// Waits for the next command; None once worker `id` has been replaced.
fn wait_cast_cmd(id: u64) -> Option<CastCmd> {
    let mut slot = lock(&CAST_CMD.0);
    loop {
        if CAST_WORKER.load(Ordering::SeqCst) != id {
            return None;
        }
        if let Some(cmd) = slot.take() {
            return Some(cmd);
        }
        // Timeout, so a replaced idle worker notices and exits.
        slot = CAST_CMD.1.wait_timeout(slot, Duration::from_secs(1)).unwrap_or_else(PoisonError::into_inner).0;
    }
}

/// A connection to a Cast device with the Default Media Receiver running on it.
struct CastSession {
    dev: CastDevice<'static>,
    app: Application,
    addr: SocketAddr,
    /// Photo server URL as the device sees it.
    base: String,
}

impl CastSession {
    fn connect(addr: SocketAddr, name: &str) -> Res<Self> {
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
        Ok(Self { dev, app, addr, base })
    }

    fn stop(self) {
        let _ = self.dev.receiver.stop_app(self.app.session_id.as_str());
    }
}

/// The only thread that talks to Cast devices. It takes commands from `CAST_CMD` and keeps the
/// connection and receiver app when only the album changes, so an album switch is just the next photo.
fn cast_worker(id: u64) {
    let mut session: Option<CastSession> = None;
    let mut next = None;
    loop {
        let Some(cmd) = next.take().or_else(|| wait_cast_cmd(id)) else { return };
        match cmd {
            CastCmd::Stop => {
                if let Some(old) = session.take() {
                    old.stop();
                }
            }
            CastCmd::Play { addr, name, album, ids, start } => {
                // A panic would otherwise kill the worker and leave the UI on "Starting…" forever.
                let played =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| play(&mut session, id, addr, &name, &album, &ids, start)))
                        .unwrap_or_else(|_| Err("internal error, see the console".into()));
                match played {
                    Ok(Some(cmd)) => next = Some(cmd),
                    Ok(None) => return, // replaced by a new worker
                    Err(e) => {
                        session = None;
                        set_cast_status(id, json!({"active": false, "text": format!("Cast error: {e}")}));
                    }
                }
            }
        }
    }
}

/// True if `msg` says the photo was stopped on the device (Back on the TV remote).
fn cancelled_on_device(msg: &ChannelMessage) -> bool {
    matches!(msg, ChannelMessage::Media(MediaResponse::Status(st)) if st.entries.iter().any(|e| e.idle_reason == Some(IdleReason::Cancelled)))
}

/// True if `msg` hints that our app has closed (Home on the TV remote, or another app took over). Only a hint:
/// `rust_cast` hands over messages it held back while launching, such as an older status from before our app started.
fn app_maybe_closed(msg: &ChannelMessage, session_id: &str) -> bool {
    match msg {
        ChannelMessage::Connection(ConnectionResponse::Close) => true,
        ChannelMessage::Receiver(ReceiverResponse::Status(st)) => !st.applications.iter().any(|a| a.session_id == session_id),
        _ => false,
    }
}

/// Shows `ids` on `addr` in a loop until a new command arrives, and returns it (None if this worker was replaced).
fn play(
    session: &mut Option<CastSession>,
    id: u64,
    addr: SocketAddr,
    name: &str,
    album: &str,
    ids: &[String],
    start: usize,
) -> Res<Option<CastCmd>> {
    if ids.is_empty() {
        return Err("the album has no photos".into());
    }
    if session.as_ref().is_none_or(|s| s.addr != addr) {
        if let Some(old) = session.take() {
            old.stop();
        }
        *session = Some(CastSession::connect(addr, name)?);
    }
    let s = session.as_ref().unwrap();
    for i in (0..ids.len()).cycle().skip(start) {
        let media = Media {
            content_id: format!("{}/img/{}", s.base, ids[i]),
            content_type: "image/jpeg".into(),
            stream_type: StreamType::None,
            metadata: None,
            duration: None,
        };
        s.dev.media.load(s.app.transport_id.as_str(), s.app.session_id.as_str(), &media)?;
        mark_cast_alive();
        // Warm the cache with the next photo, so the TV doesn't wait for Flickr on the next slide.
        if let Some(next) = lock(&PHOTOS).get(&ids[(i + 1) % ids.len()]).cloned() {
            std::thread::spawn(move || {
                let _ = fetch(&next);
            });
        }
        set_cast_status(
            id,
            json!({"active": true, "text": format!("Casting to {name} · {}/{}", i + 1, ids.len()), "device": name, "addr": addr.to_string(), "album": album}),
        );
        // Keep the connection alive while the photo is shown, and check for new commands.
        // shortcut: receive() blocks until the next message (pings arrive every ~5s), so slides, album switches
        // and Stop may lag a few seconds; fixing that needs a Cast client with a read timeout instead of rust_cast.
        let end = Instant::now() + Duration::from_secs(SLIDE_SECS.load(Ordering::SeqCst));
        while Instant::now() < end {
            let msg = s.dev.receive()?;
            mark_cast_alive();
            if matches!(msg, ChannelMessage::Heartbeat(HeartbeatResponse::Ping)) {
                s.dev.heartbeat.pong()?;
            } else if cancelled_on_device(&msg)
                || (app_maybe_closed(&msg, &s.app.session_id)
                    && !s.dev.receiver.get_status()?.applications.iter().any(|a| a.session_id == s.app.session_id))
            {
                // Back/Home on the TV remote: stop for real (not just pause until the next slide), so the TV can leave the app.
                println!("cast: stopped on {name}: {msg:?}");
                set_cast_status(id, json!({"active": false, "text": format!("Stopped on {name}")}));
                return Ok(Some(CastCmd::Stop));
            }
            if CAST_WORKER.load(Ordering::SeqCst) != id {
                return Ok(None);
            }
            if let Some(cmd) = lock(&CAST_CMD.0).take() {
                return Ok(Some(cmd));
            }
        }
    }
    unreachable!("cycle over a non-empty list never ends")
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
        let p = |a: &[&str]| parse_args(a.iter().map(|s| (*s).to_owned()));
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
        lock(&ALBUM_CACHE).insert("cached-user".into(), (Instant::now(), v.clone()));
        assert_eq!(albums("no-key", "cached-user").unwrap(), v); // no Flickr call needed
    }

    #[test]
    fn best_photo_size() {
        assert_eq!(best_url(&json!({"url_k": "k", "url_l": "l"})), Some("k"));
        assert_eq!(best_url(&json!({"url_l": "l", "url_z": "z"})), Some("l")); // owner blocks > 1024 px
        assert_eq!(best_url(&json!({"url_o": "o", "url_q": "q"})), None); // original and thumbnail aren't used
    }

    #[test]
    fn stop_from_tv_remote() {
        assert!(app_maybe_closed(&ChannelMessage::Connection(ConnectionResponse::Close), "s1"));
        assert!(!app_maybe_closed(&ChannelMessage::Heartbeat(HeartbeatResponse::Ping), "s1"));
        assert!(!cancelled_on_device(&ChannelMessage::Heartbeat(HeartbeatResponse::Ping)));
    }

    // One test for all cast status logic, since CAST_CMD/CAST_STATUS/CAST_WORKER are globals shared between tests.
    // No worker runs here: commands stay in the mailbox until the test takes them.
    #[test]
    fn cast_status_updates() {
        let id = CAST_WORKER.load(Ordering::SeqCst);
        let tv: SocketAddr = "192.168.1.222:8009".parse().unwrap();
        let play = || CastCmd::Play { addr: tv, name: "TV".into(), album: "a".into(), ids: vec!["1".into()], start: 0 };
        post_cast_cmd(play(), json!({"active": true, "text": "Starting on TV…", "device": "TV"}));
        // While a command waits, the worker's updates from the old cast are dropped.
        set_cast_status(id, json!({"active": true, "text": "Casting 6/30"}));
        assert_eq!(cast_status()["text"], "Starting on TV…");
        // Quick clicks: only the latest command is kept.
        post_cast_cmd(CastCmd::Stop, json!({"active": false, "text": "Stopped"}));
        assert!(matches!(lock(&CAST_CMD.0).take(), Some(CastCmd::Stop)));
        // Picked up: the worker's updates show again, but not those of a replaced worker.
        set_cast_status(id, json!({"active": true, "text": "Casting", "device": "TV"}));
        assert_eq!(cast_status()["text"], "Casting");
        set_cast_status(id + 1, json!({"active": true, "text": "Casting 7/30"}));
        assert_eq!(cast_status()["text"], "Casting");
        // A device that has gone silent is reported as lost, a new worker takes over, and the old one is ignored.
        *lock(&CAST_LAST_SEEN) = Instant::now().checked_sub(CAST_SILENCE_LIMIT + Duration::from_secs(1)).unwrap();
        assert_eq!(cast_status()["text"], "Lost connection to TV");
        assert_eq!(CAST_WORKER.load(Ordering::SeqCst), id + 1);
        set_cast_status(id, json!({"active": true, "text": "Casting 8/30"}));
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
        lock(&CACHE.0).insert(url, Arc::from([1u8, 2, 3]), CACHE_MAX_BYTES);
        assert_eq!(&*fetch(url).unwrap(), &[1, 2, 3]); // served without network
        let old = Instant::now().checked_sub(CACHE_TTL + Duration::from_secs(1)).unwrap();
        lock(&CACHE.0).photos.insert(url.into(), (old, Arc::from([1u8, 2, 3])));
        assert!(fetch(url).is_err()); // expired, so it refetches (and the fake URL fails)
        assert!(fetch(url).is_err()); // the failed download cleared its loading mark, so this doesn't hang
    }

    #[test]
    fn cache_size_limit() {
        let mut c = PhotoCache::default();
        c.insert("a", Arc::from([0u8; 6]), 10);
        c.insert("b", Arc::from([0u8; 6]), 10); // over the limit: the older photo goes, never the new one
        assert!(c.get("a").is_none() && c.get("b").is_some());
        assert_eq!(c.bytes, 6);
    }

    #[test]
    fn allowed_hosts() {
        for ok in ["localhost:8080", "192.168.1.61:8080", "[::1]:8080", "MyPC:8080", "mypc.local", "nas.home.arpa:8080"] {
            assert!(host_allowed(ok), "{ok}");
        }
        for bad in ["evil.com:8080", "192.168.1.61.nip.io:8080", "attacker.example"] {
            assert!(!host_allowed(bad), "{bad}");
        }
    }
}
