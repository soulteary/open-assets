//! Single-instance atomic admission budgets shared by all public routers.
use crate::public_access::{BucketLimit, Policy, RateLimits};
use anyhow::{Context, Result};
use http_body_util::Full;
use hyper::{body::Bytes, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::{
    collections::HashMap,
    convert::Infallible,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Semaphore};
const CAPACITY: usize = 16_384;
const IDLE: Duration = Duration::from_secs(60);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Images,
    Oss,
}
#[derive(Debug)]
struct Bucket {
    limit: BucketLimit,
    tokens: f64,
    at: Duration,
}
impl Bucket {
    fn new(limit: BucketLimit, at: Duration) -> Self {
        Self {
            limit,
            tokens: limit.burst as f64,
            at,
        }
    }
    fn available(&mut self, now: Duration) -> bool {
        self.tokens = (self.tokens
            + now.saturating_sub(self.at).as_secs_f64() * self.limit.average as f64)
            .min(self.limit.burst as f64);
        self.at = now;
        self.tokens + 1e-9 >= 1.
    }
    fn take(&mut self) {
        self.tokens = (self.tokens - 1.).max(0.);
    }
}
struct Client {
    images: Bucket,
    oss: Bucket,
    used: Duration,
}
struct Gate {
    limits: RateLimits,
    global: Bucket,
    oss: Bucket,
    clients: HashMap<IpAddr, Client>,
    accepted: [u64; 2],
    rejected: u64,
}
impl Gate {
    fn new(limits: RateLimits) -> Self {
        Self {
            global: Bucket::new(limits.global, Duration::ZERO),
            oss: Bucket::new(limits.oss_global, Duration::ZERO),
            limits,
            clients: HashMap::new(),
            accepted: [0, 0],
            rejected: 0,
        }
    }
    fn admit(&mut self, class: Class, ip: IpAddr, now: Duration) -> bool {
        let allowed = self.try_admit(class, ip, now);
        if !allowed {
            self.rejected = self.rejected.saturating_add(1);
        }
        allowed
    }
    fn try_admit(&mut self, class: Class, ip: IpAddr, now: Duration) -> bool {
        // Check all applicable buckets before spending any token. Rejected OSS
        // requests cannot consume the image/global budget.
        if !self.global.available(now) || (class == Class::Oss && !self.oss.available(now)) {
            return false;
        }
        if !self.clients.contains_key(&ip) && self.clients.len() >= CAPACITY {
            self.clients
                .retain(|_, c| now.saturating_sub(c.used) < IDLE);
            if self.clients.len() >= CAPACITY {
                return false;
            }
        }
        let client = self.clients.entry(ip).or_insert_with(|| Client {
            images: Bucket::new(self.limits.images_per_ip, now),
            oss: Bucket::new(self.limits.oss_per_ip, now),
            used: now,
        });
        let bucket = match class {
            Class::Images => &mut client.images,
            Class::Oss => &mut client.oss,
        };
        if !bucket.available(now) {
            return false;
        }
        bucket.take();
        self.global.take();
        if class == Class::Oss {
            self.oss.take();
        }
        client.used = now;
        let index = usize::from(class == Class::Oss);
        self.accepted[index] = self.accepted[index].saturating_add(1);
        true
    }
}
fn client_ip(headers: &hyper::HeaderMap) -> Option<IpAddr> {
    let mut values = headers.get_all("x-forwarded-for").iter();
    let raw = values.next()?.to_str().ok()?;
    if values.next().is_some() || raw != raw.trim() {
        return None;
    }
    let ip = raw.parse::<IpAddr>().ok()?;
    Some(match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    })
}
fn reply(code: StatusCode, body: String) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = code;
    for (k, v) in [
        ("cache-control", "no-store"),
        ("content-type", "application/json"),
        ("x-content-type-options", "nosniff"),
    ] {
        r.headers_mut().insert(k, v.parse().unwrap());
    }
    if code == StatusCode::TOO_MANY_REQUESTS {
        r.headers_mut().insert("retry-after", "1".parse().unwrap());
    }
    r
}
async fn handle(
    req: Request<hyper::body::Incoming>,
    gate: Arc<Mutex<Gate>>,
    start: Instant,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.method() != hyper::Method::GET || req.uri().query().is_some() {
        return Ok(reply(StatusCode::BAD_REQUEST, String::new()));
    }
    // Admission RPCs have no body. Traefik forwardBody is explicitly disabled.
    if req.headers().contains_key("transfer-encoding")
        || req
            .headers()
            .get("content-length")
            .is_some_and(|v| v != "0")
    {
        return Ok(reply(StatusCode::BAD_REQUEST, String::new()));
    }
    let path = req.uri().path();
    let Ok(mut g) = gate.lock() else {
        return Ok(reply(StatusCode::SERVICE_UNAVAILABLE, String::new()));
    };
    if path == "/healthz" {
        return Ok(reply(StatusCode::OK, "{}".into()));
    }
    if path == "/status" {
        return Ok(reply(StatusCode::OK,json!({"limits":g.limits,"accepted_images":g.accepted[0],"accepted_oss":g.accepted[1],"rejected":g.rejected,"clients":g.clients.len(),"uptime_seconds":start.elapsed().as_secs()}).to_string()));
    }
    let class = match path {
        "/images" => Class::Images,
        "/oss" => Class::Oss,
        _ => return Ok(reply(StatusCode::NOT_FOUND, String::new())),
    };
    let Some(ip) = client_ip(req.headers()) else {
        return Ok(reply(StatusCode::BAD_REQUEST, String::new()));
    };
    Ok(if g.admit(class, ip, start.elapsed()) {
        reply(StatusCode::NO_CONTENT, String::new())
    } else {
        reply(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"status\":\"rate_limited\"}".into(),
        )
    })
}
pub fn serve() -> Result<()> {
    let path = std::env::var_os("PUBLIC_ACCESS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/config/public-access.json".into());
    // Fail closed when the required mounted policy is absent, unlike optional
    // Reader policy fallback during older deployments.
    let _: std::fs::Metadata =
        std::fs::metadata(&path).context("public limiter policy required")?;
    let limits = Policy::load(&path)?.effective_limits();
    limits.validate()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move { run(TcpListener::bind("0.0.0.0:8088").await?, limits).await })
}
async fn run(listener: TcpListener, limits: RateLimits) -> Result<()> {
    let gate = Arc::new(Mutex::new(Gate::new(limits)));
    let start = Instant::now();
    let slots = Arc::new(Semaphore::new(256));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let gate = gate.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.keep_alive(false).max_headers(32).max_buf_size(8192);
            let conn = builder.serve_connection(
                TokioIo::new(stream),
                service_fn(move |req| handle(req, gate.clone(), start)),
            );
            let _ = tokio::time::timeout(Duration::from_secs(2), conn).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ip(n: u16) -> IpAddr {
        format!("2001:db8::{n:x}").parse().unwrap()
    }
    #[test]
    fn independent_ip_and_class_budgets_and_continuous_refill() {
        let mut g = Gate::new(RateLimits::default());
        for _ in 0..8 {
            assert!(g.admit(Class::Oss, ip(1), Duration::ZERO));
        }
        assert!(!g.admit(Class::Oss, ip(1), Duration::ZERO));
        assert!(g.admit(Class::Images, ip(1), Duration::ZERO));
        assert!(g.admit(Class::Oss, ip(2), Duration::ZERO));
        assert!(!g.admit(Class::Oss, ip(1), Duration::from_millis(49)));
        assert!(g.admit(Class::Oss, ip(1), Duration::from_millis(50)));
    }
    #[test]
    fn true_global_budget_spans_ips_classes_and_does_not_spend_on_rejection() {
        let mut g = Gate::new(RateLimits::default());
        for n in 0..12 {
            assert!(g.admit(Class::Oss, ip(n), Duration::ZERO));
        }
        assert!(!g.admit(Class::Oss, ip(99), Duration::ZERO));
        for n in 0..88 {
            assert!(g.admit(Class::Images, ip(n), Duration::ZERO));
        }
        assert!(!g.admit(Class::Images, ip(100), Duration::ZERO));
        assert!(!g.admit(Class::Oss, ip(100), Duration::ZERO));
        assert!(g.admit(Class::Images, ip(100), Duration::from_millis(10)));
    }
    #[test]
    fn ip_rate_is_not_only_a_burst_setting() {
        let mut g = Gate::new(RateLimits::default());
        for _ in 0..100 {
            assert!(g.admit(Class::Images, ip(1), Duration::ZERO));
        }
        assert!(!g.admit(Class::Images, ip(1), Duration::from_millis(19)));
        assert!(g.admit(Class::Images, ip(1), Duration::from_millis(20)));
    }
    #[test]
    fn bounded_client_cache_fails_closed_without_resetting_active_tokens() {
        let r = RateLimits {
            global: BucketLimit {
                average: 100_000,
                burst: 100_000,
            },
            ..RateLimits::default()
        };
        let mut g = Gate::new(r);
        for n in 0..CAPACITY as u16 {
            assert!(g.admit(Class::Images, ip(n), Duration::ZERO));
        }
        assert!(!g.admit(Class::Images, ip(CAPACITY as u16), Duration::ZERO));
        assert_eq!(g.clients.len(), CAPACITY);
        assert!(g.admit(Class::Images, ip(CAPACITY as u16), IDLE));
        assert_eq!(g.clients.len(), 1);
    }
    #[test]
    fn concurrent_admission_cannot_overspend_global_tokens() {
        let gate = Arc::new(Mutex::new(Gate::new(RateLimits::default())));
        let barrier = Arc::new(std::sync::Barrier::new(32));
        let workers: Vec<_> = (0..32)
            .map(|n| {
                let gate = gate.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..10)
                        .filter(|_| {
                            gate.lock()
                                .unwrap()
                                .admit(Class::Images, ip(n), Duration::ZERO)
                        })
                        .count()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .sum::<usize>(),
            100
        );
    }
    #[test]
    fn real_http_rpc_limits_and_protocol_validation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let limits = RateLimits {
                images_per_ip: BucketLimit {
                    average: 1,
                    burst: 1,
                },
                ..RateLimits::default()
            };
            let task = tokio::spawn(run(listener, limits));
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let url = format!("http://{address}");
            assert_eq!(
                client
                    .get(format!("{url}/healthz"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
            assert_eq!(
                client
                    .get(format!("{url}/images"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                400
            );
            let r = client
                .get(format!("{url}/images"))
                .header("X-Forwarded-For", "192.0.2.1")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 204);
            let r = client
                .get(format!("{url}/images"))
                .header("X-Forwarded-For", "192.0.2.1")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 429);
            assert_eq!(r.headers()["retry-after"], "1");
            assert_eq!(r.headers()["cache-control"], "no-store");
            assert_eq!(
                client
                    .get(format!("{url}/images"))
                    .header("X-Forwarded-For", "192.0.2.2")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                204
            );
            for bad in ["192.0.2.1, 192.0.2.2", "unknown"] {
                assert_eq!(
                    client
                        .get(format!("{url}/images"))
                        .header("X-Forwarded-For", bad)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    400
                );
            }
            assert_eq!(
                client
                    .post(format!("{url}/images"))
                    .body("x")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                400
            );
            assert_eq!(
                client
                    .get(format!("{url}/images?class=oss"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                400
            );
            let status = client
                .get(format!("{url}/status"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(!status.contains("192.0.2"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&status).unwrap()["accepted_images"],
                2
            );
            task.abort();
            let _ = task.await;
        });
    }
    #[test]
    fn strict_forwarded_peer_address() {
        for raw in ["", "1.2.3.4, 5.6.7.8", " 1.2.3.4", "1.2.3.4:123", "unknown"] {
            let mut h = hyper::HeaderMap::new();
            h.insert("x-forwarded-for", raw.parse().unwrap());
            assert!(client_ip(&h).is_none());
        }
        let mut h = hyper::HeaderMap::new();
        h.insert("x-forwarded-for", "::ffff:192.0.2.1".parse().unwrap());
        assert_eq!(client_ip(&h), Some("192.0.2.1".parse().unwrap()));
        h.append("x-forwarded-for", "192.0.2.2".parse().unwrap());
        assert!(client_ip(&h).is_none());
    }
}
