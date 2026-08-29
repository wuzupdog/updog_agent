use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SDK_NAME: &str = "updog_agent";
const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");
static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct UpdogError(String);

impl fmt::Display for UpdogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for UpdogError {}

#[derive(Debug, Clone)]
pub struct DeliveryOptions {
    pub flush_interval: Duration,
    pub max_queue_records: usize,
    pub max_queue_bytes: usize,
    pub max_record_bytes: usize,
    pub max_batch_records: usize,
    pub max_batch_bytes: usize,
}

impl Default for DeliveryOptions {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(5),
            max_queue_records: 2_048,
            max_queue_bytes: 8 * 1024 * 1024,
            max_record_bytes: 64 * 1024,
            max_batch_records: 512,
            max_batch_bytes: 512 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryStats {
    pub queued: u64,
    pub sent: u64,
    pub retried: u64,
    pub dropped: HashMap<String, u64>,
    pub queue_records: usize,
    pub queue_bytes: usize,
    pub in_flight: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
    #[serde(rename = "type")]
    pub metric_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<String>,
}

impl Metric {
    pub fn gauge(name: impl Into<String>, value: f64) -> Self {
        Self::new(name, value, "gauge")
    }

    pub fn new(name: impl Into<String>, value: f64, metric_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value,
            metric_type: metric_type.into(),
            unit: None,
            tags: HashMap::new(),
            event_id: None,
            recorded_at: None,
            service: None,
            environment: None,
            release: None,
            hostname: None,
            sdk_name: None,
            sdk_version: None,
        }
    }

    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    pub fn with_tags(mut self, tags: HashMap<String, String>) -> Self {
        self.tags.extend(tags);
        self
    }

    fn with_defaults(mut self, config: &ClientConfig) -> Self {
        self.event_id.get_or_insert_with(|| generate_id("met"));
        self.recorded_at.get_or_insert_with(now_iso8601);
        self.environment
            .get_or_insert_with(|| config.environment.clone());
        self.service = self.service.or_else(|| Some(config.service.clone()));
        self.release = self.release.or_else(|| Some(config.release.clone()));
        self.hostname.get_or_insert_with(hostname);
        self.sdk_name.get_or_insert_with(|| SDK_NAME.to_string());
        self.sdk_version
            .get_or_insert_with(|| SDK_VERSION.to_string());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    PayloadTooLarge,
    Permanent,
    RetriesExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeliveryOutcome {
    Delivered { retries: u32 },
    Failed { kind: FailureKind, retries: u32 },
}

trait Transport: Send + Sync {
    fn post_json(
        &self,
        url: &str,
        api_key: &str,
        request_id: &str,
        payload: Value,
    ) -> DeliveryOutcome;
}

struct HttpTransport {
    max_retries: u32,
    timeout: Duration,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self {
            max_retries: 3,
            timeout: Duration::from_secs(5),
        }
    }
}

impl Transport for HttpTransport {
    fn post_json(
        &self,
        url: &str,
        api_key: &str,
        request_id: &str,
        payload: Value,
    ) -> DeliveryOutcome {
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let mut retries = 0;

        loop {
            let result = agent
                .post(url)
                .set("Content-Type", "application/json")
                .set("X-API-Key", api_key)
                .set("X-Updog-Request-ID", request_id)
                .send_json(payload.clone());

            match result {
                Ok(response) if (200..300).contains(&response.status()) => {
                    return DeliveryOutcome::Delivered { retries };
                }
                Err(ureq::Error::Status(413, _)) => {
                    return DeliveryOutcome::Failed {
                        kind: FailureKind::PayloadTooLarge,
                        retries,
                    };
                }
                Err(ureq::Error::Status(status, response)) if retryable_status(status) => {
                    if retries >= self.max_retries {
                        return DeliveryOutcome::Failed {
                            kind: FailureKind::RetriesExhausted,
                            retries,
                        };
                    }

                    retries += 1;
                    thread::sleep(
                        retry_after(response.header("Retry-After"))
                            .unwrap_or_else(|| full_jitter(retries)),
                    );
                }
                Err(ureq::Error::Status(_, _)) | Ok(_) => {
                    return DeliveryOutcome::Failed {
                        kind: FailureKind::Permanent,
                        retries,
                    };
                }
                Err(ureq::Error::Transport(_)) => {
                    if retries >= self.max_retries {
                        return DeliveryOutcome::Failed {
                            kind: FailureKind::RetriesExhausted,
                            retries,
                        };
                    }

                    retries += 1;
                    thread::sleep(full_jitter(retries));
                }
            }
        }
    }
}

struct QueuedMetric {
    payload: Value,
    bytes: usize,
}

struct QueueState {
    records: VecDeque<QueuedMetric>,
    bytes: usize,
    in_flight: usize,
    flush_requested: bool,
    stopping: bool,
    stats: DeliveryStats,
}

struct ClientConfig {
    api_key: String,
    endpoint: String,
    environment: String,
    service: String,
    release: String,
    delivery: DeliveryOptions,
    transport: Arc<dyn Transport>,
}

struct Inner {
    config: Mutex<ClientConfig>,
    queue: Mutex<QueueState>,
    ready: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

pub struct UpdogClient {
    inner: Arc<Inner>,
}

impl Drop for UpdogClient {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 2 {
            let _ = self.shutdown(Duration::from_secs(5));
        }
    }
}

impl UpdogClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        let inner = Arc::new(Inner {
            config: Mutex::new(ClientConfig {
                api_key: api_key.into(),
                endpoint: "https://wuzupdog.com".to_string(),
                environment: "production".to_string(),
                service: "updog-host-agent".to_string(),
                release: SDK_VERSION.to_string(),
                delivery: DeliveryOptions::default(),
                transport: Arc::new(HttpTransport::default()),
            }),
            queue: Mutex::new(QueueState {
                records: VecDeque::new(),
                bytes: 0,
                in_flight: 0,
                flush_requested: false,
                stopping: false,
                stats: DeliveryStats::default(),
            }),
            ready: Condvar::new(),
            worker: Mutex::new(None),
        });

        let worker_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("updog-delivery".to_string())
            .spawn(move || worker_loop(worker_inner))
            .expect("failed to start Updog delivery worker");
        *inner.worker.lock().unwrap() = Some(handle);
        Self { inner }
    }

    pub fn with_endpoint(self, endpoint: impl Into<String>) -> Self {
        self.inner.config.lock().unwrap().endpoint = endpoint.into();
        self
    }

    pub fn with_environment(self, environment: impl Into<String>) -> Self {
        self.inner.config.lock().unwrap().environment = environment.into();
        self
    }

    pub fn with_service(self, service: impl Into<String>) -> Self {
        self.inner.config.lock().unwrap().service = service.into();
        self
    }

    pub fn with_release(self, release: impl Into<String>) -> Self {
        self.inner.config.lock().unwrap().release = release.into();
        self
    }

    #[cfg(test)]
    fn with_delivery_options(self, options: DeliveryOptions) -> Self {
        self.inner.config.lock().unwrap().delivery = options;
        self
    }

    #[cfg(test)]
    fn with_transport<T: Transport + 'static>(self, transport: T) -> Self {
        self.inner.config.lock().unwrap().transport = Arc::new(transport);
        self
    }

    pub fn report_metric(&self, metric: Metric) -> Result<(), UpdogError> {
        let config = self.inner.config.lock().unwrap();
        let payload = serde_json::to_value(metric.with_defaults(&config))
            .map_err(|error| UpdogError(error.to_string()))?;
        drop(config);

        let bytes = serde_json::to_vec(&payload)
            .map_err(|error| UpdogError(error.to_string()))?
            .len();
        let options = self.inner.config.lock().unwrap().delivery.clone();
        let mut state = self.inner.queue.lock().unwrap();

        if bytes > options.max_record_bytes {
            increment_drop(&mut state, "record_too_large", 1);
            return Ok(());
        }
        if state.records.len() >= options.max_queue_records
            || state.bytes + bytes > options.max_queue_bytes
        {
            increment_drop(&mut state, "queue_full", 1);
            return Ok(());
        }

        let was_empty = state.records.is_empty();
        state.records.push_back(QueuedMetric { payload, bytes });
        state.bytes += bytes;
        state.stats.queued += 1;

        if state.records.len() >= options.max_batch_records
            || state.bytes >= options.max_batch_bytes
        {
            state.flush_requested = true;
            self.inner.ready.notify_one();
        } else if was_empty {
            self.inner.ready.notify_one();
        }
        Ok(())
    }

    pub fn flush(&self, timeout: Duration) -> Result<(), UpdogError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.inner.queue.lock().unwrap();
        state.flush_requested = true;
        self.inner.ready.notify_one();

        while !state.records.is_empty() || state.in_flight > 0 {
            let now = Instant::now();
            if now >= deadline {
                return Err(UpdogError("flush timed out".to_string()));
            }
            let (next, wait) = self
                .inner
                .ready
                .wait_timeout(state, deadline - now)
                .unwrap();
            state = next;
            if wait.timed_out() && (!state.records.is_empty() || state.in_flight > 0) {
                return Err(UpdogError("flush timed out".to_string()));
            }
        }
        Ok(())
    }

    pub fn shutdown(&self, timeout: Duration) -> Result<(), UpdogError> {
        let flush_result = self.flush(timeout);
        let mut state = self.inner.queue.lock().unwrap();
        if flush_result.is_err() {
            let count = state.records.len() as u64;
            increment_drop(&mut state, "shutdown_timeout", count);
            state.records.clear();
            state.bytes = 0;
        }
        state.stopping = true;
        self.inner.ready.notify_all();
        drop(state);

        if let Some(handle) = self.inner.worker.lock().unwrap().take() {
            if flush_result.is_ok() {
                let _ = handle.join();
            } else {
                drop(handle);
            }
        }
        flush_result
    }

    pub fn delivery_stats(&self) -> DeliveryStats {
        let state = self.inner.queue.lock().unwrap();
        let mut stats = state.stats.clone();
        stats.queue_records = state.records.len();
        stats.queue_bytes = state.bytes;
        stats.in_flight = state.in_flight;
        stats
    }
}

fn worker_loop(inner: Arc<Inner>) {
    loop {
        let batch = {
            let mut state = inner.queue.lock().unwrap();
            loop {
                if state.stopping && state.records.is_empty() {
                    return;
                }
                if state.records.is_empty() {
                    state = inner.ready.wait(state).unwrap();
                    continue;
                }

                let options = inner.config.lock().unwrap().delivery.clone();
                if !state.flush_requested && !state.stopping {
                    let (next, _) = inner
                        .ready
                        .wait_timeout(state, options.flush_interval)
                        .unwrap();
                    state = next;
                    if state.records.is_empty() {
                        continue;
                    }
                }

                let batch = take_batch(&mut state, &options);
                state.in_flight = batch.len();
                state.flush_requested = !state.records.is_empty();
                break batch;
            }
        };

        let (sent, retried, drops) = deliver_batch(&inner, &batch);
        let mut state = inner.queue.lock().unwrap();
        state.stats.sent += sent as u64;
        state.stats.retried += retried as u64;
        for (reason, count) in drops {
            increment_drop(&mut state, reason, count as u64);
        }
        state.in_flight = 0;
        inner.ready.notify_all();
    }
}

fn take_batch(state: &mut QueueState, options: &DeliveryOptions) -> Vec<QueuedMetric> {
    let mut batch = Vec::new();
    let mut bytes = 14;
    while let Some(record) = state.records.front() {
        if batch.len() >= options.max_batch_records {
            break;
        }
        let separator_bytes = usize::from(!batch.is_empty());
        if !batch.is_empty() && bytes + separator_bytes + record.bytes > options.max_batch_bytes {
            break;
        }
        let record = state.records.pop_front().unwrap();
        state.bytes -= record.bytes;
        bytes += separator_bytes + record.bytes;
        batch.push(record);
    }
    batch
}

fn deliver_batch(
    inner: &Inner,
    batch: &[QueuedMetric],
) -> (usize, u32, Vec<(&'static str, usize)>) {
    let payload =
        json!({"metrics": batch.iter().map(|record| record.payload.clone()).collect::<Vec<_>>()});
    let (url, api_key, transport) = {
        let config = inner.config.lock().unwrap();
        (
            format!("{}/api/v1/metrics", config.endpoint.trim_end_matches('/')),
            config.api_key.clone(),
            Arc::clone(&config.transport),
        )
    };
    let outcome = transport.post_json(&url, &api_key, &generate_id("req"), payload);

    match outcome {
        DeliveryOutcome::Delivered { retries } => (batch.len(), retries, Vec::new()),
        DeliveryOutcome::Failed {
            kind: FailureKind::PayloadTooLarge,
            retries,
        } if batch.len() > 1 => {
            let midpoint = batch.len() / 2;
            let left = deliver_batch(inner, &batch[..midpoint]);
            let right = deliver_batch(inner, &batch[midpoint..]);
            (
                left.0 + right.0,
                retries + left.1 + right.1,
                left.2.into_iter().chain(right.2).collect(),
            )
        }
        DeliveryOutcome::Failed { kind, retries } => {
            let reason = match kind {
                FailureKind::PayloadTooLarge => "record_too_large",
                FailureKind::Permanent => "permanent_http_error",
                FailureKind::RetriesExhausted => "retries_exhausted",
            };
            (0, retries, vec![(reason, batch.len())])
        }
    }
}

fn increment_drop(state: &mut QueueState, reason: impl Into<String>, count: u64) {
    *state.stats.dropped.entry(reason.into()).or_default() += count;
}

fn retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

fn retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds.min(30)));
    }
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| {
            let seconds = (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .num_seconds()
                .clamp(0, 30) as u64;
            Duration::from_secs(seconds)
        })
}

fn full_jitter(attempt: u32) -> Duration {
    let ceiling_ms = (250_u64.saturating_mul(1_u64 << attempt.saturating_sub(1))).min(30_000);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    Duration::from_millis(nanos % (ceiling_ms + 1))
}

fn generate_id(prefix: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{timestamp:x}{counter:x}")
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default()
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    }

    impl Transport for FakeTransport {
        fn post_json(
            &self,
            url: &str,
            api_key: &str,
            _request_id: &str,
            payload: Value,
        ) -> DeliveryOutcome {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_string(), api_key.to_string(), payload));
            DeliveryOutcome::Delivered { retries: 0 }
        }
    }

    #[test]
    fn bulk_sends_metric_payload_with_resource_defaults() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let client = UpdogClient::new("test-key")
            .with_endpoint("https://example.test")
            .with_environment("test")
            .with_transport(FakeTransport {
                calls: Arc::clone(&calls),
            });

        client
            .report_metric(Metric::gauge("host.cpu.utilization", 42.0))
            .unwrap();
        client.flush(Duration::from_secs(1)).unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "https://example.test/api/v1/metrics");
        assert_eq!(calls[0].1, "test-key");
        assert_eq!(calls[0].2["metrics"][0]["name"], "host.cpu.utilization");
        assert_eq!(calls[0].2["metrics"][0]["environment"], "test");
        assert_eq!(calls[0].2["metrics"][0]["sdk_name"], SDK_NAME);
    }

    #[test]
    fn drops_an_oversized_metric_without_blocking() {
        let client = UpdogClient::new("test-key").with_delivery_options(DeliveryOptions {
            max_record_bytes: 1,
            ..DeliveryOptions::default()
        });
        client
            .report_metric(Metric::gauge("host.cpu.utilization", 42.0))
            .unwrap();

        let stats = client.delivery_stats();
        assert_eq!(stats.queue_records, 0);
        assert_eq!(stats.dropped["record_too_large"], 1);
    }
}
