//! Flickr → Chromecast bridge. A web app (UI_PORT) to browse albums, preview them
//! and cast them; a media server (MEDIA_PORT) the Chromecast fetches photos from.

use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{Media, StreamType};
use rust_cast::channels::receiver::CastDeviceApp;
use rust_cast::{CastDevice, ChannelMessage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const API: &str = "https://api.flickr.com/services/rest/";
const UI_PORT: u16 = 8080;
const MEDIA_PORT: u16 = 8765;
const SLIDE_SECS: u64 = 8;
// shortcut: in-memory cache (~100-200 MB for a big album), lost on restart; move to disk if that matters.
const CACHE_TTL: Duration = Duration::from_secs(2 * 60 * 60);

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Resp = tiny_http::Response<std::io::Cursor<Vec<u8>>>;

/// Flickr photo id → large photo URL, filled as albums are opened. Only these are served.
static PHOTOS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
/// Album id → photo ids in order, for casting a whole album.
static ALBUMS: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(Default::default);
/// Bumped to start or stop a cast; a running cast stops when it no longer matches.
static CAST_GEN: AtomicU64 = AtomicU64::new(0);
static CAST_STATUS: LazyLock<Mutex<Value>> = LazyLock::new(|| Mutex::new(json!({"active": false, "text": ""})));

fn main() -> Res<()> {
    let key = parse_key(&std::fs::read_to_string("secrets/flickrkey.txt")?)
        .ok_or("no API key found in secrets/flickrkey.txt")?;

    serve(MEDIA_PORT, Arc::new(|req| match req.url().strip_prefix("/img/") {
        Some(id) => photo(id),
        None => Ok(text(404, "not found")),
    }))?;
    serve(UI_PORT, Arc::new(move |req| ui_route(&key, req)))?;

    let mut url = format!("http://localhost:{UI_PORT}/");
    if let Some(user) = std::env::args().nth(1) {
        url += &format!("?user={}", url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>());
    }
    println!("NymFlickrCast running at {url} (Ctrl+C to quit)");
    if let Ok(ip) = local_ip() {
        println!("On a phone/iPad on the same Wi-Fi: http://{ip}:{UI_PORT}/");
    }
    open_browser(&url);
    loop {
        std::thread::park();
    }
}

/// Run an HTTP server on its own thread, one thread per request.
fn serve(port: u16, handler: Arc<dyn Fn(&tiny_http::Request) -> Res<Resp> + Send + Sync>) -> Res<()> {
    let server = tiny_http::Server::http(("0.0.0.0", port))?;
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let resp = handler(&req).unwrap_or_else(|e| {
                    tiny_http::Response::from_string(json!({"error": e.to_string()}).to_string())
                        .with_status_code(500)
                        .with_header(header("application/json"))
                });
                let _ = req.respond(resp);
            });
        }
    });
    Ok(())
}

fn ui_route(key: &str, req: &tiny_http::Request) -> Res<Resp> {
    let (path, query) = req.url().split_once('?').unwrap_or((req.url(), ""));
    let q: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let arg = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let post = *req.method() == tiny_http::Method::Post;
    Ok(match path {
        "/" => tiny_http::Response::from_string(include_str!("ui.html")).with_header(header("text/html; charset=utf-8")),
        _ if path.starts_with("/img/") => photo(&path[5..])?,
        "/api/albums" => json_resp(albums(key, arg("user"))?),
        "/api/photos" => json_resp(album_photos(key, arg("user"), arg("album"))?),
        "/api/devices" => json_resp(json!(discover_cast_devices(Duration::from_secs(3)))),
        "/api/status" => json_resp(CAST_STATUS.lock().unwrap().clone()),
        "/api/cast" if post => {
            let ids = ALBUMS.lock().unwrap().get(arg("album")).cloned().ok_or("open the album first")?;
            let addr: std::net::SocketAddr = arg("device").parse()?;
            start_cast(addr, arg("name").to_string(), ids, arg("start").parse().unwrap_or(0));
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

/// The key file may hold labels/secret too; the API key is the 32-hex-char token.
fn parse_key(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|t| t.len() == 32)
        .map(str::to_string)
}

fn flickr(key: &str, method: &str, params: &[(&str, &str)]) -> Res<Value> {
    let mut req = ureq::get(API)
        .query("method", method)
        .query("api_key", key)
        .query("format", "json")
        .query("nojsoncallback", "1");
    for (k, v) in params {
        req = req.query(k, v);
    }
    let v: Value = req.call()?.into_json()?;
    if v["stat"] != "ok" {
        return Err(format!("{method}: {}", v["message"].as_str().unwrap_or("error")).into());
    }
    Ok(v)
}

/// Flickr returns numbers as either strings or ints depending on the field.
fn num(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str()?.parse().ok()).unwrap_or(0)
}

fn albums(key: &str, user: &str) -> Res<Value> {
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
            ("photoset_id", album), ("user_id", nsid), ("extras", "url_q,url_l,url_c,url_z"),
            ("per_page", "500"), ("page", &p), ("media", "photos"),
        ])?;
        for ph in r["photoset"]["photo"].as_array().into_iter().flatten() {
            // largest size Flickr offers without OAuth, falling back for small originals
            if let (Some(id), Some(u)) = (ph["id"].as_str(), ["url_l", "url_c", "url_z"].iter().find_map(|k| ph[*k].as_str())) {
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

/// Serve a known photo by Flickr id, through the cache.
fn photo(id: &str) -> Res<Resp> {
    let Some(url) = PHOTOS.lock().unwrap().get(id).cloned() else { return Ok(text(404, "unknown photo")) };
    Ok(tiny_http::Response::from_data(fetch(&url)?).with_header(header("image/jpeg")))
}

/// Photo bytes by Flickr URL, kept for CACHE_TTL.
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Vec<u8>)>>> = LazyLock::new(Default::default);

fn fetch(url: &str) -> Res<Vec<u8>> {
    if let Some((t, bytes)) = CACHE.lock().unwrap().get(url) {
        if t.elapsed() < CACHE_TTL {
            return Ok(bytes.clone());
        }
    }
    let mut buf = Vec::new();
    ureq::get(url).call()?.into_reader().read_to_end(&mut buf)?;
    let mut cache = CACHE.lock().unwrap();
    cache.retain(|_, (t, _)| t.elapsed() < CACHE_TTL);
    cache.insert(url.to_string(), (Instant::now(), buf.clone()));
    Ok(buf)
}

fn header(content_type: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes("Content-Type", content_type).unwrap()
}

fn json_resp(v: Value) -> Resp {
    tiny_http::Response::from_string(v.to_string()).with_header(header("application/json"))
}

fn text(code: u16, s: &str) -> Resp {
    tiny_http::Response::from_string(s).with_status_code(code)
}

/// The LAN address the Chromecast can reach us on (no packet is actually sent).
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

/// Start a slideshow on a Cast device in the background, replacing any running one.
fn start_cast(addr: std::net::SocketAddr, name: String, ids: Vec<String>, start: usize) {
    let generation = CAST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    *CAST_STATUS.lock().unwrap() = json!({"active": true, "text": format!("Connecting to {name}…")});
    std::thread::spawn(move || {
        let result = cast_slideshow(addr, &name, &ids, start, generation);
        if CAST_GEN.load(Ordering::SeqCst) == generation {
            let text = match result {
                Ok(()) => "Stopped".to_string(),
                Err(e) => format!("Cast error: {e}"),
            };
            *CAST_STATUS.lock().unwrap() = json!({"active": false, "text": text});
        }
    });
}

fn cast_slideshow(addr: std::net::SocketAddr, name: &str, ids: &[String], start: usize, generation: u64) -> Res<()> {
    let base = format!("http://{}:{MEDIA_PORT}", local_ip()?);
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
        *CAST_STATUS.lock().unwrap() = json!({"active": true, "text": format!("Casting to {name} · {}/{}", i + 1, ids.len()), "device": name});
        // Keep the connection alive while the photo is shown.
        // shortcut: receive() blocks until the next message (pings arrive every ~5s), so slides and Stop may lag a few seconds.
        let end = Instant::now() + Duration::from_secs(SLIDE_SECS);
        while Instant::now() < end {
            if CAST_GEN.load(Ordering::SeqCst) != generation {
                let _ = dev.receiver.stop_app(app.session_id.as_str());
                return Ok(());
            }
            if let ChannelMessage::Heartbeat(HeartbeatResponse::Ping) = dev.receive()? {
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
    fn cache_hit_and_expiry() {
        let url = "http://invalid.test/x.jpg";
        CACHE.lock().unwrap().insert(url.into(), (Instant::now(), vec![1, 2, 3]));
        assert_eq!(fetch(url).unwrap(), vec![1, 2, 3]); // served without network
        let old = Instant::now().checked_sub(CACHE_TTL + Duration::from_secs(1)).unwrap();
        CACHE.lock().unwrap().insert(url.into(), (old, vec![1, 2, 3]));
        assert!(fetch(url).is_err()); // expired, so it refetches (and the fake URL fails)
    }
}
