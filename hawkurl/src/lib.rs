use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use base64::Engine;
use blake2::{Blake2s, Digest, digest};
use rand::RngExt;
use tokio::sync::watch::Sender;
use trillium::{Conn, Handler, Headers, Init, KnownHeaderName, Status, conn_try, conn_unwrap};
use trillium_client::{Client, Url};
use trillium_router::{Router, RouterConnExt};

type ContentHasher = Blake2s<digest::consts::U6>;

type ContentHash = [u8; {
    let bytes_len: usize = <<ContentHasher as digest::OutputSizeUser>::OutputSize as digest::typenum::ToInt<usize>>::INT;
    assert!(bytes_len % 3 == 0);
    bytes_len * 4 / 3
}];

#[derive(Default)]
struct Subscriptions(RwLock<Inner>);

#[derive(Default)]
struct Inner {
    by_url: HashMap<String, Arc<Mutex<Subscription>>>,
    by_id: HashMap<String, Arc<Mutex<Subscription>>>,
}

impl Subscriptions {
    fn by_url(&self, url: &str, client: &Client) -> Arc<Mutex<Subscription>> {
        if let Some(sub) = self.0.read().unwrap().by_url.get(url) {
            sub.clone()
        } else {
            self.0
                .write()
                .unwrap()
                .by_url
                .entry(url.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(Subscription::new(url.to_owned(), client))))
                .clone()
        }
    }

    fn by_id(&self, id: &str) -> Option<Arc<Mutex<Subscription>>> {
        self.0.read().unwrap().by_id.get(id).cloned()
    }

    fn add_id(&self, id: String, url: &str) {
        let mut subs = self.0.write().unwrap();
        let sub = subs.by_url[url].clone();
        let existing = subs.by_id.insert(id, sub);
        assert!(existing.is_none());
    }
}

struct Subscription {
    url: String,
    id: Option<String>,
    lease_expires: Option<SystemTime>,
    notify: Sender<Option<Page>>,
}

impl Subscription {
    fn new(url: String, client: &Client) -> Self {
        let notify = Sender::new(None);
        client.connector().runtime().spawn({
            let url = url.clone();
            let client = client.clone();
            let notify = notify.clone();
            async move {
                let mut conn = client.get(url).await.unwrap();
                let content = conn.response_body().read_bytes().await.unwrap();
                notify.send_replace(Some(Page::from_http(content, conn.response_headers())));
            }
        });
        Subscription {
            url,
            id: None,
            lease_expires: None,
            notify,
        }
    }

    fn ensure_subscription(&mut self, subs: &Subscriptions, client: &Client, base_url: &str) {
        if self.id.is_some() {
            if self.lease_expires.is_none_or(|e| e > SystemTime::now()) {
                return;
            }
        }

        let id = self.id.get_or_insert_with(|| {
            let id = format!("{:x}", rand::rng().random::<u128>());
            subs.add_id(id.clone(), &self.url);
            id
        });
        let callback = format!("{base_url}/notify/{id}");
        let url = self.url.clone();

        let mut notify = self.notify.subscribe();
        let client = client.clone();

        // FIXME ensure this can't send multiple subscription requests
        // FIXME handle the case where verification of intent never arrived
        client.connector().runtime().spawn(async move {
            if notify.borrow_and_update().is_none() {
                notify.changed().await.unwrap();
            }
            let page = notify.borrow().clone().unwrap();

            let base = Url::parse(&url).unwrap();
            let mut hub = base.join(&page.hub).unwrap();
            hub.query_pairs_mut()
                .append_pair("hub.callback", &callback)
                .append_pair("hub.mode", "subscribe")
                .append_pair("hub.topic", &url)
                .finish();
            let response = client.get(hub).await.unwrap();
            assert!(response.status().unwrap() == Status::Accepted);
        });
    }
}

#[derive(Clone)]
struct Page {
    hub: String,
    content_type: String,
    content: Vec<u8>,
    hash: ContentHash,
}

impl Page {
    fn from_http(content: Vec<u8>, headers: &Headers) -> Page {
        let content_type = headers
            .get_str(KnownHeaderName::ContentType)
            .map_or(String::new(), ToOwned::to_owned);

        let hash_bytes = ContentHasher::digest(&content);
        let mut hash = ContentHash::default();
        let hash_len =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode_slice(&hash_bytes, &mut hash);
        debug_assert_eq!(hash_len, Ok(hash.len()));

        let mut hub = String::new();
        if let Some(links) = headers.get_values(KnownHeaderName::Link) {
            for link in links {
                let Some(link) = link.as_str() else { continue };
                let link = parse_link_header::parse_with_rel(link).unwrap();
                // FIXME what if multiple hubs?
                let Some(link) = link.get("hub") else {
                    continue;
                };
                hub = link.raw_uri.clone();
                break;
            }
        }

        // TODO also look in HTML/XML documents
        // FIXME what if no hub?

        Page {
            hub,
            content_type,
            content,
            hash,
        }
    }
}

struct ClientWrapper(Client);

pub fn handler(client: Client) -> impl Handler {
    (
        Init::new(|info| async move {
            info.with_shared_state(Subscriptions::default())
                .with_shared_state(ClientWrapper(client))
        }),
        Router::new()
            .get("/notify/:id", verify_intent)
            .post("/notify/:id", notify)
            .get("/cached/*", wait_for_hash_change)
            .get("/:hash/*", wait_for_hash_change),
    )
}

async fn verify_intent(conn: Conn) -> Conn {
    let subs: &Subscriptions = conn.shared_state().unwrap();
    let id = conn.param("id").unwrap();
    let sub = conn_unwrap!(subs.by_id(id), conn);
    let mut sub = sub.lock().unwrap();
    debug_assert_eq!(sub.id.as_deref(), Some(id));

    let mut mode = None;
    let mut challenge = None;
    let mut lease_seconds = String::new();
    for (k, v) in form_urlencoded::parse(conn.querystring().as_bytes()) {
        match &*k {
            "hub.mode" => mode = Some(v.into_owned()),
            "hub.topic" if sub.url != v => return conn,
            "hub.challenge" => challenge = Some(v.into_owned()),
            "hub.lease_seconds" => lease_seconds = v.into_owned(),
            _ => {}
        }
    }

    // FIXME validate challenge is within reasonable limits
    let body = conn_unwrap!(challenge, conn);
    match &*conn_unwrap!(mode, conn) {
        "subscribe" => {
            let lease_seconds = conn_try!(lease_seconds.parse(), conn);
            sub.lease_expires = Some(SystemTime::now() + Duration::from_secs(lease_seconds))
        }
        "unsubscribe" => {
            subs.0.write().unwrap().by_id.remove(id);
            sub.id = None;
            sub.lease_expires = None;
        }
        "denied" => todo!(),
        _ => return conn,
    }

    conn.with_response_header(KnownHeaderName::ContentType, "application/octet-stream")
        .with_response_header(KnownHeaderName::XcontentTypeOptions, "nosniff")
        .ok(body)
}

async fn notify(mut conn: Conn) -> Conn {
    let content = conn_try!(conn.request_body().read_bytes().await, conn);
    let subs: &Subscriptions = conn.shared_state().unwrap();
    let id = conn.param("id").unwrap();
    // TODO return 410 Gone
    let sub = conn_unwrap!(subs.by_id(id), conn);
    let sub = sub.lock().unwrap();
    debug_assert_eq!(sub.id.as_deref(), Some(id));
    sub.notify
        .send_replace(Some(Page::from_http(content, conn.request_headers())));
    conn.ok("")
}

async fn wait_for_hash_change(conn: Conn) -> Conn {
    let subs: &Subscriptions = conn.shared_state().unwrap();
    let ClientWrapper(client) = conn.shared_state().unwrap();
    // TODO append query string
    let url = conn.wildcard().unwrap();
    let expected_hash = conn
        .param("hash")
        .and_then(|s| ContentHash::try_from(s.as_bytes()).ok())
        .unwrap_or_default();
    let base_url = format!(
        "{}://{}",
        if conn.is_secure() { "https" } else { "http" },
        conn.host().unwrap()
    );
    let sub = subs.by_url(url, client);
    let mut notify = {
        let mut sub = sub.lock().unwrap();
        debug_assert_eq!(sub.url, url);
        if !expected_hash.is_empty() {
            sub.ensure_subscription(subs, client, &base_url);
        }
        sub.notify.subscribe()
    };
    loop {
        if let Some(current) = &*notify.borrow_and_update() {
            if current.hash != expected_hash {
                let current = current.clone();
                let next_version = format!(
                    "{base_url}/{}/{url}",
                    str::from_utf8(&current.hash).unwrap()
                );
                return conn
                    .with_response_header(KnownHeaderName::ContentType, current.content_type)
                    .with_response_header("next-version", next_version)
                    .ok(current.content);
            }
        }
        conn_try!(notify.changed().await, conn);
    }
}
