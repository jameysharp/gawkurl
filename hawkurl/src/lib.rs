use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use base64::Engine;
use blake2::{Blake2s, Digest, digest};
use nom_rfc8288::complete::link_lenient;
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
    fn by_url(&self, url: &str, ClientWrapper(client): &ClientWrapper) -> Arc<Mutex<Subscription>> {
        if let Some(sub) = self.0.read().unwrap().by_url.get(url) {
            sub.clone()
        } else {
            let mut subs = self.0.write().unwrap();
            let Inner { by_url, by_id } = &mut *subs;
            by_url
                .entry(url.to_owned())
                .or_insert_with(|| {
                    let sub = Subscription::new(url.to_owned(), client);
                    let id = sub.id.clone();
                    let sub = Arc::new(Mutex::new(sub));
                    let existing = by_id.insert(id, sub.clone());
                    assert!(existing.is_none());
                    sub
                })
                .clone()
        }
    }

    fn by_id(&self, id: &str) -> Option<Arc<Mutex<Subscription>>> {
        self.0.read().unwrap().by_id.get(id).cloned()
    }
}

struct Subscription {
    url: String,
    id: String,
    state: SubscriptionState,
    notify: Sender<Page>,
}

impl Subscription {
    fn new(url: String, client: &Client) -> Self {
        let notify = Sender::new(Page::Pending);
        client.connector().runtime().spawn({
            let url = url.clone();
            let client = client.clone();
            let notify = notify.clone();
            async move {
                let page = match client.get(url).await {
                    Ok(mut conn) => match conn.status().unwrap() {
                        Status::Ok => match conn.response_body().read_bytes().await {
                            Ok(body) => Page::from_http(body, conn.response_headers()),
                            Err(e) => Page::Failed(e),
                        },
                        status => Page::Failed(trillium_client::Error::Other(
                            format!("got status {status}").into(),
                        )),
                    },
                    Err(e) => Page::Failed(e),
                };
                notify.send_replace(page);
            }
        });

        Subscription {
            url,
            id: format!("{:x}", rand::rng().random::<u128>()),
            state: SubscriptionState::None,
            notify,
        }
    }
}

fn ensure_subscription(
    sub: &Arc<Mutex<Subscription>>,
    ClientWrapper(client): &ClientWrapper,
    base_url: &str,
) -> Result<(), String> {
    let mut guard = sub.lock().unwrap();
    match &guard.state {
        SubscriptionState::None => {}
        SubscriptionState::Confirmed { lease_expires } => {
            if *lease_expires > SystemTime::now() {
                return Ok(());
            }
        }
        SubscriptionState::Requested => return Ok(()),
        SubscriptionState::Failed(err) => return Err(err.to_string()),
    }
    guard.state = SubscriptionState::Requested;

    let callback = format!("{base_url}/notify/{}", &guard.id);
    let url = guard.url.clone();
    let mut notify = guard.notify.subscribe();
    drop(guard);

    let sub = sub.clone();
    let client = client.clone();

    // FIXME handle the case where verification of intent never arrived
    client.connector().runtime().spawn(async move {
        let base = Url::parse(&url).unwrap();
        let mut hub = loop {
            match &*notify.borrow_and_update() {
                Page::Pending => {}
                Page::Failed(_) => return,
                Page::Ready { hub, .. } => break base.join(hub).unwrap(),
            }
            notify.changed().await.unwrap();
        };

        hub.query_pairs_mut()
            .append_pair("hub.callback", &callback)
            .append_pair("hub.mode", "subscribe")
            .append_pair("hub.topic", &url)
            .finish();

        let err = match client.post(hub).await {
            Ok(response) => {
                let status = response.status().unwrap();
                if status == Status::Accepted {
                    return;
                }
                trillium_client::Error::Other(format!("hub returned status {status}").into())
            }
            Err(err) => err,
        };

        sub.lock().unwrap().state = SubscriptionState::Failed(err);
    });

    Ok(())
}

enum SubscriptionState {
    None,
    Requested,
    Confirmed { lease_expires: SystemTime },
    Failed(trillium_client::Error),
}

enum Page {
    Pending,
    Failed(trillium_client::Error),
    Ready {
        hub: String,
        content_type: String,
        content: Vec<u8>,
        hash: ContentHash,
    },
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
        for header in headers
            .get_values(KnownHeaderName::Link)
            .map_or(&[][..], |v| &*v)
        {
            let Some(s) = header.as_str() else { continue };
            for link in link_lenient(s).unwrap_or_default() {
                let Some(link) = link else { continue };
                if let Some(rel) = link
                    .params
                    .iter()
                    .find_map(|p| p.val.as_ref().filter(|_| p.key == "rel"))
                {
                    // FIXME what if multiple hubs?
                    if rel == "hub" {
                        hub = link.url.to_owned();
                        break;
                    }
                    // TODO also get rel=self link
                }
            }
        }

        // TODO also look in HTML/XML documents
        // FIXME what if no hub?

        Page::Ready {
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
    debug_assert_eq!(&sub.id, id);

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
            let lease_expires = SystemTime::now() + Duration::from_secs(lease_seconds);
            sub.state = SubscriptionState::Confirmed { lease_expires };
        }
        "unsubscribe" => {
            sub.state = SubscriptionState::None;
        }
        "denied" => {
            sub.state = SubscriptionState::Failed(trillium_client::Error::Other(
                "hub denied subscription".into(),
            ));
        }
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
    let Some(sub) = subs.by_id(id) else {
        return conn.with_status(Status::Gone).halt();
    };
    let sub = sub.lock().unwrap();
    debug_assert_eq!(&sub.id, id);
    sub.notify
        .send_replace(Page::from_http(content, conn.request_headers()));
    conn.ok("")
}

async fn wait_for_hash_change(mut conn: Conn) -> Conn {
    let subs: &Subscriptions = conn.shared_state().unwrap();
    // TODO append query string
    let url = conn.wildcard().unwrap().to_owned();
    let expected_hash = conn
        .param("hash")
        .and_then(|s| ContentHash::try_from(s.as_bytes()).ok())
        .unwrap_or_default();
    let base_url = format!(
        "{}://{}",
        if conn.is_secure() { "https" } else { "http" },
        conn.host().unwrap()
    );
    let sub = subs.by_url(&url, conn.shared_state().unwrap());
    let mut notify = {
        let sub = sub.lock().unwrap();
        debug_assert_eq!(sub.url, url);
        sub.notify.subscribe()
    };
    loop {
        match &*notify.borrow_and_update() {
            Page::Failed(err) => {
                return conn
                    .with_status(Status::BadGateway)
                    .with_body(format!(
                        "something went wrong fetching the page you asked for: {err}"
                    ))
                    .halt();
            }
            Page::Ready {
                hash,
                content_type,
                content,
                ..
            } if hash != &expected_hash => {
                let next_version = format!("{base_url}/{}/{url}", str::from_utf8(hash).unwrap());
                return conn
                    .with_response_header("next-version", next_version)
                    .with_response_header(KnownHeaderName::ContentType, content_type.clone())
                    .ok(content.clone());
            }
            _ => {}
        }
        if expected_hash != ContentHash::default() {
            if let Err(err) = ensure_subscription(&sub, conn.shared_state().unwrap(), &base_url) {
                return conn
                    .with_status(Status::BadGateway)
                    .with_body(format!(
                        "could not subscribe to updates for this page: {err}"
                    ))
                    .halt();
            }
        }
        // wait until the page changes, but interrupt at server shutdown or client disconnect
        // FIXME also wake up if the subscription status changes, I guess?
        let Some(Some(Ok(()))) = conn
            .cancel_on_disconnect(conn.swansong().interrupt(notify.changed()))
            .await
        else {
            return conn;
        };
    }
}
