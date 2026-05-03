# home-manager module: a daily restic backup of $HOME, with sensible
# excludes for macOS junk (caches, Library/Containers, package manager
# state, build artefacts, etc.).
#
# Two scheduling backends, picked automatically:
#
#   1. systeml (https://github.com/cognivore/systeml) if the user has
#      `systeml.enable = true` in their home configuration. Emits a
#      `systemd.user.{services,timers}.backup-home` pair which the
#      systeml daemon picks up and supervises.
#
#   2. launchd otherwise — emits a `launchd.agents.backup-home` entry
#      with `StartCalendarInterval`.
#
# The user provides only the repository URL and a password-fetch command:
#
#   services.backup-home = {
#     enable = true;
#     repo = "rclone:gdrive:backups/myhost";
#     passwordCommand = "${pkgs.passveil}/bin/passveil show restic/backup";
#     # schedule.hour = 14; schedule.minute = 0;  # defaults
#   };

{ config, lib, pkgs, ... }:

let
  cfg = config.services.backup-home;
  homeDir = config.home.homeDirectory;

  excludeFile = pkgs.writeText "restic-home-excludes" (''
    # macOS junk
    ${homeDir}/.Trash
    .DS_Store
    .zcompdump*
    ${homeDir}/.CFUserTextEncoding
    ${homeDir}/.zsh_sessions

    # Caches
    ${homeDir}/.cache
    ${homeDir}/Library/Caches
    ${homeDir}/Library/Logs
    ${homeDir}/Library/HTTPStorages
    ${homeDir}/Library/WebKit

    # Library: large regenerable app data
    ${homeDir}/Library/Developer
    ${homeDir}/Library/Containers/com.utmapp.UTM
    ${homeDir}/Library/Containers/com.docker.docker
    ${homeDir}/Library/Application Support/Steam
    ${homeDir}/Library/Application Support/Claude
    ${homeDir}/Library/Application Support/Google
    ${homeDir}/Library/Application Support/Godot
    ${homeDir}/Library/Application Support/Adobe
    ${homeDir}/Library/Application Support/Battle.net
    ${homeDir}/Library/Application Support/Spotify
    ${homeDir}/Library/Application Support/Chromium
    ${homeDir}/Library/Application Support/CEF
    ${homeDir}/Library/Application Support/com.openai.atlas
    ${homeDir}/Library/Application Support/TorBrowser-Data
    ${homeDir}/Library/Application Support/Deezer
    ${homeDir}/Library/Application Support/Slack
    ${homeDir}/Library/Application Support/Zulip
    ${homeDir}/Library/Application Support/Superhuman

    # macOS SIP/TCC-protected directories (inaccessible without FDA)
    ${homeDir}/Library/Group Containers/group.com.apple.*
    ${homeDir}/Library/HomeKit
    ${homeDir}/Library/IdentityServices
    ${homeDir}/Library/IntelligencePlatform
    ${homeDir}/Library/Mail
    ${homeDir}/Library/Messages
    ${homeDir}/Library/Metadata/CoreSpotlight
    ${homeDir}/Library/Mobile Documents
    ${homeDir}/Library/PersonalizationPortrait
    ${homeDir}/Library/Safari
    ${homeDir}/Library/Sharing
    ${homeDir}/Library/Shortcuts
    ${homeDir}/Library/StatusKit
    ${homeDir}/Library/Suggestions
    ${homeDir}/Library/Trial
    ${homeDir}/Library/Weather
    ${homeDir}/Library/com.apple.aiml.instrumentation

    # Package manager / build caches
    ${homeDir}/.cargo/registry
    ${homeDir}/.local/state/cabal
    ${homeDir}/.local/state/nix
    ${homeDir}/.npm
    ${homeDir}/go/pkg
    ${homeDir}/Library/pnpm
    ${homeDir}/Library/Python
    ${homeDir}/.swiftpm

    # IDE data
    ${homeDir}/.cursor-server

    # Build artifacts (match anywhere in tree)
    node_modules
    target/debug
    target/release
    .direnv
    __pycache__
    *.pyc
    *.o

    # Nix build outputs (match anywhere)
    result

    # Re-downloadable games
    ${homeDir}/Games/GOG
  '' + lib.optionalString (cfg.extraExcludes != [ ]) ''

    # User-supplied excludes
    ${lib.concatStringsSep "\n" cfg.extraExcludes}
  '');

  backupHome = pkgs.writeShellApplication {
    name = "backup-home";
    runtimeInputs = [ pkgs.restic pkgs.rclone pkgs.coreutils ];
    text = ''
      export RESTIC_REPOSITORY=${lib.escapeShellArg cfg.repo}
      export RESTIC_PASSWORD_COMMAND=${lib.escapeShellArg cfg.passwordCommand}

      LOG="${homeDir}/.local/log/backup-home-$(date +%Y-%m-%d_%H%M%S).log"
      mkdir -p "$(dirname "$LOG")"

      echo "=== backup-home started: $(date) ===" | tee -a "$LOG"
      echo "    repo: $RESTIC_REPOSITORY" | tee -a "$LOG"

      # Preflight: verify password access before doing anything.
      if ! eval "$RESTIC_PASSWORD_COMMAND" >/dev/null 2>&1; then
        echo "FATAL: cannot resolve restic password via configured passwordCommand." | tee -a "$LOG" >&2
        exit 1
      fi

      # Bail if another backup is already running.
      if restic list locks --no-lock 2>/dev/null | grep -q .; then
        echo "FATAL: restic repo is locked — another backup is still running." | tee -a "$LOG" >&2
        exit 1
      fi

      # Auto-init on first run.
      if ! restic snapshots --quiet >/dev/null 2>&1; then
        echo "Initializing restic repository..." | tee -a "$LOG"
        restic init 2>&1 | tee -a "$LOG"
      fi

      restic backup ${lib.escapeShellArg "${homeDir}/"} \
        --exclude-file ${lib.escapeShellArg "${excludeFile}"} \
        --verbose=2 2>&1 | tee -a "$LOG"

      restic forget \
        --keep-daily ${toString cfg.retention.daily} \
        --keep-weekly ${toString cfg.retention.weekly} \
        --keep-monthly ${toString cfg.retention.monthly} \
        --keep-yearly ${toString cfg.retention.yearly} \
        --prune 2>&1 | tee -a "$LOG"

      echo "=== backup-home complete: $(date) ===" | tee -a "$LOG"
    '';
  };

  useSysteml =
    if cfg.useSysteml == null
    then config.systeml.enable or false
    else cfg.useSysteml;

  onCalendar = lib.concatStringsSep ":" [
    "*-*-* ${lib.fixedWidthString 2 "0" (toString cfg.schedule.hour)}"
    (lib.fixedWidthString 2 "0" (toString cfg.schedule.minute))
    "00"
  ];
in
{
  options.services.backup-home = {
    enable = lib.mkEnableOption "Daily restic-based backup of the user's home directory.";

    repo = lib.mkOption {
      type = lib.types.str;
      example = "rclone:gdrive:backups/myhost";
      description = ''
        Restic repository URL. Anything `restic` understands works
        (`rclone:remote:path`, `s3:...`, `b2:...`, a local path, etc.).
        Strongly recommend a per-host path so multiple machines don't
        stomp each other.
      '';
    };

    passwordCommand = lib.mkOption {
      type = lib.types.str;
      example = lib.literalExpression ''"''${pkgs.passveil}/bin/passveil show restic/backup"'';
      description = ''
        Shell command that prints the restic repository password to
        stdout. Set as `RESTIC_PASSWORD_COMMAND`. Use a secret-store
        wrapper (passveil, pass, age, …) — never an inline plaintext
        password.
      '';
    };

    extraExcludes = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "\${homeDir}/Mirrors/Computers" ];
      description = ''
        Additional patterns appended to the bundled macOS-junk exclude
        list. Each entry is a single line of restic exclude syntax.
      '';
    };

    schedule = {
      hour = lib.mkOption {
        type = lib.types.ints.between 0 23;
        default = 14;
        description = "Hour of day (0–23) to run the backup.";
      };
      minute = lib.mkOption {
        type = lib.types.ints.between 0 59;
        default = 0;
        description = "Minute of hour (0–59) to run the backup.";
      };
    };

    retention = {
      daily   = lib.mkOption { type = lib.types.ints.unsigned; default =  7; };
      weekly  = lib.mkOption { type = lib.types.ints.unsigned; default =  4; };
      monthly = lib.mkOption { type = lib.types.ints.unsigned; default = 12; };
      yearly  = lib.mkOption { type = lib.types.ints.unsigned; default =  3; };
    };

    useSysteml = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      description = ''
        Force the scheduling backend.

        - `null` (default): pick automatically — systemd user units when
          `config.systeml.enable` is true, launchd otherwise.
        - `true`: always emit systemd user units (requires systeml or a
          real systemd).
        - `false`: always emit a launchd agent.
      '';
    };

    package = lib.mkOption {
      type = lib.types.package;
      readOnly = true;
      description = "The wrapped `backup-home` shell application as a derivation.";
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      services.backup-home.package = backupHome;

      home.packages = [
        pkgs.rclone
        pkgs.restic
        backupHome
      ];
    }

    (lib.mkIf useSysteml {
      systemd.user.services.backup-home = {
        Unit = {
          Description = "Daily home directory backup (restic + rclone)";
        };
        Service = {
          Type = "oneshot";
          ExecStart = "${backupHome}/bin/backup-home";
          StandardOutput = "append:${homeDir}/.local/log/backup-home.stdout.log";
          StandardError  = "append:${homeDir}/.local/log/backup-home.stderr.log";
        };
      };
      systemd.user.timers.backup-home = {
        Unit = {
          Description = "Daily home backup at ${onCalendar}";
        };
        Timer = {
          OnCalendar = onCalendar;
          Persistent = true;
          Unit       = "backup-home.service";
        };
        Install.WantedBy = [ "timers.target" ];
      };
    })

    (lib.mkIf (!useSysteml) {
      launchd.agents.backup-home = {
        enable = true;
        config = {
          ProgramArguments = [ "${backupHome}/bin/backup-home" ];
          StartCalendarInterval = [{
            Hour   = cfg.schedule.hour;
            Minute = cfg.schedule.minute;
          }];
          EnvironmentVariables = {
            HOME = homeDir;
          };
          StandardOutPath  = "${homeDir}/.local/log/backup-home-launchd.stdout.log";
          StandardErrorPath = "${homeDir}/.local/log/backup-home-launchd.stderr.log";
        };
      };
    })
  ]);
}
