//! The forwarding path against fake providers on real sockets: the
//! credential boundary, failover, the retry budget, both timeouts, and
//! both size caps.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, body::Bytes, http::HeaderMap, response::IntoResponse};
use lb::{
    config::{Config, Provider},
    jsonrpc::{
        METHOD_DENIED, NO_HEALTHY_PROVIDER, REQUEST_TIMED_OUT, REQUEST_TOO_LARGE,
        RESPONSE_TOO_LARGE,
    },
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// One recorded request: the headers and body a provider saw.
type Seen = (HeaderMap, Bytes);

struct Fake {
    seen: Mutex<Vec<Seen>>,
    status: axum::http::StatusCode,
    response: Vec<u8>,
    delay: Duration,
}

/// A fake provider on a real socket; answers every request the same way.
async fn fake_provider(response: &[u8], delay: Duration) -> (SocketAddr, Arc<Fake>) {
    broken_provider(axum::http::StatusCode::OK, response, delay).await
}

/// Like `fake_provider`, but answering with the given HTTP status.
async fn broken_provider(
    status: axum::http::StatusCode,
    response: &[u8],
    delay: Duration,
) -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake {
        seen: Mutex::new(Vec::new()),
        status,
        response: response.to_vec(),
        delay,
    });
    let state = fake.clone();
    let app = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let state = state.clone();
        async move {
            state.seen.lock().await.push((headers, body));
            tokio::time::sleep(state.delay).await;
            (state.status, state.response.clone()).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (addr, fake)
}

/// An address that refuses connections: bound, then dropped.
async fn dead_addr() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    listener.local_addr().expect("addr")
}

/// A provider speaking raw HTTP: answers `head`, then `body` chunks with a
/// pause between them, then holds the connection open forever. Used for
/// the cases a well-behaved server cannot produce. Also returns its
/// connection count: one per attempt the LB made.
async fn raw_provider(
    head: &'static [u8],
    chunks: Vec<Vec<u8>>,
    gap: Duration,
) -> (SocketAddr, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::Relaxed);
            let chunks = chunks.clone();
            tokio::spawn(async move {
                let mut scratch = vec![0u8; 8192];
                let _ = stream.read(&mut scratch).await;
                if stream.write_all(head).await.is_err() {
                    return;
                }
                for chunk in chunks {
                    tokio::time::sleep(gap).await;
                    if stream.write_all(&chunk).await.is_err() {
                        return;
                    }
                }
                // Never close: an unfinished body, not an EOF.
                std::future::pending::<()>().await
            });
        }
    });
    (addr, hits)
}

/// Boots the LB over the given provider addresses, everyone eligible.
async fn start_lb(
    addrs: &[SocketAddr],
    tune: impl Fn(&mut Config),
) -> (lb::service::Service, String) {
    let mut config = Config::default();
    config.listen.public = "127.0.0.1:0".parse().expect("addr");
    config.listen.admin = "127.0.0.1:0".parse().expect("addr");
    config.providers = addrs
        .iter()
        .enumerate()
        .map(|(i, addr)| Provider {
            id: format!("p{i}"),
            url: format!("http://{addr}"),
        })
        .collect();
    tune(&mut config);
    let service = lb::service::start(config).await.expect("service boots");
    for entry in service.pool.entries() {
        entry.set_eligible(true);
    }
    let public = format!("http://{}", service.public_addr);
    (service, public)
}

/// Every request in this suite goes through a client that gives up:
/// a regression that never answers fails the test instead of hanging it.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10);

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(GIVE_UP_AFTER)
        .build()
        .expect("client")
}

async fn post(url: &str, body: Value) -> (u16, Value) {
    let response = client()
        .post(url)
        .header("authorization", "Bearer client-secret")
        .header("x-forwarded-for", "203.0.113.9")
        .json(&body)
        .send()
        .await
        .expect("lb answers");
    let status = response.status().as_u16();
    (status, response.json().await.expect("json body"))
}

fn request(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "eth_blockNumber", "params": []})
}

#[tokio::test]
async fn relays_byte_identical_and_strips_client_headers() {
    let answer = br#"{"jsonrpc":"2.0","id":7,"result":"0x2a"}"#;
    let (addr, fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    let (status, body) = post(&public, request(7)).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], "0x2a");

    let seen = fake.seen.lock().await;
    let (headers, request_body) = &seen[0];
    assert_eq!(
        serde_json::from_slice::<Value>(request_body).expect("json"),
        request(7),
        "request body crosses untouched"
    );
    assert!(
        headers.get("authorization").is_none(),
        "client credentials must not cross"
    );
    assert!(
        headers.get("x-forwarded-for").is_none(),
        "client identity must not cross"
    );
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}

#[tokio::test]
async fn provider_error_is_an_answer_not_retried() {
    let error = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
    let (err_addr, err_fake) = fake_provider(error, Duration::ZERO).await;
    let (ok_addr, ok_fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[err_addr, ok_addr], |_| {}).await;

    let (status, body) = post(&public, request(1)).await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601);
    assert_eq!(err_fake.seen.lock().await.len(), 1);
    assert_eq!(
        ok_fake.seen.lock().await.len(),
        0,
        "an answer is never retried elsewhere"
    );
}

#[tokio::test]
async fn http_error_status_fails_over() {
    use axum::http::StatusCode;

    // A dead tunnel answers in the node's place: 502 with an HTML page.
    let (broken, broken_fake) = broken_provider(
        StatusCode::BAD_GATEWAY,
        b"<html>tunnel down</html>",
        Duration::ZERO,
    )
    .await;
    let answer = br#"{"jsonrpc":"2.0","id":8,"result":"ok"}"#;
    let (live, _live_fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[broken, live], |_| {}).await;

    let (status, body) = post(&public, request(8)).await;
    assert_eq!(status, 200, "the client never sees the broken provider");
    assert_eq!(body["result"], "ok");
    assert_eq!(broken_fake.seen.lock().await.len(), 1);
}

#[tokio::test]
async fn http_error_body_never_reaches_the_client() {
    use axum::http::StatusCode;

    let (broken, broken_fake) = broken_provider(
        StatusCode::SERVICE_UNAVAILABLE,
        b"<html>busy</html>",
        Duration::ZERO,
    )
    .await;
    let (_service, public) = start_lb(&[broken], |_| {}).await;

    let (status, body) = post(&public, request(2)).await;
    assert_eq!(status, 503); // Service Unavailable
    assert_eq!(
        body["error"]["code"], NO_HEALTHY_PROVIDER,
        "LB envelope, not the HTML"
    );
    assert_eq!(
        broken_fake.seen.lock().await.len(),
        3,
        "the full budget was spent trying"
    );
}

#[tokio::test]
async fn dead_provider_fails_over_invisibly() {
    let dead = dead_addr().await;
    let answer = br#"{"jsonrpc":"2.0","id":3,"result":"ok"}"#;
    let (live, fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[dead, live], |_| {}).await;

    let (status, body) = post(&public, request(3)).await;
    assert_eq!(status, 200, "the client never sees the dead provider");
    assert_eq!(body["result"], "ok");
    assert_eq!(fake.seen.lock().await.len(), 1);
}

#[tokio::test]
async fn exhausted_budget_answers_no_provider_with_the_request_id() {
    let addrs = [dead_addr().await, dead_addr().await, dead_addr().await];
    let (_service, public) = start_lb(&addrs, |_| {}).await;

    let (status, body) = post(&public, request(9)).await;
    assert_eq!(status, 503); // Service Unavailable
    assert_eq!(body["error"]["code"], NO_HEALTHY_PROVIDER);
    assert_eq!(body["id"], 9, "error envelopes echo the request id");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.starts_with("lb: "), "{message}");
}

#[tokio::test]
async fn slow_provider_times_out_and_fails_over() {
    let (slow, slow_fake) = fake_provider(b"{}", Duration::from_millis(500)).await;
    let answer = br#"{"jsonrpc":"2.0","id":4,"result":"fast"}"#;
    let (fast, _fast_fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[slow, fast], |config| {
        config.proxy.attempt_timeout = Duration::from_millis(100);
    })
    .await;

    let started = std::time::Instant::now();
    let (status, body) = post(&public, request(4)).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], "fast");
    assert!(
        started.elapsed() < Duration::from_millis(450),
        "failover, not waiting out the sleep"
    );
    assert_eq!(slow_fake.seen.lock().await.len(), 1);
}

#[tokio::test]
async fn deadline_wins_over_remaining_budget() {
    let (slow, _fake) = fake_provider(b"{}", Duration::from_secs(5)).await;
    let (_service, public) = start_lb(&[slow], |config| {
        config.proxy.attempt_timeout = Duration::from_millis(150);
        config.proxy.request_timeout = Duration::from_millis(250);
        config.proxy.max_retries = 10;
    })
    .await;

    let (status, body) = post(&public, request(5)).await;
    assert_eq!(status, 504); // Gateway Timeout
    assert_eq!(body["error"]["code"], REQUEST_TIMED_OUT);
}

#[tokio::test]
async fn oversized_response_is_terminal_not_retried() {
    let huge = vec![b'x'; 4096];
    let (big, _big_fake) = fake_provider(&huge, Duration::ZERO).await;
    let (ok, ok_fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (service, public) = start_lb(&[big, ok], |config| {
        config.proxy.max_response_size = bytesize::ByteSize::b(1024);
    })
    .await;

    let (status, body) = post(&public, request(6)).await;
    assert_eq!(status, 502); // Bad Gateway
    assert_eq!(body["error"]["code"], RESPONSE_TOO_LARGE);
    assert_eq!(
        ok_fake.seen.lock().await.len(),
        0,
        "re-downloading elsewhere helps nobody"
    );
    assert_eq!(
        service.pool.entries()[0]
            .health_streak
            .load(std::sync::atomic::Ordering::Relaxed),
        -1,
        "past the cap means a misbehaving provider, so it costs a health tick"
    );
}

#[tokio::test]
async fn oversized_request_is_refused_before_any_provider() {
    let (addr, fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |config| {
        config.proxy.max_request_size = bytesize::ByteSize::b(256);
    })
    .await;

    let big =
        json!({"jsonrpc": "2.0", "id": 1, "method": "eth_call", "params": ["x".repeat(1024)]});
    let (status, body) = post(&public, big).await;
    assert_eq!(status, 413); // Payload Too Large
    assert_eq!(body["error"]["code"], REQUEST_TOO_LARGE);
    assert_eq!(fake.seen.lock().await.len(), 0);
}

#[tokio::test]
async fn truncated_body_is_not_reported_as_too_large() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (addr, fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;
    let authority = public.strip_prefix("http://").expect("http url");

    // Promise 100 bytes, deliver 8, close the write half; the read half
    // stays open for the answer.
    let mut stream = tokio::net::TcpStream::connect(authority)
        .await
        .expect("connect");
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: lb\r\nContent-Type: application/json\r\n\
              Content-Length: 100\r\n\r\n{\"id\":1,",
        )
        .await
        .expect("send partial body");
    stream.shutdown().await.expect("close write half");

    let mut answer = String::new();
    tokio::time::timeout(GIVE_UP_AFTER, stream.read_to_string(&mut answer))
        .await
        .expect("the LB answers a truncated body instead of waiting for the rest")
        .expect("read answer");
    assert!(
        answer.starts_with("HTTP/1.1 400"), // Bad Request
        "a truncated body is a bad request, not a size verdict: {answer:?}"
    );
    assert!(
        !answer.contains(&REQUEST_TOO_LARGE.to_string()),
        "{answer:?}"
    );
    assert_eq!(
        fake.seen.lock().await.len(),
        0,
        "nothing reached a provider"
    );
}

#[tokio::test]
async fn stalled_body_read_times_out_and_fails_over() {
    // Headers and a first fragment arrive fast, then the body stops.
    // Only the attempt timeout can end this; the deadline is checked
    // between attempts, not during one.
    let (stalling, stall_hits) = raw_provider(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n",
        vec![b"{\"jsonrpc\":\"2.0\",".to_vec()],
        Duration::ZERO,
    )
    .await;
    let answer = br#"{"jsonrpc":"2.0","id":11,"result":"ok"}"#;
    let (live, live_fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[stalling, live], |config| {
        config.proxy.attempt_timeout = Duration::from_millis(150);
        config.proxy.request_timeout = Duration::from_secs(30);
    })
    .await;

    let started = std::time::Instant::now();
    let (status, body) = post(&public, request(11)).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], "ok");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(150),
        "the stall must actually have been waited out: {elapsed:?}"
    );
    assert_eq!(stall_hits.load(Ordering::Relaxed), 1);
    assert_eq!(
        live_fake.seen.lock().await.len(),
        1,
        "one failover, one request to the live provider"
    );
}

#[tokio::test]
async fn chunked_response_over_the_cap_is_refused() {
    // No content-length to check: only the running total while reading
    // can catch this one.
    let chunk = format!("400\r\n{}\r\n", "x".repeat(1024)).into_bytes();
    let (flood, flood_hits) = raw_provider(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
        vec![chunk; 8],
        Duration::from_millis(5),
    )
    .await;
    let (_service, public) = start_lb(&[flood], |config| {
        config.proxy.max_response_size = bytesize::ByteSize::b(2048);
        config.proxy.attempt_timeout = Duration::from_secs(5);
    })
    .await;

    let (status, body) = post(&public, request(12)).await;
    assert_eq!(status, 502); // Bad Gateway
    assert_eq!(body["error"]["code"], RESPONSE_TOO_LARGE);
    assert_eq!(
        flood_hits.load(Ordering::Relaxed),
        1,
        "too large is terminal: the same provider is not asked again"
    );
}

#[tokio::test]
async fn no_retries_means_one_attempt() {
    use axum::http::StatusCode;

    let (broken, broken_fake) =
        broken_provider(StatusCode::BAD_GATEWAY, b"nope", Duration::ZERO).await;
    let (_service, public) = start_lb(&[broken], |config| {
        config.proxy.max_retries = 0;
    })
    .await;

    let (status, _body) = post(&public, request(13)).await;
    assert_eq!(status, 503); // Service Unavailable
    assert_eq!(
        broken_fake.seen.lock().await.len(),
        1,
        "max_retries = 0 is one attempt, not zero and not two"
    );
}

#[tokio::test]
async fn get_is_answered_by_the_lb_not_a_provider() {
    let (addr, fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    let response = client().get(&public).send().await.expect("lb answers");
    assert_eq!(response.status(), 405); // Method Not Allowed
    assert_eq!(
        response
            .headers()
            .get("allow")
            .and_then(|v| v.to_str().ok()),
        Some("POST, OPTIONS")
    );
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    assert!(response.text().await.expect("body").contains("POST"));
    assert_eq!(
        fake.seen.lock().await.len(),
        0,
        "a browser visit must not cost a provider request"
    );
}

#[tokio::test]
async fn empty_body_is_refused_without_spending_the_budget() {
    let (addr, fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    let response = client()
        .post(&public)
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .expect("lb answers");
    assert_eq!(response.status(), 400); // Bad Request
    assert_eq!(
        fake.seen.lock().await.len(),
        0,
        "nodes answer an empty body with 400, which would burn every attempt"
    );
}

#[tokio::test]
async fn denied_method_is_refused_before_any_provider() {
    let (addr, fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    let denied = json!({"jsonrpc": "2.0", "id": 21, "method": "admin_peers", "params": []});
    let (status, body) = post(&public, denied).await;
    assert_eq!(status, 200, "a denial is an answered request");
    assert_eq!(body["error"]["code"], METHOD_DENIED);
    assert_eq!(body["id"], 21);
    let message = body["error"]["message"].as_str().expect("message");
    assert_eq!(message, "lb: method not supported: admin_", "{message}");
    assert_eq!(
        fake.seen.lock().await.len(),
        0,
        "a refused request must not reach a node"
    );
}

#[tokio::test]
async fn near_miss_methods_are_served() {
    let answer = br#"{"jsonrpc":"2.0","id":22,"result":"ok"}"#;
    let (addr, fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    for method in ["eth_sendRawTransaction", "eth_getProof", "debug_traceBlock"] {
        let call = json!({"jsonrpc": "2.0", "id": 22, "method": method, "params": []});
        let (status, body) = post(&public, call).await;
        assert_eq!(status, 200, "{method}");
        assert_eq!(body["result"], "ok", "{method} must reach a node");
    }
    assert_eq!(fake.seen.lock().await.len(), 3);
}

#[tokio::test]
async fn traffic_failures_alone_quarantine_a_provider() {
    // No Monitor exists yet: this flip comes from traffic outcomes only.
    let dead = dead_addr().await;
    let answer = br#"{"jsonrpc":"2.0","id":31,"result":"ok"}"#;
    let (live, _live_fake) = fake_provider(answer, Duration::ZERO).await;
    let (service, public) = start_lb(&[dead, live], |config| {
        config.health.flip_after = 2;
    })
    .await;

    for _ in 0..4 {
        let (status, _body) = post(&public, request(31)).await;
        assert_eq!(status, 200, "the live provider carries every request");
    }

    let entries = service.pool.entries();
    assert!(
        !entries[0].eligible(),
        "two failures in a row take the dead provider out of rotation"
    );
    assert!(entries[1].eligible(), "the answering provider stays in");
}

#[tokio::test]
async fn the_served_counter_follows_answers_not_attempts() {
    use std::sync::atomic::Ordering;

    let dead = dead_addr().await;
    let answer = br#"{"jsonrpc":"2.0","id":32,"result":"ok"}"#;
    let (live, _live_fake) = fake_provider(answer, Duration::ZERO).await;
    let (service, public) = start_lb(&[dead, live], |_| {}).await;

    for _ in 0..3 {
        let (status, _body) = post(&public, request(32)).await;
        assert_eq!(status, 200);
    }

    let entries = service.pool.entries();
    assert_eq!(
        entries[0].served.load(Ordering::Relaxed),
        0,
        "a provider that never answered bills nothing, though it was tried"
    );
    assert_eq!(entries[1].served.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn a_quarantined_provider_stops_receiving_traffic() {
    use axum::http::StatusCode;

    let (broken, broken_fake) =
        broken_provider(StatusCode::BAD_GATEWAY, b"nope", Duration::ZERO).await;
    let answer = br#"{"jsonrpc":"2.0","id":41,"result":"ok"}"#;
    let (live, live_fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[broken, live], |config| {
        config.health.flip_after = 2;
    })
    .await;

    for _ in 0..6 {
        let (status, _body) = post(&public, request(41)).await;
        assert_eq!(status, 200);
    }

    assert_eq!(
        broken_fake.seen.lock().await.len(),
        2,
        "two failures flip the provider out; after that it gets nothing"
    );
    assert_eq!(
        live_fake.seen.lock().await.len(),
        6,
        "the live provider carried every request"
    );
}

#[tokio::test]
async fn preflight_is_answered_without_a_provider() {
    let (addr, fake) = fake_provider(b"{}", Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    // What a browser sends before a cross-origin POST.
    let response = client()
        .request(reqwest::Method::OPTIONS, &public)
        .header("origin", "https://example.org")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .send()
        .await
        .expect("lb answers");
    assert!(
        response.status().is_success(),
        "preflight must succeed: {}",
        response.status()
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*"),
        "browser clients are part of the contract"
    );
    assert_eq!(fake.seen.lock().await.len(), 0);
}

#[tokio::test]
async fn a_batch_relays_untouched() {
    let answer =
        br#"[{"jsonrpc":"2.0","id":1,"result":"0x1"},{"jsonrpc":"2.0","id":2,"result":"0x2"}]"#;
    let (addr, fake) = fake_provider(answer, Duration::ZERO).await;
    let (_service, public) = start_lb(&[addr], |_| {}).await;

    let batch = json!([
        {"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []},
        {"jsonrpc": "2.0", "id": 2, "method": "eth_chainId", "params": []},
    ]);
    let (status, body) = post(&public, batch.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(body[0]["result"], "0x1");
    assert_eq!(body[1]["result"], "0x2");

    let seen = fake.seen.lock().await;
    assert_eq!(seen.len(), 1, "one batch is one provider request");
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[0].1).expect("json"),
        batch,
        "the array crosses as one body, never split or re-encoded"
    );
}
