use hawkurl::handler;
use tokio::sync::mpsc;
use trillium::{Conn, Handler, KnownHeaderName, Status};
use trillium_client::{Client, Url};
use trillium_testing::{ServerConnector, TestServer, harness, test};

struct HubRequest {
    callback: Url,
    mode: String,
    topic: String,
}

async fn mock_server(
    mock_origin: impl Fn(Conn) -> Conn + Send + Sync + 'static,
) -> (
    TestServer<impl Handler>,
    mpsc::UnboundedReceiver<HubRequest>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let client = Client::new(ServerConnector::new(move |conn: Conn| {
        let conn = match conn.host() {
            Some("origin.example") => mock_origin(conn),
            Some("hub.example") => {
                let mut callback = None;
                let mut mode = None;
                let mut topic = None;
                for (k, v) in form_urlencoded::parse(conn.querystring().as_bytes()) {
                    match &*k {
                        "hub.callback" => callback = Some(Url::parse(&v).unwrap()),
                        "hub.mode" => mode = Some(v.into_owned()),
                        "hub.topic" => topic = Some(v.into_owned()),
                        _ => {}
                    }
                }

                let request = HubRequest {
                    callback: callback.expect("hub.callback"),
                    mode: mode.expect("hub.mode"),
                    topic: topic.expect("hub.topic"),
                };
                sender.send(request).expect("hub request listener");

                conn.with_status(Status::Accepted).halt()
            }
            _ => conn,
        };
        async { conn }
    }));
    (TestServer::new(handler(client)).await, receiver)
}

#[test(harness)]
async fn basic() {
    let (app, mut receiver) = mock_server(|conn| {
        conn.with_response_header(KnownHeaderName::ContentType, "text/plain")
            .with_response_header(
                KnownHeaderName::Link,
                "<https://origin.example/>; rel=\"self\"",
            )
            .with_response_header(KnownHeaderName::Link, "<https://hub.example/>; rel=\"hub\"")
            .ok("hello")
    })
    .await;

    let cached = app.get("/cached/https://origin.example").await;
    cached
        .assert_ok()
        .assert_header("Content-Type", "text/plain")
        .assert_body("hello");
    assert!(receiver.is_empty());

    futures_lite::future::zip(
        async {
            app.get(cached.header("Next-Version").expect("HATEOAS header"))
                .await
                .assert_ok()
                .assert_header("Content-Type", "text/plain")
                .assert_body("goodbye");
        },
        async {
            let request = receiver.recv().await.expect("hub request");
            assert_eq!(request.mode, "subscribe");
            assert_eq!(request.topic, "https://origin.example");

            let challenge = "you-will-never-guess-this-surprising-challenge";
            let mut verify_intent = request.callback.clone();
            verify_intent
                .query_pairs_mut()
                .append_pair("hub.mode", &request.mode)
                .append_pair("hub.topic", &request.topic)
                .append_pair("hub.challenge", challenge)
                .append_pair("hub.lease_seconds", "3600")
                .finish();
            app.get(verify_intent.as_str())
                .await
                .assert_ok()
                .assert_body(challenge);

            app.post(request.callback.as_str())
                .with_request_header(KnownHeaderName::ContentType, "text/plain")
                .with_body("goodbye")
                .await
                .assert_ok();
        },
    )
    .await;
}
