use std::collections::{hash_map, HashMap};
use std::convert::Infallible;
use std::net::{Ipv6Addr, SocketAddr};

use futures_util::future::ready;
use futures_util::stream::StreamExt;

use hyper::body::{Bytes, HttpBody};
use hyper::http::request::Parts;

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
    println!("{id} {} {}", parts.method, parts.uri.path());

    let user_agent = parts.headers.get("user-agent");
    let is_curl = match user_agent {
        Some(value) => value
            .to_str()
            .map(|s| s.starts_with("curl"))
            .unwrap_or(false),
        None => false,
    };

    if parts.method == Method::GET {
        if parts.uri.path() == "/favicon.ico" {
            Ok(serve_favicon().await)
        } else if is_curl {
            Ok(serve_get(id, parts.uri.clone()).await)
        } else {
            Ok(serve_browser().await)
        }
    } else {
        Ok(serve_post(id, parts, req_body).await)
    }
}

/// When HTTPS is available, this will service all requests to port 80, redirecting
/// everything to HTTPS.
async fn serve_non_https(_request: Request<Body>) -> Result<Response<Body>, Infallible> {
    // TODO: actual logic. redirect to https, maybe serve static file if needed later...
    Ok(serve_browser().await)
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
    let mut valid_senders_present = false;
    let pipe_counter;
    {
        let pipes = PIPES.read().await;
        let Some(pipe) = pipes.by_path.get(parts.uri.path()) else {
            return Response::new(Body::empty());
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
                    return Response::new(Body::empty());
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
                valid_senders_present = true;
            }
        }
    }

    if need_to_delete {
        let mut pipes = PIPES.write().await;

        if pipes.counter != pipe_counter {
            return Response::new(Body::empty());
        }
        pipes.counter += 1;

        if valid_senders_present {
            let Some(pipe) = pipes.by_path.get_mut(parts.uri.path()) else {
                return Response::new(Body::empty());
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

    Response::new(Body::empty())
}

/// Serves only curl GETs. Keeps connection open forever so that you can receive
/// POSTs in real time and see their headers and body.
async fn serve_get(id: Uuid, uri: Uri) -> Response<Body> {
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
                let vec = $e;
                if let Err(e) = body_sender.send_data(Bytes::copy_from_slice(&vec)).await {
                    println!("{id} failed to send to listener {:?}", e);
                    break;
                }
            }};
        }

        while let Some(entry) = pipe_receiver.recv().await {
            send!(format!("{} {}\n", entry.method, entry.uri).into_bytes());
            for (key, value) in entry.headers.iter() {
                send!(format!("{key}: {value:?}\n").into_bytes());
            }
            send!(*b"\n");

            if let Err(e) = body_sender.send_data(entry.body).await {
                println!("{id} failed to send to listener {:?}", e);
                break;
            }
            send!(*b"\n\n");
        }
    });

    Response::new(body)
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
