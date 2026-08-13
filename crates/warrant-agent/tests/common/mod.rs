//! A scriptable HTTP server, shared by the transport tests.
//!
//! Unit tests can check that a body serialises correctly. Only a socket can
//! check that the headers arrive, that a 429 is retried and a 400 is not, and
//! that an error body is read rather than lost to the status code.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

struct Recorder {
    requests: Mutex<Vec<Value>>,
    headers: Mutex<Vec<Vec<(String, String)>>>,
    calls: AtomicUsize,
}

/// A server that answers every POST from a script of `(status, body)`.
pub struct FakeApi {
    base_url: String,
    recorder: Arc<Recorder>,
    shutdown: Option<std::thread::JoinHandle<()>>,
    server: Arc<tiny_http::Server>,
}

impl FakeApi {
    /// Start on a free port.
    pub fn start(script: Vec<(u16, Value)>) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("a free port"));
        let base_url = format!("http://{}", server.server_addr());
        let recorder = Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            headers: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });

        let worker_server = Arc::clone(&server);
        let worker_recorder = Arc::clone(&recorder);
        let handle = std::thread::spawn(move || {
            for mut request in worker_server.incoming_requests() {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(&mut request.as_reader(), &mut body);

                worker_recorder
                    .requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                worker_recorder.headers.lock().unwrap().push(
                    request
                        .headers()
                        .iter()
                        .map(|h| {
                            (
                                h.field.as_str().as_str().to_ascii_lowercase(),
                                h.value.as_str().to_owned(),
                            )
                        })
                        .collect(),
                );

                let index = worker_recorder.calls.fetch_add(1, Ordering::SeqCst);
                let (status, payload) = script.get(index).cloned().unwrap_or((
                    500,
                    json!({ "error": { "type": "script_exhausted", "message": "no more" } }),
                ));

                let encoded = serde_json::to_vec(&payload).unwrap();
                let response =
                    tiny_http::Response::from_data(encoded).with_status_code(status).with_header(
                        tiny_http::Header::from_bytes(
                            &b"content-type"[..],
                            &b"application/json"[..],
                        )
                        .unwrap(),
                    );
                let _ = request.respond(response);
            }
        });

        FakeApi { base_url, recorder, shutdown: Some(handle), server }
    }

    /// Where the server is listening.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Every request body received, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.recorder.requests.lock().unwrap().clone()
    }

    /// The headers of one request, lowercased.
    pub fn headers(&self, index: usize) -> Vec<(String, String)> {
        self.recorder.headers.lock().unwrap()[index].clone()
    }

    /// One header value, or the empty string.
    pub fn header(&self, index: usize, name: &str) -> String {
        self.headers(index)
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
            .unwrap_or_default()
    }

    /// How many requests arrived.
    pub fn call_count(&self) -> usize {
        self.recorder.calls.load(Ordering::SeqCst)
    }
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(handle) = self.shutdown.take() {
            let _ = handle.join();
        }
    }
}
