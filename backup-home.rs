#!/usr/bin/env rust-script
//! backup-home — daily restic backup of $HOME with retrieval verification
//! and replica repositories.
//!
//! The Nix module (nix/home-manager.nix) substitutes the embedded config
//! placeholder below with the serialized `services.backup-home` settings and
//! pins the shebang to the store's rust-script, so the same source doubles as
//! the scheduled program and the manual `backup-home` command.
//!
//! Workflow per run:
//!   1. preflight: password command, primary lock check, auto-init
//!   2. pre-backup retrieval test: uniformly sample `sample_size` regular
//!      files from the latest home snapshot, restore with --verify, record a
//!      SHA-256/size manifest
//!   3. restic backup of $HOME (exclude file, --verbose=2), then
//!      forget --keep-{daily,weekly,monthly,yearly} --prune
//!   4. restic copy of the new snapshot to every replica (bootstrap-inits new
//!      replicas with --copy-chunker-params), same retention applied there
//!   5. post-backup: restore the SAME old-snapshot sample again and compare
//!      manifests byte-for-byte; restore a fresh disjoint sample from the new
//!      snapshot; run the equivalent retrieval check against every replica
//!   6. aggregate stage failures — a failed pre-check never blocks the
//!      backup, but any failed stage makes the whole run exit nonzero
//!
//! ```cargo
//! [dependencies]
//! anyhow = "1"
//! chrono = "0.4"
//! rand = "0.9"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! tempfile = "3"
//! ```

use anyhow::{bail, Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

// Replaced by the Nix module with builtins.toJSON of the module config. The
// r## guard means the JSON may freely contain quotes and backslashes.
const EMBEDDED_CONFIG_JSON: &str = r##"@BACKUP_HOME_CONFIG_JSON@"##;

// ---------------------------------------------------------------------------
// Configuration

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Retention {
    daily: u32,
    weekly: u32,
    monthly: u32,
    yearly: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Verification {
    enable: bool,
    sample_size: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Config {
    home: String,
    repo: String,
    password_command: String,
    exclude_file: String,
    #[serde(default)]
    replica_repos: Vec<String>,
    retention: Retention,
    verification: Verification,
    log_dir: String,
    #[serde(default = "default_restic_bin")]
    restic_bin: String,
    // Extra args appended to `restic backup`. Unset in production; the e2e
    // tests use it to backdate snapshots (`--time`) so same-day retention
    // doesn't prune the previous test snapshot.
    #[serde(default)]
    extra_backup_args: Vec<String>,
}

fn default_restic_bin() -> String {
    "restic".to_string()
}

// ---------------------------------------------------------------------------
// Restic JSON types

#[derive(Debug, Clone, Deserialize)]
struct Snapshot {
    id: String,
    time: String,
    #[serde(default)]
    paths: Vec<String>,
    // Set by `restic copy` on the destination snapshot: the source snapshot's
    // ID. Copied snapshots get a NEW id, so replica lookups must match either.
    #[serde(default)]
    original: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RestoredFile {
    path: String,
    sha256: String,
    size: u64,
}

// ---------------------------------------------------------------------------
// Logging: every line goes to stdout (scheduler journal / launchd stdout
// file) and to the per-run log under ~/.local/log/.

struct Logger {
    file: Mutex<fs::File>,
    // log_dir/backup-home-<ts>, no extension; the log itself is
    // <prefix>.log, manifests are written as <prefix>-<name>.json.
    prefix: PathBuf,
    log_path: PathBuf,
}

impl Logger {
    fn create(log_dir: &Path) -> Result<Logger> {
        fs::create_dir_all(log_dir)
            .with_context(|| format!("cannot create log directory {}", log_dir.display()))?;
        let ts = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
        let prefix = log_dir.join(format!("backup-home-{ts}"));
        let log_path = PathBuf::from(format!("{}.log", prefix.display()));
        let file = fs::File::create(&log_path)
            .with_context(|| format!("cannot create log file {}", log_path.display()))?;
        Ok(Logger { file: Mutex::new(file), prefix, log_path })
    }

    fn line(&self, msg: &str) {
        println!("{msg}");
        let _ = std::io::stdout().flush();
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }

    fn manifest_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("{}-{name}.json", self.prefix.display()))
    }
}

// ---------------------------------------------------------------------------
// Process plumbing. std::process::Command only — no shell anywhere.

fn strs(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn args_preview(args: &[String]) -> String {
    if args.len() <= 10 {
        args.join(" ")
    } else {
        format!("{} ... ({} args total)", args[..8].join(" "), args.len())
    }
}

fn restic_cmd(cfg: &Config, repo: &str, args: &[String]) -> Command {
    let mut cmd = Command::new(&cfg.restic_bin);
    cmd.arg("--repo").arg(repo);
    cmd.args(args);
    cmd.env("RESTIC_PASSWORD_COMMAND", &cfg.password_command);
    cmd.stdin(Stdio::null());
    cmd
}

/// Run restic and capture stdout (small outputs: snapshots, lock lists).
/// Stderr is appended to the log.
fn run_restic_capture(cfg: &Config, log: &Logger, repo: &str, args: &[String]) -> Result<String> {
    let out = restic_cmd(cfg, repo, args)
        .output()
        .with_context(|| format!("cannot spawn {}", cfg.restic_bin))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    for l in stderr.lines() {
        log.line(&format!("  [restic!] {l}"));
    }
    if !out.status.success() {
        let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
        bail!(
            "restic {} (repo {repo}) failed with {}: {}",
            args_preview(args),
            out.status,
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run restic streaming stdout line-by-line into `on_stdout` while stderr is
/// streamed into the log. Used both for logged operations (backup, restore,
/// copy — callback writes to the log) and for `ls --json`, where the callback
/// feeds the sampler and the potentially millions of lines stay out of the log.
fn run_restic_streaming(
    cfg: &Config,
    log: &Logger,
    repo: &str,
    args: &[String],
    on_stdout: &mut dyn FnMut(&str),
) -> Result<()> {
    log.line(&format!("+ restic {}", args_preview(args)));
    let mut child = restic_cmd(cfg, repo, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot spawn {}", cfg.restic_bin))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_tail: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for line in BufReader::new(stderr).lines().map_while(|r| r.ok()) {
                log.line(&format!("  [restic!] {line}"));
                let mut tail = stderr_tail.lock().unwrap();
                if tail.len() >= 20 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
        for line in BufReader::new(stdout).lines().map_while(|r| r.ok()) {
            on_stdout(&line);
        }
    });
    let status = child.wait().context("wait for restic")?;
    if !status.success() {
        let tail = stderr_tail.lock().unwrap();
        let tail = tail.iter().cloned().collect::<Vec<_>>().join(" | ");
        bail!(
            "restic {} (repo {repo}) failed with {status}{}",
            args_preview(args),
            if tail.is_empty() { String::new() } else { format!(": {tail}") }
        );
    }
    Ok(())
}

/// run_restic_streaming with stdout appended to the log (the common case).
fn run_restic_logged(cfg: &Config, log: &Logger, repo: &str, args: &[String]) -> Result<()> {
    run_restic_streaming(cfg, log, repo, args, &mut |l| log.line(&format!("  [restic] {l}")))
}

// ---------------------------------------------------------------------------
// Password preflight. Mirrors restic's own RESTIC_PASSWORD_COMMAND handling
// (shell-style word splitting, direct exec — no shell). The output is only
// checked for presence, never logged.

fn split_command_words(input: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut has_word = false;
    let mut quote = Quote::None;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match quote {
            Quote::None => match c {
                '\'' => {
                    quote = Quote::Single;
                    has_word = true;
                }
                '"' => {
                    quote = Quote::Double;
                    has_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        has_word = true;
                    }
                }
                c if c.is_whitespace() => {
                    if has_word {
                        words.push(std::mem::take(&mut current));
                        has_word = false;
                    }
                }
                other => {
                    current.push(other);
                    has_word = true;
                }
            },
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(c);
                }
            }
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '\\' => match chars.next() {
                    Some(n @ ('"' | '\\' | '$' | '`')) => current.push(n),
                    Some(n) => {
                        current.push('\\');
                        current.push(n);
                    }
                    None => {}
                },
                other => current.push(other),
            },
        }
    }
    if has_word {
        words.push(current);
    }
    words
}

fn check_password(cfg: &Config) -> Result<()> {
    let words = split_command_words(&cfg.password_command);
    let (bin, rest) = words
        .split_first()
        .context("passwordCommand is empty")?;
    let out = Command::new(bin)
        .args(rest)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot execute password command {bin}"))?;
    if !out.status.success() {
        bail!("password command failed with {}", out.status);
    }
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        bail!("password command produced no output");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot listing and selection

fn snapshots(cfg: &Config, log: &Logger, repo: &str) -> Result<Vec<Snapshot>> {
    let out = run_restic_capture(cfg, log, repo, &strs(&["snapshots", "--json"]))?;
    let parsed: Option<Vec<Snapshot>> =
        serde_json::from_str(out.trim()).context("parse `restic snapshots --json` output")?;
    Ok(parsed.unwrap_or_default())
}

fn snapshot_time_key(s: &Snapshot) -> (i64, String) {
    let nanos = chrono::DateTime::parse_from_rfc3339(&s.time)
        .map(|d| {
            d.timestamp_nanos_opt()
                .unwrap_or_else(|| d.timestamp().saturating_mul(1_000_000_000))
        })
        .unwrap_or(i64::MIN);
    (nanos, s.time.clone())
}

fn latest_home_snapshot(snaps: &[Snapshot], home: &str) -> Option<Snapshot> {
    let home = home.trim_end_matches('/');
    snaps
        .iter()
        .filter(|s| s.paths.iter().any(|p| p.trim_end_matches('/') == home))
        .max_by_key(|s| snapshot_time_key(s))
        .cloned()
}

/// `restic copy` gives the destination snapshot a new ID and records the
/// source ID in `original`. Resolve a snapshot in a (possibly replica)
/// repository by either identity.
fn resolve_copied_snapshot<'a>(snaps: &'a [Snapshot], source_id: &str) -> Option<&'a Snapshot> {
    snaps
        .iter()
        .find(|s| s.id == source_id || s.original.as_deref() == Some(source_id))
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

// ---------------------------------------------------------------------------
// Uniform sampling of regular files from `restic ls SNAPSHOT --json` (NDJSON).
// Reservoir sampling (Algorithm R) is uniform-without-replacement like
// shuffle+truncate, but needs only O(sample) memory against multi-million-file
// snapshots. If fewer than `k` eligible files exist, all of them are kept.

struct Sampler {
    k: usize,
    exclude: HashSet<String>,
    eligible: usize,
    reservoir: Vec<String>,
}

impl Sampler {
    fn new(k: usize, exclude: HashSet<String>) -> Sampler {
        Sampler { k, exclude, eligible: 0, reservoir: Vec::new() }
    }

    fn offer_line(&mut self, line: &str, rng: &mut impl Rng) {
        let Some(path) = parse_file_node(line) else { return };
        if self.exclude.contains(&path) {
            return;
        }
        self.eligible += 1;
        if self.reservoir.len() < self.k {
            self.reservoir.push(path);
        } else {
            let j = rng.random_range(0..self.eligible);
            if j < self.k {
                self.reservoir[j] = path;
            }
        }
    }

    fn finish(self) -> (Vec<String>, usize) {
        (self.reservoir, self.eligible)
    }
}

/// One NDJSON line from `restic ls --json` -> path, iff it is a regular-file
/// node. Newer restic tags records with `message_type`, older with
/// `struct_type`; accept either.
fn parse_file_node(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let tag = v
        .get("struct_type")
        .and_then(|t| t.as_str())
        .or_else(|| v.get("message_type").and_then(|t| t.as_str()))?;
    if tag != "node" {
        return None;
    }
    if v.get("type").and_then(|t| t.as_str()) != Some("file") {
        return None;
    }
    Some(v.get("path")?.as_str()?.to_string())
}

fn sample_regular_files(
    cfg: &Config,
    log: &Logger,
    repo: &str,
    snapshot_id: &str,
    k: usize,
    exclude: &HashSet<String>,
) -> Result<(Vec<String>, usize)> {
    let mut rng = rand::rng();
    let mut sampler = Sampler::new(k, exclude.clone());
    let args = strs(&["ls", snapshot_id, "--json"]);
    run_restic_streaming(cfg, log, repo, &args, &mut |line| sampler.offer_line(line, &mut rng))?;
    let (mut sample, eligible) = sampler.finish();
    sample.sort();
    if eligible < k {
        log.line(&format!(
            "only {eligible} eligible regular files in snapshot {} (< sample size {k}) — testing all of them",
            short_id(snapshot_id)
        ));
    }
    log.line(&format!(
        "sampled {} of {} eligible regular files from snapshot {}",
        sample.len(),
        eligible,
        short_id(snapshot_id)
    ));
    Ok((sample, eligible))
}

// ---------------------------------------------------------------------------
// Exact restores + manifests

/// Escape restic/Go glob metacharacters so a sampled path is matched verbatim
/// by `restic restore --include`.
fn escape_include_pattern(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((hex, size))
}

/// Restore the given snapshot paths into a temp dir with `restic restore
/// --verify`, then independently stream every restored file through SHA-256.
fn restore_and_manifest(
    cfg: &Config,
    log: &Logger,
    repo: &str,
    snapshot_id: &str,
    paths: &[String],
    label: &str,
) -> Result<Vec<RestoredFile>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let tmp = tempfile::TempDir::new().context("create restore temp dir")?;
    let target = tmp.path().to_string_lossy().into_owned();
    let mut args = vec![
        "restore".to_string(),
        snapshot_id.to_string(),
        "--target".to_string(),
        target,
        "--verify".to_string(),
    ];
    for p in paths {
        args.push("--include".to_string());
        args.push(escape_include_pattern(p));
    }
    run_restic_logged(cfg, log, repo, &args)?;

    let mut manifest = Vec::with_capacity(paths.len());
    let mut errors = Vec::new();
    for p in paths {
        // Absolute snapshot path /a/b lands at TARGET/a/b.
        let restored = tmp.path().join(p.trim_start_matches('/'));
        match sha256_file(&restored) {
            Ok((sha256, size)) => manifest.push(RestoredFile { path: p.clone(), sha256, size }),
            Err(e) => errors.push(format!("{p}: {e}")),
        }
    }
    if !errors.is_empty() {
        let shown = errors.iter().take(10).cloned().collect::<Vec<_>>().join("; ");
        bail!(
            "[{label}] {} of {} restored files missing/unreadable: {shown}",
            errors.len(),
            paths.len()
        );
    }
    manifest.sort_by(|a, b| a.path.cmp(&b.path));
    log.line(&format!("[{label}] restored and hashed {} files", manifest.len()));
    Ok(manifest)
}

fn manifest_diff(before: &[RestoredFile], after: &[RestoredFile]) -> Vec<String> {
    let a: BTreeMap<&String, &RestoredFile> = before.iter().map(|f| (&f.path, f)).collect();
    let b: BTreeMap<&String, &RestoredFile> = after.iter().map(|f| (&f.path, f)).collect();
    let mut diffs = Vec::new();
    for (path, fa) in &a {
        match b.get(path) {
            None => diffs.push(format!("missing after: {path}")),
            Some(fb) => {
                if fa.sha256 != fb.sha256 {
                    diffs.push(format!("sha256 mismatch: {path} ({} -> {})", fa.sha256, fb.sha256));
                } else if fa.size != fb.size {
                    diffs.push(format!("size mismatch: {path} ({} -> {})", fa.size, fb.size));
                }
            }
        }
    }
    for path in b.keys() {
        if !a.contains_key(path) {
            diffs.push(format!("unexpected extra: {path}"));
        }
    }
    diffs
}

fn write_manifest(log: &Logger, name: &str, manifest: &[RestoredFile]) -> Result<PathBuf> {
    let path = log.manifest_path(name);
    fs::write(&path, serde_json::to_string_pretty(manifest).context("serialize manifest")?)
        .with_context(|| format!("write manifest {}", path.display()))?;
    log.line(&format!("manifest written: {}", path.display()));
    Ok(path)
}

// ---------------------------------------------------------------------------
// Backup, retention, replication

fn run_primary_backup(cfg: &Config, log: &Logger) -> Result<()> {
    let mut args = vec![
        "backup".to_string(),
        format!("{}/", cfg.home.trim_end_matches('/')),
        "--exclude-file".to_string(),
        cfg.exclude_file.clone(),
        "--verbose=2".to_string(),
    ];
    args.extend(cfg.extra_backup_args.iter().cloned());
    run_restic_logged(cfg, log, &cfg.repo, &args)
}

fn apply_retention(cfg: &Config, log: &Logger, repo: &str) -> Result<()> {
    let r = &cfg.retention;
    let args = vec![
        "forget".to_string(),
        "--keep-daily".to_string(),
        r.daily.to_string(),
        "--keep-weekly".to_string(),
        r.weekly.to_string(),
        "--keep-monthly".to_string(),
        r.monthly.to_string(),
        "--keep-yearly".to_string(),
        r.yearly.to_string(),
        "--prune".to_string(),
    ];
    run_restic_logged(cfg, log, repo, &args)
}

/// Copy the new snapshot into a replica via `restic copy` (no second
/// filesystem scan). First contact bootstraps: init with the primary's
/// chunker params, then copy every retained home snapshot. Afterwards the
/// primary's retention policy is applied to the replica too.
fn sync_replica(
    cfg: &Config,
    log: &Logger,
    replica: &str,
    new_snapshot_id: Option<&str>,
) -> Result<()> {
    let home = cfg.home.trim_end_matches('/');

    // The same stale-lock hygiene the primary gets in preflight. A run
    // interrupted mid-prune leaves a dead lock here, and every later
    // `forget --prune` on this replica fails until someone unlocks by hand
    // (observed 2026-08-01..02 on rsync.net, three stale locks deep).
    // `restic unlock` never touches locks held by live processes. Errors
    // ignored — the repo may not exist yet.
    let _ = run_restic_capture(cfg, log, replica, &strs(&["unlock"]));

    let existing = match snapshots(cfg, log, replica) {
        Ok(list) => Some(list),
        Err(_) => {
            log.line(&format!("initializing replica repository {replica} (chunker params copied from primary)"));
            let args = vec![
                "init".to_string(),
                "--copy-chunker-params".to_string(),
                "--from-repo".to_string(),
                cfg.repo.clone(),
                "--from-password-command".to_string(),
                cfg.password_command.clone(),
            ];
            run_restic_logged(cfg, log, replica, &args)
                .with_context(|| format!("cannot reach or initialize replica {replica}"))?;
            None
        }
    };
    let has_home = existing
        .as_ref()
        .map(|list| {
            list.iter()
                .any(|s| s.paths.iter().any(|p| p.trim_end_matches('/') == home))
        })
        .unwrap_or(false);

    let mut args = vec![
        "copy".to_string(),
        "--from-repo".to_string(),
        cfg.repo.clone(),
        "--from-password-command".to_string(),
        cfg.password_command.clone(),
    ];
    if has_home {
        match new_snapshot_id {
            Some(id) => args.push(id.to_string()),
            None => {
                log.line("no new snapshot this run — nothing to copy to the replica");
                return Ok(());
            }
        }
    } else {
        log.line("bootstrapping replica with all retained home snapshots");
        args.push("--path".to_string());
        args.push(home.to_string());
    }
    // Multi-hour copies over two cloud backends die to transient transport
    // failures (observed: the sftp ssh session timing out 15h into the
    // rsync.net bootstrap). Re-running `restic copy` is a cheap resume —
    // blobs already in the destination index are skipped — so retry with a
    // fresh connection instead of failing the whole stage.
    const COPY_ATTEMPTS: u32 = 5;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match run_restic_logged(cfg, log, replica, &args) {
            Ok(()) => break,
            Err(e) if attempt < COPY_ATTEMPTS => {
                log.line(&format!(
                    "replica copy attempt {attempt}/{COPY_ATTEMPTS} failed ({e:#}); \
                     retrying in 60s — already-copied data is skipped on resume"
                ));
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
            Err(e) => return Err(e),
        }
    }
    apply_retention(cfg, log, replica)
}

// ---------------------------------------------------------------------------
// The run itself

fn record(failures: &mut Vec<String>, log: &Logger, stage: &str, detail: &str) {
    let msg = format!("FAILURE [{stage}]: {detail}");
    log.line(&msg);
    failures.push(msg);
}

fn run(cfg: &Config, log: &Logger) -> Result<Vec<String>> {
    let mut failures: Vec<String> = Vec::new();
    let start = chrono::Local::now();
    log.line(&format!("=== backup-home started: {} ===", start.format("%Y-%m-%d %H:%M:%S %z")));
    log.line(&format!("    primary repo: {}", cfg.repo));
    for r in &cfg.replica_repos {
        log.line(&format!("    replica repo: {r}"));
    }
    log.line(&format!(
        "    verification: {}",
        if cfg.verification.enable {
            format!("enabled, sample size {}", cfg.verification.sample_size)
        } else {
            "disabled".to_string()
        }
    ));
    log.line(&format!("    log file: {}", log.log_path.display()));

    // -- fatal preflight ----------------------------------------------------
    check_password(cfg).context("cannot resolve restic password via configured passwordCommand")?;

    // Drop stale locks first: a lock whose owning process is dead would
    // otherwise fail every future run until someone notices (this silently
    // killed every daily run 2026-07-13..27). `restic unlock` never touches
    // locks held by live processes, so a genuinely running backup still
    // trips the check below. Errors ignored — the repo may not exist yet.
    let _ = run_restic_capture(cfg, log, &cfg.repo, &strs(&["unlock"]));

    // Bail if another backup is already running. A failing lock listing means
    // the repo may simply not exist yet — auto-init below handles that.
    if let Ok(out) = run_restic_capture(cfg, log, &cfg.repo, &strs(&["list", "locks", "--no-lock"])) {
        if out.lines().any(|l| !l.trim().is_empty()) {
            bail!("restic repo is locked — another backup is still running");
        }
    }

    let pre_snapshots = match snapshots(cfg, log, &cfg.repo) {
        Ok(list) => list,
        Err(_) => {
            log.line("Initializing restic repository...");
            run_restic_logged(cfg, log, &cfg.repo, &strs(&["init"]))
                .context("cannot reach or initialize the primary repository")?;
            Vec::new()
        }
    };

    // -- pre-backup retrieval test ------------------------------------------
    let old_snapshot = latest_home_snapshot(&pre_snapshots, &cfg.home);
    let k = cfg.verification.sample_size;
    let mut sample_a: Vec<String> = Vec::new();
    let mut manifest_a: Option<Vec<RestoredFile>> = None;

    if cfg.verification.enable {
        match &old_snapshot {
            None => log.line("no previous home snapshot — skipping pre-backup retrieval test"),
            Some(old) => {
                log.line(&format!("--- pre-backup retrieval test (snapshot {}) ---", short_id(&old.id)));
                match sample_regular_files(cfg, log, &cfg.repo, &old.id, k, &HashSet::new()) {
                    Err(e) => record(&mut failures, log, "pre-backup sample", &format!("{e:#}")),
                    Ok((sample, _)) => {
                        sample_a = sample;
                        match restore_and_manifest(cfg, log, &cfg.repo, &old.id, &sample_a, "pre-backup") {
                            Err(e) => record(&mut failures, log, "pre-backup restore", &format!("{e:#}")),
                            Ok(m) => {
                                if let Err(e) = write_manifest(log, "pre-old-snapshot", &m) {
                                    record(&mut failures, log, "pre-backup manifest", &format!("{e:#}"));
                                }
                                manifest_a = Some(m);
                            }
                        }
                    }
                }
            }
        }
    }

    // -- backup (always attempted, even after a failed pre-check) -----------
    log.line("--- backup ---");
    let backup_ok = match run_primary_backup(cfg, log) {
        Ok(()) => true,
        Err(e) => {
            record(&mut failures, log, "backup", &format!("{e:#}"));
            false
        }
    };

    let new_snapshot = match snapshots(cfg, log, &cfg.repo) {
        Ok(list) => {
            let latest = latest_home_snapshot(&list, &cfg.home);
            match (latest, &old_snapshot) {
                (Some(l), Some(o)) if l.id == o.id => {
                    if backup_ok {
                        record(&mut failures, log, "backup", "backup reported success but produced no new snapshot");
                    }
                    None
                }
                (Some(l), _) => Some(l),
                (None, _) => {
                    if backup_ok {
                        record(&mut failures, log, "backup", "no home snapshot found after backup");
                    }
                    None
                }
            }
        }
        Err(e) => {
            record(&mut failures, log, "snapshot listing", &format!("{e:#}"));
            None
        }
    };
    if let Some(s) = &new_snapshot {
        log.line(&format!("new snapshot: {}", short_id(&s.id)));
    }

    // -- retention on the primary -------------------------------------------
    log.line("--- retention (primary) ---");
    if let Err(e) = apply_retention(cfg, log, &cfg.repo) {
        record(&mut failures, log, "prune", &format!("{e:#}"));
    }

    // -- replicas ------------------------------------------------------------
    for replica in &cfg.replica_repos {
        log.line(&format!("--- replica sync: {replica} ---"));
        if let Err(e) = sync_replica(cfg, log, replica, new_snapshot.as_ref().map(|s| s.id.as_str())) {
            record(&mut failures, log, &format!("replica sync {replica}"), &format!("{e:#}"));
        }
    }

    // -- post-backup: original sample from the same old snapshot ------------
    if let (Some(old), Some(before)) = (&old_snapshot, &manifest_a) {
        log.line(&format!(
            "--- post-backup retrieval test: original sample, snapshot {} ---",
            short_id(&old.id)
        ));
        match snapshots(cfg, log, &cfg.repo) {
            Err(e) => record(&mut failures, log, "post-backup original sample", &format!("{e:#}")),
            Ok(list) => {
                if !list.iter().any(|s| s.id == old.id) {
                    // Legitimate when several runs happen the same day: the
                    // retention policy keeps only the newest snapshot per day.
                    log.line("old snapshot was removed by the retention policy — skipping original-sample comparison");
                } else {
                    match restore_and_manifest(cfg, log, &cfg.repo, &old.id, &sample_a, "post-backup original sample") {
                        Err(e) => record(&mut failures, log, "post-backup original sample", &format!("{e:#}")),
                        Ok(after) => {
                            if let Err(e) = write_manifest(log, "post-old-snapshot", &after) {
                                record(&mut failures, log, "post-backup manifest", &format!("{e:#}"));
                            }
                            let diffs = manifest_diff(before, &after);
                            if diffs.is_empty() {
                                log.line("original sample manifests match byte-for-byte");
                            } else {
                                let shown = diffs.iter().take(10).cloned().collect::<Vec<_>>().join("; ");
                                record(
                                    &mut failures,
                                    log,
                                    "post-backup original sample",
                                    &format!("{} difference(s): {shown}", diffs.len()),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // -- post-backup: fresh disjoint sample from the new snapshot -----------
    let mut sample_b: Vec<String> = Vec::new();
    let mut manifest_b: Option<Vec<RestoredFile>> = None;
    if cfg.verification.enable {
        if let Some(new) = &new_snapshot {
            log.line(&format!(
                "--- post-backup retrieval test: fresh sample from new snapshot {} ---",
                short_id(&new.id)
            ));
            let exclude: HashSet<String> = sample_a.iter().cloned().collect();
            match sample_regular_files(cfg, log, &cfg.repo, &new.id, k, &exclude) {
                Err(e) => record(&mut failures, log, "new-snapshot sample", &format!("{e:#}")),
                Ok((sample, _)) => {
                    sample_b = sample;
                    if sample_b.is_empty() {
                        log.line("no eligible files outside the original sample — skipping new-snapshot restore test");
                    } else {
                        match restore_and_manifest(cfg, log, &cfg.repo, &new.id, &sample_b, "new-snapshot sample") {
                            Err(e) => record(&mut failures, log, "new-snapshot restore", &format!("{e:#}")),
                            Ok(m) => {
                                if let Err(e) = write_manifest(log, "post-new-snapshot", &m) {
                                    record(&mut failures, log, "new-snapshot manifest", &format!("{e:#}"));
                                }
                                manifest_b = Some(m);
                            }
                        }
                    }
                }
            }
        }
    }

    // -- equivalent retrieval checks against every replica -------------------
    if cfg.verification.enable {
        if let (Some(new), Some(expected)) = (&new_snapshot, &manifest_b) {
            for (i, replica) in cfg.replica_repos.iter().enumerate() {
                log.line(&format!("--- replica retrieval test: {replica} ---"));
                let stage = format!("replica retrieval {replica}");
                match snapshots(cfg, log, replica) {
                    Err(e) => record(&mut failures, log, &stage, &format!("{e:#}")),
                    Ok(list) => match resolve_copied_snapshot(&list, &new.id) {
                        None => record(&mut failures, log, &stage, "replica has no copy of the new snapshot"),
                        Some(target) => {
                            let target_id = target.id.clone();
                            match restore_and_manifest(cfg, log, replica, &target_id, &sample_b, &stage) {
                                Err(e) => record(&mut failures, log, &stage, &format!("{e:#}")),
                                Ok(m) => {
                                    if let Err(e) = write_manifest(log, &format!("replica-{}", i + 1), &m) {
                                        record(&mut failures, log, &stage, &format!("{e:#}"));
                                    }
                                    let diffs = manifest_diff(expected, &m);
                                    if diffs.is_empty() {
                                        log.line("replica sample matches the primary manifest");
                                    } else {
                                        let shown =
                                            diffs.iter().take(10).cloned().collect::<Vec<_>>().join("; ");
                                        record(
                                            &mut failures,
                                            log,
                                            &stage,
                                            &format!("{} difference(s): {shown}", diffs.len()),
                                        );
                                    }
                                }
                            }
                        }
                    },
                }
            }
        } else if !cfg.replica_repos.is_empty() {
            log.line("skipping replica retrieval tests — no new snapshot or no fresh sample this run");
        }
    }

    // -- summary --------------------------------------------------------------
    let end = chrono::Local::now();
    if failures.is_empty() {
        log.line(&format!(
            "=== backup-home complete: {} (all stages OK) ===",
            end.format("%Y-%m-%d %H:%M:%S %z")
        ));
    } else {
        log.line(&format!(
            "=== backup-home finished with {} FAILURE(S): {} ===",
            failures.len(),
            end.format("%Y-%m-%d %H:%M:%S %z")
        ));
        for f in &failures {
            log.line(&format!("    {f}"));
        }
    }
    Ok(failures)
}

// ---------------------------------------------------------------------------
// Entry point

fn load_config() -> Result<Config> {
    let mut config_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = Some(args.next().context("--config needs a path")?),
            "--help" | "-h" => {
                println!("backup-home [--config CONFIG.json]");
                println!("Configuration is normally embedded by the Nix module;");
                println!("--config or $BACKUP_HOME_CONFIG (a JSON file path) override it.");
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    let json = if let Some(p) = config_path {
        fs::read_to_string(&p).with_context(|| format!("read config {p}"))?
    } else if let Ok(p) = std::env::var("BACKUP_HOME_CONFIG") {
        fs::read_to_string(&p).with_context(|| format!("read config {p}"))?
    } else if !EMBEDDED_CONFIG_JSON.starts_with('@') {
        EMBEDDED_CONFIG_JSON.to_string()
    } else {
        bail!("no configuration: this copy was not built by the Nix module — pass --config or set BACKUP_HOME_CONFIG");
    };
    serde_json::from_str(&json).context("parse backup-home config JSON")
}

fn real_main() -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("backup-home: {e:#}");
            return 1;
        }
    };
    let log = match Logger::create(Path::new(&cfg.log_dir)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("backup-home: {e:#}");
            return 1;
        }
    };
    match run(&cfg, &log) {
        Ok(failures) if failures.is_empty() => 0,
        Ok(_) => 1,
        Err(e) => {
            log.line(&format!("FATAL: {e:#}"));
            1
        }
    }
}

fn main() {
    std::process::exit(real_main());
}

// ---------------------------------------------------------------------------
// Tests. Run with:
//   nix shell nixpkgs#rust-script nixpkgs#cargo nixpkgs#rustc nixpkgs#restic \
//     -c rust-script --test backup-home.rs
// The e2e tests use temporary local restic repositories and skip themselves
// when restic is not on PATH.

#[cfg(test)]
mod tests {
    use super::*;

    fn node_line(path: &str, node_type: &str) -> String {
        format!(r#"{{"struct_type":"node","type":"{node_type}","path":"{path}"}}"#)
    }

    // -- restic JSON parsing -------------------------------------------------

    #[test]
    fn parses_regular_file_nodes() {
        assert_eq!(
            parse_file_node(&node_line("/h/a.txt", "file")),
            Some("/h/a.txt".to_string())
        );
        assert_eq!(parse_file_node(&node_line("/h/d", "dir")), None);
        assert_eq!(parse_file_node(&node_line("/h/l", "symlink")), None);
        // snapshot header record
        assert_eq!(
            parse_file_node(r#"{"struct_type":"snapshot","id":"abc","paths":["/h"]}"#),
            None
        );
        // newer restic tags with message_type instead
        assert_eq!(
            parse_file_node(r#"{"message_type":"node","type":"file","path":"/h/b"}"#),
            Some("/h/b".to_string())
        );
        assert_eq!(parse_file_node("not json"), None);
        assert_eq!(parse_file_node(""), None);
    }

    #[test]
    fn parses_snapshot_list_json() {
        let json = r#"[
            {"id":"aaaa","time":"2026-07-29T14:00:00.5+02:00","paths":["/Users/x"],"hostname":"h"},
            {"id":"bbbb","time":"2026-07-30T14:00:00.5+02:00","paths":["/Users/x"],"original":"aaaa"}
        ]"#;
        let snaps: Vec<Snapshot> = serde_json::from_str(json).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].original, None);
        assert_eq!(snaps[1].original.as_deref(), Some("aaaa"));
        // restic prints null for an empty repo
        let empty: Option<Vec<Snapshot>> = serde_json::from_str("null").unwrap();
        assert!(empty.is_none());
    }

    // -- snapshot selection ----------------------------------------------------

    fn snap(id: &str, time: &str, path: &str, original: Option<&str>) -> Snapshot {
        Snapshot {
            id: id.to_string(),
            time: time.to_string(),
            paths: vec![path.to_string()],
            original: original.map(|s| s.to_string()),
        }
    }

    #[test]
    fn latest_snapshot_selected_by_home_path_and_time() {
        let snaps = vec![
            snap("old", "2026-07-01T10:00:00Z", "/Users/x", None),
            snap("new", "2026-07-20T10:00:00Z", "/Users/x/", None),
            snap("other", "2026-07-25T10:00:00Z", "/Users/y", None),
        ];
        // trailing slashes on either side are irrelevant
        assert_eq!(latest_home_snapshot(&snaps, "/Users/x/").unwrap().id, "new");
        assert_eq!(latest_home_snapshot(&snaps, "/Users/x").unwrap().id, "new");
        assert!(latest_home_snapshot(&snaps, "/Users/z").is_none());
        assert!(latest_home_snapshot(&[], "/Users/x").is_none());
    }

    #[test]
    fn copied_snapshot_resolved_by_id_or_original() {
        let snaps = vec![
            snap("copy1", "2026-07-01T10:00:00Z", "/Users/x", Some("src1")),
            snap("plain", "2026-07-02T10:00:00Z", "/Users/x", None),
        ];
        assert_eq!(resolve_copied_snapshot(&snaps, "src1").unwrap().id, "copy1");
        assert_eq!(resolve_copied_snapshot(&snaps, "plain").unwrap().id, "plain");
        assert!(resolve_copied_snapshot(&snaps, "missing").is_none());
    }

    // -- include-pattern escaping ---------------------------------------------

    #[test]
    fn include_patterns_escape_glob_metacharacters() {
        assert_eq!(escape_include_pattern("/h/plain.txt"), "/h/plain.txt");
        assert_eq!(
            escape_include_pattern(r"/h/a*b?c[d]e\f"),
            r"/h/a\*b\?c\[d]e\\f"
        );
    }

    // -- sampling ---------------------------------------------------------------

    fn run_sampler(lines: &[String], k: usize, exclude: &[&str]) -> (Vec<String>, usize) {
        let exclude: HashSet<String> = exclude.iter().map(|s| s.to_string()).collect();
        let mut rng = rand::rng();
        let mut sampler = Sampler::new(k, exclude);
        for line in lines {
            sampler.offer_line(line, &mut rng);
        }
        sampler.finish()
    }

    #[test]
    fn sampling_takes_all_files_when_fewer_than_sample_size() {
        let lines: Vec<String> = (0..10).map(|i| node_line(&format!("/h/f{i}"), "file")).collect();
        let (sample, eligible) = run_sampler(&lines, 300, &[]);
        assert_eq!(eligible, 10);
        let set: HashSet<&String> = sample.iter().collect();
        assert_eq!(set.len(), 10);
    }

    #[test]
    fn sampling_is_distinct_and_bounded() {
        let lines: Vec<String> = (0..1000).map(|i| node_line(&format!("/h/f{i}"), "file")).collect();
        let (sample, eligible) = run_sampler(&lines, 300, &[]);
        assert_eq!(eligible, 1000);
        assert_eq!(sample.len(), 300);
        let set: HashSet<&String> = sample.iter().collect();
        assert_eq!(set.len(), 300, "sample must be without replacement");
        for p in &sample {
            assert!(p.starts_with("/h/f"));
        }
    }

    #[test]
    fn sampling_excludes_prior_sample_and_non_files() {
        let mut lines: Vec<String> = (0..10).map(|i| node_line(&format!("/h/f{i}"), "file")).collect();
        lines.push(node_line("/h/somedir", "dir"));
        lines.push(node_line("/h/somelink", "symlink"));
        let (sample, eligible) = run_sampler(&lines, 300, &["/h/f0", "/h/f1", "/h/f2"]);
        assert_eq!(eligible, 7);
        assert_eq!(sample.len(), 7);
        for banned in ["/h/f0", "/h/f1", "/h/f2", "/h/somedir", "/h/somelink"] {
            assert!(!sample.contains(&banned.to_string()), "{banned} must be excluded");
        }
    }

    // -- manifests ---------------------------------------------------------------

    fn rf(path: &str, sha: &str, size: u64) -> RestoredFile {
        RestoredFile { path: path.to_string(), sha256: sha.to_string(), size }
    }

    #[test]
    fn manifest_diff_reports_all_mismatch_kinds() {
        let before = vec![rf("/a", "s1", 1), rf("/b", "s2", 2), rf("/c", "s3", 3)];
        assert!(manifest_diff(&before, &before.clone()).is_empty());

        let after = vec![rf("/a", "sX", 1), rf("/b", "s2", 99)];
        let diffs = manifest_diff(&before, &after);
        assert_eq!(diffs.len(), 3, "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("sha256 mismatch: /a")));
        assert!(diffs.iter().any(|d| d.contains("size mismatch: /b")));
        assert!(diffs.iter().any(|d| d.contains("missing after: /c")));

        let extra = vec![rf("/a", "s1", 1), rf("/b", "s2", 2), rf("/c", "s3", 3), rf("/d", "s4", 4)];
        let diffs = manifest_diff(&before, &extra);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("unexpected extra: /d"));
    }

    // -- failure aggregation ------------------------------------------------------

    #[test]
    fn failures_accumulate_with_stage_labels() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = Logger::create(dir.path()).unwrap();
        let mut failures = Vec::new();
        record(&mut failures, &log, "pre-backup restore", "boom");
        record(&mut failures, &log, "prune", "bang");
        assert_eq!(failures.len(), 2);
        assert!(failures[0].contains("FAILURE [pre-backup restore]: boom"));
        assert!(failures[1].contains("FAILURE [prune]: bang"));
    }

    // -- password command splitting -----------------------------------------------

    #[test]
    fn command_words_split_like_a_shell() {
        assert_eq!(
            split_command_words("passveil show restic/backup"),
            vec!["passveil", "show", "restic/backup"]
        );
        assert_eq!(
            split_command_words(r#"sh -c "echo hi there""#),
            vec!["sh", "-c", "echo hi there"]
        );
        assert_eq!(
            split_command_words(r"cat /My\ Secrets/pw 'single quoted'"),
            vec!["cat", "/My Secrets/pw", "single quoted"]
        );
        assert!(split_command_words("   ").is_empty());
    }

    // -- offline end-to-end tests against local restic repositories ---------------

    fn restic_available() -> bool {
        Command::new("restic")
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn yesterday_string() -> String {
        (chrono::Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn make_fixture_home(home: &Path, n: usize) -> Result<()> {
        for i in 0..n {
            let sub = match i % 4 {
                0 => "docs",
                1 => "code/deep/nested",
                2 => "media [old]",
                _ => "misc",
            };
            let dir = home.join(sub);
            fs::create_dir_all(&dir)?;
            let name = match i % 5 {
                0 => format!("file-{i}.txt"),
                1 => format!("spaced name {i}.txt"),
                2 => format!("star*{i}.txt"),
                3 => format!("quest?{i}.txt"),
                _ => format!("bracket[{i}].txt"),
            };
            fs::write(dir.join(name), format!("fixture contents {i}\n"))?;
        }
        // A moderately large incompressible blob so the corruption test's
        // deleted pack is guaranteed to hold real file data.
        let mut blob = vec![0u8; 256 * 1024];
        rand::rng().fill(&mut blob[..]);
        fs::create_dir_all(home.join("big"))?;
        fs::write(home.join("big/blob.bin"), &blob)?;
        // Content the exclude file must keep out of every snapshot.
        fs::create_dir_all(home.join("Excluded"))?;
        fs::write(home.join("Excluded/secret.txt"), "never backed up")?;
        fs::write(home.join("docs/scratch.ex-tmp"), "excluded by pattern")?;
        Ok(())
    }

    fn e2e_setup(
        n_files: usize,
        sample_size: usize,
        with_replica: bool,
    ) -> Result<(tempfile::TempDir, Config, Logger)> {
        let root = tempfile::TempDir::new()?;
        let home = root.path().join("home");
        make_fixture_home(&home, n_files)?;
        let pw = root.path().join("restic-password");
        fs::write(&pw, "test-password\n")?;
        let exclude = root.path().join("excludes");
        fs::write(&exclude, format!("{}\n*.ex-tmp\n", home.join("Excluded").display()))?;
        let log_dir = root.path().join("log");
        let cfg = Config {
            home: home.to_string_lossy().into_owned(),
            repo: root.path().join("repo-primary").to_string_lossy().into_owned(),
            password_command: format!("cat {}", pw.display()),
            exclude_file: exclude.to_string_lossy().into_owned(),
            replica_repos: if with_replica {
                vec![root.path().join("repo-replica").to_string_lossy().into_owned()]
            } else {
                Vec::new()
            },
            retention: Retention { daily: 7, weekly: 4, monthly: 12, yearly: 3 },
            verification: Verification { enable: true, sample_size },
            log_dir: log_dir.to_string_lossy().into_owned(),
            restic_bin: "restic".to_string(),
            extra_backup_args: Vec::new(),
        };
        let log = Logger::create(&log_dir)?;
        Ok((root, cfg, log))
    }

    #[test]
    fn e2e_two_runs_with_replica() -> Result<()> {
        if !restic_available() {
            eprintln!("SKIPPED: restic not on PATH");
            return Ok(());
        }
        let (_root, mut cfg, log) = e2e_setup(700, 300, true)?;

        // Run 1, backdated a day: the retention policy keeps one snapshot per
        // day, so run 2's forget must not prune run 1's snapshot.
        cfg.extra_backup_args = vec!["--time".to_string(), yesterday_string()];
        let f1 = run(&cfg, &log)?;
        assert!(f1.is_empty(), "first run failures: {f1:?}");

        // Run 2: pre-check against run 1's snapshot, backup, replicate,
        // post-checks — the full path.
        cfg.extra_backup_args.clear();
        let f2 = run(&cfg, &log)?;
        assert!(f2.is_empty(), "second run failures: {f2:?}");

        // Both repositories contain the two-snapshot lineage.
        let primary = snapshots(&cfg, &log, &cfg.repo)?;
        assert_eq!(primary.len(), 2, "primary snapshots: {primary:?}");
        let replica = snapshots(&cfg, &log, &cfg.replica_repos[0])?;
        assert_eq!(replica.len(), 2, "replica snapshots: {replica:?}");
        let latest = latest_home_snapshot(&primary, &cfg.home).unwrap();
        assert!(
            resolve_copied_snapshot(&replica, &latest.id).is_some(),
            "new snapshot must resolve in the replica via id/original"
        );
        for r in &replica {
            assert!(
                r.original.is_some() || primary.iter().any(|p| p.id == r.id),
                "replica snapshot {} has no lineage to the primary",
                r.id
            );
        }

        // The sample checks completed: their manifests exist on disk.
        let names: Vec<String> = fs::read_dir(&cfg.log_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for needle in ["pre-old-snapshot", "post-old-snapshot", "post-new-snapshot", "replica-1"] {
            assert!(
                names.iter().any(|n| n.contains(needle)),
                "missing manifest {needle} in {names:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn e2e_corruption_fails_the_run_but_backup_is_still_attempted() -> Result<()> {
        if !restic_available() {
            eprintln!("SKIPPED: restic not on PATH");
            return Ok(());
        }
        // sample_size > file count → every file (including the blob) is in
        // the pre-check sample, so the deleted pack is guaranteed to be hit.
        let (_root, mut cfg, log) = e2e_setup(650, 1000, false)?;
        cfg.extra_backup_args = vec!["--time".to_string(), yesterday_string()];
        let f1 = run(&cfg, &log)?;
        assert!(f1.is_empty(), "first run failures: {f1:?}");
        cfg.extra_backup_args.clear();

        let before = snapshots(&cfg, &log, &cfg.repo)?.len();

        // Remove the largest data pack — restic keeps trees and data in
        // separate packs, so this destroys real file contents.
        let data_dir = Path::new(&cfg.repo).join("data");
        let mut packs: Vec<(u64, PathBuf)> = Vec::new();
        for sub in fs::read_dir(&data_dir)? {
            let sub = sub?.path();
            if sub.is_dir() {
                for f in fs::read_dir(&sub)? {
                    let p = f?.path();
                    packs.push((fs::metadata(&p)?.len(), p));
                }
            }
        }
        packs.sort_by_key(|(size, _)| *size);
        let (_, victim) = packs.pop().expect("repository has data packs");
        fs::remove_file(&victim)?;

        let failures = run(&cfg, &log)?;
        assert!(!failures.is_empty(), "corruption must surface as a nonzero result");
        assert!(
            failures.iter().any(|f| f.contains("pre-backup")),
            "expected a pre-backup retrieval failure: {failures:?}"
        );
        let after = snapshots(&cfg, &log, &cfg.repo)?;
        assert!(
            after.len() > before,
            "the backup must still be attempted after a failed pre-check ({} -> {})",
            before,
            after.len()
        );
        Ok(())
    }
}
