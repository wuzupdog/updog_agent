# Updog Agent development guide

- Keep metric capture non-blocking and fail-safe. Capture calls enqueue into the bounded in-memory queue and never perform HTTP.
- Preserve the default limits: 2,048 records, 8 MiB total, 64 KiB per record, 512 records or 512 KiB per batch, and a five-second partial flush.
- Keep one HTTP worker and at most one in-flight metric request. Retry only network failures, 408, 429, and 5xx responses; honor `Retry-After` and keep request IDs stable across retries.
- Never log, commit, or accept the ingestion key as a command-line argument. Applications should use the loopback StatsD listener instead of receiving the key.
- Keep StatsD bound to loopback by default and keep the systemd service unprivileged.
- Run `cargo fmt -- --check`, `cargo test --locked`, `bash -n scripts/install-agent.sh`, and a release build before committing.
