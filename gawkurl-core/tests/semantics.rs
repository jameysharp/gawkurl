use std::convert::Infallible;
use std::time::{Duration, SystemTime};

use gawkurl_core::{Bytes, Client, watch_url};
use http::{Response, StatusCode, header};
use http_body_util::Full;
use httpdate::HttpDate;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

struct Request {
    etag: Option<header::HeaderValue>,
    last_modified: Option<HttpDate>,
}

enum Call {
    Fetch(Request, oneshot::Sender<Response<Full<Bytes>>>),
    Changed(gawkurl_core::Page),
}

struct CallStream(mpsc::UnboundedReceiver<Call>);

impl CallStream {
    async fn fetch(&mut self, f: impl AsyncFnOnce(Request) -> Response<Full<Bytes>>) {
        if let Some(Call::Fetch(request, result)) = self.0.recv().await {
            result.send(f(request).await).unwrap();
        } else {
            panic!("expected fetch()");
        }
    }

    async fn changed(&mut self) -> gawkurl_core::Page {
        if let Some(Call::Changed(page)) = self.0.recv().await {
            page
        } else {
            panic!("expected changed()");
        }
    }
}

#[test]
fn unchanged() {
    run(async |mut call| {
        let contents = Bytes::from_static(b"test");

        call.fetch(async |request| {
            assert_eq!(request.etag, None);
            assert_eq!(request.last_modified, None);
            Response::new(Full::new(contents.clone()))
        })
        .await;

        let page = call.changed().await;
        assert_eq!(page.contents, contents);
        assert_eq!(str::from_utf8(&page.hash).unwrap(), "qqI6ndHe");

        call.fetch(async |request| {
            assert_eq!(request.etag, None);
            assert_eq!(request.last_modified, None);
            Response::new(Full::new(contents.clone()))
        })
        .await;

        call.fetch(async |request| {
            assert_eq!(request.etag, None);
            assert_eq!(request.last_modified, None);
            Response::new(Full::new(contents.clone()))
        })
        .await;
    });
}

#[test]
fn changed() {
    run(async |mut call| {
        let contents = Bytes::from_static(b"test");

        call.fetch(async |request| {
            assert_eq!(request.etag, None);
            assert_eq!(request.last_modified, None);
            Response::new(Full::new(contents.clone()))
        })
        .await;

        let page = call.changed().await;
        assert_eq!(page.contents, contents);
        assert_eq!(str::from_utf8(&page.hash).unwrap(), "qqI6ndHe");

        let contents = Bytes::from_static(b"test 2");

        call.fetch(async |request| {
            assert_eq!(request.etag, None);
            assert_eq!(request.last_modified, None);
            Response::new(Full::new(contents.clone()))
        })
        .await;

        let page = call.changed().await;
        assert_eq!(page.contents, contents);
        assert_eq!(str::from_utf8(&page.hash).unwrap(), "TJ8x3sfE");
    });
}

#[test]
fn unchanged_304() {
    run(async |mut call| {
        let contents = Bytes::from_static(b"test");
        let etag = header::HeaderValue::from_static("\"etag\"");

        call.fetch(async |request| {
            assert_eq!(request.etag, None);
            assert_eq!(request.last_modified, None);
            let mut response = Response::new(Full::new(contents.clone()));
            response.headers_mut().insert(header::ETAG, etag.clone());
            response
        })
        .await;

        let page = call.changed().await;
        assert_eq!(page.contents, contents);
        assert_eq!(str::from_utf8(&page.hash).unwrap(), "qqI6ndHe");

        call.fetch(async |request| {
            assert_eq!(request.etag.as_ref(), Some(&etag));
            assert_eq!(request.last_modified, None);
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            response.headers_mut().insert(header::ETAG, etag.clone());
            response
        })
        .await;

        call.fetch(async |request| {
            assert_eq!(request.etag.as_ref(), Some(&etag));
            assert_eq!(request.last_modified, None);
            Response::new(Full::new(contents.clone()))
        })
        .await;
    });
}

struct TestClient {
    start: Instant,
    call: mpsc::UnboundedSender<Call>,
}

impl Client for TestClient {
    type Error = Infallible;
    type Body = Full<Bytes>;

    fn fetch(
        &mut self,
        etag: Option<&header::HeaderValue>,
        last_modified: Option<&HttpDate>,
    ) -> impl Future<Output = Result<http::Response<Self::Body>, Self::Error>> {
        let etag = etag.cloned();
        let last_modified = last_modified.cloned();
        async move {
            let (result, done) = oneshot::channel();
            self.call
                .send(Call::Fetch(
                    Request {
                        etag,
                        last_modified,
                    },
                    result,
                ))
                .unwrap();
            Ok(done.await.unwrap())
        }
    }

    fn changed(&mut self, page: gawkurl_core::Page) {
        self.call.send(Call::Changed(page)).unwrap();
    }

    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(40 * (365 * 4 + 1) / 4 * 86400)
            + self.start.elapsed()
    }
}

fn run(f: impl AsyncFnOnce(CallStream)) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(async {
            let (tx, rx) = mpsc::unbounded_channel();
            let client = TestClient {
                start: Instant::now(),
                call: tx,
            };
            futures_lite::future::or(f(CallStream(rx)), async {
                watch_url(client).await;
            })
            .await;
        })
}
