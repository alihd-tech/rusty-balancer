use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::{Duration, Instant},
};

const BOUNDS: [f64; 8] = [0.005, 0.01, 0.025, 0.1, 0.25, 1.0, 5.0, 30.0];
pub struct Metrics {
    started: Instant,
    requests: AtomicU64,
    canceled: AtomicU64,
    classes: [AtomicU64; 5],
    buckets: [AtomicU64; 8],
    micros: AtomicU64,
    completed: AtomicU64,
}
impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            requests: AtomicU64::new(0),
            canceled: AtomicU64::new(0),
            classes: std::array::from_fn(|_| AtomicU64::new(0)),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            micros: AtomicU64::new(0),
            completed: AtomicU64::new(0),
        }
    }
}
pub struct Observation<'a> {
    metrics: &'a Metrics,
    start: Instant,
    done: bool,
}
impl Metrics {
    pub fn begin(&self) -> Observation<'_> {
        self.requests.fetch_add(1, Relaxed);
        Observation {
            metrics: self,
            start: Instant::now(),
            done: false,
        }
    }
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);
        let _ = writeln!(
            out,
            "# TYPE rusty_balancer_requests_total counter\nrusty_balancer_requests_total {}",
            self.requests.load(Relaxed)
        );
        let _ = writeln!(out, "# TYPE rusty_balancer_request_cancellations_total counter\nrusty_balancer_request_cancellations_total {}", self.canceled.load(Relaxed));
        let _ = writeln!(out, "# TYPE rusty_balancer_responses_total counter");
        for (i, value) in self.classes.iter().enumerate() {
            let _ = writeln!(
                out,
                "rusty_balancer_responses_total{{class=\"{}xx\"}} {}",
                i + 1,
                value.load(Relaxed)
            );
        }
        let metric = "rusty_balancer_response_header_seconds";
        let _ = writeln!(out, "# TYPE {metric} histogram");
        // Read individual atomic counters; like most lock-free telemetry, scrape is approximate.
        let mut cumulative = 0u64;
        for (i, bound) in BOUNDS.iter().enumerate() {
            cumulative = cumulative.saturating_add(self.buckets[i].load(Relaxed));
            let _ = writeln!(out, "{metric}_bucket{{le=\"{bound}\"}} {cumulative}");
        }
        let completed = self.completed.load(Relaxed).max(cumulative);
        let _ = writeln!(out, "{metric}_bucket{{le=\"+Inf\"}} {completed}\n{metric}_count {completed}\n{metric}_sum {}", self.micros.load(Relaxed) as f64 / 1_000_000.0);
        let _ = writeln!(
            out,
            "# TYPE rusty_balancer_uptime_seconds gauge\nrusty_balancer_uptime_seconds {}",
            self.started.elapsed().as_secs()
        );
        out
    }
}
impl Observation<'_> {
    pub fn finish(mut self, status: u16) -> Duration {
        let elapsed = self.start.elapsed();
        if (100..600).contains(&status) {
            self.metrics.classes[(status / 100 - 1) as usize].fetch_add(1, Relaxed);
        }
        if let Some(index) = BOUNDS.iter().position(|b| elapsed.as_secs_f64() <= *b) {
            self.metrics.buckets[index].fetch_add(1, Relaxed);
        }
        self.metrics
            .micros
            .fetch_add(elapsed.as_micros().min(u64::MAX as u128) as u64, Relaxed);
        self.metrics.completed.fetch_add(1, Relaxed);
        self.done = true;
        elapsed
    }
}
impl Drop for Observation<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.metrics.canceled.fetch_add(1, Relaxed);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_requests_and_cancellations() {
        let m = Metrics::default();
        m.begin().finish(200);
        drop(m.begin());
        let text = m.render();
        assert!(text.contains("rusty_balancer_requests_total 2"));
        assert!(text.contains("rusty_balancer_request_cancellations_total 1"));
        assert!(text.contains("rusty_balancer_responses_total{class=\"2xx\"} 1"));
    }
}
