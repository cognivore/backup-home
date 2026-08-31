# backup-home

Home Manager module: daily restic + rclone backup of `$HOME` on macOS with retrieval verification and optional replica repositories, plus sensible excludes for the usual junk (caches, container app data, package manager state, build artefacts, TCC-protected directories you can't read anyway).

The operational logic lives in [`backup-home.rs`](./backup-home.rs), a [rust-script](https://rust-script.org/) program. The Nix module is declarative glue only: it generates the exclude file, embeds the serialized configuration into the Rust source, pins the interpreter, and wires up the scheduler.

## Schedule

The module picks its scheduling backend automatically:

- If [`systeml`](https://github.com/cognivore/systeml) is enabled in your home configuration (`systeml.enable = true`), it emits `systemd.user.{services,timers}.backup-home` and lets the systeml daemon supervise.
- Otherwise it emits `launchd.agents.backup-home` with `StartCalendarInterval`.

Force one or the other with `services.backup-home.useSysteml = true | false`.

## Usage

```nix
{
  inputs.backup-home.url = "github:cognivore/backup-home";

  # in your home configuration
  imports = [ inputs.backup-home.homeManagerModules.default ];

  services.backup-home = {
    enable = true;

    # Anything restic understands. Per-host path strongly recommended so
    # multiple machines don't stomp each other.
    repo = "rclone:gdrive:backups/${osConfig.networking.hostName}";

    # Shell command that prints the restic password to stdout. Use a
    # secret store — passveil, pass, age, gpg-agent. Never an inline
    # password literal. Also used for the replicas.
    passwordCommand = "${pkgs.passveil}/bin/passveil show restic/backup";

    # Optional: replica repositories the new snapshot is copied to with
    # `restic copy` on every run. Auto-initialized (with the primary's
    # chunker params) on first contact; same retention policy applied.
    # sftp: replicas need key-only (BatchMode) SSH.
    replicaRepos = [ "sftp:user@host.rsync.net:backups/myhost" ];

    # Optional: retrieval verification (defaults shown). Every run samples
    # `sampleSize` regular files uniformly from the latest snapshot,
    # restores them with --verify before the backup, re-restores the same
    # sample after backup+prune and byte-compares the SHA-256/size
    # manifests, then restores a fresh disjoint sample from the new
    # snapshot — and repeats the equivalent check against every replica.
    verification = { enable = true; sampleSize = 300; };

    # Optional: extra patterns appended to the bundled excludes.
    extraExcludes = [
      "${config.home.homeDirectory}/Mirrors"
    ];

    # Optional: time of day. Defaults to 14:00.
    schedule = { hour = 14; minute = 0; };

    # Optional: drop run logs and manifests older than this at the start of
    # every run. `--verbose=2` writes one line per file, so an unpruned log
    # directory grows by a couple of hundred megabytes a day. 0 disables.
    logRetentionDays = 30;

    # Optional: small JSON document rewritten at the start and end of every
    # run, for desktop monitors. Empty string disables it.
    statusFile = "${config.home.homeDirectory}/.local/state/backup-home/status.json";

    # Optional: retention policy passed to `restic forget`.
    retention = { daily = 7; weekly = 4; monthly = 12; yearly = 3; };
  };
}
```

`sampleSize` is floored at 300: sampling 300 files without replacement gives at least `1 - 0.99^300 = 95.1%` probability of catching a bad file when 1% or more of the snapshot's regular files are unretrievable. Snapshots with fewer eligible files are tested in full.

## What a run does

1. Preflight: resolve the password, bail if the repo is locked, auto-`init` on first run.
2. Pre-backup retrieval test: sample from the latest snapshot, `restic restore --verify` into a temp dir, record a SHA-256/size manifest (written next to the log).
3. `restic backup $HOME/ --exclude-file <generated> --verbose=2`, then `restic forget --keep-daily/weekly/monthly/yearly --prune`.
4. `restic copy` the new snapshot to every replica; apply the same retention there. (Copied snapshots get new IDs — the program resolves them via the `original` field.)
5. Post-backup: restore the *same* pre-backup sample from the *same* old snapshot again and compare manifests byte-for-byte; restore a fresh disjoint sample from the new snapshot; run the equivalent retrieval check against every replica.
6. A failed pre-check never blocks the backup. All stage failures are collected and the run exits nonzero if any stage failed — after all useful work was attempted.
7. Rewrite `statusFile` and delete run logs older than `logRetentionDays`.

## Backup vs. recovery

Stage failures are classified into two independent halves, because they answer
different questions and fail for different reasons:

- **backup** — `backup`, `snapshot listing`, `prune`, `replica sync`. Did we
  store it?
- **recovery** — `pre-backup *`, `post-backup *`, `new-snapshot *`,
  `replica retrieval *`. Can we get it back?

A snapshot can save perfectly while the replica prune trips over a stale lock,
and a repository can keep accepting writes long after it stopped being
readable. Both halves are reported separately in the log:

```text
=== recovery: OK — 4 check(s) passed, 0 failed, 1200 file(s) restored and compared ===
=== backup-home complete: 2026-08-31 15:48:24 +0100 (all stages OK) ===
```

and in `statusFile`:

```json
{
  "schema": 1,
  "state": "finished",
  "started_at": "2026-08-31 15:00:02 +0100",
  "backup":   { "status": "ok", "snapshot": "093147cd", "last_ok_unix": 1756652904 },
  "recovery": { "status": "ok", "checks_ok": 4, "checks_failed": 0,
                "files_verified": 1200, "last_ok_unix": 1756652904 }
}
```

`status` is one of `running`, `ok`, `failed`, `untested` (verification was on
but nothing could be checked — a first run, say) or `disabled`. `last_ok_*` is
carried forward across runs that did not succeed, so a monitor can answer "how
long since this last actually worked?" rather than only "did today's run
pass?".

## What gets backed up

`$HOME`, minus a fairly aggressive default exclude list:

- macOS junk: `.Trash`, `.DS_Store`, `.zcompdump*`, `.CFUserTextEncoding`, `.zsh_sessions`
- Caches: `~/.cache`, `~/Library/{Caches,Logs,HTTPStorages,WebKit}`
- Regenerable app data: `~/Library/Containers/com.{utmapp.UTM,docker.docker}`, Steam, Claude, Google, Adobe, Battle.net, Spotify, Chromium, CEF, OpenAI Atlas, TorBrowser-Data, Deezer, Slack, Zulip, Superhuman
- macOS SIP/TCC-protected directories you can't read without Full Disk Access anyway: `~/Library/{HomeKit,IdentityServices,Mail,Messages,Mobile Documents,Safari,Sharing,...}`
- Package manager state: `~/.cargo/registry`, `~/.npm`, `~/go/pkg`, `~/Library/{pnpm,Python}`, `~/.swiftpm`
- Build artefacts (matched anywhere in the tree): `node_modules`, `target/{debug,release}`, `.direnv`, `__pycache__`, `*.pyc`, `*.o`, `result`
- Re-downloadable games: `~/Games/GOG`

Add to this list with `services.backup-home.extraExcludes`. Read the [home-manager.nix](./nix/home-manager.nix) source for the full default list.

## Logs

- Per-run log: `~/.local/log/backup-home-YYYY-MM-DD_HHMMSS.log` (pruned after `logRetentionDays`, and excluded from the backup itself)
- Sample manifests: `~/.local/log/backup-home-YYYY-MM-DD_HHMMSS-{pre-old,post-old,post-new}-snapshot.json` and `...-replica-N.json`
- Scheduler output is retained too: systeml journal, or `~/.local/log/backup-home-launchd.{stdout,stderr}.log` under launchd.

## Manual run

The module exposes the program as a package:

```sh
backup-home   # already on $PATH after enabling
```

## Development

```sh
nix shell nixpkgs#rust-script nixpkgs#cargo nixpkgs#rustc nixpkgs#restic \
  -c rust-script --test backup-home.rs
```

Unit tests cover restic JSON parsing, snapshot selection, copied-snapshot resolution, include-pattern escaping, sampling, manifest comparison, and failure aggregation. Two offline end-to-end tests run the full workflow (twice, so pre- and post-check paths execute) against temporary local restic repositories, including a negative test that corrupts a pack file and asserts the run fails without suppressing the backup.

## License

MIT.
