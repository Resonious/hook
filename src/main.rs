use std::convert::Infallible;
use std::time::Duration;

use hyper::body::Bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};

use uuid::Uuid;

async fn serve(request: Request<Body>) -> Result<Response<Body>, Infallible> {
    let (parts, _req_body) = request.into_parts();
    let id = Uuid::new_v4();

    println!("{id} {} {}", parts.method, parts.uri.path());

    let (mut sender, body) = Body::channel();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            if let Err(e) = sender.send_data(Bytes::from_static(b"hello\n")).await {
                println!("{id} Cut off {:?}", e);
                break;
            }
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