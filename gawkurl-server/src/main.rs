use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use gawkurl_core::{Bytes, HttpDate, Page, watch_url};
use http::{HeaderValue, header};
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::sync::watch::error::RecvError;
use tokio::sync::watch::{Receiver, Sender};

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
            service_fn(move |req| service(req, pages.clone())),
        );
        tokio::spawn(conn.into_owned());
    }
}

async fn service<B>(req: Request<B>, pages: Pages) -> Result<Response<Full<Bytes>>, Infallible> {
    let Some((action, uri)) = route(req.uri()) else {
        let mut response = Response::new("not found".into());
        *response.status_mut() = StatusCode::NOT_FOUND;
        return Ok(response);
    };

    let result = match action {
        "cached" => pages.wait_for_change(uri, "").await,
        _ => pages.wait_for_change(uri, action).await,
    };

    match result {
        Ok(page) => {
            let mut response = Response::new(Full::new(page.contents));
            let headers = response.headers_mut();
            headers.insert(
                header::HeaderName::from_static("next-version"),
                header::HeaderValue::from_str(&format!(
                    "/{}/{}",
                    str::from_utf8(&page.hash).unwrap(),
                    uri
                ))
                .unwrap(),
            );
            headers.insert(
                header::HeaderName::from_static("last-changed"),
                header::HeaderValue::from_str(&page.last_changed.elapsed().as_secs().to_string())
                    .unwrap(),
            );
            Ok(response)
        }
        Err(e) => {
            let mut response = Response::new(e.to_string().into());
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            Ok(response)
        }
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

#[derive(Clone)]
pub struct Pages {
    watching: Arc<RwLock<HashMap<String, Sender<Option<Page>>>>>,
    client: Client,
}

impl Pages {
    pub fn new(client: Client) -> Pages {
        Pages {
            watching: Default::default(),
            client,
        }
    }

    pub async fn wait_for_change(&self, uri: &str, seen: &str) -> Result<Page, RecvError> {
        let mut changes = self.lookup(uri);

        loop {
            if let Some(current) = &*changes.borrow_and_update() {
                if current.hash != seen.as_bytes() {
                    return Ok(current.clone());
                }
            }
            changes.changed().await?;
        }
    }

    fn lookup(&self, uri: &str) -> Receiver<Option<Page>> {
        // optimistically assume that this uri is already in the map, and look
        // it up with only a read lock held, to avoid blocking other lookups
        // happening in parallel
        if let Some(page) = self.watching.read().unwrap().get(uri) {
            page.subscribe()
        } else {
            // if it wasn't there, try again with a write lock, which blocks all
            // other lookups until it's done so we can add the entry to the map.
            // however we may have raced with another writer so we might find we
            // actually do find it this time
            self.watching
                .write()
                .unwrap()
                .entry(uri.to_owned())
                .or_insert_with_key(|uri| {
                    let sender = Sender::new(None);
                    tokio::spawn(watch_url(ReqwestClient {
                        client: self.client.clone(),
                        uri: uri.clone(),
                        sender: sender.clone(),
                    }));
                    sender
                })
                .subscribe()
        }
    }
}

pub struct ReqwestClient {
    client: Client,
    uri: String,
    sender: Sender<Option<Page>>,
}

impl gawkurl_core::Client for ReqwestClient {
    type Error = reqwest::Error;
    type Body = reqwest::Body;

    fn fetch(
        &mut self,
        etag: Option<&HeaderValue>,
        last_modified: Option<&HttpDate>,
    ) -> impl Future<Output = Result<http::Response<reqwest::Body>, reqwest::Error>> {
        let mut request = self.client.get(&self.uri);
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header(header::IF_MODIFIED_SINCE, last_modified.to_string());
        }
        async move {
            let response = request.send().await;
            response.map(Into::into)
        }
    }

    fn changed(&mut self, page: Page) {
        self.sender.send_replace(Some(page));
    }
}
