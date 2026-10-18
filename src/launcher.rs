//! The launcher: a small supervisor for one background lgw serve process.
//!
//! It owns a state directory, not a daemon loop: launch starts a detached child,
//! exit stops the process recorded there, and update delegates to the repository
//! install script so signing and installation keep one implementation.
//!
//! A pid alone is not an identity. Every state records the process start time
//! reported by ps; a reused pid therefore becomes a stale record instead of a
//! reason to kill an unrelated process.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const STATE_DIR_ENV: &str = "LGWL_STATE_DIR";
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
const STOP_GRACE: Duration = Duration::from_secs(10);
const FORCE_GRACE: Duration = Duration::from_secs(2);
const START_GRACE: Duration = Duration::from_millis(300);
const STATE_VERSION: u32 = 1;

/// Where all launcher state lives.
#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
    pub lock: PathBuf,
    pub manifest: PathBuf,
}

impl Paths {
    pub fn from_dir(dir: PathBuf) -> Self {
        Paths {
            state: dir.join("state.json"),
            log: dir.join("lgw.log"),
            lock: dir.join("lgwl.lock"),
            manifest: dir.join("install.conf"),
            dir,
        }
    }

    pub fn discover() -> Result<Self> {
        if let Some(dir) = std::env::var_os(STATE_DIR_ENV).filter(|value| !value.is_empty()) {
            return Ok(Self::from_dir(PathBuf::from(dir)));
        }
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .context("HOME is not set; set LGWL_STATE_DIR to a writable directory")?;
        Ok(Self::from_dir(
            PathBuf::from(home).join(".light-ai-gateway/lgwl"),
        ))
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating state directory {}", self.dir.display()))?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting mode on {}", self.dir.display()))?;
        Ok(())
    }
}

/// The launch record. The start string is what makes the pid an identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub version: u32,
    pub pid: u32,
    pub started_at: i64,
    pub process_start: String,
    /// Arguments passed to lgw before the serve subcommand.
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running(State),
    Stopped { stale: bool },
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Arguments before serve, with --color never and an optional --config.
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl LaunchOptions {
    pub fn new(config: Option<PathBuf>, cwd: PathBuf) -> Self {
        let mut args = vec!["--color".to_string(), "never".to_string()];
        if let Some(path) = config {
            args.push("--config".to_string());
            args.push(path.display().to_string());
        }
        LaunchOptions { args, cwd }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    Started(State),
    AlreadyRunning(State),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitOutcome {
    Stopped { pid: u32, forced: bool },
    NotRunning,
}

#[derive(Debug, Clone)]
pub struct UpdateOptions {
    pub repo: Option<PathBuf>,
    pub no_restart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub repo: PathBuf,
    pub was_running: bool,
    pub restarted: bool,
}

/// What the install script recorded about its own install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallManifest {
    pub repo: PathBuf,
    pub identity: String,
    pub lgw: PathBuf,
}

pub struct Launcher {
    paths: Paths,
    lgw: PathBuf,
}

impl Launcher {
    /// Resolve the paths and the lgw binary the launcher should drive.
    ///
    /// The sibling binary is preferred over PATH: an install puts both in the
    /// same directory, and a path lookup could pick a different build.
    pub fn discover() -> Result<Self> {
        let paths = Paths::discover()?;
        let lgw = resolve_lgw_binary()?;
        Ok(Launcher { paths, lgw })
    }

    pub fn with_paths(paths: Paths, lgw: PathBuf) -> Self {
        Launcher { paths, lgw }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn lgw(&self) -> &Path {
        &self.lgw
    }

    pub fn status(&self) -> Result<Status> {
        match load_state(&self.paths)? {
            None => Ok(Status::Stopped { stale: false }),
            Some(state) if process_matches(&state) => Ok(Status::Running(state)),
            Some(_) => Ok(Status::Stopped { stale: true }),
        }
    }

    pub fn launch(&self, options: &LaunchOptions) -> Result<LaunchOutcome> {
        self.paths.ensure()?;
        let _lock = Lock::acquire(&self.paths)?;
        self.launch_locked(options)
    }

    pub fn exit(&self) -> Result<ExitOutcome> {
        self.paths.ensure()?;
        let _lock = Lock::acquire(&self.paths)?;
        self.stop_locked()
    }

    pub fn update(&self, options: &UpdateOptions) -> Result<UpdateOutcome> {
        self.paths.ensure()?;
        let _lock = Lock::acquire(&self.paths)?;

        let prior = load_state(&self.paths)?;
        let was_running = prior.as_ref().is_some_and(process_matches);
        if was_running {
            self.stop_locked()?;
        }

        let script = find_install_script(options.repo.as_deref(), &self.paths)?;
        let repo = script
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| script.clone());
        let manifest = read_manifest(&self.paths);
        let mut command = Command::new("bash");
        command.arg(&script).current_dir(&repo);
        if let Some(manifest) = &manifest {
            if !manifest.identity.is_empty() {
                command.env("LGW_SIGN_IDENTITY", &manifest.identity);
            }
            if manifest.lgw.is_file() {
                command.env("LGW_BIN", &manifest.lgw);
            }
        }
        let status = command
            .status()
            .with_context(|| format!("running {}", script.display()))?;
        if !status.success() {
            let state = if was_running {
                "lgw is stopped because the update failed."
            } else {
                "lgw was not running."
            };
            bail!("installer exited with {status}. {state} Fix the build and run lgwl launch.");
        }

        let mut restarted = false;
        if was_running && !options.no_restart {
            let Some(prior) = prior else {
                bail!("the previous launch record disappeared during update");
            };
            self.launch_locked(&LaunchOptions {
                args: prior.args,
                cwd: prior.cwd,
            })
            .context("lgw installed, but it could not be relaunched")?;
            restarted = true;
        }
        Ok(UpdateOutcome {
            repo,
            was_running,
            restarted,
        })
    }

    fn launch_locked(&self, options: &LaunchOptions) -> Result<LaunchOutcome> {
        if !self.lgw.is_file() {
            bail!(
                "lgw binary not found at {}. Reinstall it, or set LGW_BIN.",
                self.lgw.display()
            );
        }
        if let Some(state) = load_state(&self.paths)? {
            if process_matches(&state) {
                return Ok(LaunchOutcome::AlreadyRunning(state));
            }
            remove_state(&self.paths)?;
        }

        rotate_log(&self.paths.log, LOG_ROTATE_BYTES)?;
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.paths.log)
            .with_context(|| format!("opening {}", self.paths.log.display()))?;
        let _ = writeln!(
            log,
            "[lgwl] {} launching {} {:?} cwd={}",
            timestamp(),
            self.lgw.display(),
            options.args,
            options.cwd.display(),
        );
        let _ = log.flush();
        let _ = fs::set_permissions(&self.paths.log, fs::Permissions::from_mode(0o600));
        let stdout = Stdio::from(log.try_clone().context("cloning the log handle")?);
        let stderr = Stdio::from(log.try_clone().context("cloning the log handle")?);

        let mut command = Command::new(&self.lgw);
        command
            .args(&options.args)
            .arg("serve")
            .current_dir(&options.cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        // A new session survives the terminal that launched it. Without this the
        // shell's SIGHUP on close would stop the proxy.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", self.lgw.display()))?;
        let pid = child.id();
        std::thread::sleep(START_GRACE);
        if let Some(status) = child.try_wait().context("checking the child")? {
            remove_state(&self.paths)?;
            let tail = read_tail(&self.paths.log, 20).unwrap_or_default();
            let tail = tail.join("\n");
            bail!("lgw exited immediately ({status}). Last log lines:\n{tail}");
        }
        let process_start = match process_start(pid) {
            Some(start) => start,
            None => {
                let _ = terminate(pid, libc::SIGTERM);
                bail!("could not read the start time of pid {pid}; refusing to track it");
            }
        };
        if !alive(pid) {
            remove_state(&self.paths)?;
            bail!("lgw exited before its process identity could be recorded");
        }
        let state = State {
            version: STATE_VERSION,
            pid,
            started_at: now_secs(),
            process_start,
            args: options.args.clone(),
            cwd: options.cwd.clone(),
        };
        if let Err(error) = write_state(&self.paths, &state) {
            let _ = terminate(pid, libc::SIGTERM);
            return Err(error)
                .context("could not record the launch state; stopped the new process");
        }
        Ok(LaunchOutcome::Started(state))
    }

    fn stop_locked(&self) -> Result<ExitOutcome> {
        let Some(state) = load_state(&self.paths)? else {
            return Ok(ExitOutcome::NotRunning);
        };
        if !process_matches(&state) {
            remove_state(&self.paths)?;
            return Ok(ExitOutcome::NotRunning);
        }
        terminate(state.pid, libc::SIGTERM)?;
        let mut forced = false;
        if !wait_for_exit(state.pid, STOP_GRACE) {
            forced = true;
            terminate(state.pid, libc::SIGKILL)?;
            if !wait_for_exit(state.pid, FORCE_GRACE) {
                bail!(
                    "pid {} did not exit after SIGKILL; the launch record is kept",
                    state.pid
                );
            }
        }
        let _ = append_log(
            &self.paths.log,
            &format!(
                "[lgwl] {} stopped pid {}{}",
                timestamp(),
                state.pid,
                if forced { " after SIGKILL" } else { "" }
            ),
        );
        remove_state(&self.paths)?;
        Ok(ExitOutcome::Stopped {
            pid: state.pid,
            forced,
        })
    }
}

/// Tail the log without loading it more than once per command.
pub fn read_tail(path: &Path, lines: usize) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut all: Vec<String> = text.lines().map(str::to_string).collect();
    if all.len() > lines {
        all.drain(0..all.len() - lines);
    }
    Ok(all)
}

/// One-generation rotation. The launcher calls this before every launch.
pub fn rotate_log(path: &Path, max_bytes: u64) -> Result<bool> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    if metadata.len() <= max_bytes {
        return Ok(false);
    }
    let old = path.with_extension("log.1");
    let _ = fs::remove_file(&old);
    fs::rename(path, &old)
        .with_context(|| format!("rotating {} to {}", path.display(), old.display()))?;
    Ok(true)
}

fn append_log(path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn resolve_lgw_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LGW_BIN").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().context("reading the lgwl executable path")?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("lgw");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Ok(path) = which("lgw") {
        return Ok(path);
    }
    bail!("lgw was not found next to lgwl and is not on PATH; set LGW_BIN")
}

fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{name} is not on PATH")
}

fn find_install_script(repo: Option<&Path>, paths: &Paths) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(repo) = repo {
        candidates.push(repo.to_path_buf());
    }
    if let Some(env) = std::env::var_os("LGW_REPO").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(env));
    }
    if let Some(manifest) = read_manifest(paths) {
        candidates.push(manifest.repo);
    }
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            candidates.push(ancestor.to_path_buf());
        }
    }
    for candidate in candidates {
        let script = candidate.join("scripts/lgw-install.sh");
        if script.is_file() {
            return Ok(script);
        }
    }
    bail!(
        "could not find scripts/lgw-install.sh. Run lgwl update --repo PATH, set LGW_REPO, or run it from the repository."
    )
}

fn read_manifest(paths: &Paths) -> Option<InstallManifest> {
    let raw = fs::read_to_string(&paths.manifest).ok()?;
    let mut lines = raw.lines();
    let repo = lines.next()?.trim().to_string();
    let identity = lines.next().unwrap_or_default().trim().to_string();
    let lgw = lines.next().unwrap_or_default().trim().to_string();
    if repo.is_empty() {
        return None;
    }
    Some(InstallManifest {
        repo: PathBuf::from(repo),
        identity,
        lgw: PathBuf::from(lgw),
    })
}

fn load_state(paths: &Paths) -> Result<Option<State>> {
    let raw = match fs::read_to_string(&paths.state) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", paths.state.display()));
        }
    };
    match serde_json::from_str(&raw) {
        Ok(state) => Ok(Some(state)),
        Err(error) => {
            eprintln!(
                "warning: ignoring corrupt launch state {}: {error}",
                paths.state.display()
            );
            let mut bad = paths.state.clone();
            bad.set_extension("json.bad");
            let _ = fs::remove_file(&bad);
            let _ = fs::rename(&paths.state, &bad);
            Ok(None)
        }
    }
}

fn write_state(paths: &Paths, state: &State) -> Result<()> {
    let tmp = paths.state.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    serde_json::to_writer_pretty(&mut file, state)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(&tmp, &paths.state)
        .with_context(|| format!("installing {}", paths.state.display()))?;
    Ok(())
}

fn remove_state(paths: &Paths) -> Result<()> {
    match fs::remove_file(&paths.state) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", paths.state.display())),
    }
}

struct Lock {
    _file: File,
}

impl Lock {
    fn acquire(paths: &Paths) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock)
            .with_context(|| format!("opening {}", paths.lock.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", paths.lock.display()));
        }
        Ok(Lock { _file: file })
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| format!("unix {}", now_secs()))
}

fn alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }
    // A child that has exited but has not been reaped yet answers kill(pid, 0).
    // The launcher may be the parent when it is kept alive by a test or a shell
    // wrapper, so a zombie must not be mistaken for a running gateway.
    process_stat(pid).is_some_and(|state| !state.starts_with('Z'))
}

fn process_stat(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn process_start(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn process_matches(state: &State) -> bool {
    alive(state.pid) && process_start(state.pid).as_deref() == Some(state.process_start.as_str())
}

fn terminate(pid: u32, signal: libc::c_int) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("sending signal {signal} to pid {pid}"))
}

fn wait_for_exit(pid: u32, limit: Duration) -> bool {
    let deadline = SystemTime::now() + limit;
    while SystemTime::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !alive(pid)
}

/// Follow a file that may be rotated by a later launch. The viewer stops with
/// Ctrl-C but the server is untouched.
pub fn follow_log(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut inode = file.metadata()?.ino();
    file.seek(SeekFrom::End(0))?;
    let mut buffer = [0u8; 8192];
    let stdout = std::io::stdout();
    loop {
        let read = file.read(&mut buffer)?;
        if read > 0 {
            let mut lock = stdout.lock();
            lock.write_all(&buffer[..read])?;
            lock.flush()?;
            continue;
        }
        std::thread::sleep(Duration::from_millis(300));
        let current = match fs::metadata(path) {
            Ok(metadata) => metadata.ino(),
            Err(_) => continue,
        };
        if current != inode {
            file = OpenOptions::new()
                .read(true)
                .open(path)
                .with_context(|| format!("reopening {}", path.display()))?;
            inode = file.metadata()?.ino();
            let _ = writeln!(std::io::stdout(), "[lgwl] log rotated");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "lgwl-test-{}-{}-{}",
            std::process::id(),
            name,
            now_secs()
        ));
        let _ = fs::remove_dir_all(&dir);
        Paths::from_dir(dir)
    }

    fn fake_lgw(dir: &Path) -> PathBuf {
        let path = dir.join("lgw");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "#!/usr/bin/env bash").unwrap();
        writeln!(file, "echo fake-lgw-ready").unwrap();
        writeln!(file, "trap 'exit 0' TERM").unwrap();
        writeln!(file, "while true; do sleep 0.2; done").unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn launch_tracks_the_process_and_exit_stops_it() {
        let paths = temp_paths("lifecycle");
        paths.ensure().unwrap();
        let lgw = fake_lgw(&paths.dir);
        let launcher = Launcher::with_paths(paths.clone(), lgw);
        let outcome = launcher
            .launch(&LaunchOptions {
                args: vec!["--color".to_string(), "never".to_string()],
                cwd: paths.dir.clone(),
            })
            .unwrap();
        let LaunchOutcome::Started(state) = outcome else {
            panic!("expected started");
        };
        assert!(alive(state.pid));
        assert!(matches!(launcher.status().unwrap(), Status::Running(_)));
        assert!(
            read_tail(&paths.log, 10)
                .unwrap()
                .iter()
                .any(|line| line.contains("fake-lgw-ready"))
        );
        let stopped = launcher.exit().unwrap();
        assert_eq!(
            stopped,
            ExitOutcome::Stopped {
                pid: state.pid,
                forced: false
            }
        );
        assert_eq!(launcher.status().unwrap(), Status::Stopped { stale: false });
        let _ = fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn update_stops_the_gateway_runs_the_installer_and_relaunches_it() {
        let paths = temp_paths("update");
        paths.ensure().unwrap();
        let lgw = fake_lgw(&paths.dir);
        let repo = paths.dir.join("repo");
        fs::create_dir_all(repo.join("scripts")).unwrap();
        let script = repo.join("scripts/lgw-install.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\ntouch \"$0.installed\"\nexit 0\n",
        )
        .unwrap();
        let launcher = Launcher::with_paths(paths.clone(), lgw);
        launcher
            .launch(&LaunchOptions {
                args: vec!["--color".to_string(), "never".to_string()],
                cwd: paths.dir.clone(),
            })
            .unwrap();
        let first_pid = match launcher.status().unwrap() {
            Status::Running(state) => state.pid,
            other => panic!("expected running, got {other:?}"),
        };
        let outcome = launcher
            .update(&UpdateOptions {
                repo: Some(repo.clone()),
                no_restart: false,
            })
            .unwrap();
        assert!(outcome.was_running);
        assert!(outcome.restarted);
        assert!(repo.join("scripts/lgw-install.sh.installed").is_file());
        let second_pid = match launcher.status().unwrap() {
            Status::Running(state) => state.pid,
            other => panic!("expected relaunched, got {other:?}"),
        };
        assert_ne!(first_pid, second_pid);
        launcher.exit().unwrap();
        assert_eq!(launcher.status().unwrap(), Status::Stopped { stale: false });
        let _ = fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn update_can_leave_the_gateway_stopped() {
        let paths = temp_paths("update-stop");
        paths.ensure().unwrap();
        let lgw = fake_lgw(&paths.dir);
        let repo = paths.dir.join("repo");
        fs::create_dir_all(repo.join("scripts")).unwrap();
        fs::write(
            repo.join("scripts/lgw-install.sh"),
            "#!/usr/bin/env bash\nexit 0\n",
        )
        .unwrap();
        let launcher = Launcher::with_paths(paths.clone(), lgw);
        launcher
            .launch(&LaunchOptions {
                args: vec!["--color".to_string(), "never".to_string()],
                cwd: paths.dir.clone(),
            })
            .unwrap();
        let outcome = launcher
            .update(&UpdateOptions {
                repo: Some(repo),
                no_restart: true,
            })
            .unwrap();
        assert!(outcome.was_running);
        assert!(!outcome.restarted);
        assert_eq!(launcher.status().unwrap(), Status::Stopped { stale: false });
        let _ = fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn a_state_with_a_different_process_start_is_never_killed() {
        let paths = temp_paths("stale");
        paths.ensure().unwrap();
        let launcher = Launcher::with_paths(paths.clone(), fake_lgw(&paths.dir));
        let bogus = State {
            version: STATE_VERSION,
            pid: std::process::id(),
            started_at: 0,
            process_start: "a different process".to_string(),
            args: Vec::new(),
            cwd: paths.dir.clone(),
        };
        write_state(&paths, &bogus).unwrap();
        assert_eq!(launcher.status().unwrap(), Status::Stopped { stale: true });
        assert_eq!(launcher.exit().unwrap(), ExitOutcome::NotRunning);
        assert!(alive(std::process::id()));
        let _ = fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn tail_returns_the_last_lines_and_rotation_keeps_one_generation() {
        let paths = temp_paths("log");
        paths.ensure().unwrap();
        fs::write(&paths.log, "a\nb\nc\nd\n").unwrap();
        assert_eq!(read_tail(&paths.log, 2).unwrap(), vec!["c", "d"]);
        fs::write(&paths.log, vec![b'x'; 20]).unwrap();
        assert!(rotate_log(&paths.log, 10).unwrap());
        assert!(paths.log.with_extension("log.1").exists());
        assert!(!rotate_log(&paths.log, 10).unwrap());
        let _ = fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn manifest_parsing_is_three_plain_lines() {
        let paths = temp_paths("manifest");
        paths.ensure().unwrap();
        fs::write(&paths.manifest, "/repo\naigw-dev\n/bin/lgw\n").unwrap();
        let manifest = read_manifest(&paths).expect("manifest");
        assert_eq!(manifest.repo, PathBuf::from("/repo"));
        assert_eq!(manifest.identity, "aigw-dev");
        assert_eq!(manifest.lgw, PathBuf::from("/bin/lgw"));
        let _ = fs::remove_dir_all(&paths.dir);
    }
}
