use std::collections::{hash_map, HashMap};
use std::convert::Infallible;

use hyper::body::{Bytes, HttpBody};
use hyper::http::request::Parts;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, HeaderMap, Method, Request, Response, Server, Uri};

use lazy_static::lazy_static;
use tokio::sync::{mpsc, RwLock};

use uuid::Uuid;

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

lazy_static! {
    static ref PIPES: RwLock<Pipes> = RwLock::new(Pipes {
        by_path: HashMap::new(),
        counter: 0,
    });
}

async fn serve(request: Request<Body>) -> Result<Response<Body>, Infallible> {
    let (parts, req_body) = request.into_parts();

    let id = Uuid::new_v4();
    println!("{id} {} {}", parts.method, parts.uri.path());

    if parts.method == Method::GET {
        serve_get(id, parts.uri.clone()).await
    } else {
        serve_post(id, parts, req_body).await
    }
}

async fn serve_post(id: Uuid, parts: Parts, mut body: Body) -> Result<Response<Body>, Infallible> {
    let mut senders_to_delete = Vec::<bool>::with_capacity(128);
    let mut need_to_delete = false;
    let mut valid_senders_present = false;
    let pipe_counter;
    {
        let pipes = PIPES.read().await;
        let Some(pipe) = pipes.by_path.get(parts.uri.path()) else {
            return Ok(Response::new(Body::empty()));
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
                    return Ok(Response::new(Body::empty()));
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
            return Ok(Response::new(Body::empty()));
        }
        pipes.counter += 1;

        if valid_senders_present {
            let Some(pipe) = pipes.by_path.get_mut(parts.uri.path()) else {
                return Ok(Response::new(Body::empty()));
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

    Ok(Response::new(Body::empty()))
}

async fn serve_get(id: Uuid, uri: Uri) -> Result<Response<Body>, Infallible> {
    let path = uri.path().to_string();

    let (mut body_sender, body) = Body::channel();

    {
        let vec = format!("Your URL is {uri}\ncurl -d \"Hello World!\" {uri}\n\n").into_bytes();
        let _ = body_sender.send_data(Bytes::copy_from_slice(&vec)).await;
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

    Ok(Response::new(body))
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // For every connection, we must make a `Service` to handle all
    // incoming HTTP requests on said connection.
    let make_svc = make_service_fn(|_conn| {
        // This is the `Service` that will handle the connection.
        // `service_fn` is a helper to convert a function that
        // returns a Response into a `Service`.
        async { Ok::<_, Infallible>(service_fn(serve)) }
    });

    let addr = ([127, 0, 0, 1], 3005).into();

    let server = Server::bind(&addr).serve(make_svc);

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}
