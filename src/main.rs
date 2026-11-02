//! ug-lite: a low-memory drop-in for `ug mcp-proxy` and `ug auth-token`
//! (https://github.com/databricks/unity-gateway). Every other command or flag
//! is forwarded to the original Python `ug` (see `fallback`).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio, exit};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUTH_FAILURE_EXIT_CODE: i32 = 2;
const VISITED_ENV: &str = "UG_LITE_VISITED";
// Codes ug-lite answers transport failures with (README); the same ones the MCP SDK picks.
const CONNECTION_CLOSED: i64 = -32000;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const PARSE_ERROR: i64 = -32700;
const TOKEN_MEMO_EXPIRY_MARGIN_S: f64 = 300.0;
const TOKEN_MEMO_DEFAULT_TTL_S: f64 = 60.0;
// Caps an absurd lifetime instead of dropping the memo; the CLI re-serves its cached token.
const TOKEN_MEMO_MAX_TTL_S: f64 = 3600.0;
const TOKEN_FETCH_MAX_ATTEMPTS: u32 = 4;
const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(150);
const TOKEN_CACHE_LOCK_MARKERS: &[&str] = &["cache update", "exit status 45"];
const TOKEN_CREDENTIAL_FAILURE_MARKERS: &[&str] = &[
    "invalid_grant",
    "invalid refresh token",
    "refresh token is invalid",
    "expired refresh token",
    "refresh token has expired",
    "no cached token",
    "no cached credentials",
    "not configured for this host",
];

#[derive(Default)]
struct Opts {
    url: Option<String>,
    host: Option<String>,
    profile: Option<String>,
    use_pat: bool,
    force_refresh: bool,
    browser_login: bool,
}

fn main() {
    // args_os: a non-UTF-8 argument is not ours, but must still reach the original `ug` intact.
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args: Vec<String> = raw
        .iter()
        .filter_map(|a| a.to_str().map(String::from))
        .collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let opts = (args.len() == raw.len())
        .then(|| parse_opts(cmd, args.get(1..).unwrap_or_default()))
        .flatten();
    let Some(opts) = opts else { fallback(&raw) };
    let state = load_state();
    if cmd == "mcp-proxy" {
        mcp_proxy(opts, state)
    } else {
        auth_token(opts, state)
    }
}

/// `None` means "not ours": an unknown command, flag, or a missing value.
fn parse_opts(cmd: &str, args: &[String]) -> Option<Opts> {
    let (value_flags, bool_flags): (&[&str], &[&str]) = match cmd {
        "mcp-proxy" => (&["--url", "--host", "--profile"], &["--use-pat"]),
        "auth-token" => (
            &["--host", "--profile"],
            &["--use-pat", "--force-refresh", "--browser-login"],
        ),
        _ => return None,
    };
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        if bool_flags.contains(&flag) && inline.is_none() {
            match flag {
                "--use-pat" => o.use_pat = true,
                "--force-refresh" => o.force_refresh = true,
                _ => o.browser_login = true,
            }
        } else if value_flags.contains(&flag) {
            let v = inline.or_else(|| it.next().cloned())?;
            // typer treats a blank value differently from "unset" (auth-token keeps an empty
            // --profile, `--host ' '` is used as is); leave those to `ug`.
            if v.trim().is_empty() {
                return None;
            }
            let v = Some(v);
            match flag {
                "--url" => o.url = v,
                "--host" => o.host = v,
                _ => o.profile = v,
            }
        } else {
            return None;
        }
    }
    if cmd == "mcp-proxy" && o.url.is_none() {
        return None;
    }
    Some(o)
}

/// Hand everything we don't implement to the original `ug`: `$UG_LITE_FALLBACK`,
/// else the next `ug` on PATH that isn't this binary.
fn fallback(args: &[OsString]) -> ! {
    // Every ug-lite copy that forwarded is recorded here, so copies (or a
    // UG_LITE_FALLBACK pointing at one) can never exec each other in a loop.
    // Fails closed: forwarding without a complete record could loop.
    let refuse = |why: &str| -> ! {
        eprintln!("ug-lite: not forwarding to the original `ug`: {why}");
        exit(2)
    };
    let mut visited: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os(VISITED_ENV).unwrap_or_default()).collect();
    match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(me) => visited.push(me),
        Err(e) => refuse(&format!("cannot resolve its own path: {e}")),
    }
    let Ok(visited_env) = std::env::join_paths(&visited) else {
        refuse(&format!("cannot record {VISITED_ENV} for {visited:?}"))
    };
    let is_new = |p: &PathBuf| p.canonicalize().is_ok_and(|c| !visited.contains(&c));
    let real = std::env::var_os("UG_LITE_FALLBACK")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let name = if cfg!(windows) { "ug.exe" } else { "ug" };
            std::env::split_paths(&std::env::var_os("PATH")?)
                // A relative entry (empty = cwd) would be re-resolved via PATH by exec.
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|p| is_executable(p) && is_new(p))
        })
        .filter(is_new);
    let Some(real) = real else {
        eprintln!(
            "ug-lite: only `mcp-proxy` and `auth-token` are supported here. \
             Set UG_LITE_FALLBACK to the original `ug` to forward other commands."
        );
        exit(2)
    };
    let mut cmd = Command::new(&real);
    cmd.args(args);
    cmd.env(VISITED_ENV, visited_env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("ug-lite: could not run {}: {err}", real.display());
        exit(127)
    }
    #[cfg(not(unix))]
    exit(cmd.status().map(|s| s.code().unwrap_or(1)).unwrap_or(127))
}

/// Like `which`: skip a non-executable `ug` and keep scanning PATH.
fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    p.is_file()
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

/// Saved state of `ug configure`: (workspace, profile, use_pat) of the current workspace.
#[derive(Default)]
struct State {
    workspace: Option<String>,
    profile: Option<String>,
    use_pat: bool,
}

fn load_state() -> State {
    let path = home().join(".ucode/state.json");
    let Some(v) = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v["state_version"] == 3)
    else {
        return State::default();
    };
    let Some(ws) = v["current_workspace"].as_str().filter(|s| !s.is_empty()) else {
        return State::default();
    };
    let w = &v["workspaces"][ws];
    State {
        workspace: Some(ws.to_string()),
        profile: w["profile"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        use_pat: truthy(&w["use_pat"]),
    }
}

/// Python's bool(): Python only ever writes true/false, but read it the way it reads it.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn auth_token(o: Opts, state: State) {
    let explicit_host = o.host.is_some();
    let Some(workspace) = o.host.or(state.workspace) else {
        eprintln!("No workspace configured. Run `ug configure` first.");
        exit(1)
    };
    let profile = if o.profile.is_none() && !explicit_host {
        state.profile
    } else {
        o.profile
    };
    let use_pat = o.use_pat || state.use_pat;
    let auth = match Auth::new(workspace, profile, use_pat, o.browser_login) {
        Ok(a) => a,
        Err(profile) => {
            let p = profile.as_deref();
            eprintln!(
                "--use-pat: no personal access token available for profile '{}'. Add a \
                 `token = <PAT>` entry under [{}] in ~/.databrickscfg, or re-run `ug configure` \
                 without --use-pat to use OAuth.",
                p.unwrap_or("<none>"),
                p.unwrap_or("your-profile")
            );
            exit(1)
        }
    };
    match auth.token(o.force_refresh) {
        // Only the bare token may reach stdout: the agent reads all of it as the credential.
        Ok(t) => println!("{t}"),
        Err(e) => {
            eprintln!("{e}");
            exit(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Token minting (port of ucode.databricks.get_databricks_token)

struct Auth {
    workspace: String,
    profile: Option<String>,
    browser_login: bool,
    /// DATABRICKS_BEARER, or the profile's PAT under --use-pat.
    static_bearer: Option<String>,
    /// Minted token and the wall-clock time until which it may be reused
    /// (wall clock, so a laptop sleep can't stretch it past expiry).
    memo: Mutex<Option<(String, SystemTime)>>,
}

impl Auth {
    /// `Err(profile)` when --use-pat is set but no PAT can be found.
    fn new(
        workspace: String,
        profile: Option<String>,
        use_pat: bool,
        browser_login: bool,
    ) -> Result<Auth, Option<String>> {
        let mut static_bearer = env_nonempty("DATABRICKS_BEARER");
        if use_pat && static_bearer.is_none() {
            static_bearer = profile.as_deref().and_then(pat_for_profile);
            if static_bearer.is_none() {
                return Err(profile);
            }
        }
        Ok(Auth {
            workspace,
            profile,
            browser_login,
            static_bearer,
            memo: Mutex::new(None),
        })
    }

    fn token(&self, force_refresh: bool) -> Result<String, String> {
        if let Some(t) = &self.static_bearer {
            return Ok(t.clone());
        }
        if let Some(cmd) = env_nonempty("DATABRICKS_BEARER_COMMAND") {
            return bearer_from_command(&cmd);
        }
        let mut memo = self.memo.lock().unwrap();
        if !force_refresh
            && let Some((t, until)) = memo.as_ref()
            && SystemTime::now() < *until
        {
            return Ok(t.clone());
        }
        let profile = self
            .profile
            .clone()
            .or_else(|| find_profile(&cfg_sections(), &self.workspace));
        let fetched = match self.fetch_with_lock_retry(profile.as_deref(), force_refresh) {
            Err((transient, _)) if self.browser_login && !transient => {
                self.browser_login(profile.as_deref(), force_refresh)
            }
            r => r,
        };
        // shortcut: no `auth login --no-browser` re-auth; current CLIs (v1.17+) reject that flag.
        match fetched {
            Ok((token, lifetime)) => {
                let ttl = lifetime
                    .map_or(TOKEN_MEMO_DEFAULT_TTL_S, |l| l - TOKEN_MEMO_EXPIRY_MARGIN_S)
                    .min(TOKEN_MEMO_MAX_TTL_S);
                // try_/checked_: an absurd lifetime must not panic (panic=abort) mid-proxy.
                *memo = Duration::try_from_secs_f64(ttl)
                    .ok()
                    .filter(|d| !d.is_zero())
                    .and_then(|d| SystemTime::now().checked_add(d))
                    .map(|until| (token.clone(), until));
                Ok(token)
            }
            Err((transient, detail)) => {
                Err(self.no_token_error(profile.as_deref(), transient, &detail))
            }
        }
    }

    fn no_token_error(&self, profile: Option<&str>, transient: bool, detail: &str) -> String {
        let ws = &self.workspace;
        let mut e = format!("Databricks CLI returned no access token for {ws}. ");
        if transient {
            e += "Retry the request and check network connectivity or Databricks CLI diagnostics.";
            let detail: String = detail.trim().chars().take(500).collect();
            if !detail.is_empty() {
                e += &format!("\nDatabricks CLI: {detail}");
            }
        } else {
            e += "Run `databricks auth login` to re-authenticate.";
            if let Some(p) = profile {
                e += &format!(
                    " The saved Databricks CLI profile may be stale or invalid. Try:\n  \
                     databricks auth logout --profile {p}\n  \
                     databricks auth login --host {ws} --profile {p}"
                );
            }
        }
        e
    }

    fn cli(&self, profile: Option<&str>, args: &[&str]) -> Command {
        // shortcut: runs the first `databricks` on PATH, not the newest of several; pin PATH if that matters.
        let mut c = Command::new("databricks");
        c.args(args).args(["--host", &self.workspace]);
        if let Some(p) = profile {
            c.args(["--profile", p]);
        } else {
            c.env_remove("DATABRICKS_CONFIG_PROFILE");
        }
        c.env("DATABRICKS_HOST", &self.workspace);
        c
    }

    /// `Ok((token, lifetime_s))`, or `Err((transient, lock_contention, detail))`.
    /// `detail` is the CLI's stderr only: stdout is never echoed, it may hold a credential.
    fn fetch(
        &self,
        profile: Option<&str>,
        force_refresh: bool,
    ) -> Result<(String, Option<f64>), (bool, bool, String)> {
        let mut c = self.cli(profile, &["auth", "token", "--output", "json"]);
        if force_refresh {
            c.arg("--force-refresh");
        }
        let (code, out, err) = run(c, Duration::from_secs(15), true).map_err(|e| {
            (
                true,
                false,
                format!("could not run `databricks auth token`: {e}"),
            )
        })?;
        if code == 0 {
            let v: Value = serde_json::from_str(&out)
                .map_err(|e| (true, false, format!("invalid token JSON: {e}")))?;
            let token = v["access_token"].as_str().unwrap_or("");
            return if token.is_empty() {
                Err((true, false, "token response has no access_token".into()))
            } else {
                Ok((token.to_string(), token_lifetime_s(&v)))
            };
        }
        let diag = format!("{err}\n{out}").to_lowercase();
        let transient = !TOKEN_CREDENTIAL_FAILURE_MARKERS
            .iter()
            .any(|m| diag.contains(m));
        let lock = TOKEN_CACHE_LOCK_MARKERS.iter().any(|m| diag.contains(m));
        Err((transient, lock, err))
    }

    /// Concurrent `databricks auth token` calls race on the shared token cache; retry those.
    fn fetch_with_lock_retry(
        &self,
        profile: Option<&str>,
        force: bool,
    ) -> Result<(String, Option<f64>), (bool, String)> {
        let mut last = String::new();
        for attempt in 0..TOKEN_FETCH_MAX_ATTEMPTS {
            match self.fetch(profile, force) {
                Ok(t) => return Ok(t),
                Err((_, true, detail)) => {
                    last = detail;
                    if attempt + 1 < TOKEN_FETCH_MAX_ATTEMPTS {
                        let max_ms = 100u64 << attempt;
                        thread::sleep(Duration::from_millis(50 + nanos() % (max_ms - 49)));
                    }
                }
                Err((transient, false, detail)) => return Err((transient, detail)),
            }
        }
        Err((true, last))
    }

    /// Desktop helpers (`--browser-login`) have no terminal, so sign in via the browser.
    /// Serialized by a lock file because every helper shares the CLI's callback port.
    fn browser_login(
        &self,
        profile: Option<&str>,
        force: bool,
    ) -> Result<(String, Option<f64>), (bool, String)> {
        let started = Instant::now();
        let lock_path = home().join(".ucode/auth-login.lock");
        let _ = fs::create_dir_all(lock_path.parent().unwrap());
        let lock = File::options().create(true).append(true).open(&lock_path);
        // Bounded like the sign-in itself: a helper stuck on an abandoned browser login
        // must not block this one forever.
        if let Ok(f) = &lock {
            loop {
                match f.try_lock() {
                    Ok(()) => break,
                    Err(fs::TryLockError::WouldBlock)
                        if started.elapsed() < BROWSER_LOGIN_TIMEOUT =>
                    {
                        thread::sleep(Duration::from_millis(200))
                    }
                    Err(fs::TryLockError::WouldBlock) => {
                        return Err((
                            false,
                            "timed out waiting for another browser sign-in".into(),
                        ));
                    }
                    Err(fs::TryLockError::Error(_)) => break,
                }
            }
        }
        // Another helper may have signed in while we waited.
        if let Ok(t) = self.fetch_with_lock_retry(profile, force) {
            return Ok(t);
        }
        let Some(remaining) = BROWSER_LOGIN_TIMEOUT.checked_sub(started.elapsed()) else {
            return Err((false, "browser sign-in timed out".into()));
        };
        let c = self.cli(profile, &["auth", "login"]);
        let _ = run(c, remaining, false);
        self.fetch_with_lock_retry(profile, force)
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
}

/// Re-run on every fetch; fails closed rather than falling back to OAuth.
fn bearer_from_command(command: &str) -> Result<String, String> {
    let argv = shlex::split(command)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            format!("DATABRICKS_BEARER_COMMAND could not be parsed. Command: {command}")
        })?;
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    let (code, out, err) = run(c, Duration::from_secs(15), true).map_err(|e| {
        format!("DATABRICKS_BEARER_COMMAND could not be run: {e}. Command: {command}")
    })?;
    let token = out.trim();
    if code == 0 && !token.is_empty() {
        return Ok(token.to_string());
    }
    let reason = if code != 0 {
        format!("exited {code}")
    } else {
        "printed no token".into()
    };
    let err: String = err.trim().chars().take(500).collect();
    let detail = if err.is_empty() {
        String::new()
    } else {
        format!(" Stderr: {err}")
    };
    Err(format!(
        "DATABRICKS_BEARER_COMMAND {reason}. Command: {command}.{detail}"
    ))
}

/// Run with a timeout. `capture` pipes stdout/stderr back; otherwise both go to our
/// stderr (stdout is the MCP wire / the token). Returns (exit code, stdout, stderr).
fn run(mut c: Command, timeout: Duration, capture: bool) -> Result<(i32, String, String), String> {
    c.stdin(Stdio::null());
    if capture {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        c.stdout(Stdio::from(io::stderr()))
            .stderr(Stdio::from(io::stderr()));
    }
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let drain = |p: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        // No panic if no thread can start: the dropped sender makes `collect` fail instead.
        let _ = thread::Builder::new().spawn(move || {
            let _ = tx.send(p.and_then(|p| read_lossy(p).ok()).unwrap_or_default());
        });
        rx
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + timeout;
    let timed_out = || format!("timed out after {}s", timeout.as_secs());
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(timed_out());
        }
        thread::sleep(Duration::from_millis(5));
    };
    // Bounded too: a grandchild that inherited the pipes can keep them open after exit.
    let collect = |rx: mpsc::Receiver<String>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => timed_out(),
                mpsc::RecvTimeoutError::Disconnected => "could not read its output".into(),
            })
    };
    Ok((status.code().unwrap_or(-1), collect(out)?, collect(err)?))
}

/// Seconds until expiry: the earlier of `expires_in` and the absolute `expiry`
/// (the CLI can echo a stale as-issued `expires_in`).
fn token_lifetime_s(v: &Value) -> Option<f64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    let a = v["expires_in"].as_f64();
    let b = v["expiry"]
        .as_str()
        .and_then(parse_rfc3339)
        .map(|t| t as f64 - now);
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` -> unix seconds. No offset -> None.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    let mut rest = s.get(19..)?;
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = if rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        sign * (oh * 3600 + om * 60)
    };
    // days_from_civil (Howard Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se - offset)
}

// ---------------------------------------------------------------------------
// ~/.databrickscfg (read directly instead of spawning `databricks auth profiles`)

fn cfg_sections() -> Vec<(String, HashMap<String, String>)> {
    // `~/` is expanded like Python's Path.expanduser(), since the value may come unexpanded.
    let path = std::env::var("DATABRICKS_CONFIG_FILE")
        .ok()
        .filter(|v| !v.is_empty())
        // shortcut: `~user/...` is not expanded (needs a passwd lookup); bare `~` is a directory
        // and reads as empty either way.
        .map(|v| match v.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(v),
        })
        .unwrap_or_else(|| home().join(".databrickscfg"));
    parse_ini(&fs::read_to_string(path).unwrap_or_default())
}

fn parse_ini(text: &str) -> Vec<(String, HashMap<String, String>)> {
    let mut out: Vec<(String, HashMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        // Like configparser's `\[(?P<header>.+)\]`: text after the last `]` (a comment) is ignored.
        if let Some(name) = t
            .strip_prefix('[')
            .and_then(|r| r.rfind(']').map(|i| &r[..i]))
        {
            out.push((name.trim().to_string(), HashMap::new()));
        } else if let (Some((k, v)), Some(last)) = (t.split_once(['=', ':']), out.last_mut()) {
            last.1.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    out
}

fn is_pat(s: &HashMap<String, String>) -> bool {
    s.get("auth_type")
        .map_or(s.get("token").is_some_and(|t| !t.is_empty()), |a| {
            a == "pat"
        })
}

/// First non-PAT profile whose host matches (avoids CLI errors on duplicate-host configs).
fn find_profile(sections: &[(String, HashMap<String, String>)], workspace: &str) -> Option<String> {
    let ws = workspace.trim_end_matches('/');
    sections
        .iter()
        .find(|(_, s)| !is_pat(s) && s.get("host").is_some_and(|h| h.trim_end_matches('/') == ws))
        .map(|(name, _)| name.clone())
}

fn pat_for_profile(profile: &str) -> Option<String> {
    cfg_sections()
        .into_iter()
        .find(|(name, _)| name == profile)
        .filter(|(_, s)| is_pat(s))
        .and_then(|(_, mut s)| s.remove("token"))
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// stdio <-> streamable-HTTP MCP bridge

struct Proxy {
    url: String,
    auth: Auth,
    http: ureq::Agent,
    out: Mutex<Box<dyn Write + Send>>,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
}

fn fail_fast(msg: &str) -> ! {
    // stdout is the MCP wire, so diagnostics go to stderr (clients show it on failure).
    eprintln!("ug mcp-proxy: {msg}");
    exit(AUTH_FAILURE_EXIT_CODE)
}

/// Same trust as the SDK's httpx2: SSL_CERT_FILE, else SSL_CERT_DIR, else the OS trust store
/// (so corporate CAs installed in the keychain work).
fn root_certs() -> ureq::tls::RootCerts {
    let pem = if let Some(file) = env_nonempty("SSL_CERT_FILE") {
        fs::read(&file).map_err(|e| format!("SSL_CERT_FILE {file}: {e}"))
    } else if let Some(dirs) = env_nonempty("SSL_CERT_DIR") {
        // OpenSSL capath: every PEM file in each listed directory.
        std::env::split_paths(&dirs)
            .map(|d| fs::read_dir(&d).map_err(|e| format!("SSL_CERT_DIR {}: {e}", d.display())))
            .collect::<Result<Vec<_>, _>>()
            .map(|dirs| {
                dirs.into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| fs::read(e.path()).ok())
                    .flat_map(|mut b| {
                        b.push(b'\n');
                        b
                    })
                    .collect()
            })
    } else {
        return ureq::tls::RootCerts::PlatformVerifier;
    };
    let pem = pem.unwrap_or_else(|e| fail_fast(&e));
    let certs: Vec<_> = ureq::tls::parse_pem(&pem)
        .filter_map(|item| match item {
            Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
            _ => None,
        })
        .collect();
    if certs.is_empty() {
        fail_fast("no certificates found in SSL_CERT_FILE / SSL_CERT_DIR");
    }
    ureq::tls::RootCerts::new_with_certs(&certs)
}

fn mcp_proxy(o: Opts, state: State) {
    let Some(workspace) = o.host.or(state.workspace) else {
        eprintln!("No workspace configured. Run `ug configure` first.");
        exit(1)
    };
    let profile = o.profile.or(state.profile);
    let auth =
        Auth::new(workspace, profile, o.use_pat || state.use_pat, false).unwrap_or_else(|p| {
            fail_fast(&format!(
                "--use-pat is set but no personal access token was found for profile '{}' in \
             ~/.databrickscfg (expected auth_type = pat). Set DATABRICKS_BEARER, or reconfigure \
             the profile.",
                p.as_deref().unwrap_or("<none>")
            ))
        });
    // Fail before serving, so the agent shows the CLI's message instead of a startup timeout.
    if let Err(e) = auth.token(false) {
        fail_fast(&e)
    }
    let http: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_write(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(300)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(root_certs())
                .build(),
        )
        .build()
        .into();
    let proxy = Arc::new(Proxy {
        url: o.url.unwrap(),
        auth,
        http,
        out: Mutex::new(Box::new(io::stdout())),
        session: Mutex::new(None),
        protocol: Mutex::new(None),
    });
    // shortcut: no standalone GET SSE stream for server-initiated messages outside a request,
    // and no per-connection sign-in for connection-backed services (see README).
    // Read like the SDK's stdio_server: utf-8 with errors="replace" and universal newlines.
    let mut stdin = Lines::new(io::stdin().lock());
    while let Ok(Some(line)) = stdin.next_line() {
        if line.trim().is_empty() {
            continue;
        }
        // JSON-RPC 2.0: an unparsable or invalid message gets an error with id null and is
        // not forwarded, so every reply the client sees carries a valid id.
        let msg = client_message(&line);
        let msg = match msg {
            Ok(v) => v,
            Err((code, e)) => {
                eprintln!("ug mcp-proxy: rejected a client message: {e}");
                proxy.write(
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": e}}),
                );
                continue;
            }
        };
        // Requests run concurrently (a tool call can take minutes); notifications and
        // responses go in order, like the MCP SDK's streamable-HTTP client.
        if msg.get("method").is_some() && msg.get("id").is_some() {
            let (p, id) = (Arc::clone(&proxy), msg["id"].clone());
            // Out of threads must not abort the proxy (panic = abort): answer this request.
            if let Err(e) = thread::Builder::new().spawn(move || p.post(msg)) {
                let e = format!("cannot start a thread for this request: {e}");
                eprintln!("ug mcp-proxy: {e}");
                proxy.write(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": INTERNAL_ERROR, "message": e}}));
            }
        } else {
            proxy.post(msg);
        }
    }
    proxy.close();
}

impl Proxy {
    fn post(&self, msg: Value) {
        let request_id = msg.get("method").and(msg.get("id")).cloned();
        let is_init = msg["method"] == "initialize";
        if let Err((code, e)) = self.send(&msg, request_id.as_ref(), is_init) {
            eprintln!("ug mcp-proxy: {e}");
            if let Some(id) = request_id {
                self.write(
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": e}}),
                );
            }
        }
    }

    /// `session` is the session id to send (none on initialize, which starts a new one).
    fn request(
        &self,
        req: ureq::RequestBuilder<ureq::typestate::WithBody>,
        token: &str,
        session: Option<&str>,
    ) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let mut req = req.header("Authorization", format!("Bearer {token}"));
        if let Some(s) = session {
            req = req.header("mcp-session-id", s);
            if let Some(v) = self.protocol.lock().unwrap().as_deref() {
                req = req.header("mcp-protocol-version", v);
            }
        }
        req
    }

    /// `Err((JSON-RPC error code, message))` is answered to the client by `post`.
    /// Mirrors the MCP SDK's `_handle_post_request`, including its stand-in error codes.
    fn send(
        &self,
        msg: &Value,
        request_id: Option<&Value>,
        is_init: bool,
    ) -> Result<(), (i64, String)> {
        let internal = |e: String| (INTERNAL_ERROR, e);
        let closed = |e: String| (CONNECTION_CLOSED, e);
        // After startup a token failure is answered, not fatal: the session survives a re-login.
        let token = self.auth.token(false).map_err(internal)?;
        // MCP: an InitializeRequest is sent without a session id.
        let sent = (!is_init)
            .then(|| self.session.lock().unwrap().clone())
            .flatten();
        let resp = self
            .request(self.http.post(&self.url), &token, sent.as_deref())
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .send(msg.to_string())
            .map_err(|e| closed(format!("upstream request failed: {e}")))?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        };
        let ctype = header("content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let session = header("mcp-session-id");
        let body = resp.into_body().into_reader();
        if status == 202 {
            return match request_id {
                Some(_) => Err((
                    INVALID_REQUEST,
                    "server answered a request with 202 Accepted".into(),
                )),
                None => Ok(()),
            };
        }
        if !(200..300).contains(&status) {
            // MCP: 404 to a request with a session id means the session is gone. Forget it
            // first, whatever the body says (unless a re-initialize already replaced it), so
            // the client can initialize anew.
            if status == 404 && sent.is_some() {
                let mut current = self.session.lock().unwrap();
                if *current == sent {
                    *current = None;
                }
            }
            let text = read_lossy(body.take(64 * 1024)).unwrap_or_default();
            // A server may put the request's JSON-RPC error in a 4xx/5xx body (e.g. -32006);
            // pass it on under the request's id when it is a well-formed error response.
            let error = serde_json::from_str::<Value>(&text)
                .ok()
                .filter(|v| status >= 400 && rpc_kind(v) == Some(Rpc::Response))
                .and_then(|mut v| v.get_mut("error").map(Value::take));
            if let (Some(id), Some(error)) = (request_id, error) {
                self.write(&json!({"jsonrpc": "2.0", "id": id, "error": error}));
                return Ok(());
            }
            let detail = format!("upstream HTTP {status}: {}", text.trim());
            return Err(match status {
                404 if sent.is_some() => {
                    (INVALID_REQUEST, format!("Session terminated ({detail})"))
                }
                404 => (METHOD_NOT_FOUND, detail),
                300..=399 => (INVALID_REQUEST, detail),
                _ => (INTERNAL_ERROR, detail),
            });
        }
        // MCP: the session id comes with the response holding the InitializeResult.
        let init = is_init.then_some(session.as_deref());
        // A server MUST NOT answer notifications or responses; the SDK never reads that body.
        let Some(id) = request_id else {
            return Ok(());
        };
        let parse_error =
            |what: &str, e: String| (PARSE_ERROR, format!("Failed to parse {what}: {e}"));
        if ctype.starts_with("text/event-stream") {
            // The SDK stops reading once the request's response (or a parse error) arrives.
            let mut outcome = None;
            read_sse(BufReader::new(body), |data| {
                // MCP: an event with empty data is a resumption priming event, not a message.
                if data.is_empty() {
                    return false;
                }
                match self.forward(data, init, id) {
                    Ok(false) => return false,
                    Ok(true) => outcome = Some(Ok(())),
                    Err(e) => outcome = Some(Err(parse_error("SSE message", e))),
                }
                true
            })
            .map_err(|e| closed(format!("upstream SSE stream failed: {e}")))?;
            // shortcut: no SSE resumption (Last-Event-ID); a stream cut before the response fails the request.
            outcome.unwrap_or_else(|| {
                Err(closed(
                    "upstream SSE stream ended without a response".into(),
                ))
            })
        } else if ctype.starts_with("application/json") {
            let text =
                read_lossy(body).map_err(|e| closed(format!("upstream read failed: {e}")))?;
            match self.forward(&text, init, id) {
                Ok(true) => Ok(()),
                Ok(false) => Err(internal("upstream JSON body held no response".into())),
                Err(e) => Err(parse_error("JSON response", e)),
            }
        } else {
            Err((INVALID_REQUEST, format!("Unexpected content type: {ctype}")))
        }
    }

    /// Write one upstream message, received on the POST of request `id`, to the client.
    /// Returns whether it was the response; `Err` (nothing written) when it isn't a
    /// well-formed MCP JSON-RPC message, which the client would drop and then wait forever.
    /// `init` is `Some(session header)` on an initialize POST.
    fn forward(&self, data: &str, init: Option<Option<&str>>, id: &Value) -> Result<bool, String> {
        let mut v: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
        let is_response = match rpc_kind(&v) {
            Some(kind) => kind == Rpc::Response,
            None => return Err("not a well-formed JSON-RPC message".into()),
        };
        // An InitializeResult names its protocolVersion; nothing else starts a session.
        if let Some(session) = init
            && let Some(p) = v.pointer("/result/protocolVersion").and_then(Value::as_str)
        {
            *self.protocol.lock().unwrap() = Some(p.to_string());
            *self.session.lock().unwrap() = session.map(String::from);
        }
        // The response on this request's POST answers it, whatever id it carries.
        if is_response {
            v["id"] = id.clone();
        }
        self.write(&v);
        Ok(is_response)
    }

    fn write(&self, v: &Value) {
        // Serialized compactly so each message is exactly one line on the stdio transport.
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{v}").and_then(|_| out.flush());
    }

    /// Client closed stdin: end the upstream session, like the SDK's terminate_on_close.
    fn close(&self) {
        let Some(session) = self.session.lock().unwrap().clone() else {
            return;
        };
        // Best effort: a token failure at shutdown must not turn a clean EOF into exit 2.
        let Ok(token) = self.auth.token(false) else {
            return;
        };
        let req = self.request(
            self.http.delete(&self.url).force_send_body(),
            &token,
            Some(&session),
        );
        let _ = req
            .config()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .send_empty();
    }
}

/// A client line as a JSON-RPC message, or the (code, message) to reject it with.
/// Rejects only what the proxy cannot route: unparsable JSON, a non-object (MCP has no
/// batches), or an id that is not a string/number. Everything else goes upstream, whose
/// server owns request validation and answers under the request's id. Rejections carry
/// id null, as JSON-RPC 2.0 requires when the id is not detectable.
fn client_message(line: &str) -> Result<Value, (i64, String)> {
    let v: Value =
        serde_json::from_str(line).map_err(|e| (PARSE_ERROR, format!("Parse error: {e}")))?;
    if !v.is_object() || !v.get("id").is_none_or(is_request_id) {
        return Err((INVALID_REQUEST, "Invalid Request".into()));
    }
    Ok(v)
}

/// MCP RequestId: a string or a number.
fn is_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

#[derive(PartialEq)]
enum Rpc {
    Message,
    Response,
}

/// Shape check from the MCP schema's JSON-RPC types (not the SDK's implementation):
/// request/notification = string `method` (+ object `params`); response = object `result`,
/// or `error` with an integer `code` and a string `message`. The id is the caller's concern.
fn rpc_kind(v: &Value) -> Option<Rpc> {
    if v["jsonrpc"] != "2.0" {
        return None;
    }
    let id = v.get("id");
    if v.get("method").is_some() {
        let params_ok = v.get("params").is_none_or(Value::is_object);
        let ok = v["method"].is_string() && params_ok && id.is_none_or(is_request_id);
        return ok.then_some(Rpc::Message);
    }
    // A response needs an id; null only on an error whose request id was undetectable.
    let ok = match (v.get("result"), v.get("error")) {
        (Some(r), None) => r.is_object() && id.is_some_and(is_request_id),
        (None, Some(e)) => {
            e["code"].is_i64()
                && e["message"].is_string()
                && id.is_some_and(|i| i.is_null() || is_request_id(i))
        }
        _ => false,
    };
    ok.then_some(Rpc::Response)
}

/// Calls `on_data` per SSE `message` event until it returns true or the stream ends.
fn read_sse(r: impl BufRead, mut on_data: impl FnMut(&str) -> bool) -> io::Result<()> {
    let (mut event, mut data) = (String::new(), String::new());
    let mut lines = Lines::new(r);
    let mut first = true;
    // WHATWG event-stream parsing; an event cut off by EOF (no blank line) is discarded.
    while let Some(line) = lines.next_line()? {
        let line = match std::mem::take(&mut first) {
            true => line
                .strip_prefix('\u{feff}')
                .map(String::from)
                .unwrap_or(line),
            false => line,
        };
        if line.is_empty() {
            // Each data line adds "value\n"; an event is dispatched when the buffer is
            // non-empty, with the last "\n" dropped (so a bare `data:` dispatches "").
            let payload = data.strip_suffix('\n').unwrap_or(&data);
            if (event.is_empty() || event == "message") && !data.is_empty() && on_data(payload) {
                return Ok(());
            }
            event.clear();
            data.clear();
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = value.to_string(),
            "data" => {
                data.push_str(value);
                data.push('\n');
            }
            _ => {}
        }
    }
    Ok(())
}

/// Lines ending in LF, CRLF, or a lone CR (the SSE spec, and Python's universal newlines),
/// decoded lossily. A CR ends its line at once; an LF right after it is skipped later, so a
/// pause between them never delays the line.
struct Lines<R> {
    r: R,
    skip_lf: bool,
}

impl<R: BufRead> Lines<R> {
    fn new(r: R) -> Self {
        Lines { r, skip_lf: false }
    }

    /// `Ok(None)` at EOF.
    fn next_line(&mut self) -> io::Result<Option<String>> {
        let mut line = Vec::new();
        loop {
            let buf = self.r.fill_buf()?;
            if buf.is_empty() {
                return Ok((!line.is_empty()).then(|| String::from_utf8_lossy(&line).into_owned()));
            }
            let start = usize::from(std::mem::take(&mut self.skip_lf) && buf[0] == b'\n');
            match buf[start..].iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(i) => {
                    line.extend_from_slice(&buf[start..start + i]);
                    self.skip_lf = buf[start + i] == b'\r';
                    self.r.consume(start + i + 1);
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
                None => {
                    line.extend_from_slice(&buf[start..]);
                    let n = buf.len();
                    self.r.consume(n);
                }
            }
        }
    }
}

/// Lossy like the SDK: one stray non-UTF-8 byte must not lose a whole message.
fn read_lossy(mut r: impl Read) -> io::Result<String> {
    let mut b = Vec::new();
    r.read_to_end(&mut b)?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-11-02T21:18:59.576522+09:00"),
            Some(1793621939)
        );
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00-01:30"), Some(951831000));
        assert_eq!(parse_rfc3339("2026-11-02T21:18:59"), None);
    }

    #[test]
    fn lifetime_takes_earliest() {
        let v = json!({"expires_in": 86400, "expiry": "1970-01-01T00:00:00Z"});
        assert!(token_lifetime_s(&v).unwrap() < 0.0);
        assert_eq!(token_lifetime_s(&json!({"expires_in": 10})), Some(10.0));
        assert_eq!(token_lifetime_s(&json!({})), None);
    }

    #[test]
    fn ini_profiles() {
        let s = parse_ini(
            "; c\n[pat]\nhost = https://a.com\ntoken = dapi1\n\n[oauth]\nhost=https://a.com/\n\
             auth_type = databricks-cli\n[DEFAULT]\nhost: https://b.com\n[work] # c\nhost = https://w.com\n",
        );
        assert_eq!(find_profile(&s, "https://a.com"), Some("oauth".into()));
        assert_eq!(find_profile(&s, "https://b.com/"), Some("DEFAULT".into()));
        assert_eq!(find_profile(&s, "https://c.com"), None);
        assert_eq!(find_profile(&s, "https://w.com"), Some("work".into()));
        assert!(is_pat(&s[0].1) && !is_pat(&s[1].1));
    }

    #[test]
    fn sse() {
        let stream = ": ping\n\nevent: message\ndata: {\"a\":1}\n\nevent: other\ndata: x\n\n\
                      data: {\"b\":\ndata: 2}\r\n\r\ndata: {\"c\":3}\n";
        let mut got = vec![];
        read_sse(stream.as_bytes(), |d| {
            got.push(d.to_string());
            false
        })
        .unwrap();
        // The last event has no terminating blank line, so EOF discards it.
        assert_eq!(got, ["{\"a\":1}", "{\"b\":\n2}"]);
        let mut n = 0;
        read_sse(stream.as_bytes(), |_| {
            n += 1;
            true
        })
        .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn opts() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = parse_opts("mcp-proxy", &a(&["--url", "u", "--profile=p", "--use-pat"])).unwrap();
        assert_eq!(
            (o.url.as_deref(), o.profile.as_deref(), o.use_pat),
            (Some("u"), Some("p"), true)
        );
        assert!(parse_opts("mcp-proxy", &a(&["--host", "h"])).is_none());
        assert!(parse_opts("auth-token", &a(&["--client-id", "x"])).is_none());
        assert!(parse_opts("auth-token", &a(&["--help"])).is_none());
        assert!(parse_opts("configure", &[]).is_none());
        assert!(parse_opts("auth-token", &a(&["--profile", ""])).is_none());
        assert!(parse_opts("auth-token", &a(&["--profile="])).is_none());
        assert!(parse_opts("auth-token", &a(&["--host", " "])).is_none());
        assert!(
            parse_opts("auth-token", &a(&["--browser-login", "--host", "h"]))
                .unwrap()
                .browser_login
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_when_grandchild_holds_pipes() {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 5 & echo hi"]);
        let t = Instant::now();
        assert!(run(c, Duration::from_millis(500), true).is_err());
        assert!(t.elapsed() < Duration::from_secs(3));
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; echo err >&2; exit 3"]);
        let (code, out, err) = run(c, Duration::from_secs(5), true).unwrap();
        assert_eq!((code, out.as_str(), err.as_str()), (3, "out\n", "err\n"));
    }

    /// Serves one canned HTTP response on a local port and returns its URL.
    fn mock(response: &'static str) -> String {
        mock_held(response, Duration::ZERO)
    }

    /// Like `mock`, but keeps the connection open for `hold` after writing.
    fn mock_held(response: &'static str, hold: Duration) -> String {
        mock_seen(response, hold).0
    }

    /// Like `mock_held`, also handing back the raw request it received.
    fn mock_seen(response: &'static str, hold: Duration) -> (String, mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", l.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_lowercase());
            let _ = s.write_all(response.as_bytes());
            thread::sleep(hold);
        });
        (url, rx)
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl Write for Captured {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Captured {
        fn lines(&self) -> Vec<Value> {
            let b = self.0.lock().unwrap();
            String::from_utf8_lossy(&b)
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    fn proxy(response: &'static str) -> (Proxy, Captured) {
        proxy_at(mock(response))
    }

    fn proxy_at(url: String) -> (Proxy, Captured) {
        let out = Captured::default();
        let p = Proxy {
            url,
            auth: Auth {
                workspace: "https://w".into(),
                profile: None,
                browser_login: false,
                static_bearer: Some("t".into()),
                memo: Mutex::new(None),
            },
            http: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .build()
                .into(),
            out: Mutex::new(Box::new(out.clone())),
            session: Mutex::new(None),
            protocol: Mutex::new(None),
        };
        (p, out)
    }

    fn reply(response: &'static str, msg: Value, session: bool) -> Vec<Value> {
        let (p, out) = proxy(response);
        if session {
            *p.session.lock().unwrap() = Some("s0".into());
        }
        p.post(msg);
        out.lines()
    }

    #[test]
    fn every_request_gets_exactly_one_response() {
        let req = || json!({"jsonrpc": "2.0", "id": 7, "method": "ping"});
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let sse_note_only = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                             data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let sse_other_id = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                            data: {\"jsonrpc\":\"2.0\",\"id\":\"x\",\"result\":{}}\n\n";
        let accepted = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let empty_json = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let rpc_error_400 = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 77\r\n\
                             connection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32006,\"message\":\"auth required\"}}";
        let not_found = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let redirect = "HTTP/1.1 302 Found\r\nlocation: /login\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let code = |v: &[Value]| {
            assert_eq!(v.len(), 1, "{v:?}");
            assert_eq!(v[0]["id"], 7);
            v[0]["error"]["code"].as_i64()
        };
        let notes = reply(sse_note_only, req(), false);
        assert_eq!(notes.len(), 2);
        assert_eq!(code(&notes[1..]), Some(CONNECTION_CLOSED));
        assert_eq!(code(&reply(sse_other_id, req(), false)), None);
        assert_eq!(code(&reply(accepted, req(), false)), Some(INVALID_REQUEST));
        assert_eq!(code(&reply(empty_json, req(), false)), Some(PARSE_ERROR));
        let text_plain = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        assert_eq!(
            code(&reply(text_plain, req(), false)),
            Some(INVALID_REQUEST)
        );
        let sse_bad_json = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                            data: {oops\n\n";
        assert_eq!(code(&reply(sse_bad_json, req(), false)), Some(PARSE_ERROR));
        let error_not_object = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 39\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":\"boom\"}";
        assert_eq!(
            code(&reply(error_not_object, req(), false)),
            Some(PARSE_ERROR)
        );
        let error_no_code = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 48\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"x\"}}";
        assert_eq!(code(&reply(error_no_code, req(), false)), Some(PARSE_ERROR));
        let result_null = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 38\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}";
        assert_eq!(code(&reply(result_null, req(), false)), Some(PARSE_ERROR));
        let bad_params = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 41\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"n\",\"params\":1}";
        assert_eq!(code(&reply(bad_params, req(), false)), Some(PARSE_ERROR));
        let rate_limited = "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: 47\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"rate limited\",\"type\":\"x\"}}";
        assert_eq!(
            code(&reply(rate_limited, req(), false)),
            Some(INTERNAL_ERROR)
        );
        let redirect_json = "HTTP/1.1 302 Found\r\nlocation: /x\r\ncontent-type: application/json\r\ncontent-length: 58\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-1,\"message\":\"m\"}}";
        assert_eq!(
            code(&reply(redirect_json, req(), false)),
            Some(INVALID_REQUEST)
        );
        let not_rpc = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 24\r\n\
                       connection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1}";
        assert_eq!(code(&reply(not_rpc, req(), false)), Some(PARSE_ERROR));
        assert_eq!(code(&reply(rpc_error_400, req(), false)), Some(-32006));
        assert_eq!(code(&reply(not_found, req(), true)), Some(INVALID_REQUEST));
        assert_eq!(
            code(&reply(not_found, req(), false)),
            Some(METHOD_NOT_FOUND)
        );
        assert_eq!(code(&reply(redirect, req(), false)), Some(INVALID_REQUEST));
        assert!(reply(accepted, note, false).is_empty());
    }

    #[test]
    fn session_id_only_from_successful_initialize() {
        let ok_json = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\n\
                       content-length: 36\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let failed = "HTTP/1.1 500 Oops\r\nmcp-session-id: s2\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let init = || json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let (p, out) = proxy(ok_json);
        p.post(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
        assert_eq!(*p.session.lock().unwrap(), None);
        assert_eq!(
            out.lines(),
            [json!({"jsonrpc": "2.0", "id": 1, "result": {}})]
        );
        let (p, _) = proxy(failed);
        p.post(init());
        assert_eq!(*p.session.lock().unwrap(), None);
        let init_error = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s3\r\n\
                          content-length: 64\r\nconnection: close\r\n\r\n\
                          {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"bad\"}}";
        let (p, _) = proxy(init_error);
        p.post(init());
        assert_eq!(*p.session.lock().unwrap(), None);
        let init_ok = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\n\
                       content-length: 66\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\"}}";
        // A bare result is not an InitializeResult.
        let (p, _) = proxy(ok_json);
        p.post(init());
        assert_eq!(*p.session.lock().unwrap(), None);
        let (p, _) = proxy(init_ok);
        p.post(init());
        assert_eq!(p.session.lock().unwrap().as_deref(), Some("s1"));
    }

    #[test]
    fn notification_never_waits_on_a_response_body() {
        let sse_open = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {}\n\n";
        let (p, out) = proxy_at(mock_held(sse_open, Duration::from_secs(10)));
        let t = Instant::now();
        p.post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(out.lines().is_empty());
    }

    #[test]
    fn sse_lossy_and_line_endings() {
        let mut got = vec![];
        let stream: &[u8] = b"data: a\xffb\n\ndata: c\rdata: d\r\rdata: e\r\n\r\n";
        read_sse(stream, |d| {
            got.push(d.to_string());
            false
        })
        .unwrap();
        assert_eq!(got, ["a\u{fffd}b", "c\nd", "e"]);
    }

    #[cfg(unix)]
    #[test]
    fn run_decodes_lossily_and_executable_check() {
        let mut c = Command::new("sh");
        c.args(["-c", "printf 'invalid_grant \\377' >&2"]);
        let (_, _, err) = run(c, Duration::from_secs(5), true).unwrap();
        assert!(err.starts_with("invalid_grant"));
        let dir = std::env::temp_dir().join(format!("ug-lite-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("ug");
        fs::write(&f, "").unwrap();
        assert!(!is_executable(&f));
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&f));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lines_split_lf_crlf_cr_without_waiting() {
        let mut l = Lines::new(&b"a\rb\r\nc\n\r\nd"[..]);
        let mut got = vec![];
        while let Some(x) = l.next_line().unwrap() {
            got.push(x);
        }
        assert_eq!(got, ["a", "b", "c", "", "d"]);
        // A lone CR ends the line now; reading on would hit the error.
        struct Once(bool);
        impl Read for Once {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    return Err(io::Error::other("read past the line"));
                }
                b[..2].copy_from_slice(b"x\r");
                Ok(2)
            }
        }
        let mut l = Lines::new(BufReader::new(Once(false)));
        assert_eq!(l.next_line().unwrap().as_deref(), Some("x"));
        assert!(l.next_line().is_err());
    }

    #[test]
    fn sse_data_lines_follow_the_spec() {
        let mut got = vec![];
        read_sse(
            &b"data:\ndata: x\n\ndata:\n\nevent: message\ndata: y"[..],
            |d| {
                got.push(d.to_string());
                false
            },
        )
        .unwrap();
        assert_eq!(got, ["\nx", ""]);
    }

    #[test]
    fn client_messages_are_validated() {
        let ok = |l: &str| client_message(l).is_ok();
        let code = |l: &str| client_message(l).unwrap_err().0;
        assert!(ok(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        assert!(ok(r#"{"jsonrpc":"2.0","id":"a","result":{}}"#));
        assert!(ok(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        ));
        // Routable but malformed: the upstream server validates and answers these.
        assert!(ok(r#"{"jsonrpc":"2.0","id":1,"method":5}"#));
        assert!(ok(r#"{"jsonrpc":"2.0","id":5,"method":"t","params":[1]}"#));
        assert_eq!(code("{oops"), PARSE_ERROR);
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":{},"method":"x"}"#),
            INVALID_REQUEST
        );
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":true,"result":{}}"#),
            INVALID_REQUEST
        );
        assert_eq!(
            code(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#),
            INVALID_REQUEST
        );
        // Upstream direction: rpc_kind checks shapes and ids before anything reaches the client.
        assert!(rpc_kind(&json!({"jsonrpc": "2.0", "id": {}, "method": "roots/list"})).is_none());
        assert!(rpc_kind(&json!({"jsonrpc": "2.0", "id": true, "method": "n"})).is_none());
        let null_id_error =
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": 1, "message": "m"}});
        assert!(rpc_kind(&null_id_error).is_some());
        assert!(rpc_kind(&json!({"jsonrpc": "2.0", "id": null, "result": {}})).is_none());
    }

    #[test]
    fn key_order_and_big_numbers_survive_forwarding() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"z":1,"a":{"q":2,"b":3},"n":123456789012345678901234567890}}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(v.to_string(), raw);
    }

    #[test]
    fn session_lifecycle_allows_reinitialize() {
        let accepted = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let not_found = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let with_session = |url: String| {
            let (p, out) = proxy_at(url);
            *p.session.lock().unwrap() = Some("old".into());
            *p.protocol.lock().unwrap() = Some("2025-06-18".into());
            (p, out)
        };
        // Initialize starts a new session: no old session or protocol headers.
        let (url, seen) = mock_seen(accepted, Duration::ZERO);
        let (p, _) = with_session(url);
        p.post(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}));
        let req = seen.recv().unwrap();
        assert!(!req.contains("mcp-session-id") && !req.contains("mcp-protocol-version"));
        // Other messages carry both.
        let (url, seen) = mock_seen(accepted, Duration::ZERO);
        let (p, _) = with_session(url);
        p.post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        let req = seen.recv().unwrap();
        assert!(
            req.contains("mcp-session-id: old") && req.contains("mcp-protocol-version: 2025-06-18")
        );
        // 404 on the session forgets it, so the next initialize is clean, even when the
        // body is a JSON-RPC error that is passed through.
        let not_found_rpc = "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: 81\r\n\
                             connection: close\r\n\r\n\
                             {\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32001,\"message\":\"Session not found\"}}";
        let (p, out) = with_session(mock(not_found_rpc));
        p.post(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
        assert_eq!(*p.session.lock().unwrap(), None);
        assert_eq!(out.lines()[0]["error"]["code"], -32001);
        assert_eq!(out.lines()[0]["id"], 3);
        let (p, out) = with_session(mock(not_found));
        p.post(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
        assert_eq!(*p.session.lock().unwrap(), None);
        assert_eq!(out.lines()[0]["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn python_truthiness() {
        for (v, t) in [
            (json!(true), true),
            (json!(1), true),
            (json!("x"), true),
            (json!(false), false),
            (json!(0), false),
            (json!(""), false),
            (json!(null), false),
            (json!([]), false),
        ] {
            assert_eq!(truthy(&v), t, "{v}");
        }
    }
}
