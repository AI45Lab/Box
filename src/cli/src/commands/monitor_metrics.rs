//! Prometheus metrics + health endpoint for the `a3s-box monitor` daemon.
//!
//! The monitor is the one always-on process in the daemonless design, and it
//! already polls every box's state — so it is the natural place to expose
//! operator-scrapable observability. When `monitor --metrics-addr <ADDR>` is
//! set it serves:
//!
//! - `GET /healthz` → `200 ok` (liveness probe)
//! - `GET /metrics` → Prometheus text of box-state metrics
//!
//! The endpoint is **off by default** and intended to bind loopback (e.g.
//! `127.0.0.1:9100`); there is no auth, so do not expose it publicly.

use crate::state::{BoxRecord, StateFile};

/// The minimal per-box view the metrics need — decouples the renderer from the
/// large `BoxRecord` so it is trivially constructable in tests.
struct BoxMetric<'a> {
    status: &'a str,
    restart_count: u32,
    health_status: &'a str,
}

/// Render box-state metrics in Prometheus text exposition format.
pub fn render_box_metrics(records: &[BoxRecord]) -> String {
    render_from(records.iter().map(|r| BoxMetric {
        status: &r.status,
        restart_count: r.restart_count,
        health_status: &r.health_status,
    }))
}

fn render_from<'a>(items: impl Iterator<Item = BoxMetric<'a>>) -> String {
    let (mut total, mut running, mut paused, mut dead, mut created, mut other) =
        (0u64, 0, 0, 0, 0, 0);
    let (mut restarts_total, mut healthy, mut unhealthy) = (0u64, 0u64, 0u64);

    for m in items {
        total += 1;
        match m.status {
            "running" => running += 1,
            "paused" => paused += 1,
            "dead" => dead += 1,
            "created" => created += 1,
            _ => other += 1,
        }
        restarts_total += u64::from(m.restart_count);
        match m.health_status {
            "healthy" => healthy += 1,
            "unhealthy" => unhealthy += 1,
            _ => {}
        }
    }

    let mut out = String::new();
    out.push_str("# HELP a3s_box_total Total boxes tracked in state.\n");
    out.push_str("# TYPE a3s_box_total gauge\n");
    out.push_str(&format!("a3s_box_total {total}\n"));

    out.push_str("# HELP a3s_box_state Boxes by lifecycle status.\n");
    out.push_str("# TYPE a3s_box_state gauge\n");
    out.push_str(&format!("a3s_box_state{{status=\"running\"}} {running}\n"));
    out.push_str(&format!("a3s_box_state{{status=\"paused\"}} {paused}\n"));
    out.push_str(&format!("a3s_box_state{{status=\"dead\"}} {dead}\n"));
    out.push_str(&format!("a3s_box_state{{status=\"created\"}} {created}\n"));
    out.push_str(&format!("a3s_box_state{{status=\"other\"}} {other}\n"));

    out.push_str("# HELP a3s_box_restarts_total Sum of per-box restart counts.\n");
    out.push_str("# TYPE a3s_box_restarts_total counter\n");
    out.push_str(&format!("a3s_box_restarts_total {restarts_total}\n"));

    out.push_str("# HELP a3s_box_health Boxes by health-check status.\n");
    out.push_str("# TYPE a3s_box_health gauge\n");
    out.push_str(&format!("a3s_box_health{{status=\"healthy\"}} {healthy}\n"));
    out.push_str(&format!(
        "a3s_box_health{{status=\"unhealthy\"}} {unhealthy}\n"
    ));

    out
}

/// Build a minimal HTTP/1.1 response (no keep-alive).
fn http_response(status_line: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status_line}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    )
}

/// Extract the request target (path) from an HTTP request's first line.
/// Returns `None` if it is not a `GET`.
fn parse_get_path(request: &str) -> Option<&str> {
    let first = request.lines().next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" {
        return None;
    }
    Some(target.split('?').next().unwrap_or(target))
}

/// Current Unix time in seconds (0 on a pre-epoch clock).
pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Render the monitor's own liveness gauges from the shared last-poll clock.
fn render_monitor_liveness(last_poll_secs: u64, stale_after_secs: u64) -> String {
    let age = now_secs().saturating_sub(last_poll_secs);
    let up = u8::from(age <= stale_after_secs);
    format!(
        "# HELP a3s_box_monitor_up 1 if the monitor poll loop completed a poll within the staleness threshold.\n\
         # TYPE a3s_box_monitor_up gauge\n\
         a3s_box_monitor_up {up}\n\
         # HELP a3s_box_monitor_seconds_since_last_poll Seconds since the monitor last completed a poll iteration.\n\
         # TYPE a3s_box_monitor_seconds_since_last_poll gauge\n\
         a3s_box_monitor_seconds_since_last_poll {age}\n"
    )
}

/// Serve `/metrics` and `/healthz` on `addr` until the process exits.
///
/// `last_poll` is the shared Unix-seconds timestamp the monitor's poll loop
/// updates after every iteration; `/healthz` reports **503** (not a lying 200)
/// if the loop has not polled within `stale_after_secs` — i.e. it actually
/// reflects whether the supervision loop is alive, not just that the HTTP task
/// is. Each `/metrics` scrape reads a fresh **read-only** state snapshot.
pub async fn serve(
    addr: String,
    last_poll: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stale_after_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(&addr).await?;
    println!("a3s-box monitor: metrics/health on http://{addr}/metrics");

    loop {
        let mut sock = match listener.accept().await {
            Ok((sock, _peer)) => sock,
            Err(e) => {
                eprintln!("monitor metrics: accept error: {e}");
                continue;
            }
        };
        let last_poll = std::sync::Arc::clone(&last_poll);
        tokio::spawn(async move {
            // Read the request head (bounded — we only need the first line).
            let mut buf = [0u8; 2048];
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let response = match parse_get_path(&request) {
                Some("/healthz") => {
                    let age = now_secs().saturating_sub(last_poll.load(Ordering::Relaxed));
                    if age <= stale_after_secs {
                        http_response("200 OK", "text/plain", "ok\n")
                    } else {
                        http_response(
                            "503 Service Unavailable",
                            "text/plain",
                            &format!(
                                "stale: monitor poll loop has not run for {age}s (threshold {stale_after_secs}s)\n"
                            ),
                        )
                    }
                }
                Some("/metrics") => {
                    let mut body = StateFile::load_readonly()
                        .map(|s| render_box_metrics(s.records()))
                        .unwrap_or_else(|e| format!("# state load error: {e}\n"));
                    body.push_str(&render_monitor_liveness(
                        last_poll.load(Ordering::Relaxed),
                        stale_after_secs,
                    ));
                    http_response("200 OK", "text/plain; version=0.0.4", &body)
                }
                Some(_) => http_response("404 Not Found", "text/plain", "not found\n"),
                None => http_response(
                    "405 Method Not Allowed",
                    "text/plain",
                    "method not allowed\n",
                ),
            };
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m<'a>(status: &'a str, restart_count: u32, health: &'a str) -> BoxMetric<'a> {
        BoxMetric {
            status,
            restart_count,
            health_status: health,
        }
    }

    #[test]
    fn renders_counts_and_restarts() {
        let items = vec![
            m("running", 2, "healthy"),
            m("running", 1, "unhealthy"),
            m("dead", 5, "none"),
            m("paused", 0, "none"),
        ];
        let out = render_from(items.into_iter());
        assert!(out.contains("a3s_box_total 4"));
        assert!(out.contains("a3s_box_state{status=\"running\"} 2"));
        assert!(out.contains("a3s_box_state{status=\"dead\"} 1"));
        assert!(out.contains("a3s_box_state{status=\"paused\"} 1"));
        assert!(out.contains("a3s_box_restarts_total 8"));
        assert!(out.contains("a3s_box_health{status=\"healthy\"} 1"));
        assert!(out.contains("a3s_box_health{status=\"unhealthy\"} 1"));
    }

    #[test]
    fn empty_state_is_valid_exposition() {
        let out = render_from(std::iter::empty());
        assert!(out.contains("a3s_box_total 0"));
        assert!(out.contains("# TYPE a3s_box_state gauge"));
    }

    #[test]
    fn parse_get_path_handles_method_and_query() {
        assert_eq!(
            parse_get_path("GET /metrics HTTP/1.1\r\n"),
            Some("/metrics")
        );
        assert_eq!(
            parse_get_path("GET /healthz?x=1 HTTP/1.1\r\n"),
            Some("/healthz")
        );
        assert_eq!(parse_get_path("POST /metrics HTTP/1.1\r\n"), None);
        assert_eq!(parse_get_path(""), None);
    }

    #[test]
    fn http_response_has_content_length() {
        let r = http_response("200 OK", "text/plain", "ok\n");
        assert!(r.contains("Content-Length: 3"));
        assert!(r.contains("Connection: close"));
        assert!(r.ends_with("ok\n"));
    }

    #[test]
    fn monitor_liveness_up_when_fresh_down_when_stale() {
        let now = now_secs();
        let fresh = render_monitor_liveness(now, 60);
        assert!(fresh.contains("a3s_box_monitor_up 1"));
        assert!(fresh.contains("a3s_box_monitor_seconds_since_last_poll 0"));

        // A poll 1000s ago with a 60s threshold is stale → down.
        let stale = render_monitor_liveness(now.saturating_sub(1000), 60);
        assert!(stale.contains("a3s_box_monitor_up 0"));
    }
}
