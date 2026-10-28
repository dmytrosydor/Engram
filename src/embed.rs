//! Embeddings from a local llama-server (bge-m3, Metal) over a Unix socket.
//!
//! The server runs with `--sleep-idle-seconds`, so after 10 idle minutes it unloads
//! the model and keeps only a small process. If it is not running at all (no launchd
//! agent installed), the first request starts it in the background.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

pub const DIM: usize = 1024;
pub const MODEL_FILE: &str = "bge-m3-Q8_0.gguf";
const IDLE_SECONDS: u32 = 600;
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest text sent to the model; items are ≤1500 chars and chunks ≈800 tokens.
const MAX_CHARS: usize = 6000;

/// The model server is shared by every database on the machine, so it lives in
/// `~/.engram` even when `ENGRAM_HOME` points elsewhere (e.g. in tests).
fn runtime_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".engram")
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("ENGRAM_EMBED_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("embed.sock"))
}

pub fn model_path() -> PathBuf {
    runtime_dir().join("models").join(MODEL_FILE)
}

pub fn server_bin() -> PathBuf {
    std::env::var_os("ENGRAM_LLAMA_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/bin/llama-server"))
}

/// llama-server arguments, shared by the launchd agent and the on-demand start.
pub fn server_args() -> Vec<String> {
    let fixed = [
        "--embedding",
        "--pooling",
        "cls",
        "-c",
        "8192",
        "-b",
        "2048",
        "-ub",
        "2048",
        "-np",
        "4",
        "-ngl",
        "99",
        "--no-webui",
        "--log-disable",
    ];
    let mut args = vec!["-m".into(), model_path().to_string_lossy().into_owned()];
    args.extend(fixed.iter().map(|s| s.to_string()));
    args.extend([
        "--host".into(),
        socket_path().to_string_lossy().into_owned(),
    ]);
    args.extend(["--sleep-idle-seconds".into(), IDLE_SECONDS.to_string()]);
    args
}

/// Embeds texts; vectors are L2-normalised, so dot product = cosine.
pub fn embed(texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let input: Vec<&str> = texts.iter().map(|t| truncate(t, MAX_CHARS)).collect();
    let body = json!({ "input": input }).to_string();
    let (status, resp) = match post("/v1/embeddings", &body) {
        Ok(r) => r,
        Err(_) => {
            start_server()?;
            post("/v1/embeddings", &body)?
        }
    };
    if status != 200 && resp.contains("too large") {
        // Dense text (code, ids) can exceed the batch even under MAX_CHARS: find the
        // offender by embedding one at a time, and keep its head, which is enough for search.
        if texts.len() > 1 {
            return texts.iter().map(|t| embed_one(t)).collect();
        }
        let shorter = truncate(texts[0], texts[0].chars().count().min(MAX_CHARS) / 2);
        if shorter.is_empty() {
            bail!(
                "llama-server refuses even a short input: {}",
                truncate(&resp, 300)
            );
        }
        return embed(&[shorter]);
    }
    if status != 200 {
        bail!("llama-server returned {status}: {}", truncate(&resp, 300));
    }

    #[derive(Deserialize)]
    struct Item {
        index: usize,
        embedding: Vec<f32>,
    }
    #[derive(Deserialize)]
    struct Resp {
        data: Vec<Item>,
    }
    let mut data = serde_json::from_str::<Resp>(&resp)
        .context("parse embeddings")?
        .data;
    data.sort_by_key(|d| d.index);
    if data.len() != texts.len() || data.iter().any(|d| d.embedding.len() != DIM) {
        bail!("unexpected embeddings shape from llama-server");
    }
    Ok(data.into_iter().map(|d| d.embedding).collect())
}

pub fn embed_one(text: &str) -> Result<Vec<f32>> {
    Ok(embed(&[text])?.remove(0))
}

/// float32 little-endian, the format vec0 expects.
pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn is_running() -> bool {
    matches!(request("GET", "/health", ""), Ok((200, _)))
}

/// Starts llama-server in its own process group unless another caller already did.
fn start_server() -> Result<()> {
    std::fs::create_dir_all(runtime_dir())?;
    let lock = File::create(runtime_dir().join("embed.lock"))?;
    lock.lock()?; // released when `lock` drops
    if is_running() {
        return Ok(());
    }
    if !model_path().exists() {
        bail!("model not found at {}", model_path().display());
    }
    let _ = std::fs::remove_file(socket_path()); // stale socket from a dead server
    let mut child = Command::new(server_bin())
        .args(server_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {}", server_bin().display()))?;

    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if is_running() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("llama-server exited during start ({status})");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("llama-server did not become ready in {START_TIMEOUT:?}")
}

fn post(path: &str, body: &str) -> Result<(u16, String)> {
    request("POST", path, body)
}

/// Minimal HTTP/1.1 over the Unix socket: one request per connection.
fn request(method: &str, path: &str, body: &str) -> Result<(u16, String)> {
    let mut s = UnixStream::connect(socket_path())?;
    s.set_read_timeout(Some(Duration::from_secs(120)))?;
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed HTTP response")?;
    let head = std::str::from_utf8(&raw[..split])?;
    let body = &raw[split + 4..];
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("no HTTP status")?;
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok((status, String::from_utf8(body)?))
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("bad chunk")?;
        let size_hex = std::str::from_utf8(&b[..eol])?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_hex, 16)?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(b.get(..size).context("truncated chunk")?);
        b = b.get(size + 2..).context("truncated chunk")?;
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        assert_eq!(parse_response(plain).unwrap(), (200, "ok".into()));
        let chunked =
            b"HTTP/1.1 503 X\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (503, "abcde".into()));
    }

    #[test]
    fn blob_roundtrip_and_truncate() {
        let v = vec![1.5f32, -2.0, 0.25];
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert_eq!(truncate("абвгд", 3), "абв");
        assert_eq!(truncate("аб", 3), "аб");
    }
}
