//! Bounded HTTP/1 event receiver; a fixed pool and absolute request deadline.
use crate::common::{load, read_regular};
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::json;
use std::path::Path;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .context("request deadline exceeded")
}
fn line(reader: &mut BufReader<TcpStream>, deadline: Instant) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        reader
            .get_ref()
            .set_read_timeout(Some(remaining(deadline)?))?;
        let available = reader.fill_buf()?;
        ensure!(!available.is_empty(), "short HTTP header");
        let n = available
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        ensure!(out.len() + n <= 8192, "HTTP header line limit");
        out.extend_from_slice(&available[..n]);
        reader.consume(n);
        if out.last() == Some(&b'\n') {
            ensure!(out.ends_with(b"\r\n"), "invalid HTTP header framing");
            out.truncate(out.len() - 2);
            return Ok(out);
        }
    }
}
fn read_request(stream: TcpStream, deadline: Instant) -> Result<(Request, TcpStream)> {
    let mut reader = BufReader::new(stream);
    let start = line(&mut reader, deadline)?;
    let parts = std::str::from_utf8(&start)?.split(' ').collect::<Vec<_>>();
    ensure!(
        parts.len() == 3 && ["HTTP/1.0", "HTTP/1.1"].contains(&parts[2]),
        "invalid HTTP request line"
    );
    let (method, path) = (parts[0].to_owned(), parts[1].to_owned());
    let mut headers = BTreeMap::new();
    let mut total = start.len();
    let mut count = 0;
    loop {
        let raw = line(&mut reader, deadline)?;
        total += raw.len() + 2;
        count += 1;
        ensure!(total <= 65536 && count <= 100, "HTTP header budget");
        if raw.is_empty() {
            break;
        }
        let text = std::str::from_utf8(&raw)?;
        let (name, value) = text.split_once(':').context("invalid HTTP header")?;
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid HTTP header name"
        );
        let name = name.to_ascii_lowercase();
        ensure!(
            headers.insert(name, value.trim().to_string()).is_none(),
            "duplicate HTTP header"
        );
    }
    let mut body = Vec::new();
    if method == "POST" {
        ensure!(
            path == "/hooks/publish-assets" && !headers.contains_key("transfer-encoding"),
            "invalid event route"
        );
        let text = headers
            .get("content-length")
            .context("missing content length")?;
        ensure!(
            !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
            "invalid content length"
        );
        let size = text.parse::<usize>()?;
        ensure!((1..=1048576).contains(&size), "invalid event size");
        body.reserve(size);
        while body.len() < size {
            reader
                .get_ref()
                .set_read_timeout(Some(remaining(deadline)?))?;
            let available = reader.fill_buf()?;
            ensure!(!available.is_empty(), "short event body");
            let n = available.len().min(size - body.len());
            body.extend_from_slice(&available[..n]);
            reader.consume(n);
        }
    }
    Ok((
        Request {
            method,
            path,
            headers,
            body,
        },
        reader.into_inner(),
    ))
}
fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        403 => "Forbidden",
        404 => "Not Found",
        501 => "Not Implemented",
        _ => "Service Unavailable",
    };
    write!(stream,"HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",body.len(),if status==503{"Retry-After: 60\r\n"}else{""})?;
    stream.write_all(body)?;
    stream.flush()
}
fn handle(mut stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let response = stream.try_clone()?;
    let request = read_request(response, Instant::now() + Duration::from_secs(15));
    let mut phase = "request";
    let mut diagnostic_config = None;
    let result = (|| -> Result<(u16, Vec<u8>)> {
        let (request, _) = request?;
        if request.method == "GET" {
            return Ok((
                if request.path == "/healthz" { 200 } else { 404 },
                Vec::new(),
            ));
        }
        if request.method != "POST" {
            return Ok((501, Vec::new()));
        }
        phase = "event";
        let signature = request
            .headers
            .get("x-gitea-signature")
            .context("missing signature")?;
        let event = request
            .headers
            .get("x-gitea-event")
            .map(String::as_str)
            .unwrap_or("");
        let delivery = request
            .headers
            .get("x-gitea-delivery")
            .map(String::as_str)
            .unwrap_or("");
        phase = "configuration";
        let secret = read_regular(Path::new(&std::env::var("WEBHOOK_SECRET_FILE")?), 65536)?;
        let c = load(None)?;
        diagnostic_config = Some(c.clone());
        phase = "event";
        let id = super::admit_event(
            &c,
            &request.body,
            signature,
            event,
            delivery,
            secret.trim_ascii(),
        )?;
        Ok((202, serde_json::to_vec(&json!({"job":id}))?))
    })();
    let (status, body) = match result {
        Ok(v) => v,
        Err(error) => {
            super::private_diagnostic(
                diagnostic_config
                    .as_ref()
                    .unwrap_or(&json!({"state_root":"/state"})),
                "webhook",
                phase,
                &error,
            );
            let code = error.to_string();
            if error.downcast_ref::<std::io::Error>().is_some()
                || code.starts_with("queue_")
                || code == "scan_entries"
            {
                (503, serde_json::to_vec(&json!({"error":"unavailable"}))?)
            } else {
                (403, serde_json::to_vec(&json!({"error":"forbidden"}))?)
            }
        }
    };
    respond(&mut stream, status, &body)?;
    Ok(())
}
pub fn serve() -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:9000").context("webhook bind failed")?;
    let (sender, receiver) = mpsc::sync_channel::<TcpStream>(8);
    let receiver = Arc::new(Mutex::new(receiver));
    for _ in 0..8 {
        let receiver = receiver.clone();
        thread::spawn(move || loop {
            let stream = match receiver.lock() {
                Ok(r) => r.recv(),
                Err(_) => return,
            };
            match stream {
                Ok(stream) => {
                    let _ = handle(stream);
                }
                Err(_) => return,
            }
        });
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match sender.try_send(stream) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(mut stream)) => {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let _ = respond(&mut stream, 503, b"{}");
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(anyhow!("webhook workers unavailable"))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_deadline_is_absolute() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let start = Instant::now();
            assert!(read_request(stream, start + Duration::from_millis(60)).is_err());
            assert!(start.elapsed() < Duration::from_secs(1));
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"POST /hooks/publish-assets HTTP/1.1\r\nContent-Length: 2\r\n\r\n{")
            .unwrap();
        server.join().unwrap();
    }
    #[test]
    fn ambiguous_or_unbounded_body_is_rejected() {
        for raw in ["POST /hooks/publish-assets HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n{}","POST /hooks/publish-assets HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n","POST /hooks/publish-assets HTTP/1.1\r\nContent-Length: 1048577\r\n\r\n"]{let listener=TcpListener::bind("127.0.0.1:0").unwrap();let address=listener.local_addr().unwrap();let server=thread::spawn(move||{let(stream,_)=listener.accept().unwrap();assert!(read_request(stream,Instant::now()+Duration::from_secs(1)).is_err());});let mut client=TcpStream::connect(address).unwrap();client.write_all(raw.as_bytes()).unwrap();server.join().unwrap();}
    }
}
