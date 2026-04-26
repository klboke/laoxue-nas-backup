# cPanel NAS Backup

Rust backup runner for pulling a cPanel full backup into a NAS-mounted Docker volume.

The container is designed for NAS use:

- triggers a cPanel full backup with UAPI
- polls FTP until the new backup archive is visible
- downloads the archive through FTP into `/backups`
- writes a `.sha256` checksum next to the archive
- optionally verifies expected archive entries, such as `homedir/public_html` and `mysql/example_database.sql`
- optionally deletes the temporary backup archive from the cPanel account
- prunes old local backup archives by count

## Security model

Keep secrets out of Git. Copy `config.example.env` to `.env` on the NAS and fill in the cPanel password or API token and the FTP password there.

The runner uses cPanel UAPI over HTTPS with either BasicAuth (`CPANEL_PASSWORD`) or an API token (`CPANEL_API_TOKEN`). It uses plain FTP for downloading the generated backup file. If the host enables FTPS/SFTP-only access later, add that transport before disabling FTP.

Some cPanel hosts return an empty `Backup/list_backups` response even after the archive has been created. For that reason, completion detection uses FTP directory listing rather than the cPanel backup list API.

## Build

```bash
docker build -t cpanel-nas-backup:latest .
```

The runtime image is `scratch`; it contains only the compiled Rust binary.

The published multi-architecture image is:

```text
ghcr.io/klboke/cpanel-nas-backup:latest
```

## Run once

```bash
cp config.example.env .env
docker compose run --rm cpanel-nas-backup
```

Backups will appear in `./backups` by default.

## Run as a simple daemon

Set this in `.env`:

```env
RUN_MODE=daemon
SCHEDULE_INTERVAL_SECS=86400
```

Then start:

```bash
docker compose up -d
```

For an exact wall-clock schedule, keep `RUN_MODE=once` and use the NAS scheduler to start the container at the desired time.

## Required environment variables

| Variable | Description |
| --- | --- |
| `CPANEL_BASE_URL` | cPanel base URL, for example `https://cpanel.example.com:2083` |
| `CPANEL_USERNAME` | cPanel account username |
| `CPANEL_PASSWORD` | cPanel account password for BasicAuth; leave empty when using `CPANEL_API_TOKEN` |
| `CPANEL_FTP_HOST` | FTP host for the cPanel account |
| `CPANEL_FTP_USERNAME` | FTP username |
| `CPANEL_FTP_PASSWORD` | FTP password |

## Useful optional variables

| Variable | Default | Description |
| --- | --- | --- |
| `BACKUP_DIR` | `/backups` | Local directory inside the container |
| `CPANEL_API_TOKEN` | empty | cPanel API token with access to backup functions; takes precedence over `CPANEL_PASSWORD` |
| `CPANEL_INCLUDE_HOME` | `true` | Include home directory in the full backup |
| `CPANEL_BACKUP_EMAIL` | empty | Optional cPanel completion email |
| `CPANEL_FTP_PORT` | `21` | FTP port |
| `CPANEL_FTP_BACKUP_DIR` | empty | FTP directory containing cPanel full backups |
| `DELETE_REMOTE_AFTER_DOWNLOAD` | `true` | Remove the cPanel-side temporary backup after a verified download |
| `VERIFY_ARCHIVE` | `true` | Check archive entries after download |
| `EXPECTED_ARCHIVE_ENTRIES` | `homedir/public_html,mysql/` | Comma-separated archive path prefixes to verify |
| `RETENTION_KEEP_LAST` | `7` | Keep the latest N local backup archives |
| `POLL_INTERVAL_SECS` | `60` | Poll interval while cPanel creates the backup |
| `BACKUP_TIMEOUT_SECS` | `7200` | Maximum wait time for cPanel backup completion |
| `RUN_MODE` | `once` | `once` or `daemon` |
| `SCHEDULE_INTERVAL_SECS` | `86400` | Sleep interval in daemon mode |

## Restore check

A cPanel full backup should contain at least:

- `homedir/public_html/`
- `mysql/example_database.sql`

The checksum file can be checked with:

```bash
sha256sum -c backup-*.tar.gz.sha256
```
