use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::paths::display_path;
use crate::report::Reporter;

const BIN_NAME: &str = "nodus";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
const INSTALL_MARKER_FILE: &str = "nodus.install.json";
const REPO_SLUG: &str = "nodus-rs/nodus";
const STATE_FILE: &str = "update-check.json";

#[derive(Debug, Clone, PartialEq, Eq)]
struct LatestRelease {
    tag: String,
    version: Version,
}

#[derive(Debug, Clone)]
struct CheckOptions {
    now_unix_secs: u64,
    current_exe: PathBuf,
    current_version: Version,
    cargo_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct UpdateCheckState {
    last_attempted_at_unix_secs: Option<u64>,
    latest_known_tag: Option<String>,
    latest_known_version: Option<String>,
    last_notified_tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallTarget {
    CargoRegistry { binary_path: PathBuf },
    GithubRelease { binary_path: PathBuf },
    Unsupported(UnsupportedInstall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnsupportedInstall {
    Ambiguous {
        binary_path: PathBuf,
    },
    CargoPath {
        binary_path: PathBuf,
        source: String,
    },
    CargoGit {
        binary_path: PathBuf,
        source: String,
    },
    CargoOther {
        binary_path: PathBuf,
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannedUpgrade {
    AlreadyCurrent {
        version: Version,
    },
    CargoRegistry {
        current_version: Version,
        latest: LatestRelease,
        binary_path: PathBuf,
        command: Vec<String>,
    },
    GithubRelease {
        current_version: Version,
        latest: LatestRelease,
        binary_path: PathBuf,
        install_dir: PathBuf,
        script_url: String,
    },
    Unsupported {
        latest: LatestRelease,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPlatform {
    Unix,
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReleaseInstallMarker {
    install_method: String,
    repo_slug: String,
    binary_name: String,
    binary_path: PathBuf,
}

struct InstallerInvocation {
    program: &'static str,
    action: &'static str,
    script_path: PathBuf,
    base_args: Vec<String>,
    version_flag: &'static str,
    install_dir_flag: &'static str,
}

impl InstallerInvocation {
    fn args_for(&self, tag: &str, install_dir: &Path) -> Vec<String> {
        let mut args = self.base_args.clone();
        args.extend([
            self.script_path.to_string_lossy().into_owned(),
            self.version_flag.into(),
            tag.into(),
            self.install_dir_flag.into(),
            install_dir.to_string_lossy().into_owned(),
        ]);
        args
    }
}

#[derive(Debug, Deserialize)]
struct CargoInstallState {
    installs: BTreeMap<String, CargoInstallEntry>,
}

#[derive(Debug, Deserialize)]
struct CargoInstallEntry {
    bins: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyCargoInstallState {
    v1: BTreeMap<String, Vec<String>>,
}

pub fn maybe_notify(store_root: &Path, reporter: &Reporter) {
    let options = match CheckOptions::for_current_binary() {
        Ok(options) => options,
        Err(_) => return,
    };

    let _ = maybe_notify_with(store_root, reporter, &options, fetch_latest_release);
}

pub fn upgrade(reporter: &Reporter, check_only: bool) -> Result<()> {
    let options = CheckOptions::for_current_binary()?;
    reporter.status("Checking", "latest Nodus release")?;
    let latest = fetch_latest_release()?.ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine the latest Nodus release from {}",
            releases_latest_url()
        )
    })?;

    let plan = plan_upgrade(&options, &latest);
    if check_only {
        reporter.finish(upgrade_available_message(&options, &latest, &plan))?;
        return Ok(());
    }

    match plan {
        PlannedUpgrade::AlreadyCurrent { version } => {
            reporter.finish(format!("nodus {version} is already current"))?;
            Ok(())
        }
        PlannedUpgrade::CargoRegistry {
            current_version,
            latest,
            binary_path,
            command,
        } => {
            ensure_install_directory_writable(&binary_path).map_err(|_| {
                anyhow::anyhow!(cargo_permission_guidance(&latest.version, &binary_path))
            })?;
            reporter.status(
                "Updating",
                format!(
                    "nodus {current_version} -> {} via cargo install",
                    latest.version
                ),
            )?;
            run_checked_command(
                &command[0],
                &command[1..],
                "cargo install",
                "failed to update nodus via cargo install",
            )?;
            reporter.finish(format!(
                "updated nodus {current_version} -> {}",
                latest.version
            ))?;
            Ok(())
        }
        PlannedUpgrade::GithubRelease {
            current_version,
            latest,
            binary_path,
            install_dir,
            script_url,
        } => {
            ensure_install_directory_writable(&binary_path).map_err(|_| {
                anyhow::anyhow!(release_permission_guidance(&latest.tag, &install_dir))
            })?;
            let temp = tempfile::TempDir::new().context("failed to create temp dir")?;
            let installer =
                release_installer_invocation(&temp.path().join(release_installer_script_name()));
            let script_path = installer.script_path.clone();
            reporter.status("Downloading", format!("installer for {}", latest.tag))?;
            download_to_path(&script_url, &script_path)?;
            reporter.status(
                "Updating",
                format!(
                    "nodus {current_version} -> {} via GitHub release installer",
                    latest.version
                ),
            )?;
            run_checked_command(
                installer.program,
                &installer.args_for(latest.tag.as_str(), &install_dir),
                installer.action,
                "failed to update nodus via the GitHub release installer",
            )?;
            reporter.finish(format!(
                "updated nodus {current_version} -> {}",
                latest.version
            ))?;
            Ok(())
        }
        PlannedUpgrade::Unsupported { message, .. } => anyhow::bail!(message),
    }
}

impl CheckOptions {
    fn for_current_binary() -> Result<Self> {
        let current_exe =
            env::current_exe().context("failed to determine the current nodus executable path")?;
        Ok(Self {
            now_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before the Unix epoch")?
                .as_secs(),
            current_exe: canonicalize_or_identity(&current_exe),
            current_version: Version::parse(env!("CARGO_PKG_VERSION"))
                .context("failed to parse the current package version")?,
            cargo_home: resolve_cargo_home(),
        })
    }
}

impl UpdateCheckState {
    fn latest_known_release(&self) -> Option<LatestRelease> {
        let tag = self.latest_known_tag.clone()?;
        let version = self.latest_known_version.as_deref()?;
        Some(LatestRelease {
            tag,
            version: Version::parse(version).ok()?,
        })
    }
}

fn maybe_notify_with<F>(
    store_root: &Path,
    reporter: &Reporter,
    options: &CheckOptions,
    fetch_latest: F,
) -> Result<()>
where
    F: FnOnce() -> Result<Option<LatestRelease>>,
{
    let state_path = state_path(store_root);
    let mut state = load_state(&state_path)?;
    let mut latest_known = state.latest_known_release();

    if should_attempt_remote_check(state.last_attempted_at_unix_secs, options.now_unix_secs) {
        state.last_attempted_at_unix_secs = Some(options.now_unix_secs);

        match fetch_latest() {
            Ok(Some(release)) => {
                state.latest_known_tag = Some(release.tag.clone());
                state.latest_known_version = Some(release.version.to_string());
                latest_known = Some(release);
            }
            Ok(None) => {}
            Err(_) => {}
        }

        persist_state(&state_path, &state)?;
    }

    let Some(latest_release) = latest_known else {
        return Ok(());
    };

    if latest_release.version <= options.current_version {
        return Ok(());
    }

    if state.last_notified_tag.as_deref() == Some(latest_release.tag.as_str()) {
        return Ok(());
    }

    let plan = plan_upgrade(options, &latest_release);
    let notice = upgrade_available_message(options, &latest_release, &plan);
    reporter.warning(notice)?;
    state.last_notified_tag = Some(latest_release.tag);
    persist_state(&state_path, &state)
}

fn plan_upgrade(options: &CheckOptions, latest: &LatestRelease) -> PlannedUpgrade {
    plan_upgrade_for_platform(options, latest, current_host_platform())
}

fn plan_upgrade_for_platform(
    options: &CheckOptions,
    latest: &LatestRelease,
    platform: HostPlatform,
) -> PlannedUpgrade {
    if latest.version <= options.current_version {
        return PlannedUpgrade::AlreadyCurrent {
            version: options.current_version.clone(),
        };
    }

    match detect_install_target(options) {
        InstallTarget::CargoRegistry { binary_path } => PlannedUpgrade::CargoRegistry {
            current_version: options.current_version.clone(),
            latest: latest.clone(),
            binary_path,
            command: cargo_update_command(&latest.version),
        },
        InstallTarget::GithubRelease { binary_path } => PlannedUpgrade::GithubRelease {
            current_version: options.current_version.clone(),
            latest: latest.clone(),
            install_dir: binary_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            binary_path,
            script_url: tagged_install_script_url_for_platform(&latest.tag, platform),
        },
        InstallTarget::Unsupported(install) => PlannedUpgrade::Unsupported {
            latest: latest.clone(),
            message: unsupported_upgrade_message(latest, &install, platform),
        },
    }
}

fn detect_install_target(options: &CheckOptions) -> InstallTarget {
    if let Some(install) = detect_cargo_install(&options.current_exe, options.cargo_home.as_deref())
    {
        return install;
    }

    detect_release_install(&options.current_exe)
}

fn detect_cargo_install(current_exe: &Path, cargo_home: Option<&Path>) -> Option<InstallTarget> {
    let cargo_home = cargo_home?;
    let cargo_bin = cargo_home
        .join("bin")
        .join(executable_file_name(current_host_platform()));
    if current_exe != cargo_bin {
        return None;
    }

    let sources = load_cargo_install_sources(cargo_home, BIN_NAME);
    let binary_path = current_exe.to_path_buf();

    match sources.as_slice() {
        [source] if source == CRATES_IO_SOURCE => {
            Some(InstallTarget::CargoRegistry { binary_path })
        }
        [source] if source.starts_with("path+") => {
            Some(InstallTarget::Unsupported(UnsupportedInstall::CargoPath {
                binary_path,
                source: source.clone(),
            }))
        }
        [source] if source.starts_with("git+") => {
            Some(InstallTarget::Unsupported(UnsupportedInstall::CargoGit {
                binary_path,
                source: source.clone(),
            }))
        }
        [source] => Some(InstallTarget::Unsupported(UnsupportedInstall::CargoOther {
            binary_path,
            source: source.clone(),
        })),
        _ => Some(InstallTarget::Unsupported(UnsupportedInstall::Ambiguous {
            binary_path,
        })),
    }
}

fn detect_release_install(current_exe: &Path) -> InstallTarget {
    let marker_path = install_marker_path(current_exe);
    let Some(marker) = load_release_install_marker(&marker_path) else {
        return InstallTarget::Unsupported(UnsupportedInstall::Ambiguous {
            binary_path: current_exe.to_path_buf(),
        });
    };

    if marker.install_method != "github_release"
        || marker.repo_slug != REPO_SLUG
        || marker.binary_name != BIN_NAME
        || canonicalize_or_identity(&marker.binary_path) != current_exe
    {
        return InstallTarget::Unsupported(UnsupportedInstall::Ambiguous {
            binary_path: current_exe.to_path_buf(),
        });
    }

    InstallTarget::GithubRelease {
        binary_path: current_exe.to_path_buf(),
    }
}

fn upgrade_available_message(
    options: &CheckOptions,
    latest: &LatestRelease,
    plan: &PlannedUpgrade,
) -> String {
    match plan {
        PlannedUpgrade::AlreadyCurrent { .. } => {
            format!("nodus {} is already current", options.current_version)
        }
        PlannedUpgrade::CargoRegistry { .. } | PlannedUpgrade::GithubRelease { .. } => format!(
            "nodus {} is available (current {}); run `nodus upgrade`",
            latest.version, options.current_version
        ),
        PlannedUpgrade::Unsupported { .. } => format!(
            "nodus {} is available (current {}); see {}",
            latest.version,
            options.current_version,
            install_url()
        ),
    }
}

fn should_attempt_remote_check(
    last_attempted_at_unix_secs: Option<u64>,
    now_unix_secs: u64,
) -> bool {
    match last_attempted_at_unix_secs {
        None => true,
        Some(last_attempted) => now_unix_secs.saturating_sub(last_attempted) >= CHECK_INTERVAL_SECS,
    }
}

fn fetch_latest_release() -> Result<Option<LatestRelease>> {
    let headers = match curl_head_request(&releases_latest_url()) {
        Ok(headers) => headers,
        Err(error) if is_missing_command_error(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(location) = last_location_header(&headers) else {
        return Ok(None);
    };

    Ok(parse_latest_release_from_location(&location))
}

fn curl_head_request(url: &str) -> Result<String> {
    let output = Command::new("curl")
        .args(["-fsSLI", url])
        .output()
        .with_context(|| format!("failed to run curl for {url}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "curl failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_checked_command(
    program: &str,
    args: &[String],
    action: &str,
    failure_context: &str,
) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {action}"))?;
    if !output.status.success() {
        anyhow::bail!("{failure_context}: {}", command_failure_output(&output));
    }

    Ok(())
}

fn download_to_path(url: &str, output_path: &Path) -> Result<()> {
    let output = Command::new("curl")
        .arg("-fsSL")
        .arg(url)
        .arg("-o")
        .arg(output_path)
        .output()
        .with_context(|| format!("failed to run curl for {url}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to download {}: {}",
            url,
            command_failure_output(&output)
        );
    }

    Ok(())
}

fn command_failure_output(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }

    format!("process exited with status {}", output.status)
}

fn ensure_install_directory_writable(binary_path: &Path) -> Result<()> {
    let install_dir = binary_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine the install directory for {}",
            binary_path.display()
        )
    })?;
    let probe = NamedTempFile::new_in(install_dir)
        .with_context(|| format!("failed to write into {}", install_dir.display()))?;
    drop(probe);
    Ok(())
}

fn is_missing_command_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
}

fn last_location_header(headers: &str) -> Option<String> {
    headers.lines().rev().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("location") {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

fn parse_latest_release_from_location(location: &str) -> Option<LatestRelease> {
    let tag = location
        .rsplit('/')
        .next()?
        .split('?')
        .next()?
        .trim()
        .to_string();
    let version = parse_release_version(&tag)?;

    Some(LatestRelease { tag, version })
}

fn parse_release_version(tag: &str) -> Option<Version> {
    let normalized = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(normalized).ok()
}

fn cargo_update_command(version: &Version) -> Vec<String> {
    vec![
        "cargo".into(),
        "install".into(),
        "--locked".into(),
        "--force".into(),
        BIN_NAME.into(),
        "--version".into(),
        version.to_string(),
    ]
}

fn current_host_platform() -> HostPlatform {
    if cfg!(windows) {
        HostPlatform::Windows
    } else {
        HostPlatform::Unix
    }
}

fn executable_file_name(platform: HostPlatform) -> &'static str {
    match platform {
        HostPlatform::Unix => BIN_NAME,
        HostPlatform::Windows => "nodus.exe",
    }
}

fn release_installer_script_name() -> &'static str {
    release_installer_script_name_for_platform(current_host_platform())
}

fn release_installer_script_name_for_platform(platform: HostPlatform) -> &'static str {
    match platform {
        HostPlatform::Unix => "install.sh",
        HostPlatform::Windows => "install.ps1",
    }
}

fn tagged_install_script_url_for_platform(tag: &str, platform: HostPlatform) -> String {
    format!(
        "https://raw.githubusercontent.com/{REPO_SLUG}/{tag}/{}",
        release_installer_script_name_for_platform(platform)
    )
}

fn cargo_permission_guidance(version: &Version, binary_path: &Path) -> String {
    format!(
        "the current install target {} is not writable by this user.\nRerun `{}` in the account or shell environment that owns that install.",
        display_path(binary_path),
        cargo_update_command(version).join(" ")
    )
}

fn release_permission_guidance(tag: &str, install_dir: &Path) -> String {
    format!(
        "the current install target {} is not writable by this user.\nRerun `{}` in a shell with permission to write there.",
        display_path(install_dir),
        manual_release_update_command(tag, install_dir)
    )
}

fn unsupported_upgrade_message(
    latest: &LatestRelease,
    install: &UnsupportedInstall,
    platform: HostPlatform,
) -> String {
    match install {
        UnsupportedInstall::CargoPath {
            binary_path,
            source,
        } => format!(
            "automatic upgrades only support crates.io cargo installs.\nThe current binary {} was installed from `{source}`.\nReinstall it from that original Cargo path source.",
            display_path(binary_path),
        ),
        UnsupportedInstall::CargoGit {
            binary_path,
            source,
        } => format!(
            "automatic upgrades only support crates.io cargo installs.\nThe current binary {} was installed from `{source}`.\nReinstall it from that original Cargo git source.",
            display_path(binary_path),
        ),
        UnsupportedInstall::CargoOther {
            binary_path,
            source,
        } => format!(
            "automatic upgrades do not support the current Cargo install source for {}.\nDetected source: `{source}`.\nUpdate it manually using that original installation method.",
            display_path(binary_path),
        ),
        UnsupportedInstall::Ambiguous { binary_path } => {
            let install_dir = binary_path.parent().unwrap_or_else(|| Path::new("."));
            format!(
                "could not determine how {} was installed.\nUpdate it manually using the original installation method.\nCommon commands:\n  {}\n  {}",
                display_path(binary_path),
                cargo_update_command(&latest.version).join(" "),
                manual_release_update_command_for_platform(&latest.tag, install_dir, platform),
            )
        }
    }
}

fn manual_release_update_command(tag: &str, install_dir: &Path) -> String {
    manual_release_update_command_for_platform(tag, install_dir, current_host_platform())
}

fn manual_release_update_command_for_platform(
    tag: &str,
    install_dir: &Path,
    platform: HostPlatform,
) -> String {
    match platform {
        HostPlatform::Unix => format!(
            "curl -fsSL {} | bash -s -- --version {} --install-dir {}",
            shell_quote(&tagged_install_script_url_for_platform(tag, platform)),
            shell_quote(tag),
            shell_quote(&install_dir.to_string_lossy()),
        ),
        HostPlatform::Windows => format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -Command \"& {{ $script = Join-Path $env:TEMP 'nodus-install.ps1'; Invoke-WebRequest {} -OutFile $script; & $script -Version {} -InstallDir {}; Remove-Item $script }}\"",
            powershell_quote(&tagged_install_script_url_for_platform(tag, platform)),
            powershell_quote(tag),
            powershell_quote(&install_dir.to_string_lossy()),
        ),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn release_installer_invocation(script_path: &Path) -> InstallerInvocation {
    release_installer_invocation_for_platform(script_path, current_host_platform())
}

fn release_installer_invocation_for_platform(
    script_path: &Path,
    platform: HostPlatform,
) -> InstallerInvocation {
    match platform {
        HostPlatform::Unix => InstallerInvocation {
            program: "bash",
            action: "bash",
            script_path: script_path.to_path_buf(),
            base_args: Vec::new(),
            version_flag: "--version",
            install_dir_flag: "--install-dir",
        },
        HostPlatform::Windows => InstallerInvocation {
            program: "powershell",
            action: "powershell",
            script_path: script_path.to_path_buf(),
            base_args: vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
            ],
            version_flag: "-Version",
            install_dir_flag: "-InstallDir",
        },
    }
}

fn state_path(store_root: &Path) -> PathBuf {
    store_root.join(STATE_FILE)
}

fn releases_latest_url() -> String {
    format!("https://github.com/{REPO_SLUG}/releases/latest")
}

fn install_url() -> String {
    format!("https://github.com/{REPO_SLUG}#install")
}

fn install_marker_path(binary_path: &Path) -> PathBuf {
    binary_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(INSTALL_MARKER_FILE)
}

fn load_release_install_marker(path: &Path) -> Option<ReleaseInstallMarker> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn resolve_cargo_home() -> Option<PathBuf> {
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        let cargo_home = PathBuf::from(cargo_home);
        if cargo_home.is_absolute() {
            return Some(cargo_home);
        }
    }

    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo"))
}

fn load_cargo_install_sources(cargo_home: &Path, bin_name: &str) -> Vec<String> {
    let json_path = cargo_home.join(".crates2.json");
    if let Ok(contents) = fs::read_to_string(&json_path) {
        if let Ok(state) = serde_json::from_str::<CargoInstallState>(&contents) {
            let sources = state
                .installs
                .into_iter()
                .filter_map(|(package_id, install)| {
                    install
                        .bins
                        .iter()
                        .any(|bin| bin == bin_name)
                        .then_some(package_id)
                })
                .filter_map(|package_id| parse_cargo_source(&package_id))
                .collect::<Vec<_>>();
            if !sources.is_empty() {
                return sources;
            }
        }
    }

    let toml_path = cargo_home.join(".crates.toml");
    if let Ok(contents) = fs::read_to_string(&toml_path) {
        if let Ok(state) = toml::from_str::<LegacyCargoInstallState>(&contents) {
            return state
                .v1
                .into_iter()
                .filter_map(|(package_id, bins)| {
                    bins.iter().any(|bin| bin == bin_name).then_some(package_id)
                })
                .filter_map(|package_id| parse_cargo_source(&package_id))
                .collect();
        }
    }

    Vec::new()
}

fn parse_cargo_source(package_id: &str) -> Option<String> {
    let open = package_id.rfind(" (")?;
    package_id
        .strip_suffix(')')?
        .get(open + 2..)
        .map(ToOwned::to_owned)
}

fn canonicalize_or_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn load_state(path: &Path) -> Result<UpdateCheckState> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(UpdateCheckState::default())
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn persist_state(path: &Path, state: &UpdateCheckState) -> Result<()> {
    let contents =
        serde_json::to_vec_pretty(state).context("failed to serialize update check state")?;
    crate::store::write_atomic(path, &contents)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::process::Command as ProcessCommand;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::report::{ColorMode, Reporter};

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn reporter_with_buffer() -> (Reporter, SharedBuffer) {
        let buffer = SharedBuffer::default();
        let reporter = Reporter::sink(ColorMode::Never, buffer.clone());
        (reporter, buffer)
    }

    fn host_binary_name() -> &'static str {
        executable_file_name(current_host_platform())
    }

    fn host_installer_name() -> &'static str {
        release_installer_script_name_for_platform(current_host_platform())
    }

    fn test_options(current_exe: PathBuf) -> CheckOptions {
        CheckOptions {
            now_unix_secs: CHECK_INTERVAL_SECS,
            current_exe,
            current_version: Version::parse("0.5.0").unwrap(),
            cargo_home: None,
        }
    }

    fn write_release_marker(binary_path: &Path) {
        fs::create_dir_all(binary_path.parent().unwrap()).unwrap();
        fs::write(
            install_marker_path(binary_path),
            serde_json::to_vec_pretty(&ReleaseInstallMarker {
                install_method: "github_release".into(),
                repo_slug: REPO_SLUG.into(),
                binary_name: BIN_NAME.into(),
                binary_path: binary_path.to_path_buf(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    fn read_state(path: &Path) -> UpdateCheckState {
        let contents = fs::read_to_string(path).unwrap();
        serde_json::from_str(&contents).unwrap()
    }

    fn script_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")
    }

    fn powershell_script_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("install.ps1")
    }

    #[cfg(not(target_os = "windows"))]
    fn pwsh_available() -> bool {
        ProcessCommand::new("pwsh")
            .args([
                "-NoProfile",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn parses_release_tags_with_or_without_a_v_prefix() {
        assert_eq!(
            parse_release_version("v0.5.1").unwrap(),
            Version::parse("0.5.1").unwrap()
        );
        assert_eq!(
            parse_release_version("0.5.1").unwrap(),
            Version::parse("0.5.1").unwrap()
        );
        assert!(parse_release_version("release-0.5.1").is_none());
    }

    #[test]
    fn round_trips_update_check_state() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = state_path(temp.path());
        let state = UpdateCheckState {
            last_attempted_at_unix_secs: Some(42),
            latest_known_tag: Some("v0.5.1".into()),
            latest_known_version: Some("0.5.1".into()),
            last_notified_tag: Some("v0.5.1".into()),
        };

        persist_state(&path, &state).unwrap();

        assert_eq!(load_state(&path).unwrap(), state);
    }

    #[test]
    fn detects_registry_cargo_installs_from_crates2_json() {
        let temp = tempfile::TempDir::new().unwrap();
        let cargo_home = temp.path().join(".cargo");
        fs::create_dir_all(cargo_home.join("bin")).unwrap();
        fs::write(
            cargo_home.join(".crates2.json"),
            r#"{"installs":{"nodus 0.5.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["nodus"]}}}"#,
        )
        .unwrap();
        let binary_path = cargo_home.join("bin").join(host_binary_name());
        let mut options = test_options(binary_path.clone());
        options.cargo_home = Some(cargo_home);

        assert_eq!(
            detect_install_target(&options),
            InstallTarget::CargoRegistry { binary_path }
        );
    }

    #[test]
    fn rejects_cargo_path_installs_for_upgrade() {
        let temp = tempfile::TempDir::new().unwrap();
        let cargo_home = temp.path().join(".cargo");
        fs::create_dir_all(cargo_home.join("bin")).unwrap();
        fs::write(
            cargo_home.join(".crates2.json"),
            r#"{"installs":{"nodus 0.5.0 (path+file:///tmp/nodus)":{"bins":["nodus"]}}}"#,
        )
        .unwrap();
        let binary_path = cargo_home.join("bin").join(host_binary_name());
        let mut options = test_options(binary_path.clone());
        options.cargo_home = Some(cargo_home);

        assert_eq!(
            detect_install_target(&options),
            InstallTarget::Unsupported(UnsupportedInstall::CargoPath {
                binary_path,
                source: "path+file:///tmp/nodus".into(),
            })
        );
    }

    #[test]
    fn detects_release_installs_from_a_marker_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let binary_path = temp.path().join("bin").join(BIN_NAME);
        write_release_marker(&binary_path);

        assert_eq!(
            detect_install_target(&test_options(binary_path.clone())),
            InstallTarget::GithubRelease { binary_path }
        );
    }

    #[test]
    fn falls_back_to_manual_guidance_for_ambiguous_installs() {
        let temp = tempfile::TempDir::new().unwrap();
        let binary_path = temp.path().join("bin").join(BIN_NAME);
        let latest = LatestRelease {
            tag: "v0.5.1".into(),
            version: Version::parse("0.5.1").unwrap(),
        };

        match plan_upgrade(&test_options(binary_path), &latest) {
            PlannedUpgrade::Unsupported { message, .. } => {
                assert!(message.contains("could not determine"));
                assert!(message.contains("cargo install --locked --force nodus --version 0.5.1"));
                assert!(message.contains(host_installer_name()));
            }
            other => panic!("expected unsupported plan, got {other:?}"),
        }
    }

    #[test]
    fn plans_cargo_registry_updates_with_an_exact_version() {
        let temp = tempfile::TempDir::new().unwrap();
        let cargo_home = temp.path().join(".cargo");
        fs::create_dir_all(cargo_home.join("bin")).unwrap();
        fs::write(
            cargo_home.join(".crates2.json"),
            r#"{"installs":{"nodus 0.5.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["nodus"]}}}"#,
        )
        .unwrap();
        let binary_path = cargo_home.join("bin").join(host_binary_name());
        let latest = LatestRelease {
            tag: "v0.5.1".into(),
            version: Version::parse("0.5.1").unwrap(),
        };
        let mut options = test_options(binary_path.clone());
        options.cargo_home = Some(cargo_home);

        assert_eq!(
            plan_upgrade(&options, &latest),
            PlannedUpgrade::CargoRegistry {
                current_version: Version::parse("0.5.0").unwrap(),
                latest,
                binary_path,
                command: vec![
                    "cargo".into(),
                    "install".into(),
                    "--locked".into(),
                    "--force".into(),
                    "nodus".into(),
                    "--version".into(),
                    "0.5.1".into(),
                ],
            }
        );
    }

    #[test]
    fn plans_release_updates_against_the_tagged_installer_script() {
        let temp = tempfile::TempDir::new().unwrap();
        let binary_path = temp.path().join("bin").join(BIN_NAME);
        write_release_marker(&binary_path);
        let latest = LatestRelease {
            tag: "v0.5.1".into(),
            version: Version::parse("0.5.1").unwrap(),
        };

        assert_eq!(
            plan_upgrade(&test_options(binary_path.clone()), &latest),
            PlannedUpgrade::GithubRelease {
                current_version: Version::parse("0.5.0").unwrap(),
                latest,
                binary_path: binary_path.clone(),
                install_dir: binary_path.parent().unwrap().to_path_buf(),
                script_url: tagged_install_script_url_for_platform(
                    "v0.5.1",
                    current_host_platform(),
                ),
            }
        );
    }

    #[test]
    fn plans_windows_release_updates_against_the_powershell_installer() {
        let temp = tempfile::TempDir::new().unwrap();
        let binary_path = temp.path().join("bin").join("nodus.exe");
        write_release_marker(&binary_path);
        let latest = LatestRelease {
            tag: "v0.5.1".into(),
            version: Version::parse("0.5.1").unwrap(),
        };

        assert_eq!(
            plan_upgrade_for_platform(
                &test_options(binary_path.clone()),
                &latest,
                HostPlatform::Windows
            ),
            PlannedUpgrade::GithubRelease {
                current_version: Version::parse("0.5.0").unwrap(),
                latest,
                binary_path: binary_path.clone(),
                install_dir: binary_path.parent().unwrap().to_path_buf(),
                script_url: tagged_install_script_url_for_platform("v0.5.1", HostPlatform::Windows),
            }
        );
    }

    #[test]
    fn builds_a_powershell_manual_update_command_on_windows() {
        let install_dir = Path::new(r"C:\Users\tester\AppData\Local\Programs\nodus\bin");

        let command = manual_release_update_command_for_platform(
            "v0.5.1",
            install_dir,
            HostPlatform::Windows,
        );

        assert!(command.contains("powershell -NoProfile -ExecutionPolicy Bypass"));
        assert!(command.contains("install.ps1"));
        assert!(command.contains("-Version 'v0.5.1'"));
        assert!(
            command.contains(r"-InstallDir 'C:\Users\tester\AppData\Local\Programs\nodus\bin'")
        );
    }

    #[test]
    fn uses_a_powershell_installer_invocation_on_windows() {
        let install_dir = Path::new(r"C:\Users\tester\AppData\Local\Programs\nodus\bin");
        let invocation = release_installer_invocation_for_platform(
            &powershell_script_path(),
            HostPlatform::Windows,
        );

        assert_eq!(invocation.program, "powershell");
        assert_eq!(
            invocation.args_for("v0.5.1", install_dir),
            vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                powershell_script_path().to_string_lossy().into_owned(),
                "-Version".into(),
                "v0.5.1".into(),
                "-InstallDir".into(),
                r"C:\Users\tester\AppData\Local\Programs\nodus\bin".into(),
            ]
        );
    }

    #[test]
    fn notices_suggest_upgrade_for_supported_installs() {
        let temp = tempfile::TempDir::new().unwrap();
        let cargo_home = temp.path().join(".cargo");
        fs::create_dir_all(cargo_home.join("bin")).unwrap();
        fs::write(
            cargo_home.join(".crates2.json"),
            r#"{"installs":{"nodus 0.5.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["nodus"]}}}"#,
        )
        .unwrap();
        let binary_path = cargo_home.join("bin").join(host_binary_name());
        let mut options = test_options(binary_path);
        options.cargo_home = Some(cargo_home);
        let latest = LatestRelease {
            tag: "v0.5.1".into(),
            version: Version::parse("0.5.1").unwrap(),
        };
        let plan = plan_upgrade(&options, &latest);

        assert_eq!(
            upgrade_available_message(&options, &latest, &plan),
            "nodus 0.5.1 is available (current 0.5.0); run `nodus upgrade`"
        );
    }

    #[test]
    fn notices_fall_back_to_install_docs_for_unsupported_installs() {
        let temp = tempfile::TempDir::new().unwrap();
        let binary_path = temp.path().join("bin").join(BIN_NAME);
        let latest = LatestRelease {
            tag: "v0.5.1".into(),
            version: Version::parse("0.5.1").unwrap(),
        };
        let options = test_options(binary_path);
        let plan = plan_upgrade(&options, &latest);

        assert_eq!(
            upgrade_available_message(&options, &latest, &plan),
            format!(
                "nodus 0.5.1 is available (current 0.5.0); see {}",
                install_url()
            )
        );
    }

    #[test]
    fn upgrade_check_reports_when_current_version_is_already_latest() {
        let options = test_options(PathBuf::from("/tmp/nodus"));
        let latest = LatestRelease {
            tag: "v0.5.0".into(),
            version: Version::parse("0.5.0").unwrap(),
        };
        let plan = plan_upgrade(&options, &latest);

        assert_eq!(
            upgrade_available_message(&options, &latest, &plan),
            "nodus 0.5.0 is already current"
        );
    }

    #[test]
    fn notifies_once_for_a_newer_release_and_persists_state() {
        let temp = tempfile::TempDir::new().unwrap();
        let state_file = state_path(temp.path());
        let (reporter, buffer) = reporter_with_buffer();

        maybe_notify_with(
            temp.path(),
            &reporter,
            &test_options(temp.path().join("bin").join(BIN_NAME)),
            || {
                Ok(Some(LatestRelease {
                    tag: "v0.5.1".into(),
                    version: Version::parse("0.5.1").unwrap(),
                }))
            },
        )
        .unwrap();

        assert_eq!(
            buffer.contents(),
            format!(
                "warning: nodus 0.5.1 is available (current 0.5.0); see {}\n",
                install_url()
            )
        );

        let state = read_state(&state_file);
        assert_eq!(state.last_attempted_at_unix_secs, Some(CHECK_INTERVAL_SECS));
        assert_eq!(state.latest_known_tag.as_deref(), Some("v0.5.1"));
        assert_eq!(state.last_notified_tag.as_deref(), Some("v0.5.1"));
    }

    #[test]
    fn skips_remote_probe_when_the_last_attempt_is_recent() {
        let temp = tempfile::TempDir::new().unwrap();
        persist_state(
            &state_path(temp.path()),
            &UpdateCheckState {
                last_attempted_at_unix_secs: Some(100),
                latest_known_tag: Some("v0.5.1".into()),
                latest_known_version: Some("0.5.1".into()),
                last_notified_tag: None,
            },
        )
        .unwrap();
        let (reporter, buffer) = reporter_with_buffer();

        maybe_notify_with(
            temp.path(),
            &reporter,
            &CheckOptions {
                now_unix_secs: 100 + CHECK_INTERVAL_SECS - 1,
                ..test_options(temp.path().join("bin").join(BIN_NAME))
            },
            || panic!("throttled checks should not probe remotely"),
        )
        .unwrap();

        assert_eq!(
            buffer.contents(),
            format!(
                "warning: nodus 0.5.1 is available (current 0.5.0); see {}\n",
                install_url()
            )
        );
    }

    #[test]
    fn does_not_repeat_a_notice_for_the_same_release_tag() {
        let temp = tempfile::TempDir::new().unwrap();
        persist_state(
            &state_path(temp.path()),
            &UpdateCheckState {
                last_attempted_at_unix_secs: Some(0),
                latest_known_tag: Some("v0.5.1".into()),
                latest_known_version: Some("0.5.1".into()),
                last_notified_tag: Some("v0.5.1".into()),
            },
        )
        .unwrap();
        let (reporter, buffer) = reporter_with_buffer();

        maybe_notify_with(
            temp.path(),
            &reporter,
            &CheckOptions {
                now_unix_secs: CHECK_INTERVAL_SECS - 1,
                ..test_options(temp.path().join("bin").join(BIN_NAME))
            },
            || panic!("throttled checks should not probe remotely"),
        )
        .unwrap();

        assert!(buffer.contents().is_empty());
    }

    #[test]
    fn notifies_again_when_a_newer_release_than_the_last_notice_appears() {
        let temp = tempfile::TempDir::new().unwrap();
        persist_state(
            &state_path(temp.path()),
            &UpdateCheckState {
                last_attempted_at_unix_secs: Some(0),
                latest_known_tag: Some("v0.5.1".into()),
                latest_known_version: Some("0.5.1".into()),
                last_notified_tag: Some("v0.5.1".into()),
            },
        )
        .unwrap();
        let (reporter, buffer) = reporter_with_buffer();

        maybe_notify_with(
            temp.path(),
            &reporter,
            &CheckOptions {
                now_unix_secs: CHECK_INTERVAL_SECS,
                ..test_options(temp.path().join("bin").join(BIN_NAME))
            },
            || {
                Ok(Some(LatestRelease {
                    tag: "v0.5.2".into(),
                    version: Version::parse("0.5.2").unwrap(),
                }))
            },
        )
        .unwrap();

        assert_eq!(
            buffer.contents(),
            format!(
                "warning: nodus 0.5.2 is available (current 0.5.0); see {}\n",
                install_url()
            )
        );
        assert_eq!(
            read_state(&state_path(temp.path()))
                .last_notified_tag
                .as_deref(),
            Some("v0.5.2")
        );
    }

    #[test]
    fn does_not_notify_when_current_version_is_up_to_date() {
        let temp = tempfile::TempDir::new().unwrap();
        let (reporter, buffer) = reporter_with_buffer();
        let mut options = test_options(temp.path().join("bin").join(BIN_NAME));
        options.current_version = Version::parse("0.3.4").unwrap();

        maybe_notify_with(temp.path(), &reporter, &options, || {
            Ok(Some(LatestRelease {
                tag: "v0.3.4".into(),
                version: Version::parse("0.3.4").unwrap(),
            }))
        })
        .unwrap();

        assert!(buffer.contents().is_empty());
    }

    #[test]
    fn does_not_notify_when_the_probe_returns_no_release() {
        let temp = tempfile::TempDir::new().unwrap();
        let (reporter, buffer) = reporter_with_buffer();

        maybe_notify_with(
            temp.path(),
            &reporter,
            &test_options(temp.path().join("bin").join(BIN_NAME)),
            || Ok(None),
        )
        .unwrap();

        assert!(buffer.contents().is_empty());
    }

    #[test]
    fn updates_last_attempt_time_even_when_the_probe_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let state_file = state_path(temp.path());
        let (reporter, buffer) = reporter_with_buffer();

        maybe_notify_with(
            temp.path(),
            &reporter,
            &CheckOptions {
                now_unix_secs: 123,
                ..test_options(temp.path().join("bin").join(BIN_NAME))
            },
            || anyhow::bail!("network unavailable"),
        )
        .unwrap();

        assert!(buffer.contents().is_empty());
        assert_eq!(
            read_state(&state_file).last_attempted_at_unix_secs,
            Some(123)
        );
    }

    #[test]
    fn extracts_the_latest_release_from_redirect_headers() {
        let headers = "\
HTTP/2 302 \r\n\
location: https://github.com/nodus-rs/nodus/releases/tag/v0.3.4\r\n\
\r\n\
HTTP/2 200 \r\n\
\r\n";

        assert_eq!(
            last_location_header(headers).as_deref(),
            Some("https://github.com/nodus-rs/nodus/releases/tag/v0.3.4")
        );
        let release = parse_latest_release_from_location(
            "https://github.com/nodus-rs/nodus/releases/tag/v0.3.4?foo=bar",
        )
        .unwrap();
        assert_eq!(release.tag, "v0.3.4");
        assert_eq!(release.version, Version::parse("0.3.4").unwrap());
    }

    #[test]
    fn release_urls_are_derived_from_the_repo_slug() {
        assert_eq!(
            releases_latest_url(),
            format!("https://github.com/{REPO_SLUG}/releases/latest")
        );
        assert_eq!(
            install_url(),
            format!("https://github.com/{REPO_SLUG}#install")
        );
        assert_eq!(
            tagged_install_script_url_for_platform("v0.3.4", HostPlatform::Unix),
            format!("https://raw.githubusercontent.com/{REPO_SLUG}/v0.3.4/install.sh")
        );
        assert_eq!(
            tagged_install_script_url_for_platform("v0.3.4", HostPlatform::Windows),
            format!("https://raw.githubusercontent.com/{REPO_SLUG}/v0.3.4/install.ps1")
        );
    }

    #[test]
    fn install_script_writes_and_removes_the_release_install_marker() {
        let temp = tempfile::TempDir::new().unwrap();
        let fake_bin = temp.path().join("fake-bin");
        let install_dir = temp.path().join("install");
        let asset_root = temp
            .path()
            .join("asset")
            .join("nodus-v0.3.4-x86_64-unknown-linux-gnu");
        let asset_path = temp
            .path()
            .join("nodus-v0.3.4-x86_64-unknown-linux-gnu.tar.gz");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&asset_root).unwrap();
        fs::write(asset_root.join(BIN_NAME), "#!/usr/bin/env bash\nexit 0\n").unwrap();
        let tar_status = ProcessCommand::new("tar")
            .args(["-czf", asset_path.to_str().unwrap(), "-C"])
            .arg(temp.path().join("asset"))
            .arg(asset_root.file_name().unwrap())
            .status()
            .unwrap();
        assert!(tar_status.success());

        fs::write(
            fake_bin.join("uname"),
            "#!/usr/bin/env bash\ncase \"$1\" in\n  -s) printf 'Linux\\n' ;;\n  -m) printf 'x86_64\\n' ;;\n  *) printf 'unexpected uname args: %s\\n' \"$*\" >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        fs::write(
            fake_bin.join("curl"),
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\noutput=''\nprev=''\nurl=''\nfor arg in \"$@\"; do\n  if [ \"$prev\" = '-o' ]; then\n    output=\"$arg\"\n    prev=''\n    continue\n  fi\n  case \"$arg\" in\n    -o) prev='-o' ;;\n    http://*|https://*) url=\"$arg\" ;;\n  esac\ndone\ncase \"$url\" in\n  *nodus-v0.3.4-x86_64-unknown-linux-gnu.tar.gz)\n    cp {} \"$output\"\n    ;;\n  *)\n    printf 'unexpected curl url: %s\\n' \"$url\" >&2\n    exit 1\n    ;;\nesac\n",
                shell_quote(&asset_path.to_string_lossy())
            ),
        )
        .unwrap();
        for helper in ["uname", "curl"] {
            let status = ProcessCommand::new("chmod")
                .args(["+x", fake_bin.join(helper).to_str().unwrap()])
                .status()
                .unwrap();
            assert!(status.success());
        }

        let path = format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap());
        let install_output = ProcessCommand::new("bash")
            .arg(script_path())
            .args(["--version", "v0.3.4", "--install-dir"])
            .arg(&install_dir)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            install_output.status.success(),
            "{}",
            String::from_utf8_lossy(&install_output.stderr)
        );

        let marker_path = install_dir.join(INSTALL_MARKER_FILE);
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&marker_path).unwrap()).unwrap();
        assert_eq!(marker["install_method"], "github_release");
        assert_eq!(marker["repo_slug"], REPO_SLUG);
        assert_eq!(marker["binary_name"], BIN_NAME);
        let marker_binary_path = PathBuf::from(marker["binary_path"].as_str().unwrap());
        assert_eq!(
            canonicalize_or_identity(&marker_binary_path),
            canonicalize_or_identity(&install_dir.join(BIN_NAME))
        );

        let uninstall_output = ProcessCommand::new("bash")
            .arg(script_path())
            .args(["--uninstall", "--install-dir"])
            .arg(&install_dir)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            uninstall_output.status.success(),
            "{}",
            String::from_utf8_lossy(&uninstall_output.stderr)
        );
        assert!(!install_dir.join(BIN_NAME).exists());
        assert!(!marker_path.exists());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn install_script_handles_windows_release_assets_from_msys_shells() {
        let temp = tempfile::TempDir::new().unwrap();
        let fake_bin = temp.path().join("fake-bin");
        let install_dir = temp.path().join("install");
        let asset_root = temp.path().join("asset");
        let asset_path = temp.path().join("nodus-v0.3.4-x86_64-pc-windows-msvc.zip");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&asset_root).unwrap();
        fs::write(asset_root.join("nodus.exe"), "windows-binary").unwrap();
        fs::write(asset_root.join("README.md"), "readme").unwrap();
        fs::write(asset_root.join("LICENSE"), "license").unwrap();
        let zip_status = ProcessCommand::new("zip")
            .args([
                "-qr",
                asset_path.to_str().unwrap(),
                "nodus.exe",
                "README.md",
                "LICENSE",
            ])
            .current_dir(&asset_root)
            .status()
            .unwrap();
        assert!(zip_status.success());

        fs::write(
            fake_bin.join("uname"),
            "#!/usr/bin/env bash\ncase \"$1\" in\n  -s) printf 'MINGW64_NT-10.0\\n' ;;\n  -m) printf 'x86_64\\n' ;;\n  *) printf 'unexpected uname args: %s\\n' \"$*\" >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        fs::write(
            fake_bin.join("curl"),
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\noutput=''\nprev=''\nurl=''\nfor arg in \"$@\"; do\n  if [ \"$prev\" = '-o' ]; then\n    output=\"$arg\"\n    prev=''\n    continue\n  fi\n  case \"$arg\" in\n    -o) prev='-o' ;;\n    http://*|https://*) url=\"$arg\" ;;\n  esac\ndone\ncase \"$url\" in\n  *nodus-v0.3.4-x86_64-pc-windows-msvc.zip)\n    cp {} \"$output\"\n    ;;\n  *)\n    printf 'unexpected curl url: %s\\n' \"$url\" >&2\n    exit 1\n    ;;\nesac\n",
                shell_quote(&asset_path.to_string_lossy())
            ),
        )
        .unwrap();
        fs::write(
            fake_bin.join("cygpath"),
            "#!/usr/bin/env bash\ncase \"$1\" in\n  -w|-u) printf '%s\\n' \"$2\" ;;\n  *) printf 'unexpected cygpath args: %s\\n' \"$*\" >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        for helper in ["uname", "curl", "cygpath"] {
            let status = ProcessCommand::new("chmod")
                .args(["+x", fake_bin.join(helper).to_str().unwrap()])
                .status()
                .unwrap();
            assert!(status.success());
        }

        let path = format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap());
        let install_output = ProcessCommand::new("bash")
            .arg(script_path())
            .args(["--version", "v0.3.4", "--install-dir"])
            .arg(&install_dir)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            install_output.status.success(),
            "{}",
            String::from_utf8_lossy(&install_output.stderr)
        );

        let marker_path = install_dir.join(INSTALL_MARKER_FILE);
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&marker_path).unwrap()).unwrap();
        assert_eq!(marker["binary_name"], BIN_NAME);
        let marker_binary_path = PathBuf::from(marker["binary_path"].as_str().unwrap());
        assert_eq!(
            canonicalize_or_identity(&marker_binary_path),
            canonicalize_or_identity(&install_dir.join("nodus.exe"))
        );
        assert!(install_dir.join("nodus.exe").exists());

        let uninstall_output = ProcessCommand::new("bash")
            .arg(script_path())
            .args(["--uninstall", "--install-dir"])
            .arg(&install_dir)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            uninstall_output.status.success(),
            "{}",
            String::from_utf8_lossy(&uninstall_output.stderr)
        );
        assert!(!install_dir.join("nodus.exe").exists());
        assert!(!marker_path.exists());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn install_script_handles_windows_arm64_release_assets_from_msys_shells() {
        let temp = tempfile::TempDir::new().unwrap();
        let fake_bin = temp.path().join("fake-bin");
        let install_dir = temp.path().join("install");
        let asset_root = temp.path().join("asset");
        let asset_path = temp.path().join("nodus-v0.3.4-aarch64-pc-windows-msvc.zip");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&asset_root).unwrap();
        fs::write(asset_root.join("nodus.exe"), "windows-binary").unwrap();
        fs::write(asset_root.join("README.md"), "readme").unwrap();
        fs::write(asset_root.join("LICENSE"), "license").unwrap();
        let zip_status = ProcessCommand::new("zip")
            .args([
                "-qr",
                asset_path.to_str().unwrap(),
                "nodus.exe",
                "README.md",
                "LICENSE",
            ])
            .current_dir(&asset_root)
            .status()
            .unwrap();
        assert!(zip_status.success());

        fs::write(
            fake_bin.join("uname"),
            "#!/usr/bin/env bash\ncase \"$1\" in\n  -s) printf 'MINGW64_NT-10.0\\n' ;;\n  -m) printf 'arm64\\n' ;;\n  *) printf 'unexpected uname args: %s\\n' \"$*\" >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        fs::write(
            fake_bin.join("curl"),
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\noutput=''\nprev=''\nurl=''\nfor arg in \"$@\"; do\n  if [ \"$prev\" = '-o' ]; then\n    output=\"$arg\"\n    prev=''\n    continue\n  fi\n  case \"$arg\" in\n    -o) prev='-o' ;;\n    http://*|https://*) url=\"$arg\" ;;\n  esac\ndone\ncase \"$url\" in\n  *nodus-v0.3.4-aarch64-pc-windows-msvc.zip)\n    cp {} \"$output\"\n    ;;\n  *)\n    printf 'unexpected curl url: %s\\n' \"$url\" >&2\n    exit 1\n    ;;\nesac\n",
                shell_quote(&asset_path.to_string_lossy())
            ),
        )
        .unwrap();
        fs::write(
            fake_bin.join("cygpath"),
            "#!/usr/bin/env bash\ncase \"$1\" in\n  -w|-u) printf '%s\\n' \"$2\" ;;\n  *) printf 'unexpected cygpath args: %s\\n' \"$*\" >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        for helper in ["uname", "curl", "cygpath"] {
            let status = ProcessCommand::new("chmod")
                .args(["+x", fake_bin.join(helper).to_str().unwrap()])
                .status()
                .unwrap();
            assert!(status.success());
        }

        let path = format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap());
        let install_output = ProcessCommand::new("bash")
            .arg(script_path())
            .args(["--version", "v0.3.4", "--install-dir"])
            .arg(&install_dir)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            install_output.status.success(),
            "{}",
            String::from_utf8_lossy(&install_output.stderr)
        );

        let marker_path = install_dir.join(INSTALL_MARKER_FILE);
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&marker_path).unwrap()).unwrap();
        assert_eq!(marker["binary_name"], BIN_NAME);
        let marker_binary_path = PathBuf::from(marker["binary_path"].as_str().unwrap());
        assert_eq!(
            canonicalize_or_identity(&marker_binary_path),
            canonicalize_or_identity(&install_dir.join("nodus.exe"))
        );
        assert!(install_dir.join("nodus.exe").exists());

        let uninstall_output = ProcessCommand::new("bash")
            .arg(script_path())
            .args(["--uninstall", "--install-dir"])
            .arg(&install_dir)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            uninstall_output.status.success(),
            "{}",
            String::from_utf8_lossy(&uninstall_output.stderr)
        );
        assert!(!install_dir.join("nodus.exe").exists());
        assert!(!marker_path.exists());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn powershell_install_script_handles_windows_release_assets_from_flat_zip_root() {
        if !pwsh_available() {
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        let install_dir = temp.path().join("install");
        let asset_dir = temp.path().join("assets");
        fs::create_dir_all(&asset_dir).unwrap();

        fs::write(asset_dir.join("nodus.exe"), "windows-binary").unwrap();
        fs::write(asset_dir.join("README.md"), "readme").unwrap();
        fs::write(asset_dir.join("LICENSE"), "license").unwrap();

        let asset_name = "nodus-v0.3.4-aarch64-pc-windows-msvc.zip";
        let asset_path = asset_dir.join(asset_name);
        let compress_cmd = format!(
            "Compress-Archive -Path 'nodus.exe','README.md','LICENSE' -DestinationPath '{}' -Force",
            asset_path.to_string_lossy().replace('\'', "''")
        );
        let zip_status = ProcessCommand::new("pwsh")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &compress_cmd,
            ])
            .current_dir(&asset_dir)
            .status()
            .unwrap();
        assert!(zip_status.success());

        let script_body = fs::read_to_string(powershell_script_path()).unwrap();
        let patched_script = script_body
            .replacen(
                "Invoke-WebRequest -Uri $Url -OutFile $OutputPath",
                "Copy-Item -LiteralPath (Join-Path $env:NODUS_TEST_ASSET_DIR (Split-Path -Leaf $Url)) -Destination $OutputPath -Force",
                1,
            )
            .replacen(
                "$Target = Resolve-Target",
                "$Target = 'aarch64-pc-windows-msvc'",
                1,
            );
        assert_ne!(patched_script, script_body);

        let test_script_path = temp.path().join("install-test.ps1");
        fs::write(&test_script_path, patched_script).unwrap();

        let install_output = ProcessCommand::new("pwsh")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &test_script_path.to_string_lossy(),
                "-Version",
                "v0.3.4",
                "-InstallDir",
                &install_dir.to_string_lossy(),
            ])
            .env("NODUS_TEST_ASSET_DIR", &asset_dir)
            .output()
            .unwrap();
        assert!(
            install_output.status.success(),
            "{}",
            String::from_utf8_lossy(&install_output.stderr)
        );

        let marker_path = install_dir.join(INSTALL_MARKER_FILE);
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&marker_path).unwrap()).unwrap();
        assert_eq!(marker["binary_name"], BIN_NAME);
        let marker_binary_path = PathBuf::from(marker["binary_path"].as_str().unwrap());
        assert_eq!(
            canonicalize_or_identity(&marker_binary_path),
            canonicalize_or_identity(&install_dir.join("nodus.exe"))
        );
        assert!(install_dir.join("nodus.exe").exists());

        let uninstall_output = ProcessCommand::new("pwsh")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &test_script_path.to_string_lossy(),
                "-Uninstall",
                "-InstallDir",
                &install_dir.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(
            uninstall_output.status.success(),
            "{}",
            String::from_utf8_lossy(&uninstall_output.stderr)
        );
        assert!(!install_dir.join("nodus.exe").exists());
        assert!(!marker_path.exists());
    }
}
