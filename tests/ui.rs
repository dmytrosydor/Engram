use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn request(port: u16, raw: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn ui_serves_local_pages_and_refuses_cross_site_requests() {
    let dir = TempDir::new().unwrap();
    let port = 20000 + (std::process::id() % 20000) as u16;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .args(["ui", "--port", &port.to_string()])
            .env("ENGRAM_HOME", dir.path())
            .env("ENGRAM_EMBED_SOCK", "/nonexistent/engram-test.sock")
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "ui did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let host = format!("127.0.0.1:{port}");

    for path in [
        "/",
        "/queue",
        "/feed",
        "/map",
        "/recall",
        "/health",
        "/assets/htmx.min.js",
        "/assets/cytoscape.min.js",
    ] {
        let resp = request(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        );
        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "{path}: {}",
            &resp[..resp.len().min(200)]
        );
        assert!(!resp.contains("class=err"), "{path} rendered an error");
        assert!(!resp.contains("unpkg.com"), "{path} loads third-party code");
    }

    let rebinding = request(
        port,
        "GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    );
    assert!(rebinding.starts_with("HTTP/1.1 403"));

    let form = "POST /review/1/reject HTTP/1.1\r\nHost: HOST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let cross_site = request(port, &form.replace("HOST", &host));
    assert!(
        cross_site.starts_with("HTTP/1.1 403"),
        "POST without HX-Request must be refused"
    );
    let htmx = request(
        port,
        &form
            .replace("HOST", &host)
            .replace("Content-Length", "HX-Request: true\r\nContent-Length"),
    );
    assert!(htmx.starts_with("HTTP/1.1 200") && htmx.contains("no review #1"));
}
