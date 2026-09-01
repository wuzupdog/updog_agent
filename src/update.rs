use ring::{digest, signature};
use semver::Version;
use serde::Deserialize;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::thread;
use std::time::Duration;

const REPOSITORY: &str = "wuzupdog/updog_agent";
const INSTALL_PATH: &str = "/usr/local/bin/updog-agent";
const SERVICE_NAME: &str = "updog-agent.service";
const BINARY_ASSET: &str = "updog-agent-linux-x86_64";
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 16 * 1024;
const RELEASE_PUBLIC_KEY: [u8; 32] = [
    0x9e, 0x49, 0x92, 0xad, 0x40, 0x47, 0xb8, 0xf8, 0x7a, 0xd8, 0xf7, 0x89, 0x10, 0x6b, 0xa9, 0x0a,
    0x4e, 0x6e, 0x8a, 0x7f, 0x36, 0xaa, 0x27, 0xbb, 0x08, 0x96, 0xc0, 0xce, 0xcf, 0xea, 0xc3, 0x80,
];

type UpdateResult<T> = Result<T, UpdateError>;

#[derive(Debug)]
pub struct UpdateError(String);

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl Error for UpdateError {}

#[derive(Debug, Default, PartialEq, Eq)]
struct UpdateOptions {
    check: bool,
    requested_version: Option<Version>,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

struct ReleaseMetadata {
    version: Version,
    expected_sha256: String,
}

pub fn run(arguments: &[String]) -> UpdateResult<()> {
    if matches!(arguments, [argument] if argument == "--help" || argument == "-h") {
        println!("usage: updog-agent update [--check] [--version VERSION]");
        return Ok(());
    }

    let options = parse_options(arguments)?;
    let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| update_error(format!("invalid current version: {error}")))?;
    let release_version = requested_or_latest_version(&options)?;
    let metadata = fetch_release_metadata(release_version)?;

    print_version_status(&current_version, &metadata.version);
    if options.check || current_version == metadata.version {
        return Ok(());
    }
    if current_version > metadata.version {
        return Err(update_error(
            "refusing to replace the installed agent with an older release",
        ));
    }

    ensure_supported_platform()?;
    ensure_root()?;
    let binary = download_release_asset(&metadata.version, BINARY_ASSET, MAX_BINARY_BYTES)?;
    verify_checksum(&binary, &metadata.expected_sha256)?;
    install_and_activate(&binary, &metadata.version)?;
    println!("updated updog-agent to {}", metadata.version);
    Ok(())
}

fn parse_options(arguments: &[String]) -> UpdateResult<UpdateOptions> {
    let mut options = UpdateOptions::default();
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--check" if !options.check => options.check = true,
            "--version" if options.requested_version.is_none() => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| update_error("--version requires a version"))?;
                options.requested_version = Some(parse_version(value)?);
            }
            argument => return Err(update_error(format!("unknown update option: {argument}"))),
        }
        index += 1;
    }

    Ok(options)
}

fn parse_version(value: &str) -> UpdateResult<Version> {
    Version::parse(value.trim_start_matches('v'))
        .map_err(|error| update_error(format!("invalid release version {value:?}: {error}")))
}

fn requested_or_latest_version(options: &UpdateOptions) -> UpdateResult<Version> {
    match &options.requested_version {
        Some(version) => Ok(version.clone()),
        None => fetch_latest_version(),
    }
}

fn fetch_latest_version() -> UpdateResult<Version> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let payload = download(&url, MAX_METADATA_BYTES)?;
    let release: GitHubRelease = serde_json::from_slice(&payload)
        .map_err(|error| update_error(format!("invalid GitHub release response: {error}")))?;
    parse_version(&release.tag_name)
}

fn fetch_release_metadata(version: Version) -> UpdateResult<ReleaseMetadata> {
    let checksum_asset = format!("{BINARY_ASSET}.sha256");
    let checksum = download_release_asset(&version, &checksum_asset, MAX_METADATA_BYTES)?;
    let signature = download_release_asset(
        &version,
        &format!("{checksum_asset}.sig"),
        MAX_METADATA_BYTES,
    )?;
    verify_release_signature(&checksum, &signature)?;
    let expected_sha256 = parse_checksum(&checksum, BINARY_ASSET)?;

    Ok(ReleaseMetadata {
        version,
        expected_sha256,
    })
}

fn download_release_asset(version: &Version, asset: &str, limit: u64) -> UpdateResult<Vec<u8>> {
    let url = format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{asset}");
    download(&url, limit)
}

fn download(url: &str, limit: u64) -> UpdateResult<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let response = agent
        .get(url)
        .set("Accept", "application/vnd.github+json")
        .set(
            "User-Agent",
            concat!("updog-agent/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|error| update_error(format!("download failed for {url}: {error}")))?;

    if response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit)
    {
        return Err(update_error(format!(
            "download exceeds {limit} bytes: {url}"
        )));
    }

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| update_error(format!("could not read {url}: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(update_error(format!(
            "download exceeds {limit} bytes: {url}"
        )));
    }
    Ok(bytes)
}

fn verify_release_signature(message: &[u8], signature: &[u8]) -> UpdateResult<()> {
    signature::UnparsedPublicKey::new(&signature::ED25519, RELEASE_PUBLIC_KEY)
        .verify(message, signature)
        .map_err(|_| update_error("release signature verification failed"))
}

fn parse_checksum(checksum: &[u8], expected_asset: &str) -> UpdateResult<String> {
    let checksum = std::str::from_utf8(checksum)
        .map_err(|error| update_error(format!("checksum is not UTF-8: {error}")))?;
    let fields = checksum.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 || fields[1].trim_start_matches('*') != expected_asset {
        return Err(update_error("checksum file names an unexpected asset"));
    }
    if fields[0].len() != 64 || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(update_error(
            "checksum file contains an invalid SHA-256 digest",
        ));
    }
    Ok(fields[0].to_ascii_lowercase())
}

fn verify_checksum(binary: &[u8], expected: &str) -> UpdateResult<()> {
    let actual = sha256_hex(binary);
    if actual == expected {
        Ok(())
    } else {
        Err(update_error(
            "downloaded binary failed SHA-256 verification",
        ))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = digest::digest(&digest::SHA256, bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest.as_ref() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn print_version_status(current: &Version, available: &Version) {
    println!("current version: {current}");
    println!("available version: {available}");
    let status = match current.cmp(available) {
        std::cmp::Ordering::Less => "update available",
        std::cmp::Ordering::Equal => "up to date",
        std::cmp::Ordering::Greater => "requested version is older",
    };
    println!("status: {status}");
}

fn ensure_supported_platform() -> UpdateResult<()> {
    if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
        Ok(())
    } else {
        Err(update_error("self-update supports Linux x86_64 only"))
    }
}

fn ensure_root() -> UpdateResult<()> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err(update_error(
            "run `sudo updog-agent update` to install an update",
        ))
    }
}

fn install_and_activate(binary: &[u8], version: &Version) -> UpdateResult<()> {
    let install_path = Path::new(INSTALL_PATH);
    let directory = install_path
        .parent()
        .ok_or_else(|| update_error("installed binary has no parent directory"))?;
    let temporary_path = update_path(directory, "update");
    let backup_path = directory.join(".updog-agent.rollback");
    let backup_temporary_path = update_path(directory, "rollback");

    remove_file_if_present(&temporary_path)?;
    remove_file_if_present(&backup_temporary_path)?;
    write_executable(&temporary_path, binary)?;
    if let Err(error) = create_backup(install_path, &backup_temporary_path, &backup_path) {
        let _ = remove_file_if_present(&temporary_path);
        let _ = remove_file_if_present(&backup_temporary_path);
        return Err(error);
    }

    if let Err(error) = replace_and_activate(&temporary_path, install_path, directory, version) {
        let _ = remove_file_if_present(&temporary_path);
        return match rollback(install_path, &backup_path, directory) {
            Ok(()) => Err(update_error(format!(
                "{error}; restored the previous binary"
            ))),
            Err(rollback_error) => Err(update_error(format!(
                "{error}; automatic rollback also failed: {rollback_error}"
            ))),
        };
    }

    remove_file_if_present(&backup_path)?;
    sync_directory(directory)?;
    Ok(())
}

fn update_path(directory: &Path, purpose: &str) -> PathBuf {
    directory.join(format!(".updog-agent.{purpose}.{}", process::id()))
}

fn write_executable(path: &Path, binary: &[u8]) -> UpdateResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)
        .map_err(|error| update_error(format!("could not create {}: {error}", path.display())))?;
    file.write_all(binary)
        .and_then(|_| file.sync_all())
        .map_err(|error| update_error(format!("could not write {}: {error}", path.display())))
}

fn create_backup(source: &Path, temporary: &Path, destination: &Path) -> UpdateResult<()> {
    fs::copy(source, temporary).map_err(|error| {
        update_error(format!("could not back up {}: {error}", source.display()))
    })?;
    fs::set_permissions(temporary, fs::Permissions::from_mode(0o755))
        .map_err(|error| update_error(format!("could not secure backup: {error}")))?;
    File::open(temporary)
        .and_then(|file| file.sync_all())
        .map_err(|error| update_error(format!("could not sync backup: {error}")))?;
    remove_file_if_present(destination)?;
    fs::rename(temporary, destination)
        .map_err(|error| update_error(format!("could not finalize backup: {error}")))
}

fn replace_and_activate(
    temporary: &Path,
    install_path: &Path,
    directory: &Path,
    version: &Version,
) -> UpdateResult<()> {
    fs::rename(temporary, install_path)
        .map_err(|error| update_error(format!("could not replace agent binary: {error}")))?;
    sync_directory(directory)?;
    verify_installed_version(version)?;
    restart_service()?;
    verify_service_health()
}

fn verify_installed_version(version: &Version) -> UpdateResult<()> {
    let output = Command::new(INSTALL_PATH)
        .arg("--version")
        .output()
        .map_err(|error| update_error(format!("could not run updated binary: {error}")))?;
    let expected = format!("updog-agent {version}");
    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == expected {
        Ok(())
    } else {
        Err(update_error("updated binary failed its version check"))
    }
}

fn restart_service() -> UpdateResult<()> {
    let output = Command::new("systemctl")
        .args(["restart", SERVICE_NAME])
        .output()
        .map_err(|error| update_error(format!("could not restart {SERVICE_NAME}: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(update_error(format!(
            "could not restart {SERVICE_NAME}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn verify_service_health() -> UpdateResult<()> {
    for _ in 0..6 {
        thread::sleep(Duration::from_millis(500));
        let healthy = Command::new("systemctl")
            .args(["is-active", "--quiet", SERVICE_NAME])
            .status()
            .map_err(|error| update_error(format!("could not check {SERVICE_NAME}: {error}")))?
            .success();
        if !healthy {
            return Err(update_error(format!(
                "{SERVICE_NAME} did not remain active"
            )));
        }
    }
    Ok(())
}

fn rollback(install_path: &Path, backup: &Path, directory: &Path) -> UpdateResult<()> {
    fs::rename(backup, install_path)
        .map_err(|error| update_error(format!("automatic rollback failed: {error}")))?;
    sync_directory(directory)?;
    restart_service().map_err(|error| update_error(format!("rollback restart failed: {error}")))
}

fn remove_file_if_present(path: &Path) -> UpdateResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(update_error(format!(
            "could not remove {}: {error}",
            path.display()
        ))),
    }
}

fn sync_directory(directory: &Path) -> UpdateResult<()> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| update_error(format!("could not sync {}: {error}", directory.display())))
}

fn update_error(message: impl Into<String>) -> UpdateError {
    UpdateError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_check_and_pinned_version_options() {
        let options = parse_options(&[
            "--check".to_string(),
            "--version".to_string(),
            "v0.2.2".to_string(),
        ])
        .unwrap();

        assert!(options.check);
        assert_eq!(options.requested_version, Some(Version::new(0, 2, 2)));
    }

    #[test]
    fn rejects_unknown_update_options() {
        assert!(parse_options(&["--automatic".to_string()]).is_err());
    }

    #[test]
    fn parses_checksum_for_expected_asset() {
        let digest = "a".repeat(64);
        let checksum = format!("{digest}  {BINARY_ASSET}\n");
        assert_eq!(
            parse_checksum(checksum.as_bytes(), BINARY_ASSET).unwrap(),
            digest
        );
        assert!(parse_checksum(checksum.as_bytes(), "another-asset").is_err());
    }

    #[test]
    fn calculates_sha256() {
        assert_eq!(
            sha256_hex(b"updog"),
            "6ad6e240cb0536b84b0ce49dea4a9dd58233153356ffa0cc79d98db344bbd4b4"
        );
    }

    #[test]
    fn verifies_signature_from_release_key() {
        let message = b"updog release verification test";
        let signature = decode_hex::<64>(
            "833ed1d99cb9b991dc48fb7dd6ab394a450b31b59f6726b8d82d77583d0258ce23c89d5fc9a014d4678b5fd66da7aba5a2a7f6a306d32e372f5082deab70c800",
        );
        verify_release_signature(message, &signature).unwrap();
        assert!(verify_release_signature(b"tampered", &signature).is_err());
    }

    fn decode_hex<const SIZE: usize>(value: &str) -> [u8; SIZE] {
        let mut bytes = [0_u8; SIZE];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&value[offset..offset + 2], 16).unwrap();
        }
        bytes
    }
}
