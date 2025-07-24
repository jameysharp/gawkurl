use std::convert::Infallible;

use gawkurl_core::Pages;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Response, StatusCode, Uri};
use reqwest::Client;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    let client = Client::builder().referer(false).build()?;
    let pages = Pages::new(client);
    let server = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        eprintln!("connection from {peer_addr}");
        let stream = hyper_util::rt::TokioIo::new(Box::pin(stream));
        let pages = pages.clone();
        let conn = server.serve_connection_with_upgrades(
            stream,
            service_fn(move |req| {
                let pages = pages.clone();
                async move {
                    let Some((action, uri)) = route(req.uri()) else {
                        let mut response = Response::new("not found".into());
                        *response.status_mut() = StatusCode::NOT_FOUND;
                        return Ok::<_, Infallible>(response);
                    };

                    let response = match action {
                        "cached" => pages.wait_for_change(uri, "").await,
                        _ => pages.wait_for_change(uri, action).await,
                    };

                    match response {
                        Ok(contents) => Ok(Response::new(Full::new(contents))),
                        Err(e) => {
                            let mut response = Response::new(e.to_string().into());
                            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                            Ok(response)
                        }
                    }
                }
            }),
        );
        tokio::spawn(conn.into_owned());
    }
}

fn route(uri: &Uri) -> Option<(&str, &str)> {
    let (action, rest) = uri
        .path_and_query()?
        .as_str()
        .strip_prefix('/')?
        .split_once('/')?;

    // check that the provided URI is a valid absolute HTTP(S) URL
    let uri = Uri::try_from(rest).ok()?;
    let scheme = uri.scheme()?;
    if scheme != &http::uri::Scheme::HTTP && scheme != &http::uri::Scheme::HTTPS {
        return None;
    }
    uri.authority()?;
    uri.path_and_query()?;

    Some((action, rest))
}
