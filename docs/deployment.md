# Deployment

On a Linux server with systemd, 2 timers run the CLI. The units are in `deploy/systemd/`:

| Unit | When | What |
| ---- | ---- | ---- |
| `hairball-sync.timer` | every hour, at minute 5 | starts `hairball-sync.service` |
| `hairball-sync.service` | | `sync` (incremental), then `export` |
| `hairball-sync-full.timer` | every night, at 03:30 | starts `hairball-sync-full.service` |
| `hairball-sync-full.service` | | `sync --full`, then `export` |

The paths in the units:

| Path | Content | Owner, mode |
| ---- | ------- | ----------- |
| `/usr/local/bin/hairball` | the binary | `root`, `0755` |
| `/etc/hairball/config.toml` | the config | `root:hairball`, `0640` |
| `/etc/hairball/secrets.env` | the secrets | `root:root`, `0600` |
| `/var/lib/hairball/` | the database (systemd creates it) | `hairball` |
| `/srv/hairball/calendar.json` | the export, for the web server | `hairball` |

## Design

- The 2 services write to the same SQLite file, so they use the same lock (`flock /run/lock/hairball.lock`). If the hourly sync starts during the nightly sync, the hourly sync waits.
- The export runs in `ExecStopPost`, not in `ExecStartPost`. Thus the export runs when a source fails. In this case, `sync` exits with code 1, but the other sources have new data.
- systemd reads `secrets.env` as root before it starts the service. The file stays `root:root 0600`, and the `hairball` user never reads it.
- The working directory is `/var/lib/hairball`. This directory must not contain a `.env` file.
- The services run as the system user `hairball`, in a sandbox. The file system is read-only, but not the state directory, `/srv/hairball` and the lock.

## Install

1. Install the build tools. `lastfm-client` 5.1.0 uses OpenSSL on Linux, so the build needs `pkg-config` and `libssl-dev`. The time zones need `tzdata`.

   ```bash
   sudo apt install build-essential pkg-config libssl-dev tzdata
   ```

2. Build and install the binary on the server:

   ```bash
   cargo build --release && sudo install -m 0755 target/release/hairball /usr/local/bin/hairball
   ```

3. Create the system user:

   ```bash
   sudo useradd --system --no-create-home --shell /usr/sbin/nologin hairball
   ```

4. Create the export directory. Give the web server read access to this directory, for example with the group `www-data`:

   ```bash
   sudo install -d -o hairball -g www-data -m 0750 /srv/hairball
   ```

5. Write the config. Use absolute paths for the database and the export. The units run the CLI without `--out`, so the export goes to `out` in `[export]`:

   ```toml
   database = "/var/lib/hairball/hairball.db"

   [export]
   out = "/srv/hairball/calendar.json"
   ```

6. Install the config and the secrets:

   ```bash
   sudo install -d -m 0755 /etc/hairball
   ```

   ```bash
   sudo install -o root -g hairball -m 0640 config.toml /etc/hairball/config.toml
   ```

   ```bash
   sudo install -o root -g root -m 0600 .env /etc/hairball/secrets.env
   ```

7. Install the units and make sure that they are correct:

   ```bash
   sudo cp deploy/systemd/hairball-sync* /etc/systemd/system/ && sudo systemd-analyze verify /etc/systemd/system/hairball-sync*.service /etc/systemd/system/hairball-sync*.timer
   ```

8. Start the first full sync manually (the backfill):

   ```bash
   sudo systemctl daemon-reload && sudo systemctl start hairball-sync-full.service
   ```

9. Read its log:

   ```bash
   journalctl -u hairball-sync-full.service -e
   ```

10. Check the secrets with the config and the environment of the service. `systemd-run` reads `secrets.env` like the service, so the secrets are never on a command line:

    ```bash
    sudo systemd-run --uid=hairball --gid=hairball -p EnvironmentFile=/etc/hairball/secrets.env -p WorkingDirectory=/var/lib/hairball --pipe --wait /usr/local/bin/hairball --config /etc/hairball/config.toml check
    ```

11. Enable the timers:

    ```bash
    sudo systemctl enable --now hairball-sync.timer hairball-sync-full.timer
    ```

## Operate

- Next runs: `systemctl list-timers 'hairball*'`.
- Logs: `journalctl -u hairball-sync.service -e`. Under systemd, there is no progress bar, only log lines.
- State of each source: `sudo -u hairball /usr/local/bin/hairball --config /etc/hairball/config.toml status`.
- Update: build the new binary, then install it again (step 2). The next run applies the new database migrations.
- Backup: copy `/var/lib/hairball/hairball.db` when no sync runs, for example with `flock /run/lock/hairball.lock cp ...`.
