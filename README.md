# backup-home

Home Manager module: daily restic + rclone backup of `$HOME` on macOS, with sensible excludes for the usual junk (caches, container app data, package manager state, build artefacts, TCC-protected directories you can't read anyway).

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
    # password literal.
    passwordCommand = "${pkgs.passveil}/bin/passveil show restic/backup";

    # Optional: extra patterns appended to the bundled excludes.
    extraExcludes = [
      "${config.home.homeDirectory}/Mirrors"
    ];

    # Optional: time of day. Defaults to 14:00.
    schedule = { hour = 14; minute = 0; };

    # Optional: retention policy passed to `restic forget`.
    retention = { daily = 7; weekly = 4; monthly = 12; yearly = 3; };
  };
}
```

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

- Per-run log: `~/.local/log/backup-home-YYYY-MM-DD_HHMMSS.log`
- Aggregated stdout / stderr (systeml or launchd captured): `~/.local/log/backup-home{,-launchd}.{stdout,stderr}.log`

## What it actually runs

```
restic backup $HOME/                           \
  --exclude-file <generated>                   \
  --verbose=2

restic forget                                  \
  --keep-daily 7 --keep-weekly 4               \
  --keep-monthly 12 --keep-yearly 3            \
  --prune
```

Both run in the same script so a single restic process holds the repo lock end-to-end. The script bails early if another `restic` is holding the lock, and it auto-runs `restic init` on first call.

## Manual run

The module exposes the wrapper script as a package:

```sh
backup-home   # already on $PATH after enabling
```

## License

MIT.
