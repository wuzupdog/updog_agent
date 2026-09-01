# Changelog

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
