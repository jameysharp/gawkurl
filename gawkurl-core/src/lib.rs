use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use base64::Engine;
use blake2::{Blake2s, Digest, digest};
use bytes::Bytes;
use httpdate::HttpDate;
use reqwest::{Client, StatusCode, header};
use tokio::sync::watch::{Receiver, Sender};
use tokio::time::Instant;

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
                .or_insert_with_key(|uri| {
                    let sender = Sender::new(Page::default());
                    tokio::spawn(watch_url(self.client.clone(), uri.clone(), sender.clone()));
                    sender
                })
                .subscribe()
        }
    }
}

async fn watch_url(client: Client, uri: String, sender: Sender<Page>) -> ! {
    const DAY: u64 = 86400;

    // validators provided by the server
    let mut etag = None;
    let mut last_modified: Option<HttpDate> = None;

    // last_fetched is used to decide if the last-modified header is
    // plausible. if the page changed after our previous fetch, but
    // last-modified says it changed before that, it's nonsense. before
    // fetching the page the very first time, initialize this to a date
    // that's after most time epochs, including the Unix epoch in 1970
    // and the DOS epoch in 1980, so that if a file has a timestamp of
    // "0" on such systems we know to ignore it. but this date should be
    // before any web page somebody might want to monitor, so since HTTP
    // was introduced around 1991, 1990 seems like a good choice.
    // https://en.wikipedia.org/wiki/Epoch_(computing)#Notable_epoch_dates_in_computing
    let mut last_fetched =
        SystemTime::UNIX_EPOCH + Duration::from_secs(20 * (365 * 4 + 1) / 4 * DAY);

    // our best guess of the last time this page changed, taking into
    // account what time it was when we fetched the latest version, as
    // well as the last-modified header, if we trust it.
    let mut last_changed = Instant::now();
    let mut last_hash = ContentHash::default();

    loop {
        // fetch and hash the page
        // TODO: ratelimit requests per domain
        let mut request = client.get(&uri);
        if let Some(etag) = &etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &last_modified {
            request = request.header(header::IF_MODIFIED_SINCE, last_modified.to_string());
        }

        // if we don't get a server-provided max-age in response to
        // this request, treat it like the response can be revalidated
        // immediately.
        let mut max_age = 0;

        let request_time = SystemTime::now();
        if let Ok(response) = request.send().await {
            let response_time = SystemTime::now();

            let headers = response.headers();

            // the date header indicates what time the origin server
            // generated this reponse, and if present allows us to
            // interpret other date headers correctly even if our clock
            // disagrees with theirs; but if it's absent we can guess
            // our clock is probably close enough.
            let date = parse::<HttpDate>(headers, header::DATE).map_or(response_time, Into::into);

            let age = compute_age(headers, request_time, response_time);

            // the origin can specify an expiration time in
            // two different ways: either the HTTP/1.1 way with
            // `cache-control: max-age=N`, where N is the number of
            // seconds to consider the response still valid for, or
            // with the HTTP/1.0 `expires` header containing a date. per
            // https://datatracker.ietf.org/doc/html/rfc7234#section-5.3
            // we prefer max-age over expires.
            if let Some(v) = parse_max_age(headers).or_else(|| parse_expires(headers, date)) {
                max_age = v.saturating_sub(age)
            }

            // per https://datatracker.ietf.org/doc/html/rfc7234#section-4.3.3,
            // we're allowed to pretend like 5xx server errors were actually
            // the server just not responding, and reuse a cached response
            // instead. I'm choosing to do the same for all non-200 responses
            // but it's not clear yet if that's the right plan for this use.
            if response.status() == StatusCode::OK {
                etag = headers.get(header::ETAG).cloned();
                last_modified = parse(headers, header::LAST_MODIFIED);

                let contents = response.bytes().await.unwrap();

                let hash_bytes = ContentHasher::digest(&contents);
                let mut hash = ContentHash::default();
                let hash_len = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode_slice(&hash_bytes, &mut hash);
                debug_assert_eq!(hash_len, Ok(hash.len()));

                if last_hash != hash {
                    last_hash = hash;
                    sender.send_replace(Page { hash, contents });
                    last_changed = Instant::now();

                    // TODO: parse XML and look for RSS or Atom timestamps

                    // last-modified should only be compared to
                    // times measured using the server's clock, but
                    // last_fetched uses our local clock, so turn both
                    // into "seconds before now" in the appropriate
                    // clock domain before comparing them.
                    if let Some(last_modified) = last_modified {
                        if let Ok(last_modified_age) = date.duration_since(last_modified.into()) {
                            let last_modified_age =
                                last_modified_age.saturating_add(Duration::from_secs(age));
                            if let Ok(last_fetched_age) = request_time.duration_since(last_fetched)
                            {
                                if last_modified_age < last_fetched_age {
                                    last_changed -= last_modified_age;
                                }
                            }
                        }
                    }
                }
            } else {
                // handling 304 Not Modified responses is specified in
                // https://datatracker.ietf.org/doc/html/rfc7234#section-4.3.4
                // but because we only store one response at a time and
                // never add synthetic if-modified-since headers, the
                // cases boil down to this: if the response has an etag and
                // it matches the etag we have from a previous response,
                // then we should update our stored copy's headers from
                // the new response. same goes for last-modified. but
                // if those headers match then there's nothing to update
                // since we don't track any other headers yet. so, like
                // 5xx responses, 304 should preserve the old response
                // unchanged.
            }

            // only count it as a successful fetch if we actually got
            // a response from the server, because when we have network
            // trouble or similar, it's possible that we miss a change.
            last_fetched = request_time;
        }

        // https://datatracker.ietf.org/doc/html/rfc7234#section-4.2.2
        let last_changed_age = Instant::now().duration_since(last_changed).as_secs();
        let heuristic_freshness = last_changed_age / 10;

        // now we have two hints about the minimum time we should wait
        // before checking again. max-age is the origin server's hint
        // that a cache should not revalidate before that many seconds
        // have elapsed. and our heuristic freshness is an estimate
        // of how long we think it might be before it's worth checking
        // again. so wait for whichever hint is longer, but also clamp
        // to a reasonable range that neither waits forever nor sends
        // another request immediately.
        let wait = heuristic_freshness.max(max_age).clamp(600, 7 * DAY);

        // sleep, then loop
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

fn parse_max_age(headers: &header::HeaderMap) -> Option<u64> {
    headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| t.split_once('='))
        .filter(|(k, _)| k.trim_ascii() == "max-age")
        .find_map(|(_, v)| v.trim_ascii().parse().ok())
}

/// Parse the contents of an `expires` header into a max-age in seconds relative
/// to the given date. Returns None if parsing fails or the date is in the past.
/// According to https://datatracker.ietf.org/doc/html/rfc7234#section-5.3, if
/// the header exists at all then parse failures MUST be treated like a date
/// in the past. But we treat the absence of the header as if the response has
/// already expired too, so it's not a special case.
fn parse_expires(headers: &header::HeaderMap, date: SystemTime) -> Option<u64> {
    let expires = parse::<HttpDate>(headers, header::EXPIRES)?;
    let max_age = SystemTime::from(expires).duration_since(date).ok()?;
    Some(max_age.as_secs())
}

/// Compute an "estimate of the number of seconds since the response
/// was generated or validated by the origin server". As defined in
/// <https://datatracker.ietf.org/doc/html/rfc7234#section-4.2.3>, except I
/// assume there are no HTTP/1.0 caches, so I don't use "apparent-age".
fn compute_age(
    headers: &header::HeaderMap,
    request_time: SystemTime,
    response_time: SystemTime,
) -> u64 {
    let age_value = parse::<u64>(headers, header::AGE).unwrap_or(0);

    let response_delay = response_time
        .duration_since(request_time)
        .map_or(0, |d| d.as_secs());
    let corrected_age_value = age_value.saturating_add(response_delay);

    corrected_age_value
}

fn parse<T: std::str::FromStr>(headers: &header::HeaderMap, name: header::HeaderName) -> Option<T> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}
