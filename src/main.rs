use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use ftp::types::{FileType, FtpError};
use ftp::FtpStream;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Config {
    cpanel_base_url: String,
    cpanel_username: String,
    cpanel_api_token: String,
    cpanel_backup_email: Option<String>,
    cpanel_include_home: bool,
    ftp_host: String,
    ftp_port: u16,
    ftp_username: String,
    ftp_password: String,
    ftp_backup_dir: Option<String>,
    backup_dir: PathBuf,
    poll_interval: Duration,
    backup_timeout: Duration,
    delete_remote_after_download: bool,
    verify_archive: bool,
    expected_archive_entries: Vec<String>,
    retention_keep_last: usize,
    run_mode: RunMode,
    schedule_interval: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunMode {
    Once,
    Daemon,
}

#[derive(Debug, Deserialize)]
struct UapiEnvelope {
    result: UapiResult,
}

#[derive(Debug, Deserialize)]
struct UapiResult {
    status: i32,
    data: Value,
    errors: Option<Vec<Value>>,
    messages: Option<Vec<Value>>,
    warnings: Option<Vec<Value>>,
}

fn main() {
    if let Err(err) = real_main() {
        eprintln!("backup failed: {err:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let config = Config::from_env()?;
    fs::create_dir_all(&config.backup_dir)
        .with_context(|| format!("create backup dir {}", config.backup_dir.display()))?;

    match config.run_mode {
        RunMode::Once => run_once(&config),
        RunMode::Daemon => loop {
            if let Err(err) = run_once(&config) {
                eprintln!("{} run failed: {err:#}", now());
            }
            println!(
                "{} sleeping for {} seconds",
                now(),
                config.schedule_interval.as_secs()
            );
            thread::sleep(config.schedule_interval);
        },
    }
}

impl Config {
    fn from_env() -> Result<Self> {
        let run_mode = match env_opt("RUN_MODE").as_deref() {
            None | Some("once") => RunMode::Once,
            Some("daemon") => RunMode::Daemon,
            Some(other) => bail!("RUN_MODE must be once or daemon, got {other}"),
        };

        let expected_archive_entries = env_opt("EXPECTED_ARCHIVE_ENTRIES")
            .unwrap_or_else(|| "homedir/public_html,mysql/".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        Ok(Self {
            cpanel_base_url: env_req("CPANEL_BASE_URL")?
                .trim_end_matches('/')
                .to_string(),
            cpanel_username: env_req("CPANEL_USERNAME")?,
            cpanel_api_token: env_req("CPANEL_API_TOKEN")?,
            cpanel_backup_email: env_opt("CPANEL_BACKUP_EMAIL"),
            cpanel_include_home: env_bool("CPANEL_INCLUDE_HOME", true)?,
            ftp_host: env_req("CPANEL_FTP_HOST")?,
            ftp_port: env_u16("CPANEL_FTP_PORT", 21)?,
            ftp_username: env_req("CPANEL_FTP_USERNAME")?,
            ftp_password: env_req("CPANEL_FTP_PASSWORD")?,
            ftp_backup_dir: env_opt("CPANEL_FTP_BACKUP_DIR").filter(|v| !v.trim().is_empty()),
            backup_dir: PathBuf::from(
                env_opt("BACKUP_DIR").unwrap_or_else(|| "/backups".to_string()),
            ),
            poll_interval: Duration::from_secs(env_u64("POLL_INTERVAL_SECS", 60)?),
            backup_timeout: Duration::from_secs(env_u64("BACKUP_TIMEOUT_SECS", 7200)?),
            delete_remote_after_download: env_bool("DELETE_REMOTE_AFTER_DOWNLOAD", true)?,
            verify_archive: env_bool("VERIFY_ARCHIVE", true)?,
            expected_archive_entries,
            retention_keep_last: env_usize("RETENTION_KEEP_LAST", 14)?,
            run_mode,
            schedule_interval: Duration::from_secs(env_u64("SCHEDULE_INTERVAL_SECS", 86_400)?),
        })
    }
}

fn run_once(config: &Config) -> Result<()> {
    println!("{} starting backup job", now());
    let client = CpanelClient::new(config);

    let before = client
        .list_backups()
        .context("list backups before trigger")?;
    println!("{} existing cPanel backup files: {}", now(), before.len());

    let pid = client
        .start_full_backup(config)
        .context("trigger cPanel full backup")?;
    println!("{} cPanel full backup started, pid={}", now(), pid);

    let remote_file = wait_for_new_backup(config, &client, &before)
        .context("wait for cPanel backup to complete")?;
    println!("{} new backup detected: {}", now(), remote_file);

    let final_path = config.backup_dir.join(&remote_file);
    let sha256 = download_backup(config, &remote_file, &final_path)
        .with_context(|| format!("download {remote_file}"))?;
    write_sha256(&final_path, &sha256)?;
    println!(
        "{} downloaded {} sha256={}",
        now(),
        final_path.display(),
        sha256
    );

    if config.verify_archive {
        verify_archive_entries(&final_path, &config.expected_archive_entries)
            .with_context(|| format!("verify archive {}", final_path.display()))?;
        println!("{} archive content check passed", now());
    }

    if config.delete_remote_after_download {
        delete_remote_backup(config, &remote_file)
            .with_context(|| format!("delete remote backup {remote_file}"))?;
        println!("{} deleted remote backup {}", now(), remote_file);
    }

    prune_local_backups(config)?;
    println!("{} backup job complete", now());
    Ok(())
}

struct CpanelClient<'a> {
    config: &'a Config,
}

impl<'a> CpanelClient<'a> {
    fn new(config: &'a Config) -> Self {
        Self { config }
    }

    fn start_full_backup(&self, config: &Config) -> Result<String> {
        let homedir = if config.cpanel_include_home {
            "include"
        } else {
            "skip"
        };
        let mut params = vec![("homedir", homedir.to_string())];
        if let Some(email) = &config.cpanel_backup_email {
            params.push(("email", email.clone()));
        }
        let value = self.uapi_get("Backup/fullbackup_to_homedir", &params)?;
        let pid = value
            .get("pid")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Ok(pid)
    }

    fn list_backups(&self) -> Result<Vec<String>> {
        let value = self.uapi_get("Backup/list_backups", &[])?;
        let data = value
            .as_array()
            .ok_or_else(|| anyhow!("Backup/list_backups did not return an array"))?;
        Ok(data
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect())
    }

    fn uapi_get(&self, endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
        let query = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = if query.is_empty() {
            format!("{}/execute/{}", self.config.cpanel_base_url, endpoint)
        } else {
            format!(
                "{}/execute/{}?{}",
                self.config.cpanel_base_url, endpoint, query
            )
        };
        let auth = format!(
            "cpanel {}:{}",
            self.config.cpanel_username, self.config.cpanel_api_token
        );
        let response = minreq::get(url)
            .with_header("Authorization", auth)
            .with_header("Accept", "application/json")
            .with_timeout(30)
            .send()
            .context("send cPanel UAPI request")?;

        let status = response.status_code;
        let body = response
            .as_str()
            .context("read cPanel UAPI response as UTF-8")?;
        if !(200..300).contains(&status) {
            bail!("cPanel UAPI returned HTTP {status}: {body}");
        }

        let envelope: UapiEnvelope =
            serde_json::from_str(body).context("parse cPanel UAPI JSON")?;
        if envelope.result.status != 1 {
            bail!(
                "cPanel UAPI failed: errors={:?} messages={:?} warnings={:?}",
                envelope.result.errors,
                envelope.result.messages,
                envelope.result.warnings
            );
        }
        Ok(envelope.result.data)
    }
}

fn wait_for_new_backup(
    config: &Config,
    client: &CpanelClient<'_>,
    before: &[String],
) -> Result<String> {
    let before_set: HashSet<&str> = before.iter().map(String::as_str).collect();
    let started = Instant::now();
    loop {
        if started.elapsed() > config.backup_timeout {
            bail!(
                "timed out after {} seconds waiting for cPanel backup",
                config.backup_timeout.as_secs()
            );
        }

        let mut candidates = client
            .list_backups()?
            .into_iter()
            .filter(|file| !before_set.contains(file.as_str()))
            .filter(|file| file.ends_with(".tar.gz") || file.ends_with(".tar"))
            .collect::<Vec<_>>();
        candidates.sort();

        if let Some(candidate) = candidates.pop() {
            println!("{} candidate backup file: {}", now(), candidate);
            wait_for_remote_size_to_settle(config, &candidate)?;
            return Ok(candidate);
        }

        println!(
            "{} backup is still running; polling again in {} seconds",
            now(),
            config.poll_interval.as_secs()
        );
        thread::sleep(config.poll_interval);
    }
}

fn wait_for_remote_size_to_settle(config: &Config, remote_file: &str) -> Result<()> {
    let started = Instant::now();
    let mut last_size = None;
    let mut stable_checks = 0;

    loop {
        if started.elapsed() > config.backup_timeout {
            bail!(
                "timed out after {} seconds waiting for FTP size to settle",
                config.backup_timeout.as_secs()
            );
        }

        match read_remote_size(config, remote_file) {
            Ok(size) => {
                if Some(size) == last_size && size > 0 {
                    stable_checks += 1;
                } else {
                    stable_checks = 0;
                    last_size = Some(size);
                }

                if stable_checks >= 1 {
                    println!("{} remote backup size is stable: {} bytes", now(), size);
                    return Ok(());
                }

                println!(
                    "{} remote backup size is {} bytes; waiting {} seconds",
                    now(),
                    size,
                    config.poll_interval.as_secs()
                );
            }
            Err(err) => {
                println!(
                    "{} remote backup size is not available yet: {err:#}; waiting {} seconds",
                    now(),
                    config.poll_interval.as_secs()
                );
            }
        }

        thread::sleep(config.poll_interval);
    }
}

fn read_remote_size(config: &Config, remote_file: &str) -> Result<usize> {
    let mut ftp = ftp_connect(config)?;
    let size = ftp
        .size(remote_file)
        .with_context(|| format!("read remote size for {remote_file}"))?
        .unwrap_or(0);
    let _ = ftp.quit();
    Ok(size)
}

fn download_backup(config: &Config, remote_file: &str, final_path: &Path) -> Result<String> {
    let temp_path = sibling_path_with_suffix(final_path, ".part")?;
    if temp_path.exists() {
        fs::remove_file(&temp_path)
            .with_context(|| format!("remove stale temp file {}", temp_path.display()))?;
    }

    let mut ftp = ftp_connect(config)?;
    let sha256 = ftp
        .retr(remote_file, |stream| {
            let mut out = File::create(&temp_path).map_err(FtpError::ConnectionError)?;
            let mut hasher = Sha256::new();
            let mut buf = [0_u8; 64 * 1024];
            loop {
                let n = stream.read(&mut buf).map_err(FtpError::ConnectionError)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .map_err(FtpError::ConnectionError)?;
                hasher.update(&buf[..n]);
            }
            out.sync_all().map_err(FtpError::ConnectionError)?;
            Ok(format!("{:x}", hasher.finalize()))
        })
        .map_err(|err| anyhow!("FTP RETR failed: {err}"))?;
    let _ = ftp.quit();

    fs::rename(&temp_path, final_path).with_context(|| {
        format!(
            "move temp backup {} to {}",
            temp_path.display(),
            final_path.display()
        )
    })?;
    Ok(sha256)
}

fn ftp_connect(config: &Config) -> Result<FtpStream> {
    let addr = (config.ftp_host.as_str(), config.ftp_port)
        .to_socket_addrs()
        .with_context(|| format!("resolve FTP host {}", config.ftp_host))?
        .next()
        .ok_or_else(|| anyhow!("FTP host {} did not resolve", config.ftp_host))?;
    let mut ftp = FtpStream::connect(addr).context("connect FTP server")?;
    ftp.get_ref()
        .set_read_timeout(Some(Duration::from_secs(120)))
        .context("set FTP read timeout")?;
    ftp.get_ref()
        .set_write_timeout(Some(Duration::from_secs(120)))
        .context("set FTP write timeout")?;
    ftp.login(&config.ftp_username, &config.ftp_password)
        .context("login FTP server")?;
    ftp.transfer_type(FileType::Binary)
        .context("set FTP binary mode")?;
    if let Some(dir) = &config.ftp_backup_dir {
        ftp.cwd(dir)
            .with_context(|| format!("change FTP directory to {dir}"))?;
    }
    Ok(ftp)
}

fn delete_remote_backup(config: &Config, remote_file: &str) -> Result<()> {
    let mut ftp = ftp_connect(config)?;
    ftp.rm(remote_file)
        .with_context(|| format!("FTP remove {remote_file}"))?;
    let _ = ftp.quit();
    Ok(())
}

fn write_sha256(path: &Path, sha256: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid backup filename {}", path.display()))?;
    let sha_path = sibling_path_with_suffix(path, ".sha256")?;
    fs::write(&sha_path, format!("{sha256}  {file_name}\n"))
        .with_context(|| format!("write {}", sha_path.display()))?;
    Ok(())
}

fn verify_archive_entries(path: &Path, expected_entries: &[String]) -> Result<()> {
    if expected_entries.is_empty() {
        return Ok(());
    }

    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut found = vec![false; expected_entries.len()];

    for entry in archive.entries().context("read tar entries")? {
        let entry = entry.context("read tar entry")?;
        let entry_path = entry.path().context("read tar entry path")?;
        let entry_name = entry_path.to_string_lossy();
        for (idx, expected) in expected_entries.iter().enumerate() {
            if entry_name == expected.as_str()
                || entry_name.starts_with(expected.as_str())
                || entry_name.contains(expected.as_str())
            {
                found[idx] = true;
            }
        }
        if found.iter().all(|value| *value) {
            return Ok(());
        }
    }

    let missing = expected_entries
        .iter()
        .zip(found.iter())
        .filter_map(|(name, found)| if *found { None } else { Some(name.as_str()) })
        .collect::<Vec<_>>();
    bail!(
        "archive is missing expected entries: {}",
        missing.join(", ")
    );
}

fn prune_local_backups(config: &Config) -> Result<()> {
    if config.retention_keep_last == 0 {
        return Ok(());
    }

    let mut backups = fs::read_dir(&config.backup_dir)
        .with_context(|| format!("read backup dir {}", config.backup_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !(name.starts_with("backup-") && name.ends_with(".tar.gz")) {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect::<Vec<_>>();

    backups.sort_by_key(|(modified, _)| *modified);
    let delete_count = backups.len().saturating_sub(config.retention_keep_last);
    for (_, path) in backups.into_iter().take(delete_count) {
        println!("{} pruning old local backup {}", now(), path.display());
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        let sha_path = sibling_path_with_suffix(&path, ".sha256")?;
        if sha_path.exists() {
            fs::remove_file(&sha_path).with_context(|| format!("remove {}", sha_path.display()))?;
        }
    }
    Ok(())
}

fn env_req(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("missing required env var {name}"))
}

fn sibling_path_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid filename {}", path.display()))?;
    Ok(path.with_file_name(format!("{file_name}{suffix}")))
}

fn env_opt(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    match env_opt(name).as_deref() {
        None => Ok(default),
        Some("1") | Some("true") | Some("yes") | Some("on") => Ok(true),
        Some("0") | Some("false") | Some("no") | Some("off") => Ok(false),
        Some(other) => bail!("{name} must be boolean, got {other}"),
    }
}

fn env_u16(name: &str, default: u16) -> Result<u16> {
    match env_opt(name) {
        None => Ok(default),
        Some(value) => value
            .parse()
            .with_context(|| format!("{name} must be an integer")),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    match env_opt(name) {
        None => Ok(default),
        Some(value) => value
            .parse()
            .with_context(|| format!("{name} must be an integer")),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize> {
    match env_opt(name) {
        None => Ok(default),
        Some(value) => value
            .parse()
            .with_context(|| format!("{name} must be an integer")),
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}
