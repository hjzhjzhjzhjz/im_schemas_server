//! 轻量请求指标：纯原子计数，无第三方依赖，输出 Prometheus 文本格式供云监控抓取。
//! 只做聚合计数（不按 URL 打标签），避免动态路径带来的高基数问题。

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

pub struct Metrics {
    total: AtomicU64,
    s2xx: AtomicU64,
    s4xx: AtomicU64,
    s5xx: AtomicU64,
    duration_us: AtomicU64,
    inflight: AtomicI64,
}

impl Metrics {
    pub fn new() -> Self {
        Metrics {
            total: AtomicU64::new(0),
            s2xx: AtomicU64::new(0),
            s4xx: AtomicU64::new(0),
            s5xx: AtomicU64::new(0),
            duration_us: AtomicU64::new(0),
            inflight: AtomicI64::new(0),
        }
    }

    #[inline]
    pub fn begin(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn end(&self, status: u16, dur: Duration) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        self.duration_us
            .fetch_add(dur.as_micros() as u64, Ordering::Relaxed);
        match status {
            200..=299 => self.s2xx.fetch_add(1, Ordering::Relaxed),
            400..=499 => self.s4xx.fetch_add(1, Ordering::Relaxed),
            _ => self.s5xx.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Prometheus 0.0.4 文本格式。
    pub fn render(&self) -> String {
        let total = self.total.load(Ordering::Relaxed);
        let s2 = self.s2xx.load(Ordering::Relaxed);
        let s4 = self.s4xx.load(Ordering::Relaxed);
        let s5 = self.s5xx.load(Ordering::Relaxed);
        let secs = self.duration_us.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let inflight = self.inflight.load(Ordering::Relaxed);
        format!(
            "# HELP im_http_requests_total Total HTTP requests received.\n\
             # TYPE im_http_requests_total counter\n\
             im_http_requests_total {total}\n\
             # HELP im_http_requests_by_status Requests by status class.\n\
             # TYPE im_http_requests_by_status counter\n\
             im_http_requests_by_status{{class=\"2xx\"}} {s2}\n\
             im_http_requests_by_status{{class=\"4xx\"}} {s4}\n\
             im_http_requests_by_status{{class=\"5xx\"}} {s5}\n\
             # HELP im_http_request_duration_seconds_sum Cumulative request duration.\n\
             # TYPE im_http_request_duration_seconds_sum counter\n\
             im_http_request_duration_seconds_sum {secs:.6}\n\
             # HELP im_http_inflight_requests Requests currently in flight.\n\
             # TYPE im_http_inflight_requests gauge\n\
             im_http_inflight_requests {inflight}\n"
        )
    }
}
