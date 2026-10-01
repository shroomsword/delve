//! A tiny local HTTP server for plugin tests. It answers `GET`s from a
//! handler closure and records every request with its arrival time, so
//! tests can check what a plugin asked for and how fast.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use url::Url;

/// One request as the handler sees it.
#[derive(Debug, Clone)]
pub struct Request {
    target: String,
}

impl Request {
    /// The request target as sent: the path plus the query, such as
    /// `/api/firmware?limit=10`.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The target as a URL on a placeholder host, for reading the path and
    /// query pairs.
    ///
    /// # Panics
    ///
    /// If the client sent a target that is not a valid path.
    pub fn url(&self) -> Url {
        Url::parse(&format!("http://mock{}", self.target)).expect("a valid request target")
    }
}

/// What the handler answers with.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    /// `200 OK` with a JSON body.
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: body.into().into_bytes(),
        }
    }

    /// An empty response with the given status.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain".into(),
            body: Vec::new(),
        }
    }
}

/// A request the server received, in arrival order.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The path plus the query, as in [`Request::target`].
    pub target: String,
    /// When the request line arrived.
    pub at: Instant,
}

impl RecordedRequest {
    /// The query string, empty when the target has none.
    pub fn query(&self) -> String {
        Url::parse(&format!("http://mock{}", self.target))
            .ok()
            .and_then(|u| u.query().map(String::from))
            .unwrap_or_default()
    }
}

/// A local server bound to `127.0.0.1` on a free port. It stops when
/// dropped.
pub struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    /// Starts a server that answers every request with `handler`.
    ///
    /// # Panics
    ///
    /// If no local port can be bound.
    pub fn start(handler: impl Fn(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let (seen, stopping) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                serve(stream, &handler, &seen);
            }
        });

        Self {
            addr,
            requests,
            stop,
        }
    }

    /// The server's base URL, `http://127.0.0.1:<port>`, with no path. Join
    /// the vendor's API path onto it.
    ///
    /// # Panics
    ///
    /// Never in practice: the address is always a valid URL.
    pub fn url(&self) -> Url {
        Url::parse(&format!("http://{}", self.addr)).expect("a socket address is a valid URL")
    }

    /// Every request received so far, in arrival order.
    ///
    /// # Panics
    ///
    /// If the server thread panicked while recording.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request log lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocked accept() so the thread sees the flag and exits.
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(
    mut stream: TcpStream,
    handler: &impl Fn(&Request) -> Response,
    seen: &Mutex<Vec<RecordedRequest>>,
) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut lines = BufReader::new(reader).lines();
    let Some(Ok(request_line)) = lines.next() else {
        return;
    };
    // Read and ignore the headers; every request here is a bodyless GET.
    for line in lines.by_ref() {
        match line {
            Ok(l) if l.is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let Some(target) = request_line.split_whitespace().nth(1) else {
        return;
    };
    let request = Request {
        target: target.to_string(),
    };
    if let Ok(mut log) = seen.lock() {
        log.push(RecordedRequest {
            target: request.target.clone(),
            at: Instant::now(),
        });
    }

    let response = handler(&request);
    let head = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
}
