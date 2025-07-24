use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use blake2::{Blake2s, Digest, digest};
use http_body_util::Full;
use hyper::service::{HttpService, service_fn};
use hyper::{Response, StatusCode, Uri, body};
use reqwest::Client;
use tokio::sync::watch::Sender;

#[derive(Clone)]
pub struct Pages {
    watching: Arc<RwLock<HashMap<String, Sender<Page>>>>,
    client: Client,
}

type ContentHasher = Blake2s<digest::consts::U6>;

type ContentHash = [u8; {
    let bytes_len: usize = <<ContentHasher as digest::OutputSizeUser>::OutputSize as digest::typenum::ToInt<usize>>::INT;
    assert!(bytes_len % 3 == 0);
    bytes_len * 4 / 3
}];

#[derive(Default)]
struct Page {
    hash: ContentHash,
    contents: body::Bytes,
}

impl Pages {
    pub fn new(client: Client) -> Pages {
        Pages {
            watching: Default::default(),
            client,
        }
    }

    pub fn service(&self) -> impl HttpService<body::Incoming, Error = Infallible> {
        service_fn(move |req| async move {
            let Some((action, uri)) = Pages::route(req.uri()) else {
                let mut response = Response::new("not found".into());
                *response.status_mut() = StatusCode::NOT_FOUND;
                return Ok(response);
            };

            let response = match action {
                "cached" => self.wait_for_change(uri, "").await,
                _ => self.wait_for_change(uri, action).await,
            };

            response.or_else(|e| {
                let mut response = Response::new(e.to_string().into());
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                Ok(response)
            })
        })
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

    async fn wait_for_change(&self, uri: &str, seen: &str) -> Result<Response<Full<body::Bytes>>> {
        // optimistically assume that this uri is already in the map, and look
        // it up with only a read lock held, to avoid blocking other lookups
        // happening in parallel
        let mut changes = if let Some(page) = self.watching.read().unwrap().get(uri) {
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
                .or_insert_with_key(|uri| self.clone().watch(uri))
                .subscribe()
        };

        let contents = loop {
            let current = changes.borrow_and_update();
            if current.hash != ContentHash::default() && current.hash != seen.as_bytes() {
                break current.contents.clone();
            }
            drop(current);
            changes.changed().await?;
        };

        Ok(Response::new(Full::new(contents)))
    }

    fn watch(self, uri: &String) -> Sender<Page> {
        let uri = uri.clone();
        tokio::spawn(async move {
            loop {
                // fetch and hash the page
                // TODO: ratelimit requests per domain
                let response = self.client.get(&uri).send().await.unwrap();
                // TODO: check response status and cache headers
                let contents = response.bytes().await.unwrap();

                let hash_bytes = ContentHasher::digest(&contents);
                let mut hash = ContentHash::default();
                let hash_len = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode_slice(&hash_bytes, &mut hash);
                debug_assert_eq!(hash_len, Ok(hash.len()));

                // get the sender for this uri from self under a read lock and
                // conditionally update the sender if the hash has changed
                self.watching.read().unwrap()[&uri].send_if_modified(|page| {
                    let modified = hash != page.hash;
                    if modified {
                        page.hash = hash;
                        page.contents = contents;
                    }
                    modified
                });

                // sleep, then loop
                // TODO: choose delay by heuristics
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        Sender::new(Page::default())
    }
}
