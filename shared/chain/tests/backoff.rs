//! A rate-limited API: one request per 429, then a pause for every request to that host, as long as it asks.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use liq2sol_chain::esplora::{backoff, Esplora};

/// Answers the first `limited` requests with 429 and `Retry-After: 2`, then the height 100.
fn server(limited: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let reply = if n < limited {
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n100".to_string()
            };
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (url, count)
}

#[test]
fn a_429_is_asked_once_and_then_waited_out() {
    let (url, count) = server(1);
    let api = Esplora::Http(url);
    let err = api.tip().unwrap_err();
    assert!(err.contains("429"), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 1, "no quick retries into a rate limit");
    let start = Instant::now();
    assert_eq!(api.tip(), Ok(100));
    assert!(start.elapsed() >= Duration::from_millis(1900), "the next request waited for Retry-After");
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn the_pause_follows_retry_after_or_doubles_within_bounds() {
    assert_eq!(backoff(Some("7"), 3), Duration::from_secs(7));
    assert_eq!(backoff(Some(" 0 "), 0), Duration::from_secs(1), "never zero");
    assert_eq!(backoff(Some("99999"), 0), Duration::from_secs(300), "never longer than 5 minutes");
    assert_eq!(backoff(Some("Wed, 21 Oct 2026 07:28:00 GMT"), 0), Duration::from_secs(15), "a date reads as no hint");
    assert_eq!(backoff(None, 0), Duration::from_secs(15));
    assert_eq!(backoff(None, 1), Duration::from_secs(30));
    assert_eq!(backoff(None, 4), Duration::from_secs(240));
    assert_eq!(backoff(None, 9), Duration::from_secs(300));
}
