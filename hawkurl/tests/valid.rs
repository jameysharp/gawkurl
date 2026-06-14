use std::sync::{Arc, Mutex};

use hawkurl::handler;
use trillium::{Conn, Handler, KnownHeaderName, Status, conn_unwrap};
use trillium_client::{Client, Url};
use trillium_testing::{RuntimeTrait, ServerConnector, TestServer, harness, runtime, test};

struct MyTestServer<H> {
    fake: TestServer<H>,
    real: Arc<Mutex<TestServer<H>>>,
}

impl<H> MyTestServer<H> {
    fn swap(&mut self) {
        std::mem::swap(&mut *self.real.lock().unwrap(), &mut self.fake);
    }
}

impl<H> Drop for MyTestServer<H> {
    fn drop(&mut self) {
        self.swap();
    }
}

async fn mock_server() -> MyTestServer<impl Handler> {
    let client = Client::new(ServerConnector::new(|conn: Conn| async move { conn }));
    let fake = Arc::new(Mutex::new(TestServer::new(handler(client)).await));
    let app = fake.clone();
    let client = Client::new(ServerConnector::new(move |conn: Conn| {
        let app = app.clone();
        async move {
            match conn_unwrap!(conn.host(), conn) {
                "origin.example" => mock_origin(conn),
                "hub.example" => mock_hub(conn, app),
                _ => conn,
            }
        }
    }));
    let mut server = MyTestServer {
        fake: TestServer::new(handler(client)).await,
        real: fake,
    };
    server.swap();
    server
}

fn mock_origin(conn: Conn) -> Conn {
    conn.with_response_header(KnownHeaderName::ContentType, "text/plain")
        .with_response_header(
            KnownHeaderName::Link,
            "<https://origin.example/>; rel=\"self\"",
        )
        .with_response_header(KnownHeaderName::Link, "<https://hub.example/>; rel=\"hub\"")
        .ok("hello")
}

fn mock_hub(conn: Conn, app: Arc<Mutex<TestServer<impl Handler>>>) -> Conn {
    let mut callback = Url::parse("file:///").unwrap();
    let mut mode = String::new();
    let mut topic = String::new();
    for (k, v) in form_urlencoded::parse(conn.querystring().as_bytes()) {
        match &*k {
            "hub.callback" => callback = Url::parse(&v).unwrap(),
            "hub.mode" => mode = v.into_owned(),
            "hub.topic" => topic = v.into_owned(),
            _ => {}
        }
    }

    assert_eq!(mode, "subscribe");
    assert_eq!(topic, "https://origin.example");

    runtime().spawn(async move {
        let challenge = "you-will-never-guess-this-surprising-challenge";
        let mut verify_intent = callback.clone();
        verify_intent
            .query_pairs_mut()
            .append_pair("hub.mode", &mode)
            .append_pair("hub.topic", &topic)
            .append_pair("hub.challenge", challenge)
            .append_pair("hub.lease_seconds", "3600")
            .finish();
        let get = app.lock().unwrap().get(verify_intent.as_str());
        get.await.assert_ok().assert_body(challenge);

        let post = app.lock().unwrap().post(callback.as_str());
        post.with_request_header(KnownHeaderName::ContentType, "text/plain")
            .with_body("goodbye")
            .await
            .assert_ok();
    });

    conn.with_status(Status::Accepted).halt()
}

#[test(harness)]
async fn basic() {
    let app = mock_server().await;

    let cached = app
        .real
        .lock()
        .unwrap()
        .get("/cached/https://origin.example");
    let cached = cached.await;
    cached
        .assert_ok()
        .assert_header("Content-Type", "text/plain")
        .assert_body("hello");

    let next = cached.header("Next-Version").expect("HATEOAS header");

    let changed = app.real.lock().unwrap().get(next);
    changed
        .await
        .assert_ok()
        .assert_header("Content-Type", "text/plain")
        .assert_body("goodbye");
}
