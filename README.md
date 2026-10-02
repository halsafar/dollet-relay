
<div align="center">

# dollet-relay

**IPTV Proxy/Transcoder**

<a>
  <img src="docs/dollet_logo.svg" alt="Logo" width="200" height="200">
</a>

[Features](#features) ·
[Quick start](#quick-start) ·
[Configuration](#configuration) ·
[Connecting](#connecting) ·
[Backups](#backups) ·
[Migrating](#migrating) ·
[Missing features](#missing-features) ·
[Development](#development)

</div>


## Why another IPTV Proxy

RAM is expensive.

RAM usage numbers (vary based on setup, buffer settings, and stream resolution/bitrate):
- Idle = 30MB
- Per 1080p 8Mbps stream 
  - ~16MB if proxying
  - ~60MB if transcoding with ffmpeg

**Named after the Dollet Communication Tower in Final Fantasy VIII.**

## Features

- **HDHomeRun emulation** (works with Plex)
- **M3U, Xtream, and XMLTV guide** 
- **Output profiles** 
  - Control globally or per channel how a stream is proxied or transcoded.
- **Group and Channel Management**
  - Auto / Manual sorting, renumbering.
  - Use provider info or override with your own.
  - **Ordered failover.**

## Quick start

Save this as `compose.yml` and run `docker compose up -d` (or `podman-compose`):

```yaml
services:
  dollet-relay:
    image: ghcr.io/halsafar/dollet-relay:latest
    container_name: dollet-relay
    restart: unless-stopped
    ports:
      - "9191:9191"
    volumes:
      - dollet_data:/data
    environment:
      DOLLET_LOG: info
    stop_grace_period: 30s
    healthcheck:
      test: ["CMD", "/usr/local/bin/dollet", "health"]
      interval: 30s
      timeout: 5s
      start_period: 10s

volumes:
  dollet_data:
```

Open `http://localhost:9191/` and create the first administrator.

### With hardware transcoding

Only needed if you use an output profile that re-encodes. 

```yaml
    devices:
      - /dev/dri:/dev/dri          # Intel / AMD VA-API
    group_add:
      - "989"                      # the host's render gid: `getent group render`
```

The device nodes are group-owned on the host, so without `group_add` every
hardware transcode fails with a permission error.

## Configuration

Everything a user changes lives in the Settings page. These are fixed at start:

| Variable | Default | |
|---|---|---|
| `DOLLET_LISTEN` | `0.0.0.0:9191` | |
| `DOLLET_DATA_DIR` | `/data` | database, cache, artwork |
| `DOLLET_ADVERTISED_BASE_URL` | — | overrides the origin in generated URLs |
| `DOLLET_ARTWORK_BASE_URL` | — | overrides it for logo URLs alone, which a browser fetches rather than the server |
| `DOLLET_IMPORT_BACKUP` | — | import a backup on boot, but only while the instance has no users |
| `DOLLET_TRUSTED_PROXIES` | `none` | `ip`, or a CIDR list |
| `DOLLET_LOG` | `info` | `tracing` filter syntax |

### Reverse Proxy

You must set `DOLLET_TRUSTED_PROXIES` to the IP (or network range), otherwise `X-Forwarded-For`, `X-Real-IP`, `X-Forwarded-Host` and `X-Forwarded-Proto` are ignored.

### Health Check

- `dollet health` - reports status, resident memory and WAL size.
- `GET /health` - reports the same as `dollet health` (no authentication required).

## Connecting

The **Connect** page in the web UI attempts to cover all possible scenarios of how clients will connect.

### HDHomeRun Tuner
- Lineup: `http://<host>:9191/hdhr/`.
- Guide: `http://<host>:9191/output/epg`

You can also use profile-scoped paths:

```
/hdhr/<channel_profile>/
/hdhr/<channel_profile>/output_profile/<id>/
/hdhr/output_profile/<id>/
/output/m3u/<channel_profile>
/output/epg/<channel_profile>
```

### Resolving Stream Paths and Artwork

By default Dollet-Relay will provide stream paths and logos based on the destination of the request.

Example:
- https://tv.example.com/hdhr/lineup.json -> returns `https://tv.example.com/*` stream paths
- http://dollet-relay:9191/hdhr/lineup.json -> returns `http://dollet-relay:9191/*` stream paths

You can force a specific stream path domain using `DOLLET_ADVERTISED_BASE_URL`.

You can force a specific artwork/logo domain using `DOLLET_ARTWORK_BASE_URL`.


## Backups

**Settings → Backups.** A backup is one zip of the whole database: channels,
sources, settings, and users with their password hashes and every provider
credential, so keep downloaded ones somewhere private. They are written to
`backups/` in the data directory (`/data/backups/` in the container).

- **Scheduled** every 24 hours by default, keeping the newest 7 scheduled ones.
  An interval of 0 turns them off. Backups taken by hand, uploaded, or taken
  before a restore are never deleted by the schedule.
- **Download** any backup from the list, and **upload** one to restore it here
  or on another machine. An upload is checked before it is kept and refused,
  with the reason, if it is not a backup this version can restore: one from a
  newer version is refused, one from an older version is migrated forward.
- **Restore** replaces everything in this instance with the backup. A backup of
  the instance as it is now is taken first, then the server restarts to apply
  it. Under Compose with `restart: unless-stopped`, as in the quick start, it
  comes straight back. Run any other way, as a bare process or under a
  supervisor that does not restart a process that exits cleanly, it stops and
  has to be started again by hand. A backup from another instance also signs
  everyone out.

## Migrating

### Dispatcharr

**This repo has no affiliation with Dispatcharr**

The importer supports channels, streams, groups, channel profiles, logos,
M3U accounts, EPG sources, guide data, users, password hashes and API keys.

In Dispatcharr: **Settings → Backups** → download the backup `.zip` file.

### Import on first boot

Bind-mount the zip into the container.

```yaml
    environment:
      DOLLET_IMPORT_BACKUP: /data/dispatcharr-backup.zip
    volumes:
      - /path/to/dispatcharr-backup.zip:/data/dispatcharr-backup.zip      
```

Your Dispatcharr username/password combo will now work to log you in.

## Missing Features

- plugins, VOD, DVR/recordings, catch-up/timeshift, Schedules Direct, Comskip, webhooks, HLS and fMP4 output, and LLM EPG matching.

## Development

### Docs

| | |
|---|---|
| [`CLAUDE.md`](CLAUDE.md) | conventions, hard invariants, how agents share this tree |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | why the architecture is what it is |
| [`docs/TESTING.md`](docs/TESTING.md) | the rules the suite is held to |

### Building

```bash
scripts/build.sh
```

### Local Development

```bash
scripts/dev.sh                   # server, host toolchain
scripts/dev.sh web               # vite dev server
```

### Testing
```bash
scripts/test.sh                  # full, use container to build
scripts/test.sh --local          # full, use host to build
scripts/test.sh --local --coverage
scripts/smoke.sh <image>         # run build.sh first to get an image
scripts/e2e.sh                   # playwright testing
```

### Contributing

PRs are welcome.

## Licence

AGPL-3.0-only. See [`LICENSE`](LICENSE).
