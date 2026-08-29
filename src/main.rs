use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::net::UdpSocket;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod delivery;

use delivery::{Metric, UpdogClient};

#[cfg(not(target_os = "linux"))]
compile_error!("updog-agent is supported only on Linux");

const DEFAULT_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_STATSD_BIND: &str = "127.0.0.1:8125";
const PAGE_BYTES: u64 = 4096;
static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
}

extern "C" fn stop_agent(_signal: i32) {
    STOPPING.store(true, Ordering::SeqCst);
}

fn main() -> Result<(), Box<dyn Error>> {
    let api_key =
        env::var("UPDOG_API_KEY").map_err(|_| "UPDOG_API_KEY is required for the host agent")?;
    if api_key.trim().is_empty() {
        return Err("UPDOG_API_KEY cannot be empty".into());
    }
    let endpoint = env::var("UPDOG_ENDPOINT").unwrap_or_else(|_| "https://wuzupdog.com".into());
    let environment = env::var("UPDOG_ENVIRONMENT").unwrap_or_else(|_| "production".into());
    let service = env::var("UPDOG_SERVICE").unwrap_or_else(|_| "updog-host-agent".into());
    let interval = Duration::from_secs(
        env::var("UPDOG_SAMPLE_INTERVAL_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(DEFAULT_INTERVAL_SECONDS),
    );
    let process_filters = env::var("UPDOG_PROCESS_MATCH")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let statsd_bind = env::var("UPDOG_STATSD_BIND").unwrap_or_else(|_| DEFAULT_STATSD_BIND.into());

    let client = Arc::new(
        UpdogClient::new(api_key)
            .with_endpoint(endpoint)
            .with_environment(environment)
            .with_service(service)
            .with_release(env!("CARGO_PKG_VERSION")),
    );
    // The host agent is Linux-only. Register SIGINT and SIGTERM without pulling
    // an additional runtime dependency into the small installation binary.
    unsafe {
        signal(2, stop_agent);
        signal(15, stop_agent);
    }

    let statsd_handle = start_statsd_listener(statsd_bind.clone(), Arc::clone(&client))?;

    eprintln!(
        "[updog-agent] started; interval={}s statsd={statsd_bind}",
        interval.as_secs()
    );

    let mut previous = HostSnapshot::capture(&process_filters)?;
    while !STOPPING.load(Ordering::SeqCst) {
        sleep_until_stopped(interval);
        if STOPPING.load(Ordering::SeqCst) {
            break;
        }

        match HostSnapshot::capture(&process_filters) {
            Ok(current) => {
                report_snapshot(&client, &previous, &current);
                previous = current;
            }
            Err(error) => eprintln!("[updog-agent] host sample failed: {error}"),
        }
    }

    let _ = statsd_handle.join();
    client.flush(Duration::from_secs(10))?;
    client.shutdown(Duration::from_secs(10))?;
    let stats = client.delivery_stats();
    let dropped = stats.dropped.values().sum::<u64>();
    eprintln!(
        "[updog-agent] stopped; queued={} sent={} retried={} dropped={dropped}",
        stats.queued, stats.sent, stats.retried
    );
    Ok(())
}

fn sleep_until_stopped(duration: Duration) {
    let deadline = Instant::now() + duration;
    while !STOPPING.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn start_statsd_listener(
    bind: String,
    client: Arc<UpdogClient>,
) -> Result<thread::JoinHandle<()>, Box<dyn Error>> {
    let socket = UdpSocket::bind(&bind)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    Ok(thread::Builder::new()
        .name("updog-statsd".into())
        .spawn(move || {
            let mut buffer = [0_u8; 65_507];
            while !STOPPING.load(Ordering::SeqCst) {
                match socket.recv_from(&mut buffer) {
                    Ok((length, _)) => {
                        let payload = String::from_utf8_lossy(&buffer[..length]);
                        for line in payload.lines() {
                            if let Some(metric) = parse_statsd(line) {
                                let _ = client.report_metric(metric);
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => eprintln!("[updog-agent] StatsD receive failed: {error}"),
                }
            }
        })?)
}

fn parse_statsd(line: &str) -> Option<Metric> {
    let (name, rest) = line.trim().split_once(':')?;
    if name.is_empty() || name.len() > 200 {
        return None;
    }

    let mut parts = rest.split('|');
    let mut value = parts.next()?.parse::<f64>().ok()?;
    let wire_type = parts.next()?;
    let (metric_type, unit) = match wire_type {
        "g" => ("gauge", None),
        "c" => ("counter", None),
        "ms" => ("timer", Some("ms")),
        _ => return None,
    };
    let mut tags = HashMap::new();

    for part in parts {
        if let Some(sample_rate) = part
            .strip_prefix('@')
            .and_then(|raw| raw.parse::<f64>().ok())
        {
            if wire_type == "c" && sample_rate > 0.0 && sample_rate <= 1.0 {
                value /= sample_rate;
            }
        } else if let Some(raw_tags) = part.strip_prefix('#') {
            for raw_tag in raw_tags.split(',') {
                let (key, value) = raw_tag.split_once(':').unwrap_or((raw_tag, "true"));
                if !key.is_empty() {
                    tags.insert(key.to_string(), value.to_string());
                }
            }
        }
    }

    let mut metric = Metric::new(name, value, metric_type).with_tags(tags);
    if let Some(unit) = unit {
        metric.unit = Some(unit.to_string());
    }

    metric.service = take_nonempty_tag(&mut metric.tags, "service");
    metric.environment = take_nonempty_tag(&mut metric.tags, "environment");
    metric.release = take_nonempty_tag(&mut metric.tags, "release");
    metric.hostname = take_nonempty_tag(&mut metric.tags, "hostname");
    metric.sdk_name = take_nonempty_tag(&mut metric.tags, "sdk_name");
    metric.sdk_version = take_nonempty_tag(&mut metric.tags, "sdk_version");
    let tagged_unit = take_nonempty_tag(&mut metric.tags, "unit");
    if metric.unit.is_none() {
        metric.unit = tagged_unit;
    }
    Some(metric)
}

fn take_nonempty_tag(tags: &mut HashMap<String, String>, key: &str) -> Option<String> {
    tags.remove(key).filter(|value| !value.trim().is_empty())
}

#[derive(Clone, Default)]
struct CpuCounters {
    total: u64,
    idle: u64,
    iowait: u64,
}

#[derive(Clone, Default)]
struct NetworkCounters {
    rx_bytes: u64,
    rx_packets: u64,
    rx_errors: u64,
    rx_dropped: u64,
    tx_bytes: u64,
    tx_packets: u64,
    tx_errors: u64,
    tx_dropped: u64,
    speed_bits_per_second: Option<u64>,
    mtu: Option<u64>,
}

#[derive(Clone, Default)]
struct DiskCounters {
    sectors_read: u64,
    sectors_written: u64,
    io_milliseconds: u64,
}

#[derive(Clone, Default)]
struct ProcessCounters {
    name: String,
    cpu_ticks: u64,
    rss_bytes: u64,
    open_file_descriptors: Option<u64>,
}

struct HostSnapshot {
    captured_at: Instant,
    cpu: CpuCounters,
    cpu_count: usize,
    load: [f64; 3],
    memory: HashMap<String, u64>,
    network: HashMap<String, NetworkCounters>,
    disks: HashMap<String, DiskCounters>,
    udp: HashMap<String, u64>,
    sockets: HashMap<String, u64>,
    softnet: HashMap<String, u64>,
    limits: HashMap<String, u64>,
    processes: HashMap<u32, ProcessCounters>,
}

impl HostSnapshot {
    fn capture(process_filters: &[String]) -> Result<Self, Box<dyn Error>> {
        let (cpu, cpu_count) = read_cpu()?;
        Ok(Self {
            captured_at: Instant::now(),
            cpu,
            cpu_count,
            load: read_load()?,
            memory: read_memory()?,
            network: read_network()?,
            disks: read_disks()?,
            udp: read_udp()?,
            sockets: read_sockstat().unwrap_or_default(),
            softnet: read_softnet().unwrap_or_default(),
            limits: read_kernel_limits(),
            processes: read_processes(process_filters),
        })
    }
}

fn report_snapshot(client: &UpdogClient, previous: &HostSnapshot, current: &HostSnapshot) {
    let seconds = current
        .captured_at
        .saturating_duration_since(previous.captured_at)
        .as_secs_f64()
        .max(0.001);
    let total_delta = current.cpu.total.saturating_sub(previous.cpu.total);
    if total_delta > 0 {
        let idle_delta = current.cpu.idle.saturating_sub(previous.cpu.idle);
        let busy = total_delta.saturating_sub(idle_delta) as f64 / total_delta as f64 * 100.0;
        report(
            client,
            Metric::gauge("host.cpu.utilization", busy).with_unit("percent"),
        );

        let iowait = current.cpu.iowait.saturating_sub(previous.cpu.iowait) as f64
            / total_delta as f64
            * 100.0;
        report(
            client,
            Metric::gauge("host.cpu.iowait", iowait).with_unit("percent"),
        );
    }

    report(client, Metric::gauge("host.load.1", current.load[0]));
    report(client, Metric::gauge("host.load.5", current.load[1]));
    report(client, Metric::gauge("host.load.15", current.load[2]));
    for (source, name) in [
        ("MemTotal", "host.memory.total"),
        ("MemAvailable", "host.memory.available"),
        ("SwapTotal", "host.swap.total"),
        ("SwapFree", "host.swap.free"),
    ] {
        if let Some(value) = current.memory.get(source) {
            report(client, Metric::gauge(name, *value as f64).with_unit("byte"));
        }
    }

    for (interface, now) in &current.network {
        let tags = HashMap::from([("interface".to_string(), interface.clone())]);
        if let Some(speed) = now.speed_bits_per_second {
            report(
                client,
                Metric::gauge("host.network.capacity", speed as f64)
                    .with_unit("bit_per_second")
                    .with_tags(tags.clone()),
            );
        }
        if let Some(mtu) = now.mtu {
            report(
                client,
                Metric::gauge("host.network.mtu", mtu as f64)
                    .with_unit("byte")
                    .with_tags(tags.clone()),
            );
        }

        if let Some(before) = previous.network.get(interface) {
            let rx_bytes = now.rx_bytes.saturating_sub(before.rx_bytes);
            let tx_bytes = now.tx_bytes.saturating_sub(before.tx_bytes);
            for (name, delta) in [
                ("host.network.rx_bytes_per_second", rx_bytes),
                (
                    "host.network.rx_packets_per_second",
                    now.rx_packets.saturating_sub(before.rx_packets),
                ),
                (
                    "host.network.rx_errors_per_second",
                    now.rx_errors.saturating_sub(before.rx_errors),
                ),
                (
                    "host.network.rx_dropped_per_second",
                    now.rx_dropped.saturating_sub(before.rx_dropped),
                ),
                ("host.network.tx_bytes_per_second", tx_bytes),
                (
                    "host.network.tx_packets_per_second",
                    now.tx_packets.saturating_sub(before.tx_packets),
                ),
                (
                    "host.network.tx_errors_per_second",
                    now.tx_errors.saturating_sub(before.tx_errors),
                ),
                (
                    "host.network.tx_dropped_per_second",
                    now.tx_dropped.saturating_sub(before.tx_dropped),
                ),
            ] {
                report(
                    client,
                    Metric::gauge(name, delta as f64 / seconds).with_tags(tags.clone()),
                );
            }

            if let Some(speed) = now.speed_bits_per_second.filter(|speed| *speed > 0) {
                report(
                    client,
                    Metric::gauge(
                        "host.network.rx_utilization",
                        rx_bytes as f64 * 8.0 / seconds / speed as f64 * 100.0,
                    )
                    .with_unit("percent")
                    .with_tags(tags.clone()),
                );
                report(
                    client,
                    Metric::gauge(
                        "host.network.tx_utilization",
                        tx_bytes as f64 * 8.0 / seconds / speed as f64 * 100.0,
                    )
                    .with_unit("percent")
                    .with_tags(tags),
                );
            }
        }
    }

    for (device, now) in &current.disks {
        if let Some(before) = previous.disks.get(device) {
            let tags = HashMap::from([("device".to_string(), device.clone())]);
            report(
                client,
                Metric::gauge(
                    "host.disk.read_bytes_per_second",
                    now.sectors_read.saturating_sub(before.sectors_read) as f64 * 512.0 / seconds,
                )
                .with_tags(tags.clone()),
            );
            report(
                client,
                Metric::gauge(
                    "host.disk.write_bytes_per_second",
                    now.sectors_written.saturating_sub(before.sectors_written) as f64 * 512.0
                        / seconds,
                )
                .with_tags(tags.clone()),
            );
            report(
                client,
                Metric::gauge(
                    "host.disk.io_utilization",
                    (now.io_milliseconds.saturating_sub(before.io_milliseconds) as f64
                        / (seconds * 1000.0)
                        * 100.0)
                        .min(100.0),
                )
                .with_unit("percent")
                .with_tags(tags),
            );
        }
    }

    for (field, now) in &current.udp {
        if let Some(before) = previous.udp.get(field) {
            report(
                client,
                Metric::gauge(
                    format!("host.udp.{}_per_second", snake_case(field)),
                    now.saturating_sub(*before) as f64 / seconds,
                ),
            );
        }
    }

    for (name, value) in &current.sockets {
        report(
            client,
            Metric::gauge(format!("host.socket.{name}"), *value as f64),
        );
    }

    for (field, now) in &current.softnet {
        if let Some(before) = previous.softnet.get(field) {
            report(
                client,
                Metric::gauge(
                    format!("host.network.softnet_{field}_per_second"),
                    now.saturating_sub(*before) as f64 / seconds,
                ),
            );
        }
    }

    for (name, value) in &current.limits {
        report(client, Metric::gauge(name, *value as f64));
    }

    for (pid, process) in &current.processes {
        let tags = HashMap::from([
            ("pid".to_string(), pid.to_string()),
            ("process".to_string(), process.name.clone()),
        ]);
        report(
            client,
            Metric::gauge("process.memory.rss", process.rss_bytes as f64)
                .with_unit("byte")
                .with_tags(tags.clone()),
        );
        if let Some(open_file_descriptors) = process.open_file_descriptors {
            report(
                client,
                Metric::gauge(
                    "process.open_file_descriptors",
                    open_file_descriptors as f64,
                )
                .with_tags(tags.clone()),
            );
        }
        if let Some(before) = previous.processes.get(pid) {
            if total_delta > 0 {
                let cpu = process.cpu_ticks.saturating_sub(before.cpu_ticks) as f64
                    / total_delta as f64
                    * current.cpu_count as f64
                    * 100.0;
                report(
                    client,
                    Metric::gauge("process.cpu.utilization", cpu)
                        .with_unit("percent")
                        .with_tags(tags),
                );
            }
        }
    }
}

fn report(client: &UpdogClient, metric: Metric) {
    let _ = client.report_metric(metric);
}

fn read_cpu() -> Result<(CpuCounters, usize), Box<dyn Error>> {
    let stat = fs::read_to_string("/proc/stat")?;
    let mut cpu = CpuCounters::default();
    let mut cpu_count = 0;
    for line in stat.lines() {
        if line.starts_with("cpu ") {
            let values = line
                .split_whitespace()
                .skip(1)
                .filter_map(|value| value.parse::<u64>().ok())
                .collect::<Vec<_>>();
            cpu.total = values.iter().sum();
            cpu.idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
            cpu.iowait = values.get(4).copied().unwrap_or(0);
        } else if line.starts_with("cpu")
            && line
                .as_bytes()
                .get(3)
                .is_some_and(|byte| byte.is_ascii_digit())
        {
            cpu_count += 1;
        }
    }
    Ok((cpu, cpu_count.max(1)))
}

fn read_load() -> Result<[f64; 3], Box<dyn Error>> {
    let values = fs::read_to_string("/proc/loadavg")?
        .split_whitespace()
        .take(3)
        .filter_map(|value| value.parse().ok())
        .collect::<Vec<_>>();
    Ok([
        values.first().copied().unwrap_or(0.0),
        values.get(1).copied().unwrap_or(0.0),
        values.get(2).copied().unwrap_or(0.0),
    ])
}

fn read_memory() -> Result<HashMap<String, u64>, Box<dyn Error>> {
    let mut values = HashMap::new();
    for line in fs::read_to_string("/proc/meminfo")?.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(kibibytes) = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        {
            values.insert(name.to_string(), kibibytes * 1024);
        }
    }
    Ok(values)
}

fn read_network() -> Result<HashMap<String, NetworkCounters>, Box<dyn Error>> {
    let mut interfaces = HashMap::new();
    for line in fs::read_to_string("/proc/net/dev")?.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        let values = counters
            .split_whitespace()
            .filter_map(|value| value.parse::<u64>().ok())
            .collect::<Vec<_>>();
        if values.len() >= 16 {
            interfaces.insert(
                name.trim().to_string(),
                NetworkCounters {
                    rx_bytes: values[0],
                    rx_packets: values[1],
                    rx_errors: values[2],
                    rx_dropped: values[3],
                    tx_bytes: values[8],
                    tx_packets: values[9],
                    tx_errors: values[10],
                    tx_dropped: values[11],
                    speed_bits_per_second: read_u64(
                        &Path::new("/sys/class/net").join(name.trim()).join("speed"),
                    )
                    .filter(|speed| *speed > 0)
                    .and_then(|speed| speed.checked_mul(1_000_000)),
                    mtu: read_u64(&Path::new("/sys/class/net").join(name.trim()).join("mtu")),
                },
            );
        }
    }
    Ok(interfaces)
}

fn read_disks() -> Result<HashMap<String, DiskCounters>, Box<dyn Error>> {
    let mut disks = HashMap::new();
    for line in fs::read_to_string("/proc/diskstats")?.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 14 {
            continue;
        }
        let name = fields[2];
        if name.starts_with("loop") || name.starts_with("ram") {
            continue;
        }
        disks.insert(
            name.to_string(),
            DiskCounters {
                sectors_read: fields[5].parse().unwrap_or(0),
                sectors_written: fields[9].parse().unwrap_or(0),
                io_milliseconds: fields[12].parse().unwrap_or(0),
            },
        );
    }
    Ok(disks)
}

fn read_udp() -> Result<HashMap<String, u64>, Box<dyn Error>> {
    let snmp = fs::read_to_string("/proc/net/snmp")?;
    let lines = snmp
        .lines()
        .filter(|line| line.starts_with("Udp:"))
        .map(|line| line.split_whitespace().skip(1).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut values = HashMap::new();
    if lines.len() >= 2 {
        for (name, value) in lines[0].iter().zip(&lines[1]) {
            if let Ok(value) = value.parse::<u64>() {
                values.insert((*name).to_string(), value);
            }
        }
    }
    Ok(values)
}

fn read_sockstat() -> Result<HashMap<String, u64>, Box<dyn Error>> {
    let mut values = HashMap::new();
    for line in fs::read_to_string("/proc/net/sockstat")?.lines() {
        let Some((section, counters)) = line.split_once(':') else {
            continue;
        };
        let fields = counters.split_whitespace().collect::<Vec<_>>();
        for pair in fields.chunks_exact(2) {
            if let Ok(value) = pair[1].parse::<u64>() {
                values.insert(
                    format!(
                        "{}.{}",
                        section.to_ascii_lowercase(),
                        pair[0].to_ascii_lowercase()
                    ),
                    value,
                );
            }
        }
    }
    Ok(values)
}

fn read_softnet() -> Result<HashMap<String, u64>, Box<dyn Error>> {
    let mut processed = 0_u64;
    let mut dropped = 0_u64;
    let mut time_squeeze = 0_u64;
    let mut flow_limit = 0_u64;
    for line in fs::read_to_string("/proc/net/softnet_stat")?.lines() {
        let fields = line
            .split_whitespace()
            .filter_map(|field| u64::from_str_radix(field, 16).ok())
            .collect::<Vec<_>>();
        processed = processed.saturating_add(fields.first().copied().unwrap_or(0));
        dropped = dropped.saturating_add(fields.get(1).copied().unwrap_or(0));
        time_squeeze = time_squeeze.saturating_add(fields.get(2).copied().unwrap_or(0));
        flow_limit = flow_limit.saturating_add(fields.get(10).copied().unwrap_or(0));
    }
    Ok(HashMap::from([
        ("processed".to_string(), processed),
        ("dropped".to_string(), dropped),
        ("time_squeeze".to_string(), time_squeeze),
        ("flow_limit".to_string(), flow_limit),
    ]))
}

fn read_kernel_limits() -> HashMap<String, u64> {
    let mut limits = HashMap::new();
    for (name, path) in [
        (
            "host.network.socket_receive_buffer_max",
            "/proc/sys/net/core/rmem_max",
        ),
        (
            "host.network.socket_send_buffer_max",
            "/proc/sys/net/core/wmem_max",
        ),
        (
            "host.network.device_backlog_max",
            "/proc/sys/net/core/netdev_max_backlog",
        ),
        (
            "host.network.conntrack.count",
            "/proc/sys/net/netfilter/nf_conntrack_count",
        ),
        (
            "host.network.conntrack.max",
            "/proc/sys/net/netfilter/nf_conntrack_max",
        ),
    ] {
        if let Some(value) = read_u64(Path::new(path)) {
            limits.insert(name.to_string(), value);
        }
    }

    if let Ok(file_nr) = fs::read_to_string("/proc/sys/fs/file-nr") {
        let fields = file_nr
            .split_whitespace()
            .filter_map(|field| field.parse::<u64>().ok())
            .collect::<Vec<_>>();
        if fields.len() >= 3 {
            limits.insert(
                "host.file_descriptors.used".to_string(),
                fields[0].saturating_sub(fields[1]),
            );
            limits.insert("host.file_descriptors.max".to_string(), fields[2]);
        }
    }
    limits
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_processes(filters: &[String]) -> HashMap<u32, ProcessCounters> {
    if filters.is_empty() {
        return HashMap::new();
    }

    let mut processes = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return processes;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let process_path = entry.path();
        let name = fs::read_to_string(process_path.join("comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let normalized_name = name.to_ascii_lowercase();
        if !filters
            .iter()
            .any(|filter| normalized_name.contains(filter))
        {
            continue;
        }
        if let Some(counters) = read_process(&process_path, name) {
            processes.insert(pid, counters);
        }
    }
    processes
}

fn read_process(path: &Path, name: String) -> Option<ProcessCounters> {
    let stat = fs::read_to_string(path.join("stat")).ok()?;
    let end_name = stat.rfind(')')?;
    let fields = stat
        .get(end_name + 2..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let user_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let system_ticks = fields.get(12)?.parse::<u64>().ok()?;
    let rss_pages = fields.get(21)?.parse::<u64>().ok()?;
    let open_file_descriptors = fs::read_dir(path.join("fd"))
        .ok()
        .map(|entries| entries.count() as u64);
    Some(ProcessCounters {
        name,
        cpu_ticks: user_ticks + system_ticks,
        rss_bytes: rss_pages * PAGE_BYTES,
        open_file_descriptors,
    })
}

fn snake_case(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 4);
    for (index, character) in value.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_dogstatsd_metric() {
        let metric =
            parse_statsd("zone.players:350|g|#zone:night-harbor,pid:42,service:zone-server")
                .unwrap();
        assert_eq!(metric.name, "zone.players");
        assert_eq!(metric.value, 350.0);
        assert_eq!(metric.service.as_deref(), Some("zone-server"));
        assert_eq!(metric.tags["zone"], "night-harbor");
        assert_eq!(metric.tags["pid"], "42");
    }

    #[test]
    fn expands_sampled_counters() {
        let metric = parse_statsd("packets.sent:2|c|@0.5").unwrap();
        assert_eq!(metric.value, 4.0);
        assert_eq!(metric.metric_type, "counter");
    }

    #[test]
    fn promotes_and_removes_reserved_resource_tags() {
        let metric =
            parse_statsd("zone.tick:42|ms|#unit:ms,service:,environment:production").unwrap();
        assert_eq!(metric.unit.as_deref(), Some("ms"));
        assert_eq!(metric.service, None);
        assert_eq!(metric.environment.as_deref(), Some("production"));
        assert!(!metric.tags.contains_key("unit"));
        assert!(!metric.tags.contains_key("service"));
        assert!(!metric.tags.contains_key("environment"));
    }

    #[test]
    fn captures_linux_host_counters() {
        let snapshot = HostSnapshot::capture(&[]).unwrap();
        assert!(snapshot.cpu.total > 0);
        assert!(snapshot.cpu_count > 0);
        assert!(snapshot.memory.contains_key("MemTotal"));
    }
}
