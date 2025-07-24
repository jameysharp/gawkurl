use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use base64::Engine;
use blake2::{Blake2s, Digest, digest};
use bytes::Bytes;
use reqwest::Client;
use tokio::sync::watch::{Receiver, Sender};

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
    contents: Bytes,
}

impl Pages {
    pub fn new(client: Client) -> Pages {
        Pages {
            watching: Default::default(),
            client,
        }
    }

    pub async fn wait_for_change(&self, uri: &str, seen: &str) -> anyhow::Result<Bytes> {
        let mut changes = self.lookup(uri);

        loop {
            {
                let current = changes.borrow_and_update();
                if current.hash != ContentHash::default() && current.hash != seen.as_bytes() {
                    return Ok(current.contents.clone());
                }
            }
            changes.changed().await?;
        }
    }

    fn lookup(&self, uri: &str) -> Receiver<Page> {
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
                .or_insert_with_key(|uri| self.clone().watch(uri))
                .subscribe()
        }
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
