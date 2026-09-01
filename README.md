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

The installer downloads the latest static `updog-agent-linux-x86_64.tar.gz` release and verifies both its Ed25519 signature and SHA-256 checksum. It prompts for a project-scoped **ingestion key**, machine name, environment, host service/role, and optional process-name filters, then installs a restricted systemd service. The destination is always `https://wuzupdog.com`; a read-only Updog CLI key cannot ingest metrics and is not suitable here.

Pressing Enter at the process-name prompt disables process-specific metrics. CPU, memory, filesystem, disk, and network metrics are always collected.

The root-owned `/etc/updog-agent.env` file contains:

| Variable | Default | Purpose |
| --- | --- | --- |
| `UPDOG_API_KEY` | required | Project-scoped ingestion key |
| `UPDOG_MACHINE_NAME` | Linux hostname | Name shown on the project's Hosts page |
| `UPDOG_ENVIRONMENT` | `production` | Resource environment |
| `UPDOG_SERVICE` | `updog-host-agent` | Stable host role, such as `zone-host`, `world-host`, or `database-host` |
| `UPDOG_SAMPLE_INTERVAL_SECONDS` | `5` | Host sample interval |
| `UPDOG_STATSD_BIND` | `127.0.0.1:8125` | Local application-metric listener |
| `UPDOG_PROCESS_MATCH` | empty | Comma-separated process-name substrings |

After changing configuration:

```sh
sudo systemctl restart updog-agent
sudo systemctl status updog-agent --no-pager
sudo journalctl -u updog-agent -n 100 --no-pager
```

## Update

Agents before 0.2.2 must run the installer once more. It replaces the binary and systemd unit, preserves `/etc/updog-agent.env`, and restarts the service:

```sh
curl -fsSL https://raw.githubusercontent.com/wuzupdog/updog_agent/main/scripts/install-agent.sh -o /tmp/install-updog-agent.sh
less /tmp/install-updog-agent.sh
sudo bash /tmp/install-updog-agent.sh
```

Version 0.2.2 and newer can explicitly check for or install a signed release:

```sh
sudo updog-agent update --check
sudo updog-agent update
sudo updog-agent update --version 0.2.2
```

Updates are never automatic. The updater verifies the signed checksum and downloaded binary, atomically replaces `/usr/local/bin/updog-agent`, restarts `updog-agent.service`, watches its health, and restores the previous binary if activation fails. It does not read, print, or modify `/etc/updog-agent.env`.

Confirm the updated service is healthy:

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

Release checksums are signed with the Ed25519 public key in [`release/updog-release-public-key.pem`](release/updog-release-public-key.pem). Its DER SHA-256 fingerprint is `0b13b52b317aaa361f5caf7961ca56f45b6ad913be5db720f4c202d99b6855f6`.

## Release notes

### 0.2.2

- Add explicit `update`, `update --check`, and `update --version` commands with signed downloads, atomic replacement, service health checks, and automatic rollback.
- Sign release checksums with a dedicated Ed25519 key and verify signatures in both the updater and installer.
- Restart `updog-agent.service` after every installer upgrade so replacing an active binary cannot leave the previous executable running.
- Further restrict the installed systemd service without preventing host, process, StatsD, or outbound HTTPS collection.

### 0.2.1

- Fix host identity when the agent runs as a systemd service without a `HOSTNAME` environment variable. New installs ask for the machine name, while upgraded installs fall back to the Linux kernel hostname. The Updog destination is fixed to `https://wuzupdog.com`.
