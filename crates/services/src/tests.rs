#![expect(
    clippy::disallowed_methods,
    reason = "the tests connect to ports that they or the code under test listen on"
)]

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};

use super::{ArgumentError, Kind, Listener, ServeError, parse, serve};

fn words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

#[test]
fn parse_reads_one_listener_for_each_kind_and_port() {
    assert_eq!(
        parse(&words(&["dynamodb=41001"])),
        Ok(vec![Listener {
            kind: Kind::Dynamodb,
            port: 41_001,
        }])
    );
}

#[test]
fn parse_refuses_no_words_an_unknown_kind_a_bad_port_and_a_repeated_kind() {
    assert_eq!(parse(&[]), Err(ArgumentError::Empty));
    for word in ["s4=41001", "dynamodb", "dynamodb=0", "dynamodb=70000", "=1"] {
        assert_eq!(
            parse(&words(&[word])),
            Err(ArgumentError::Malformed {
                word: word.to_owned()
            }),
            "{word}"
        );
    }
    assert_eq!(
        parse(&words(&["dynamodb=1", "dynamodb=2"])),
        Err(ArgumentError::Repeated {
            kind: Kind::Dynamodb
        })
    );
}

/// Sends one DynamoDB request of `target` with `body` to `address`, with
/// the headers an SDK sends and no real signature, and returns the status
/// line and the body.
fn dynamodb(address: SocketAddr, target: &str, body: &str) -> (String, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/x-amz-json-1.0\r\nX-Amz-Target: DynamoDB_20120810.{target}\r\nX-Amz-Date: 20260101T000000Z\r\nAuthorization: AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/dynamodb/aws4_request, SignedHeaders=host, Signature=0\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (head.lines().next().unwrap().to_owned(), body.to_owned())
}

#[test]
fn serve_answers_dynamodb_in_memory_once_ready_and_returns_when_stopped() {
    let (addresses, address) = std::sync::mpsc::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = std::thread::spawn(move || {
        serve(
            &[Listener {
                kind: Kind::Dynamodb,
                port: 0,
            }],
            |bound| addresses.send(bound.to_vec()).unwrap(),
            || {
                Ok(async {
                    drop(stopped.await);
                })
            },
        )
    });
    let bound = address.recv().unwrap();
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].kind, Kind::Dynamodb);
    assert!(bound[0].address.ip().is_loopback());

    let (status, body) = dynamodb(
        bound[0].address,
        "CreateTable",
        r#"{"TableName":"items","KeySchema":[{"AttributeName":"pk","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"pk","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"}"#,
    );
    assert!(status.contains(" 200 "), "{status}: {body}");
    let (status, body) = dynamodb(bound[0].address, "ListTables", "{}");
    assert!(status.contains(" 200 "), "{status}: {body}");
    assert!(body.contains(r#""items""#), "{body}");

    stop.send(()).unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn serve_fails_before_ready_when_a_port_is_taken() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut readied = false;
    let result = serve(
        &[Listener {
            kind: Kind::Dynamodb,
            port,
        }],
        |_| readied = true,
        || Ok(std::future::pending()),
    );
    assert!(
        matches!(result, Err(ServeError::Bind { kind: Kind::Dynamodb, port: failed, .. }) if failed == port),
        "{result:?}"
    );
    assert!(!readied);
}
