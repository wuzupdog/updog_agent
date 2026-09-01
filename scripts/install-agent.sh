#!/usr/bin/env bash
set -euo pipefail

repository="wuzupdog/updog_agent"
archive_name="updog-agent-linux-x86_64.tar.gz"
download_base="${UPDOG_AGENT_DOWNLOAD_BASE:-https://github.com/${repository}/releases/latest/download}"
install_root="/usr/local/bin"
config_path="/etc/updog-agent.env"
service_path="/etc/systemd/system/updog-agent.service"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "updog-agent currently supports Linux x86_64" >&2
  exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
  echo "Run this installer with sudo" >&2
  exit 1
fi

for command in curl sha256sum install tar systemctl groupadd useradd getent hostname; do
  command -v "${command}" >/dev/null || {
    echo "Missing required command: ${command}" >&2
    exit 1
  }
done

temporary_directory="$(mktemp -d)"
trap 'rm -rf "${temporary_directory}"' EXIT
archive_path="${temporary_directory}/${archive_name}"

curl --fail --silent --show-error --location \
  "${download_base}/${archive_name}" --output "${archive_path}"
curl --fail --silent --show-error --location \
  "${download_base}/${archive_name}.sha256" --output "${archive_path}.sha256"
(
  cd "${temporary_directory}"
  sha256sum --check "${archive_name}.sha256"
)
tar -xzf "${archive_path}" -C "${temporary_directory}"
install -D -m 0755 "${temporary_directory}/updog-agent" "${install_root}/updog-agent"

if ! getent group updog >/dev/null; then
  groupadd --system updog
fi
if ! id updog >/dev/null 2>&1; then
  useradd --system --gid updog --no-create-home --shell /usr/sbin/nologin updog
fi

if [[ ! -f "${config_path}" ]]; then
  read -r -s -p "Updog ingestion key: " api_key
  echo
  if [[ -z "${api_key}" ]]; then
    echo "The Updog ingestion key cannot be empty" >&2
    exit 1
  fi
  detected_machine_name="$(hostname)"
  read -r -p "Machine name [${detected_machine_name}]: " machine_name
  read -r -p "Environment [production]: " environment
  read -r -p "Host service/role [updog-host-agent]: " service
  read -r -p "Optional process names to monitor, comma-separated [none]: " process_match
  machine_name="${machine_name:-${detected_machine_name}}"
  environment="${environment:-production}"
  service="${service:-updog-host-agent}"
  process_match="${process_match:-}"

  umask 077
  escape_environment_value() {
    local value="${1//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf '"%s"' "${value}"
  }

  {
    printf 'UPDOG_API_KEY=%s\n' "$(escape_environment_value "${api_key}")"
    printf 'UPDOG_MACHINE_NAME=%s\n' "$(escape_environment_value "${machine_name}")"
    printf 'UPDOG_ENVIRONMENT=%s\n' "$(escape_environment_value "${environment}")"
    printf 'UPDOG_SERVICE=%s\n' "$(escape_environment_value "${service}")"
    printf 'UPDOG_SAMPLE_INTERVAL_SECONDS=5\n'
    printf 'UPDOG_STATSD_BIND=127.0.0.1:8125\n'
    printf 'UPDOG_PROCESS_MATCH=%s\n' "$(escape_environment_value "${process_match}")"
  } >"${config_path}"
  chown root:root "${config_path}"
  chmod 0600 "${config_path}"
else
  echo "Keeping existing ${config_path}"
fi

install -D -m 0644 /dev/null "${service_path}"
cat >"${service_path}" <<'UNIT'
[Unit]
Description=Updog Linux host telemetry agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=updog
Group=updog
EnvironmentFile=/etc/updog-agent.env
ExecStart=/usr/local/bin/updog-agent
Restart=always
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now updog-agent.service
systemctl --no-pager --full status updog-agent.service
