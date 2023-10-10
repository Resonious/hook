use std::collections::{hash_map, HashMap};
use std::convert::Infallible;
use std::net::{Ipv6Addr, SocketAddr};

use futures_util::future::ready;
use futures_util::stream::StreamExt;

use hyper::body::{Bytes, HttpBody};
use hyper::http::request::Parts;

use hyper::http::uri::Scheme;
use hyper::http::{response, HeaderValue};
use hyper::server::accept;
use hyper::server::conn::AddrIncoming;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, HeaderMap, Method, Request, Response, Server, StatusCode, Uri};

use lazy_static::lazy_static;
use tls_listener::TlsListener;
use tokio::sync::{mpsc, RwLock};

use askama::Template;

use tokio::task::JoinSet;
use uuid::Uuid;

mod tls;

struct Pipe {
    id: Uuid,
    path: String,
    senders: Vec<mpsc::Sender<PipeEntry>>,
    last_received: Option<PipeEntry>,
}

#[derive(Clone)]
struct PipeEntry {
    uri: Uri,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
}

struct Pipes {
    by_path: HashMap<String, Pipe>,
    counter: usize,
}

#[derive(Template)]
#[template(path = "view.html")]
struct BrowserView {}

lazy_static! {
    static ref PIPES: RwLock<Pipes> = RwLock::new(Pipes {
        by_path: HashMap::new(),
        counter: 0,
    });
    pub static ref HOOK_URL: String =
        std::env::var("HOOK_URL").unwrap_or_else(|_| "http://localhost:3005".to_string());
}

static FAVICON_BYTES: &[u8; 53870] = include_bytes!("favicon.ico");

/// This is the actual "app". It's the bit that lets you curl and stream in
/// POSTS from other sources. Also serves some basic HTML for browsers.
async fn serve_app(request: Request<Body>) -> Result<Response<Body>, Infallible> {
    let (parts, req_body) = request.into_parts();

    let id = Uuid::new_v4();

    let getter_type =
        if parts.headers.get("accept") == Some(&HeaderValue::from_static("text/event-stream")) {
            GetterType::EventStream
        } else if parts.headers.get("user-agent").map(|x| x.to_str().map(|s| s.starts_with("curl")).unwrap_or(false)).unwrap_or(false) {
            GetterType::Curl
        } else {
            GetterType::Other
        };

    println!("{id} {} {} {:?}", parts.method, parts.uri.path(), getter_type);

    let is_listener = getter_type != GetterType::Other;

    match parts.method {
        Method::POST | Method::PUT | Method::PATCH => Ok(serve_post(id, parts, req_body).await),
        Method::OPTIONS => Ok(serve_options(parts).await),

        _ => {
            if parts.uri.path() == "/favicon.ico" {
                Ok(serve_favicon().await)
            } else if is_listener {
                Ok(serve_get(id, parts, getter_type).await)
            } else {
                Ok(serve_browser().await)
            }
        }
    }
}

/// When HTTPS is available, this will service all requests to port 80, redirecting
/// everything to HTTPS.
async fn serve_non_https(request: Request<Body>) -> Result<Response<Body>, Infallible> {
    let app_uri: Uri = HOOK_URL.parse().unwrap();
    let request_uri = request.uri().clone();

    let redirect_uri = Uri::builder()
        .scheme(Scheme::HTTPS)
        .authority(app_uri.authority().unwrap().as_str())
        .path_and_query(
            request_uri
                .path_and_query()
                .map(|x| x.as_str())
                .unwrap_or("/"),
        )
        .build()
        .map(|uri| uri.to_string())
        .unwrap_or_else(|e| {
            eprintln!("ERROR! Failed to generate redirect URL {e:?}");
            "https://snd.one/error".to_string()
        });

    match Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header("location", redirect_uri)
        .body("Hint: add -L to your curl command\n".into())
    {
        Ok(r) => Ok(r),
        Err(e) => {
            eprintln!("Failed to serve redirect...? {e:?}");
            Ok(Response::new(Body::empty()))
        }
    }
}

/// For now this just serves a simple HTML document.
async fn serve_browser() -> Response<Body> {
    println!("Serving HTML landing page");

    let template = BrowserView {};
    let html = template
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let (mut body_sender, body) = Body::channel();

    let mut response = Response::new(body);

    if let Err(e) = body_sender.send_data(html.into()).await {
        eprintln!("Failed to send HTML: {:?}", e);
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    }

    response
}

/// This handles POST requests, so is basically the "webhook handler".
/// In response, we dump all request info to everyone who is listening via curl and GET.
async fn serve_post(id: Uuid, parts: Parts, mut body: Body) -> Response<Body> {
    let mut senders_to_delete = Vec::<bool>::with_capacity(128);
    let mut need_to_delete = false;
    let mut valid_senders: usize = 0;
    let pipe_counter;
    {
        let pipes = PIPES.read().await;
        let Some(pipe) = pipes.by_path.get(parts.uri.path()) else {
            return empty_response(&parts);
        };
        pipe_counter = pipes.counter;

        let mut data_chunks = Vec::<Bytes>::new();
        while let Some(data) = body.data().await {
            match data {
                Ok(incoming_bytes) => {
                    data_chunks.push(incoming_bytes);
                }
                Err(e) => {
                    println!("{id} failed to receive body ({e:?})");
                    return empty_response(&parts);
                }
            }
        }
        let total_bytes = data_chunks.concat();

        let entry = PipeEntry {
            uri: parts.uri.clone(),
            method: parts.method.clone(),
            headers: parts.headers.clone(),
            body: Bytes::copy_from_slice(&total_bytes),
        };

        for sender in pipe.senders.iter() {
            if let Err(e) = sender.send(entry.clone()).await {
                println!("{id} deleting listener {} ({e:?})", pipe.id);
                senders_to_delete.push(true);
                need_to_delete = true;
            } else {
                senders_to_delete.push(false);
                valid_senders += 1;
            }
        }
    }

    if need_to_delete {
        let mut pipes = PIPES.write().await;

        if pipes.counter != pipe_counter {
            return empty_response(&parts);
        }
        pipes.counter += 1;

        if valid_senders > 0 {
            let Some(pipe) = pipes.by_path.get_mut(parts.uri.path()) else {
                return empty_response(&parts);
            };
            let mut new_senders: Vec<mpsc::Sender<PipeEntry>> =
                Vec::with_capacity(senders_to_delete.len());

            for (i, delete) in senders_to_delete.iter().enumerate() {
                if !delete {
                    new_senders.push(pipe.senders[i].clone());
                }
            }

            pipe.senders = new_senders;
        } else {
            pipes.by_path.remove(parts.uri.path());
        }
    }

    let mut response = insert_origin(Response::builder(), &parts);
    response = response.header("snd-received", valid_senders);

    response.body(Body::empty()).unwrap_or_else(|e| {
        eprintln!("Built invalid response!! {:?}", e);
        Response::new(Body::empty())
    })
}

async fn serve_options(parts: Parts) -> Response<Body> {
    empty_response(&parts)
}

fn insert_origin(response: response::Builder, parts: &Parts) -> response::Builder {
    if let Some(origin) = parts.headers.get("origin") {
        return response
            .header("access-control-allow-origin", origin)
            .header("access-control-allow-headers", "snd-received, content-type")
            .header("vary", "Origin");
    } else {
        return response;
    }
}

fn empty_response(parts: &Parts) -> Response<Body> {
    let mut response = Response::builder();
    response = insert_origin(response, &parts);
    response.body(Body::empty()).unwrap_or_else(|e| {
        eprintln!("Built invalid response!! {:?}", e);
        Response::new(Body::empty())
    })
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum GetterType {
    Curl,
    EventStream,
    Other,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum ContentType {
    Json,
    Other,
}

/// Serves only curl GETs. Keeps connection open forever so that you can receive
/// POSTs in real time and see their headers and body.
async fn serve_get(id: Uuid, parts: Parts, getter_type: GetterType) -> Response<Body> {
    let uri = parts.uri.clone();
    let path = uri.path().to_string();

    let (mut body_sender, body) = Body::channel();

    {
        let host: &str = &HOOK_URL;
        let path = uri.path();
        let vec = format!("Your URL is {host}{path}\ncurl -d \"Hello World!\" {host}{path}\n\n")
            .into_bytes();
        let _ = body_sender.send_data(vec.into()).await;
    }

    let (pipe_sender, mut pipe_receiver) = mpsc::channel::<PipeEntry>(128);
    {
        let mut pipes = PIPES.write().await;
        let pipe = match pipes.by_path.entry(path.clone()) {
            hash_map::Entry::Occupied(o) => o.into_mut(),
            hash_map::Entry::Vacant(v) => v.insert(Pipe {
                id,
                path: path.clone(),
                senders: Vec::new(),
                last_received: None,
            }),
        };
        pipe.senders.push(pipe_sender);
    }

    tokio::spawn(async move {
        macro_rules! send {
            ($e:expr) => {{
                if let Err(e) = body_sender.send_data(Bytes::from($e)).await {
                    println!("{id} failed to send to GET listener {:?}", e);
                    break;
                }
            }};
        }

        while let Some(entry) = pipe_receiver.recv().await {
            send!(format!("{} {}\n", entry.method, entry.uri));

            let mut content_type = ContentType::Other;

            for (key, value) in entry.headers.iter() {
                if key == "content-type" && value.as_bytes().starts_with(b"application/json") {
                    content_type = ContentType::Json;
                }
                if getter_type == GetterType::Curl {
                    match value.to_str() {
                        Ok(s) => send!(format!("{key}: {s}\n")),
                        Err(_) => send!(format!("{key}: {value:?}\n")),
                    }
                }
            }
            send!(Bytes::copy_from_slice(b"\n"));

            match (getter_type, content_type) {
                (GetterType::Curl, ContentType::Json) => {
                    match serde_json::from_slice::<serde_json::Value>(&entry.body) {
                        Ok(value) => {
                            match colored_json::to_colored_json(&value, colored_json::ColorMode::On)
                            {
                                Ok(colorized) => {
                                    send!(colorized);
                                }
                                Err(e) => {
                                    eprintln!("Failed to colorize JSON {e:?}");
                                    match serde_json::to_string_pretty(&value) {
                                        Ok(pretty) => send!(pretty),
                                        Err(e2) => {
                                            eprintln!("Also failed to prettify JSON (!?) {e2:?}");
                                            send!(value.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Invalid JSON {e:?}");
                            send!(entry.body);
                        }
                    }
                }

                (GetterType::EventStream, _) => match std::str::from_utf8(entry.body.as_ref()) {
                    Ok(utf8) => {
                        send!(format!("data: {utf8}"));
                    }
                    Err(e) => {
                        eprintln!("Got invalid utf8 {e:?}");
                        send!(format!("data: {{\"error\": \"invalid utf8\"}}"));
                    }
                },

                _ => send!(entry.body),
            }
            send!(Bytes::copy_from_slice(b"\n\n"));
        }
    });

    let mut response = Response::builder();
    response = insert_origin(response, &parts);

    if getter_type == GetterType::EventStream {
        response = response.header("content-type", "text/event-stream");
        response = response.header("content-type", "cache-control: no-store");
    }

    response.body(body).unwrap_or_else(|e| {
        eprintln!("Built invalid response!! {:?}", e);
        Response::new(Body::empty())
    })
}

/// Inlined favicon.ico
async fn serve_favicon() -> Response<Body> {
    let (mut body_sender, body) = Body::channel();
    let mut response = Response::new(body);

    if let Err(e) = body_sender
        .send_data(Bytes::from_static(FAVICON_BYTES))
        .await
    {
        eprintln!("Failed to send favicon: {:?}", e);
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    }

    response
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app_url: Uri = HOOK_URL
        .parse()
        .expect("HOOK_URL environment variable malformed");
    if app_url.authority().is_none() {
        panic!("HOOK_URL environment variable has bad authority section");
    }

    let tls_acceptor = if let (Some("https"), Some(host)) = (app_url.scheme_str(), app_url.host()) {
        let tls_config_path = format!("/etc/letsencrypt/renewal/{host}.conf");
        let acceptor = tls::acceptor(&tls_config_path);

        match acceptor {
            Ok(x) => Some(x),
            Err(e) => {
                eprintln!("Failed to read TLS config: {:?}", e);
                None
            }
        }
    } else {
        None
    };

    if let Some(tls) = tls_acceptor {
        let app_service =
            make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(serve_app)) });
        let control_service =
            make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(serve_non_https)) });

        println!("Starting in https mode");

        let http_ipv6_addr: SocketAddr = (Ipv6Addr::UNSPECIFIED, 80).into();
        let https_ipv6_addr: SocketAddr = (Ipv6Addr::UNSPECIFIED, 443).into();

        let http_server = Server::bind(&http_ipv6_addr).serve(control_service);

        let incoming =
            TlsListener::new(tls, AddrIncoming::bind(&https_ipv6_addr)?).filter(|conn| {
                if let Err(e) = conn {
                    eprintln!("Error with incoming TLS connection: {e:?}");
                    ready(false)
                } else {
                    ready(true)
                }
            });
        let https_server = Server::builder(accept::from_stream(incoming)).serve(app_service);

        println!("Listening on {} (port 80 and 443)", app_url);

        let mut servers = JoinSet::new();
        servers.spawn(http_server);
        servers.spawn(https_server);

        while let Some(_exited) = servers.join_next().await {
            println!("Server closed");
        }
    } else {
        let app_service =
            make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(serve_app)) });

        println!("Starting in http-only (dev) mode");
        let addr4 = ([0, 0, 0, 0], 3005).into();
        let server4 = Server::bind(&addr4).serve(app_service);

        println!("Listening on {}", addr4);

        server4.await?;
    }

    Ok(())
}
