# scoop-rs

A fast, native-Rust client for the [Scoop](https://github.com/ScoopInstaller/scoop) package ecosystem, with a **pacman-style local database**.

scoop-rs is a **pure client-side rewrite**: it consumes the same buckets, the same
JSON manifests, and the same install layout as PowerShell Scoop. **The upstream
Scoop repos and bucket contents are never modified and never need to be** - we only
read from them (git clone / git pull) and write into our own directories.

```
$ scoop-rs search 7zip          # ~35 ms cold start, served from the local SQLite index
$ scoop-rs install aria2        # parallel download, sha256 verified, ~1.3 s total
$ scoop-rs status               # instant, no PowerShell, no git
```

## Why

PowerShell Scoop is slow for structural reasons, not because it is badly written:

| Cost in PS Scoop | scoop-rs answer |
|---|---|
| ~0.5-2 s PowerShell profile/module boot for *every* invocation | Native binary, ~10-40 ms total for read-only ops |
| `Invoke-WebRequest` per file, serial | Parallel download engine (4 workers) with HTTP Range resume |
| Re-reads + re-parses every bucket JSON on search | Manifests indexed into SQLite at bucket-sync time |
| Full bucket clones / fetches | Shallow single-branch clones (`--depth 1`) |
| JSON parsing in PS for every query | serde (Rust) parse only when a manifest is actually needed |

PowerShell is still launched **only** when a manifest genuinely requires it
(`installer.script`, `pre_install`, `post_install`, `uninstall.script`,
Start-Menu shortcuts) - the hybrid approach keeps 100% bucket compatibility
while making the common path completely PS-free.

## Features

- **Core package ops**: `install`, `uninstall`, `update`, `list`, `search`,
  `info`, `status`
- **Bucket management**: `bucket add/list/known/remove` (shallow clones,
  ff-only pulls)
- **pacman-style local database** (SQLite at `<scoop>\db\scoop-rs.db`):
  - `packages` - what is installed, from which bucket, when (like pacman's
    `local/<pkg>/desc`)
  - `package_files` - installed-file lists (`scoop-rs list --files`, like `-Ql`)
  - `package_deps` - dependency graph with **orphan reporting** after uninstall
  - `package_shims` - exact shim cleanup
  - `buckets` + `search_index` - mtime-tracked manifest cache for instant
    search/status
  - `meta` - schema version + migrations (tested)
- **Parallel downloads** with resume (`.part` files + HTTP Range), streaming
  SHA256/MD5 verification, progress bars, shared cache with PS Scoop
  (`app#version#filename` naming)
- **Coexistence with PowerShell Scoop**: same root resolution (`SCOOP` env >
  config `root_path` > `~\scoop`), same buckets/apps/shims/cache/persist
  layout, first run **imports existing PS-Scoop installs** into the DB,
  reads upstream `shim.exe` if present (and writes its `name.shim` metadata
  format), writes `install.json` exactly like Scoop does
- **Extraction in pure Rust**: zip, 7z, tar.gz, tar (zip-slip protected);
  msi/innosetup/nullsoft installers executed on Windows with scoop's default
  silent arguments
- **Persist support** with junctions/hardlinks, so user data survives updates
- **Manifest schema checker** with helpful typo diagnostics
  (`scoop-rs info <app> --validate`, `scoop-rs validate <file>`) - the known-field
  list is calibrated against the *real* official buckets (incl. `##` comment
  convention, `psmodule`, `pre_uninstall`, ...)
- **Version comparison** tuned for scoop realities: `1.2.10 > 1.2.9`,
  prerelease ordering (`alpha < beta < pre < rc < release`), rolling tags
  (`nightly`, `dev`, `canary`, ...) rank above releases and compare by date
- Machine-readable output: `--json` on list/search/info/status
- pacman-style **mutual exclusion lock** (`<root>\db\scoop-rs.lock`) so two
  installs never interleave

## Install

### From source (any platform)

```
cargo install --path .
```

### Windows (MSVC, recommended)

```
rustup target add x86_64-pc-windows-msvc
cargo build --release
# binary: target/release/scoop-rs.exe
```

### Windows exe via CI

This repo ships `.github/workflows/ci.yml` which builds and uploads the
Windows x64 exe as an artifact on every push.

### Cross-compiling the Windows exe from Linux/macOS

```
rustup target add x86_64-pc-windows-gnu
# Debian/Ubuntu: sudo apt install mingw-w64
# (or extract the mingw-w64 debs into a user prefix without root)
cargo build --release --target x86_64-pc-windows-gnu
# binary: target/x86_64-pc-windows-gnu/release/scoop-rs.exe
```

The TLS stack is chosen per target at compile time: **schannel** on Windows
(same stack PS Scoop uses) and **rustls** elsewhere, so OpenSSL is never
required on any platform.

## Usage

```
scoop-rs bucket add main            # official map, shallow clone
scoop-rs bucket add extras
scoop-rs search <query>             # instant, from the SQLite index
scoop-rs info <app>                 # + --validate for schema check
scoop-rs install <app>              # deps resolved & installed first
scoop-rs install <bucket>/<app>     # bucket-qualified
scoop-rs install ./manifest.json    # local manifest file
scoop-rs list [--json] [--files]    # pacman -Q / -Ql style
scoop-rs status [--json]            # installed vs available
scoop-rs update                     # sync buckets + show updates
scoop-rs update <app> ... [--all]   # apply updates (old versions removed)
scoop-rs uninstall <app> [--purge]  # persist kept unless --purge
scoop-rs validate manifest.json     # schema check a bucket manifest
```

### The local database

Located at `<scoop root>\db\scoop-rs.db` (a directory upstream Scoop never
touches). Schema version is tracked in `meta` and upgraded by tested
migrations. The DB is the pacman-like source of truth for uninstall/cleanup;
`install.json` in each app dir stays byte-compatible with PS Scoop.

### Coexistence notes

- Both clients can be used side by side; they share buckets and the download
  cache, and each tracks its own installs (PS Scoop in its own registry/shims,
  scoop-rs in its SQLite DB). Apps installed by PS Scoop are auto-imported
  (read-only) so `list`/`status` see them immediately.
- When both clients have a `<app>\current` junction, they point at the same
  version dir; a later install by either client updates the shared junction.
- bucket repos are only ever read (clone/pull/fetch/reset against origin);
  scoop-rs never commits or pushes - upstream repos stay pristine.

## Architecture

```
src/
  cli.rs         clap definitions
  commands.rs    command handlers (JSON rendering, tables)
  config.rs      root resolution (SCOOP env > config.json > ~\scoop), layout
  db.rs          SQLite database, migrations, queries
  buckets.rs     git sync (shallow), indexing, known-bucket map
  resolve.rs     app resolution (bucket-qualified / bare / local file) + deps
  download.rs    parallel download engine (resume, hash, progress)
  extract.rs     zip / 7z / tar.gz extractors (pure Rust)
  install.rs     install + update pipeline (persist, env, current junction)
  uninstall.rs   uninstall pipeline (scripts, shims, orphans)
  shims.rs       shim creation (.shim/.cmd/.ps1, upstream shim.exe reuse)
  ps.rs          PowerShell bridge (scoop-compatible $dir/$manifest surface)
  envwin.rs      HKCU\Environment PATH/env_set + WM_SETTINGCHANGE broadcast
  manifest.rs    manifest model, arch resolution, schema validator
  version.rs     version comparator (prerelease/nightly aware)
  util.rs        git/hash/fs helpers
tests/
  integration.rs end-to-end: local HTTP server -> install -> update ->
                 uninstall, plus hash-failure, deps, import scenarios
```

## Testing

```
cargo test          # 37 unit + 6 integration tests
cargo clippy        # zero warnings on Linux and Windows targets
```

The integration suite runs a real HTTP server on localhost, builds real zip
payloads in memory, and exercises the complete install -> status -> update ->
uninstall flow including persist retention, cache reuse, hash-mismatch
abort, dependency ordering, and PS-Scoop import.

## Limitations (v1)

- `autoupdate`/`checkver` manifest generation is not implemented (updates
  come from bucket git sync, which is what `scoop update <app>` effectively
  does)
- `scoop alias`, `scoop config set`, global installs (`-g`), and virustotal
  hooks are not implemented
- `.tar.xz` / `.tar.bz2` extraction is not built in (rare on Windows buckets);
  PS-scripted manifests still work via the PowerShell bridge
- Windows-only features (registry PATH edits, junctions, msiexec, shortcuts)
  degrade to friendly no-ops/hints on other platforms

## License

MIT
