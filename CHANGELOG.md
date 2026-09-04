# Changelog

## 0.4.0

- Add optional, rotation-aware MariaDB slow-query collection that sends fingerprints and performance measurements without SQL text or database identifiers.
- Report thread count and read/write I/O rates for detected `mariadbd` and `mysqld` processes without requiring database credentials.
- Leave MariaDB collection disabled and dependency-free on hosts that do not run it.

## 0.3.0

- Replace opt-in process-name filters with the bounded union of the top ten processes by CPU and top ten by resident memory.
- Read only process names and accounting data for discovery; never read command arguments or environments.
- Remove the process filter from new installs while safely ignoring preserved `UPDOG_PROCESS_MATCH` values.

## 0.2.0

- Report memory and swap used bytes and utilization percentages.
- Report filesystem total, used, free, available, and utilization metrics for local mounted filesystems.
- Report agent health, host uptime, and logical CPU count for per-machine health views.
- Make process-name monitoring opt-in instead of defaulting to game-server process names.
- Document the in-place upgrade flow.

## 0.1.1

- Add a configurable `UPDOG_SERVICE` resource field so dashboards can separate zone, world, and database hosts.
- Document per-host role and process-filter setup.

## 0.1.0

- Initial standalone Linux host agent.
- Collect CPU, memory, disk, network, UDP, socket, softnet, conntrack, and selected-process telemetry.
- Accept loopback StatsD gauges, counters, and timers from application processes.
- Add bounded metric delivery, a checksum-verifying systemd installer, and static release packaging.
