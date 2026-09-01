# Updog Agent

`updog-agent` is the standalone Linux host and local StatsD telemetry collector for [Updog](https://wuzupdog.com). It runs outside monitored applications, keeps the project ingestion key in a root-readable systemd environment file, and forwards bounded metric batches to Updog.

## What it measures

Every five seconds the agent captures:

- aggregate CPU utilization, I/O wait, load, memory use, and swap use;
- filesystem total, used, free, and available capacity per local mount;
- disk throughput and I/O utilization;
- interface capacity, utilization, MTU, bytes, packets, errors, and drops;
- UDP errors and buffer drops, kernel softnet pressure, sockets, conntrack, and network limits;
- system file-descriptor use;
- CPU, RSS, and—when permissions allow—open descriptors for selected process names.

It also listens for tagged StatsD gauges, counters, and timers on `127.0.0.1:8125`. Unity and other server processes can therefore emit application measurements without possessing the Updog ingestion key.

## Install

Download and inspect the installer before running it as root:

```sh
curl -fsSL https://raw.githubusercontent.com/wuzupdog/updog_agent/main/scripts/install-agent.sh -o /tmp/install-updog-agent.sh
less /tmp/install-updog-agent.sh
sudo bash /tmp/install-updog-agent.sh
```

The installer downloads the latest static `updog-agent-linux-x86_64.tar.gz` release and verifies its published SHA-256 checksum. It prompts for a project-scoped **ingestion key**, environment, host service/role, and optional process-name filters, then installs a restricted systemd service. A read-only Updog CLI key cannot ingest metrics and is not suitable here.

Pressing Enter at the process-name prompt disables process-specific metrics. CPU, memory, filesystem, disk, and network metrics are always collected.

The root-owned `/etc/updog-agent.env` file contains:

| Variable | Default | Purpose |
| --- | --- | --- |
| `UPDOG_API_KEY` | required | Project-scoped ingestion key |
| `UPDOG_ENDPOINT` | `https://wuzupdog.com` | Updog service URL |
| `UPDOG_ENVIRONMENT` | `production` | Resource environment |
| `UPDOG_SERVICE` | `updog-host-agent` | Stable host role, such as `zone-host`, `world-host`, or `database-host` |
| `UPDOG_SAMPLE_INTERVAL_SECONDS` | `5` | Host sample interval |
| `UPDOG_STATSD_BIND` | `127.0.0.1:8125` | Local application-metric listener |
| `UPDOG_PROCESS_MATCH` | empty | Comma-separated process-name substrings |
| `UPDOG_HOSTNAME` | kernel hostname | Optional hostname override |

After changing configuration:

```sh
sudo systemctl restart updog-agent
sudo systemctl status updog-agent --no-pager
sudo journalctl -u updog-agent -n 100 --no-pager
```

## Upgrade

Download and run the current installer again. It replaces the binary and systemd unit while preserving the existing `/etc/updog-agent.env` file:

```sh
curl -fsSL https://raw.githubusercontent.com/wuzupdog/updog_agent/main/scripts/install-agent.sh -o /tmp/install-updog-agent.sh
less /tmp/install-updog-agent.sh
sudo bash /tmp/install-updog-agent.sh
```

Confirm the upgraded service is healthy:

```sh
/usr/local/bin/updog-agent --version
sudo systemctl status updog-agent --no-pager
sudo journalctl -u updog-agent -n 100 --no-pager
```

Install the agent on each machine with the same project ingestion key but an appropriate `UPDOG_SERVICE` and process filter. For example, use `zone-host` with `zone`, `world-host` with `world`, and `database-host` with `mysqld,mariadbd`. The machine hostname distinguishes individual hosts within a role. Missing processes simply emit no process metrics.

## Build from source

```sh
cargo build --release --bin updog-agent
```

Building from source requires Rust 1.88 or newer.

The public capture path is intentionally lossy and non-blocking: metrics enter a bounded in-memory queue, one worker bulk-sends to `/api/v1/metrics`, and full queues drop new records. The agent retries only transient failures and does not use a disk spool.

## Supported platform

The packaged agent currently supports Linux x86_64. The release workflow produces a static musl binary for compatibility across common Linux distributions.

## Release notes

### 0.2.1

- Fix host identity when the agent runs as a systemd service without a `HOSTNAME` environment variable. The agent now reads the Linux kernel hostname, so host metrics appear on the project's **Hosts** page.
