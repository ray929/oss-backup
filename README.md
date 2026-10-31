# oss-backup

Back up local directories and MySQL / PostgreSQL / SQLite databases to Alibaba Cloud OSS.

Archives are compressed with `zstd`, encrypted with `age`, and uploaded as a single stream: no temp archive file, no reading whole files into memory. Memory does not grow with the size of the backup — buffers are bounded by the multipart chunk size, plus a fixed ~64 MiB scrypt working set while an archive's key is derived.

## Features

- Four payload types — file trees, MySQL, PostgreSQL, SQLite hot copies — combinable inside one `backup` entry
- JSONC config: `//` comments and trailing commas are allowed
- Layered settings: `global` → `destination` → `backup`
- A backup may omit `destination` and falls back to the first one; when several backups share one destination, each must set `name`, which becomes a path segment
- Split a config across files by putting `{"import": "path"}` inside the `backup` array
- Only one instance runs at a time, even with different config files
- File trees: `walkdir` → glob exclude → `tar` → `zstd` → `age` → OSS, compressed while uploading
  - directory-name rules such as `node_modules` or `target` match at **any depth**, not just the backup root
  - wildcard rules such as `*.tmp` also match across directories
- Databases: `mysqldump` / `pg_dump` / `sqlite3 .backup` output goes straight into compression, no tar layer
  - `db.table` selects individual tables for both MySQL and PostgreSQL; `db` or `db.*` dumps the whole database
  - dump commands are bounded by `timeout_secs` and are killed instead of hanging forever
- Full snapshots, with "keep the newest N runs" retention
- `manifest.json` is written last and acts as the "this run is complete" marker
- Any failure calls `abort()`, including a failed finalize, so no half-visible object is left on OSS
- Config keys are validated strictly: a misspelled field is an error, not a silent no-op
- Per-process lock, optional webhook alert, explicit exit codes

## Requirements

- Rust 1.85+ (to build from source)
- `mysqldump` / `pg_dump` / `sqlite3` for the payload types you use; full paths can be set through `global.*_path`

## Environment Variables

None. Every credential — the OSS access key pair, the age passphrase, database passwords — is read from the config file, and the log level comes from `global.log_level` only. `RUST_LOG` is deliberately ignored, so that "which credential and which setting was actually used" is never ambiguous.

## Build

```bash
cargo build --release
```

Static Windows binary, with no MSVC runtime dependency:

```powershell
$env:RUSTFLAGS='-C target-feature=+crt-static'
cargo build --release
```

Tagged builds (`v*`) are cross-compiled by CI for `x86_64-unknown-linux-musl` and `x86_64-pc-windows-msvc`.

## Usage

```bash
cp config.example.jsonc config.jsonc
# edit config.jsonc: endpoint, bucket, access keys, age passphrase

chmod 600 config.jsonc   # Linux: the file holds AK/SK and passphrases
# icacls "config.jsonc" /inheritance:r /grant:r "%USERNAME%":F   # Windows

oss-backup --config config.jsonc --dry-run    # validate config and connectivity, print planned keys
oss-backup --config config.jsonc              # run the backup
oss-backup --config config.jsonc --background # also log to oss-backup.log next to the config file
```

### Exit Codes

| Code | Meaning |
|---|---|
| 0 | all backups succeeded |
| 1 | at least one backup entry failed |
| 2 | config / credential / file-permission error |
| 3 | another instance is already running |

## Object Layout

```
{prefix}/{timestamp}/            # a single backup on this destination
{prefix}/{name}/{timestamp}/     # several backups share this destination
├── files.tar.zst.age             # file trees
├── {database}.mysql.zst.age      # MySQL, one object per database
├── {database}.pgsql.zst.age      # PostgreSQL
├── {encoded_path}.sqlite.zst.age # SQLite, full path encoded
└── manifest.json                 # written last
```

- `destination` and `backup` are sibling arrays. A backup without `destination` uses the first one; if several backups end up on the same destination, **each must set `name`**, and `name` becomes the directory between `prefix` and the timestamp. This is what keeps their retention counters apart.
- SQLite objects encode the full path, so same-named files in different directories never collide:
  - `D:\data\app.db` → `D__data_app.db.sqlite.zst.age`
  - `_` is escaped as `__`, so `D:\a\app.db` and `D:\a_app.db` stay distinct
  - names longer than 180 bytes fall back to `{stem}-{hash}.sqlite.zst.age`
- A run directory without `manifest.json` is treated as incomplete and is not counted by retention.

## Retention

`retain_count` counts **run directories, per backup entry**: the newest N complete runs under that entry's prefix are kept and the rest are deleted one by one. `0` means never delete.

OSS lifecycle rules are not used — they can express "keep N days" but not "keep the newest N runs".

## Encryption

Artifacts are encrypted with [age](https://age-encryption.org/) in passphrase mode (scrypt) and are compatible with the `age` and `rage` CLIs.

The passphrase comes from `age_passphrase_file` when set, otherwise from the plaintext `age_passphrase` (a warning is logged at startup).

The scrypt work factor is pinned at `log_n = 16` (about 64 MiB and a few hundred milliseconds) instead of age's runtime-tuned default, so peak memory stays predictable. The parameter is recorded in each file's header, so decryption is unaffected and existing archives remain readable.

## Manual Restore

There is no restore command; the artifacts are standard formats and can be opened offline.

```bash
# file trees
age --decrypt -o files.tar.zst files.tar.zst.age
zstd -d files.tar.zst
tar -xf files.tar                      # entries look like D__Documents/a.txt

# databases
age --decrypt -o blog.sql.zst blog.mysql.zst.age
zstd -d blog.sql.zst                   # import blog.sql into the target database
```

Both require the age passphrase configured in `config.jsonc`.

## Security

- `config.jsonc` is the only credential store. Keep it at `600` / owner-only: on Linux a looser mode aborts startup, on Windows the ACL is inspected and a tightening command is printed.
- Use a dedicated RAM user scoped to `PutObject`, `AbortMultipartUpload`, `ListObjects` and `DeleteObject` on the target bucket.
- Access keys and passphrases are masked in logs and in `Debug` output (first 4 + last 4 characters).
- Database passwords are passed to child processes through environment variables, never through the command line.

## Debug Commands

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Release Commands

```bash
# bump the version in Cargo.toml first
git tag -a v0.x.y -m "v0.x.y"
git push origin main
git push origin v0.x.y      # triggers the CI build and GitHub release
```

## Limitations

- Full snapshots only; no incremental backup or deduplication
- Alibaba Cloud OSS only
- Backup entries run sequentially, and multipart parts of a single object upload sequentially
- Database dumps depend on external binaries, and database passwords are stored in plaintext in the config file
- A single backup is expected to stay under 50GB (default `part_size_mb = 16`, OSS caps a multipart upload at 10000 parts)
