use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

#[derive(Debug, Default)]
pub struct RequestTelemetry {
    requests: AtomicU64,
    retries: AtomicU64,
    rate_limits: AtomicU64,
    timeouts: AtomicU64,
    connect_timeouts: AtomicU64,
    errors: AtomicU64,
    total_ms: AtomicU64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RequestTelemetrySnapshot {
    pub requests: u64,
    pub retries: u64,
    pub rate_limits: u64,
    pub timeouts: u64,
    pub connect_timeouts: u64,
    pub errors: u64,
    pub total_ms: u64,
}

impl RequestTelemetry {
    pub fn record_success(&self, elapsed_ms: u64) {
        self.record_request(elapsed_ms);
    }

    pub fn record_rate_limit(&self, elapsed_ms: u64) {
        self.record_request(elapsed_ms);
        self.rate_limits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_timeout(&self, elapsed_ms: u64) {
        self.record_request(elapsed_ms);
        self.timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_connect_timeout(&self, elapsed_ms: u64) {
        self.record_request(elapsed_ms);
        self.timeouts.fetch_add(1, Ordering::Relaxed);
        self.connect_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_error(&self, elapsed_ms: u64) {
        self.record_request(elapsed_ms);
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RequestTelemetrySnapshot {
        RequestTelemetrySnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            rate_limits: self.rate_limits.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            connect_timeouts: self.connect_timeouts.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            total_ms: self.total_ms.load(Ordering::Relaxed),
        }
    }

    fn record_request(&self, elapsed_ms: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.total_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_telemetry_reports_real_attempt_outcomes_and_elapsed_time() {
        let telemetry = RequestTelemetry::default();
        telemetry.record_success(125);
        telemetry.record_rate_limit(40);
        telemetry.record_timeout(25);
        telemetry.record_connect_timeout(15);
        telemetry.record_error(10);
        telemetry.record_retry();
        telemetry.record_retry();

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.requests, 5);
        assert_eq!(snapshot.retries, 2);
        assert_eq!(snapshot.rate_limits, 1);
        assert_eq!(snapshot.timeouts, 2);
        assert_eq!(snapshot.connect_timeouts, 1);
        assert_eq!(snapshot.errors, 1);
        assert_eq!(snapshot.total_ms, 215);
    }
}
