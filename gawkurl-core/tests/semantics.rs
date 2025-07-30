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

#[test]
fn last_modified() {
    run(async |mut call| {
        let mut last_modified = None;

        // The first request will consider last-modified to be plausible even if
        // it's long ago.
        call.fetch(async |request| {
            assert_eq!(request.last_modified, last_modified);
            let date = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:00:00 GMT");
            let modified = header::HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT");
            last_modified = Some(modified.to_str().unwrap().parse().unwrap());

            let mut response = Response::new(Full::new(Bytes::from_static(b"1")));
            let headers = response.headers_mut();
            headers.insert(header::DATE, date);
            headers.insert(header::LAST_MODIFIED, modified);
            response
        })
        .await;

        assert_eq!(
            call.changed().await.last_changed.elapsed().as_secs(),
            478192223
        );

        // Subsequent requests require last-modified to be more recent than the
        // last time we fetched this page, taking clock skew and the age header
        // into account.
        call.fetch(async |request| {
            assert_eq!(request.last_modified, last_modified);
            let date = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:10:00 GMT");
            let modified = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:05:00 GMT");
            last_modified = Some(modified.to_str().unwrap().parse().unwrap());

            let mut response = Response::new(Full::new(Bytes::from_static(b"2")));
            let headers = response.headers_mut();
            headers.insert(header::DATE, date);
            headers.insert(header::LAST_MODIFIED, modified);
            headers.insert(header::AGE, header::HeaderValue::from_static("5"));
            response
        })
        .await;

        assert_eq!(call.changed().await.last_changed.elapsed().as_secs(), 305);

        // If last-modified is implausible, we approximate the last change time
        // as being right now, the moment we observed the change.
        call.fetch(async |request| {
            assert_eq!(request.last_modified, last_modified);
            let date = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:20:00 GMT");
            let modified = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:00:00 GMT");
            last_modified = Some(modified.to_str().unwrap().parse().unwrap());

            let mut response = Response::new(Full::new(Bytes::from_static(b"3")));
            let headers = response.headers_mut();
            headers.insert(header::DATE, date);
            headers.insert(header::LAST_MODIFIED, modified);
            response
        })
        .await;

        assert_eq!(call.changed().await.last_changed.elapsed().as_secs(), 0);

        // If the server takes some time to produce the response, we count that
        // time as part of the age of the response.
        call.fetch(async |request| {
            assert_eq!(request.last_modified, last_modified);
            let date = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:30:00 GMT");
            let modified = header::HeaderValue::from_static("Fri, 01 Jan 2010 00:25:00 GMT");
            last_modified = Some(modified.to_str().unwrap().parse().unwrap());

            let mut response = Response::new(Full::new(Bytes::from_static(b"4")));
            let headers = response.headers_mut();
            headers.insert(header::DATE, date);
            headers.insert(header::LAST_MODIFIED, modified);
            tokio::time::sleep(Duration::from_secs(7)).await;
            response
        })
        .await;

        assert_eq!(call.changed().await.last_changed.elapsed().as_secs(), 307);
    });
}

#[test]
fn last_modified_unix_epoch() {
    run(async |mut call| {
        // A pre-web last-modified should always be implausible.
        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            response.headers_mut().insert(
                header::LAST_MODIFIED,
                header::HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
            );
            response
        })
        .await;

        assert_eq!(call.changed().await.last_changed.elapsed().as_secs(), 0);
    });
}

#[test]
fn last_modified_dos_epoch() {
    run(async |mut call| {
        // A pre-web last-modified should always be implausible.
        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            response.headers_mut().insert(
                header::LAST_MODIFIED,
                header::HeaderValue::from_static("Tue, 01 Jan 1980 00:00:00 GMT"),
            );
            response
        })
        .await;

        assert_eq!(call.changed().await.last_changed.elapsed().as_secs(), 0);
    });
}

#[test]
fn ignore_server_errors() {
    run(async |mut call| {
        // 5xx response status codes should not trigger change notifications.
        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
        .await;

        call.fetch(async |_request| Response::new(Full::new(Bytes::new())))
            .await;

        assert_eq!(call.changed().await.contents, Bytes::new());

        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            response
        })
        .await;

        call.fetch(async |_request| Response::new(Full::new(Bytes::new())))
            .await;
    });
}

#[test]
fn respect_max_age() {
    run(async |mut call| {
        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("max-age=86400"),
            );
            response
        })
        .await;
        let last_fetch = Instant::now();
        call.changed().await;

        call.fetch(async |_request| {
            assert_eq!(last_fetch.elapsed().as_secs(), 86400);
            Response::new(Full::new(Bytes::new()))
        })
        .await;
    });
}

#[test]
fn respect_expires() {
    run(async |mut call| {
        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            response.headers_mut().insert(
                header::DATE,
                header::HeaderValue::from_static("Fri, 01 Jan 2010 00:00:00 GMT"),
            );
            response.headers_mut().insert(
                header::EXPIRES,
                header::HeaderValue::from_static("Sun, 03 Jan 2010 00:00:00 GMT"),
            );
            response
        })
        .await;
        let last_fetch = Instant::now();
        call.changed().await;

        call.fetch(async |_request| {
            assert_eq!(last_fetch.elapsed().as_secs(), 2 * 86400);
            Response::new(Full::new(Bytes::new()))
        })
        .await;
    });
}

#[test]
fn prefer_max_age() {
    run(async |mut call| {
        call.fetch(async |_request| {
            let mut response = Response::new(Full::new(Bytes::new()));
            response.headers_mut().insert(
                header::DATE,
                header::HeaderValue::from_static("Fri, 01 Jan 2010 00:00:00 GMT"),
            );
            response.headers_mut().insert(
                header::EXPIRES,
                header::HeaderValue::from_static("Wed, 01 Jan 2025 00:00:00 GMT"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("max-age=123456"),
            );
            response
        })
        .await;
        let last_fetch = Instant::now();
        call.changed().await;

        call.fetch(async |_request| {
            assert_eq!(last_fetch.elapsed().as_secs(), 123456);
            Response::new(Full::new(Bytes::new()))
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
