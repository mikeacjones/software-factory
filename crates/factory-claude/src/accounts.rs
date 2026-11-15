//! Which Claude account a run uses.
//!
//! Each account has a five-hour window and a weekly one, and capacity left
//! in a window when it resets is lost. So the factory spends first the
//! account whose week resets soonest, and between accounts whose weeks end
//! together, the one whose five-hour session resets soonest. An account at a
//! limit is left alone until that window resets, and when every account is
//! at a limit the answer is to wait for the earliest reset.
//!
//! The state is not polled. Every run reports both windows in its rate-limit
//! event, and that is what keeps this current. An account the factory has
//! never run under can be seeded from the subscription's usage endpoint,
//! which costs no tokens.
//!
//! Tokens are read where they are needed and never logged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_supervise::{Spec, capture};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{RateLimit, Window};

/// What the factory knows about one account's limits.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Limits that were hit: what they cover (`*` for everything, or a model
    /// family such as `fable`) and when each ends.
    #[serde(default)]
    pub blocked: BTreeMap<String, u64>,
    /// When this was last learned, and from what.
    pub updated_at: u64,
    /// When the usage endpoint was last asked, whether or not it answered.
    #[serde(default)]
    pub usage_checked_at: u64,
    #[serde(default)]
    pub source: String,
    /// Whose login the account's usage login is: the subscription's own ID
    /// for the account, as its profile gives it. It is set the first time
    /// the login is used, and a login that later turns out to be someone
    /// else's is refused.
    #[serde(default)]
    pub identity: Option<String>,
    /// The expiry of the stored login that `identity` was last confirmed
    /// for, in milliseconds. A login with another expiry is a new one, and
    /// is asked whose it is before it is used.
    #[serde(default)]
    pub identity_login: u64,
    /// Something about this account the owner should look at, such as its
    /// weekly window having moved, which a subscription's does not.
    #[serde(default)]
    pub concern: Option<String>,
}

/// How far apart two reports of the same window's reset may be.
const SAME_WINDOW_SECS: u64 = 5 * 60;

/// The middle of a model's name: `fable` for `claude-fable-5-2`.
pub fn family(model: &str) -> String {
    model.split('-').nth(1).unwrap_or(model).to_lowercase()
}

fn full(window: &Window) -> bool {
    window.utilization >= 1.0 || window.status == "rejected"
}

impl AccountState {
    /// When the account can next be used for `model`: zero if it can now.
    pub fn available_at(&self, model: &str, now: u64) -> u64 {
        let model = model.to_lowercase();
        let mut at = 0;
        for (scope, until) in &self.blocked {
            if *until > now && (scope == "*" || model.contains(scope.as_str())) {
                at = at.max(*until);
            }
        }
        for window in [&self.five_hour, &self.seven_day].into_iter().flatten() {
            if window.resets_at > now && full(window) {
                at = at.max(window.resets_at);
            }
        }
        at
    }

    /// When the account's open five-hour window resets, if one is open.
    pub fn session_resets(&self, now: u64) -> Option<u64> {
        self.five_hour
            .as_ref()
            .map(|window| window.resets_at)
            .filter(|resets_at| *resets_at > now)
    }

    /// Why a new report of the weekly window cannot be about the same
    /// subscription as the one known: a week that is still open does not
    /// change when it ends. It means the login or token behind this account
    /// now belongs to another account.
    pub fn week_moved(&self, week: Option<&Window>, now: u64) -> Option<String> {
        let (known, new) = (self.seven_day.as_ref()?, week?);
        let open = known.resets_at > now && new.resets_at > now;
        (open && known.resets_at.abs_diff(new.resets_at) > SAME_WINDOW_SECS).then(|| {
            format!(
                "its weekly window was known to reset in {}h and is now reported to reset in {}h: the login or token behind this account may have changed to another subscription",
                (known.resets_at - now) / 3600,
                (new.resets_at - now) / 3600
            )
        })
    }

    /// Take in what a run's rate-limit event said. `model` is the model the
    /// run was on, which is what a limit of an unknown kind is taken to cover.
    pub fn observe(&mut self, limit: &RateLimit, model: &str, now: u64) {
        if let Some(moved) = self.week_moved(limit.windows.get("seven_day"), now) {
            self.concern = Some(moved);
        }
        for (name, slot) in [
            ("five_hour", &mut self.five_hour),
            ("seven_day", &mut self.seven_day),
        ] {
            if let Some(window) = limit.windows.get(name) {
                *slot = Some(window.clone());
            }
        }
        self.blocked.retain(|_, until| *until > now);
        if limit.status == "rejected" {
            let until = limit.resets_at.unwrap_or(now + 15 * 60);
            let scope = match limit.limit_type.as_deref() {
                None | Some("five_hour" | "seven_day") => "*".to_string(),
                // A limit on one model: the account still serves the others.
                Some(_) => family(model),
            };
            self.blocked.insert(scope, until);
        }
        self.updated_at = now;
        self.source = "run".to_string();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Use(String),
    /// Every account is at a limit until at least this time.
    Wait {
        until: u64,
    },
}

/// Choose the account for a run on `model`. `accounts` is in the order the
/// factory's config lists them, which breaks ties.
pub fn choose(
    accounts: &[String],
    states: &BTreeMap<String, AccountState>,
    model: &str,
    now: u64,
) -> Choice {
    let unknown = AccountState::default();
    let state = |id: &String| states.get(id).unwrap_or(&unknown);
    // Capacity that is not used before its window resets is lost, so the
    // account whose capacity expires first is spent first. The week is what
    // is scarce, so its reset ranks before the five-hour session's. An account
    // whose window is not open, or not known, has nothing about to expire.
    let deadline = |resets_at: Option<u64>| resets_at.filter(|at| *at > now).unwrap_or(u64::MAX);
    let best = accounts
        .iter()
        .enumerate()
        .filter(|(_, id)| state(id).available_at(model, now) <= now)
        .min_by_key(|(order, id)| {
            let state = state(id);
            (
                deadline(state.seven_day.as_ref().map(|week| week.resets_at)),
                deadline(state.session_resets(now)),
                *order,
            )
        })
        .map(|(_, id)| id.clone());
    match best {
        Some(id) => Choice::Use(id),
        None => Choice::Wait {
            until: accounts
                .iter()
                .map(|id| state(id).available_at(model, now))
                .min()
                .unwrap_or(now),
        },
    }
}

/// Pairs of accounts that look like one subscription listed twice: their
/// usage logins belong to the same account, or their open weekly windows end
/// at the same moment. Balancing between such a pair spends one
/// subscription's capacity while counting it as two.
pub fn same_subscription(
    accounts: &[String],
    states: &BTreeMap<String, AccountState>,
    now: u64,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (index, first) in accounts.iter().enumerate() {
        for second in &accounts[index + 1..] {
            let (Some(a), Some(b)) = (states.get(first), states.get(second)) else {
                continue;
            };
            let same_login = a.identity.is_some() && a.identity == b.identity;
            let week = |state: &AccountState| {
                state
                    .seven_day
                    .as_ref()
                    .map(|week| week.resets_at)
                    .filter(|resets_at| *resets_at > now)
            };
            let same_week = match (week(a), week(b)) {
                (Some(a), Some(b)) => a.abs_diff(b) <= 5,
                _ => false,
            };
            if same_login || same_week {
                pairs.push((first.clone(), second.clone()));
            }
        }
    }
    pairs
}

/// The accounts' states, kept in one file so that every pipeline and every
/// director of the factory sees the same picture.
#[derive(Clone, Debug)]
pub struct Tracker {
    path: PathBuf,
}

impl Tracker {
    pub fn new(state_dir: &Path) -> Tracker {
        Tracker {
            path: state_dir.join("accounts.json"),
        }
    }

    pub fn load(&self) -> BTreeMap<String, AccountState> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Change one account's state. The file is written whole and moved into
    /// place, so a reader never sees half of it.
    pub fn update(
        &self,
        account: &str,
        change: impl FnOnce(&mut AccountState),
    ) -> std::io::Result<()> {
        let mut states = self.load();
        change(states.entry(account.to_string()).or_default());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self
            .path
            .with_extension(format!("json.{}", std::process::id()));
        let text = serde_json::to_string_pretty(&states).unwrap_or_default();
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &self.path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("account `{account}`: {reason}")]
    Token { account: String, reason: String },
    #[error("account `{account}`: its usage could not be read: {reason}")]
    Usage { account: String, reason: String },
    /// The stored login is not this account's.
    #[error("account `{account}`: {reason}")]
    Identity { account: String, reason: String },
    /// The login is this account's, but the usage endpoint did not answer
    /// with its windows. Whose login it is was learned all the same.
    #[error("account `{account}`: its usage could not be read: {reason}")]
    Unanswered {
        account: String,
        reason: String,
        identity: Option<String>,
        identity_login: u64,
    },
}

fn keychain(service: &str) -> Result<String, String> {
    let spec = Spec::new(
        ["security", "find-generic-password", "-s", service, "-w"],
        std::env::temp_dir(),
        Duration::from_secs(30),
    );
    match capture(&spec) {
        Ok(out) if out.success() && !out.stdout.trim().is_empty() => {
            Ok(out.stdout.trim().to_string())
        }
        Ok(_) => Err(format!("the Keychain has no readable item `{service}`")),
        Err(err) => Err(err.to_string()),
    }
}

/// Where an account's token for running sessions comes from.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokenSource<'a> {
    pub file: Option<&'a Path>,
    pub env: Option<&'a str>,
    pub keychain: Option<&'a str>,
}

/// The token a run under this account is given, or `None` for the CLI's own
/// login. The value goes to the worker's environment and nowhere else.
pub fn session_token(
    account: &str,
    source: TokenSource<'_>,
) -> Result<Option<String>, AccountError> {
    let fail = |reason: String| AccountError::Token {
        account: account.to_string(),
        reason,
    };
    if let Some(path) = source.file {
        let text = std::fs::read_to_string(path)
            .map_err(|err| fail(format!("{}: {err}", path.display())))?;
        return Ok(Some(text.trim().to_string()));
    }
    if let Some(name) = source.env
        && let Ok(value) = std::env::var(name)
        && !value.trim().is_empty()
    {
        return Ok(Some(value.trim().to_string()));
    }
    if let Some(service) = source.keychain {
        return keychain(service).map(Some).map_err(fail);
    }
    if let Some(name) = source.env {
        return Err(fail(format!("the environment has no {name}")));
    }
    Ok(None)
}

/// Seconds since the epoch for a time written like
/// `2026-11-14T23:30:00+00:00` or `2026-11-14T23:30:00.123Z`.
pub fn parse_time(text: &str) -> Option<u64> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut ymd = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let (clock, offset) = match rest.find(['Z', '+', '-']) {
        Some(at) => rest.split_at(at),
        None => (rest, "Z"),
    };
    let mut hms = clock
        .split(':')
        .map(|part| part.split('.').next().and_then(|n| n.parse::<i64>().ok()));
    let (hour, minute) = (hms.next()??, hms.next()??);
    let second = hms.next().flatten().unwrap_or(0);
    let offset_secs = if offset == "Z" {
        0
    } else {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (h, m) = offset[1..].split_once(':')?;
        sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
    };
    // Days from the civil date, by the usual era arithmetic.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs).ok()
}

/// Read the usage endpoint's answer into an account's state.
pub fn state_from_usage(answer: &Value, now: u64) -> Option<AccountState> {
    let limits = answer.get("limits")?.as_array()?;
    let mut state = AccountState {
        updated_at: now,
        source: "usage endpoint".to_string(),
        ..AccountState::default()
    };
    for limit in limits {
        let percent = limit.get("percent").and_then(Value::as_f64).unwrap_or(0.0);
        let resets_at = limit
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_time)
            .unwrap_or(0);
        let window = Window {
            utilization: percent / 100.0,
            resets_at,
            status: if percent >= 100.0 {
                "rejected"
            } else {
                "allowed"
            }
            .to_string(),
        };
        match limit.get("kind").and_then(Value::as_str) {
            Some("session") => state.five_hour = Some(window),
            Some("weekly_all") => state.seven_day = Some(window),
            // A weekly limit on one model, such as Fable.
            Some("weekly_scoped") if percent >= 100.0 && resets_at > now => {
                if let Some(name) = limit
                    .pointer("/scope/model/display_name")
                    .and_then(Value::as_str)
                {
                    state.blocked.insert(name.to_lowercase(), resets_at);
                }
            }
            _ => {}
        }
    }
    Some(state)
}

/// Where the CLI refreshes a login, and the client it does so as. Taken from
/// the pinned CLI's own code (2.1.325), which is the only authority on them.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const USAGE_API: &str = "https://api.anthropic.com";

/// A login about to expire within this long is refreshed first.
const REFRESH_MARGIN_SECS: u64 = 5 * 60;

/// How an account's stored login may be kept fresh.
#[derive(Clone, Copy, Debug)]
pub struct Refresh<'a> {
    pub token_url: &'a str,
    /// Where a refreshed login is kept, readable by the owner only, between
    /// receiving it and confirming it is stored.
    pub safe_dir: &'a Path,
    /// For a login Claude Code itself also refreshes: its configuration
    /// directory, whose refresh lock is held while the factory refreshes.
    /// `None` for a login nothing else touches.
    pub lock_dir: Option<&'a Path>,
    /// How often to try for that lock, a second and a half apart, before
    /// putting the refresh off: Claude Code holds it for a moment only.
    pub lock_tries: u32,
}

/// How often the factory tries for Claude Code's refresh lock.
pub const LOCK_TRIES: u32 = 5;

/// Where a full login is kept, and how it is read and replaced. In the
/// factory that is a Keychain item.
pub struct Vault {
    name: String,
    read: Box<Reading>,
    write: Box<Writing>,
}

type Reading = dyn Fn() -> Result<String, String>;
type Writing = dyn Fn(&str) -> Result<(), String>;

impl Vault {
    pub fn new(
        name: impl Into<String>,
        read: impl Fn() -> Result<String, String> + 'static,
        write: impl Fn(&str) -> Result<(), String> + 'static,
    ) -> Vault {
        Vault {
            name: name.into(),
            read: Box::new(read),
            write: Box::new(write),
        }
    }

    pub fn keychain(service: &str) -> Vault {
        let (reading, writing) = (service.to_string(), service.to_string());
        Vault::new(
            format!("the Keychain item `{service}`"),
            move || keychain(&reading),
            move |secret| keychain_write(&writing, secret),
        )
    }
}

/// A stored login, read.
struct Held {
    whole: Value,
    access: String,
    refresh: Option<String>,
    scopes: Vec<String>,
    /// When the access token expires, in milliseconds, as it is stored.
    expires_ms: u64,
}

fn held(vault: &Vault) -> Result<Held, String> {
    let whole: Value = serde_json::from_str(&(vault.read)()?)
        .map_err(|_| format!("{} is not a Claude login", vault.name))?;
    let login = whole.get("claudeAiOauth").unwrap_or(&Value::Null);
    let text = |key: &str| login.get(key).and_then(Value::as_str).map(str::to_string);
    let access =
        text("accessToken").ok_or_else(|| format!("{} holds no access token", vault.name))?;
    Ok(Held {
        access,
        refresh: text("refreshToken"),
        scopes: login
            .get("scopes")
            .and_then(Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        expires_ms: login.get("expiresAt").and_then(Value::as_u64).unwrap_or(0),
        whole,
    })
}

/// Claude Code's lock on refreshing the login of one configuration
/// directory, held the way Claude Code holds it (2.1.325): a directory
/// `.oauth_refresh.lock` inside the configuration directory and, as older
/// versions did, a directory named after the configuration directory with
/// `.lock` added. A lock not touched for a minute is taken to be left by a
/// process that died. Claude Code that finds the lock held waits, and then
/// reads the stored login again before it refreshes anything itself, which
/// is what lets the factory refresh the same login without either side
/// spending a refresh token the other still holds.
#[derive(Debug)]
pub struct RefreshLock {
    held: Vec<PathBuf>,
}

const LOCK_STALE: Duration = Duration::from_secs(60);

impl RefreshLock {
    fn paths(config_dir: &Path) -> [PathBuf; 2] {
        let real = std::fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
        let mut legacy = real.into_os_string();
        legacy.push(".lock");
        [
            config_dir.join(".oauth_refresh.lock"),
            PathBuf::from(legacy),
        ]
    }

    /// Make one lock directory. `Ok(false)` if someone holds it.
    fn take(path: &Path) -> std::io::Result<bool> {
        for _ in 0..2 {
            match std::fs::create_dir(path) {
                Ok(()) => return Ok(true),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok());
                    // Left behind by a process that died: clear it and try again.
                    if age.is_some_and(|age| age > LOCK_STALE) && std::fs::remove_dir(path).is_ok()
                    {
                        continue;
                    }
                    return Ok(false);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(false)
    }

    /// Take the lock, trying `attempts` times with `pause` between them.
    pub fn acquire(
        config_dir: &Path,
        attempts: u32,
        pause: Duration,
    ) -> Result<RefreshLock, String> {
        let [current, legacy] = RefreshLock::paths(config_dir);
        for attempt in 1..=attempts.max(1) {
            let mut lock = RefreshLock { held: Vec::new() };
            match RefreshLock::take(&current) {
                Ok(true) => lock.held.push(current.clone()),
                Ok(false) => {}
                Err(err) => {
                    return Err(format!("the refresh lock {}: {err}", current.display()));
                }
            }
            if !lock.held.is_empty() {
                match RefreshLock::take(&legacy) {
                    Ok(true) => {
                        lock.held.push(legacy.clone());
                        return Ok(lock);
                    }
                    // The older lock is held: by an older Claude Code.
                    Ok(false) => {}
                    // It cannot be made at all: the current lock is enough,
                    // as it is for Claude Code.
                    Err(_) => return Ok(lock),
                }
            }
            drop(lock);
            if attempt < attempts {
                std::thread::sleep(pause);
            }
        }
        Err(format!(
            "Claude Code is refreshing this login right now (its lock in {} is held)",
            config_dir.display()
        ))
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        for path in self.held.iter().rev() {
            let _ = std::fs::remove_dir(path);
        }
    }
}

/// When a stored login is refreshed.
#[derive(Clone, Copy, Debug)]
pub enum Renew<'a> {
    /// Whatever its expiry says.
    Always,
    /// Only if it is about to expire, or is the login whose access token
    /// (`refused`) the service has just turned down.
    IfDue { refused: Option<&'a str> },
}

/// How a refresh ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refreshed {
    /// The stored login was replaced.
    Done,
    /// It did not need it: it is not about to expire, usually because
    /// Claude Code refreshed it first. Minutes it still has.
    NotNeeded { minutes_left: u64 },
}

/// The stored login with a refresh's answer applied: new tokens and expiry,
/// everything else as it was. `None` if the answer carries no access token.
pub fn refreshed_login(stored: &Value, answer: &Value, now_ms: u64) -> Option<Value> {
    let mut whole = stored.clone();
    let login = whole.get_mut("claudeAiOauth")?.as_object_mut()?;
    let access = answer.get("access_token")?.as_str()?;
    login.insert("accessToken".to_string(), Value::from(access));
    // A refresh token that is not replaced stays valid.
    if let Some(refresh) = answer.get("refresh_token").and_then(Value::as_str) {
        login.insert("refreshToken".to_string(), Value::from(refresh));
    }
    let lifetime = answer.get("expires_in").and_then(Value::as_u64)?;
    login.insert(
        "expiresAt".to_string(),
        Value::from(now_ms + lifetime * 1000),
    );
    if let Some(lifetime) = answer
        .get("refresh_token_expires_in")
        .and_then(Value::as_u64)
    {
        login.insert(
            "refreshTokenExpiresAt".to_string(),
            Value::from(now_ms + lifetime * 1000),
        );
    }
    Some(whole)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The account name a Keychain item is filed under, from `security`'s listing.
fn account_of(listing: &str) -> Option<String> {
    let line = listing
        .lines()
        .find(|line| line.contains("\"acct\"<blob>="))?;
    let value = line.split_once("<blob>=")?.1.trim();
    Some(value.trim_matches('"').to_string())
}

/// Replace a Keychain item's secret. The secret is handed to `security` on
/// its standard input, hex-encoded, as the CLI does: it is never an argument
/// of a process, where other programs could read it.
fn keychain_write(service: &str, secret: &str) -> Result<(), String> {
    let listing = Spec::new(
        ["security", "find-generic-password", "-s", service],
        std::env::temp_dir(),
        Duration::from_secs(30),
    );
    let listed = capture(&listing).map_err(|err| err.to_string())?;
    let account = account_of(&listed.combined())
        .ok_or_else(|| format!("the Keychain has no item `{service}` to update"))?;
    if [service, account.as_str()]
        .iter()
        .any(|text| text.contains(['"', '\\', '\n']))
    {
        return Err("the Keychain item's name cannot be passed safely".to_string());
    }
    let line = format!(
        "add-generic-password -U -a \"{account}\" -s \"{service}\" -X \"{}\"\n",
        hex(secret.as_bytes())
    );
    if line.len() > 4000 {
        return Err("the login is too long to hand to `security` on its input".to_string());
    }
    let mut spec = Spec::new(
        ["security", "-i"],
        std::env::temp_dir(),
        Duration::from_secs(30),
    );
    spec.stdin = Some(line);
    let out = capture(&spec).map_err(|err| err.to_string())?;
    if !out.success() {
        return Err("`security` refused the write".to_string());
    }
    // `security -i` reports little: the only proof is reading it back.
    match keychain(service) {
        Ok(stored) if stored == secret => Ok(()),
        Ok(_) => Err("the Keychain item did not change".to_string()),
        Err(reason) => Err(reason),
    }
}

/// Refresh a stored login and write it back where it was.
///
/// With a lock directory this is done as Claude Code does it, so that the
/// two can share a login: take the lock, read the stored login again, and
/// leave it alone if it has been refreshed meanwhile.
///
/// A refresh token can be replaced by the refresh, after which the old one
/// is dead. So the new login is first written to a file only the owner can
/// read, then to where it is kept, and the file is removed once that is
/// confirmed. If the write fails the file stays, and the error says where
/// it is.
pub fn refresh_login(
    account: &str,
    vault: &Vault,
    refresh: Refresh<'_>,
    renew: Renew<'_>,
    now: u64,
) -> Result<Refreshed, AccountError> {
    let fail = |reason: String| AccountError::Usage {
        account: account.to_string(),
        reason,
    };
    let _lock = refresh
        .lock_dir
        .map(|dir| RefreshLock::acquire(dir, refresh.lock_tries, Duration::from_millis(1500)))
        .transpose()
        .map_err(|busy| fail(format!("{busy}; it is read again at the next check")))?;
    // Read under the lock: this is what another refresher left, if any.
    let stored = held(vault).map_err(fail)?;
    if let Renew::IfDue { refused } = renew {
        let expires = stored.expires_ms / 1000;
        if expires > now + REFRESH_MARGIN_SECS && refused != Some(stored.access.as_str()) {
            return Ok(Refreshed::NotNeeded {
                minutes_left: (expires - now) / 60,
            });
        }
    }
    let refresh_token = stored
        .refresh
        .as_deref()
        .ok_or_else(|| fail("its stored login has no refresh token: log in again".to_string()))?;
    let mut body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    });
    if !stored.scopes.is_empty() {
        body["scope"] = Value::from(stored.scopes.join(" "));
    }
    // Well inside the minute after which the lock counts as abandoned.
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(25)))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .post(refresh.token_url)
        .header("Content-Type", "application/json")
        .send_json(&body)
        .map_err(|err| fail(format!("the refresh request failed: {err}")))?;
    let status = response.status().as_u16();
    if status != 200 {
        // The answer may name the reason; it never carries a token.
        let reason = response
            .body_mut()
            .read_json::<Value>()
            .ok()
            .and_then(|answer| {
                answer
                    .pointer("/error/message")
                    .or_else(|| answer.get("error_description"))
                    .or_else(|| answer.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        return Err(fail(format!(
            "the refresh was refused (HTTP {status} {reason}): log in again under this account"
        )));
    }
    let answer: Value = response
        .body_mut()
        .read_json()
        .map_err(|err| fail(format!("the refresh answer was not JSON: {err}")))?;
    let fresh = refreshed_login(&stored.whole, &answer, now * 1000)
        .ok_or_else(|| fail("the refresh answer has no access token".to_string()))?;
    let text = fresh.to_string();

    let safe = refresh
        .safe_dir
        .join(format!("login-refresh-{account}.json"));
    let write_safe = || -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(refresh.safe_dir)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&safe)?;
        file.write_all(text.as_bytes())
    };
    let kept = write_safe().is_ok();
    match (vault.write)(&text) {
        Ok(()) => {
            let _ = std::fs::remove_file(&safe);
            Ok(Refreshed::Done)
        }
        Err(reason) if kept => Err(fail(format!(
            "its login was refreshed but could not be written to {} ({reason}). The new login is in {}, readable only by you: the old one may no longer work",
            vault.name,
            safe.display()
        ))),
        Err(reason) => Err(fail(format!(
            "its login was refreshed but could not be written to {} ({reason}), and could not be kept anywhere: log in again under this account",
            vault.name
        ))),
    }
}

fn agent(seconds: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(seconds)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Whose login an access token is: the subscription's ID for the account.
fn whose(api_url: &str, token: &str) -> Result<String, String> {
    let mut response = agent(20)
        .get(&format!("{api_url}/api/oauth/profile"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|err| format!("the request failed: {err}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("the profile endpoint answered HTTP {status}"));
    }
    let answer: Value = response
        .body_mut()
        .read_json()
        .map_err(|err| format!("the profile was not JSON: {err}"))?;
    answer
        .pointer("/account/uuid")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the profile names no account".to_string())
}

/// What is already known about whose login an account's stored login is.
#[derive(Clone, Copy, Debug, Default)]
pub struct Known<'a> {
    /// The account it was first seen to belong to.
    pub identity: Option<&'a str>,
    /// The expiry, in milliseconds, of the stored login that was last
    /// confirmed to be that account's.
    pub login: u64,
}

/// Ask the subscription's usage endpoint about one account, with the full
/// login kept in `vault`. No model is called and no tokens are used.
///
/// With `refresh`, a login that is about to expire, or that the endpoint
/// refuses, is refreshed once first. A stored login the factory has not
/// seen before is first asked whose it is: the first answer is kept in the
/// state that is returned, and a login that later belongs to someone else
/// is an [`AccountError::Identity`] and is not used, since its windows would
/// be another subscription's.
///
/// The endpoints are undocumented: every other failure is an error the
/// caller can ignore, since runs report the same windows anyway.
pub fn read_usage(
    account: &str,
    vault: &Vault,
    api_url: &str,
    refresh: Option<Refresh<'_>>,
    known: Known<'_>,
    now: u64,
) -> Result<AccountState, AccountError> {
    let fail = |reason: String| AccountError::Usage {
        account: account.to_string(),
        reason,
    };
    let mut login = held(vault).map_err(fail)?;
    let mut refreshed = false;
    let expires = login.expires_ms / 1000;
    if expires != 0 && expires <= now + REFRESH_MARGIN_SECS {
        match refresh {
            Some(refresh) => {
                refresh_login(account, vault, refresh, Renew::IfDue { refused: None }, now)?;
                login = held(vault).map_err(fail)?;
                refreshed = true;
            }
            None if expires <= now => {
                return Err(fail(
                    "its stored login has expired and needs refreshing".to_string(),
                ));
            }
            None => {}
        }
    }
    loop {
        // A login not seen before is asked whose it is before it is used.
        let mut identity = known.identity.map(str::to_string);
        let mut confirmed = known.login;
        if login.expires_ms != known.login || known.identity.is_none() {
            match whose(api_url, &login.access) {
                Ok(owner) => {
                    if let Some(expected) = known.identity
                        && expected != owner
                    {
                        return Err(AccountError::Identity {
                            account: account.to_string(),
                            reason: format!(
                                "the login in {} now belongs to a different account than the one this account has been so far, so its usage was not read. If that is meant, accept it with `factory accounts pin {account}`; if not, log in there as the right account",
                                vault.name
                            ),
                        });
                    }
                    identity = Some(owner);
                    confirmed = login.expires_ms;
                }
                // Not answered: the login is used as before, and asked
                // again next time.
                Err(_) if known.identity.is_some() => {}
                Err(reason) => {
                    return Err(fail(format!(
                        "whose login it is could not be read: {reason}"
                    )));
                }
            }
        }
        let mut response = agent(20)
            .get(&format!("{api_url}/api/oauth/usage"))
            .header("Authorization", &format!("Bearer {}", login.access))
            .header("anthropic-beta", "oauth-2025-04-20")
            .call()
            .map_err(|err| fail(format!("the request failed: {err}")))?;
        let status = response.status().as_u16();
        if status == 401
            && !refreshed
            && let Some(refresh) = refresh
        {
            let renew = Renew::IfDue {
                refused: Some(&login.access),
            };
            refresh_login(account, vault, refresh, renew, now)?;
            login = held(vault).map_err(fail)?;
            refreshed = true;
            continue;
        }
        if status != 200 {
            return Err(AccountError::Unanswered {
                account: account.to_string(),
                reason: format!("the endpoint answered HTTP {status}"),
                identity,
                identity_login: confirmed,
            });
        }
        let answer: Value = response
            .body_mut()
            .read_json()
            .map_err(|err| fail(format!("the answer was not JSON: {err}")))?;
        let mut state = state_from_usage(&answer, now)
            .ok_or_else(|| fail("the answer has no `limits`".to_string()))?;
        state.identity = identity;
        state.identity_login = confirmed;
        return Ok(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn window(utilization: f64, resets_in: i64) -> Option<Window> {
        Some(Window {
            utilization,
            resets_at: (NOW as i64 + resets_in) as u64,
            status: if utilization >= 1.0 {
                "rejected"
            } else {
                "allowed"
            }
            .to_string(),
        })
    }

    fn state(five: Option<Window>, seven: Option<Window>) -> AccountState {
        AccountState {
            five_hour: five,
            seven_day: seven,
            ..AccountState::default()
        }
    }

    fn pick(states: &[(&str, AccountState)], model: &str) -> Choice {
        let accounts: Vec<String> = states.iter().map(|(id, _)| id.to_string()).collect();
        let states: BTreeMap<String, AccountState> = states
            .iter()
            .map(|(id, s)| (id.to_string(), s.clone()))
            .collect();
        choose(&accounts, &states, model, NOW)
    }

    fn used(id: &str) -> Choice {
        Choice::Use(id.to_string())
    }

    #[test]
    fn the_session_that_resets_soonest_is_spent_first() {
        let states = [
            ("a", state(window(0.2, 4 * 3600), window(0.5, 86_400))),
            ("b", state(window(0.6, 3600), window(0.8, 86_400))),
        ];
        assert_eq!(pick(&states, "claude-opus-5-5"), used("b"));
    }

    #[test]
    fn the_week_that_ends_soonest_is_spent_before_the_session_that_does() {
        // The owner's own accounts on 2026-11-14: one has half its week left
        // and six hours to use it; the other's session ends sooner, but its
        // week runs for three more days.
        let states = [
            (
                "claude2",
                state(window(0.23, 2 * 3600), window(0.86, 82 * 3600)),
            ),
            (
                "claude",
                state(window(0.0, 4 * 3600), window(0.52, 6 * 3600)),
            ),
        ];
        assert_eq!(pick(&states, "claude-opus-5-5"), used("claude"));
        // Once that week has reset, its next reset is the further one.
        let after = [
            ("claude2", state(window(0.4, 3600), window(0.9, 76 * 3600))),
            (
                "claude",
                state(window(0.1, 4 * 3600), window(0.02, 168 * 3600)),
            ),
        ];
        assert_eq!(pick(&after, "claude-opus-5-5"), used("claude2"));
        // A week the factory knows nothing about has nothing known to lose.
        let unknown = [
            ("a", state(window(0.1, 600), None)),
            ("b", state(window(0.1, 9000), window(0.5, 86_400))),
        ];
        assert_eq!(pick(&unknown, "m"), used("b"));
    }

    #[test]
    fn an_account_with_no_session_open_is_kept_for_later() {
        // b's last window has ended; a's is open. Starting b's clock now
        // would waste what a still has.
        let states = [
            ("b", state(window(0.3, -60), None)),
            ("c", AccountState::default()),
            ("a", state(window(0.9, 2 * 3600), None)),
        ];
        assert_eq!(pick(&states, "m"), used("a"));
        // With nothing known about any of them, the config's order decides.
        let fresh = [
            ("a", AccountState::default()),
            ("b", AccountState::default()),
        ];
        assert_eq!(pick(&fresh, "m"), used("a"));
    }

    #[test]
    fn an_account_at_its_session_limit_is_skipped_until_the_reset() {
        let states = [
            ("a", state(window(1.0, 1800), window(0.5, 86_400))),
            ("b", state(None, window(0.4, 86_400))),
        ];
        assert_eq!(pick(&states, "m"), used("b"));
        // Once a's reset time has passed its old window no longer counts.
        let later = [("a", state(window(1.0, -1), None))];
        assert_eq!(pick(&later, "m"), used("a"));
    }

    #[test]
    fn a_capped_week_rules_an_account_out_even_with_session_left() {
        let states = [
            ("a", state(window(0.1, 600), window(1.0, 3 * 86_400))),
            ("b", state(window(0.7, 4 * 3600), window(0.9, 86_400))),
        ];
        assert_eq!(pick(&states, "m"), used("b"));
    }

    #[test]
    fn with_every_account_at_a_limit_the_answer_is_to_wait_for_the_earliest_reset() {
        let states = [
            ("a", state(window(1.0, 5000), window(0.5, 86_400))),
            ("b", state(window(0.2, 600), window(1.0, 2000))),
        ];
        assert_eq!(pick(&states, "m"), Choice::Wait { until: NOW + 2000 });
    }

    fn rejected(limit_type: Option<&str>, resets_in: u64) -> RateLimit {
        RateLimit {
            status: "rejected".to_string(),
            resets_at: Some(NOW + resets_in),
            limit_type: limit_type.map(str::to_string),
            windows: BTreeMap::new(),
        }
    }

    #[test]
    fn a_limit_on_one_model_leaves_the_account_usable_for_others() {
        let mut a = AccountState::default();
        a.observe(
            &rejected(Some("seven_day_fable"), 9000),
            "claude-fable-5-2",
            NOW,
        );
        assert_eq!(a.available_at("claude-fable-5-2", NOW), NOW + 9000);
        assert_eq!(a.available_at("claude-opus-5-5", NOW), 0);
        let states = [("a", a), ("b", AccountState::default())];
        assert_eq!(pick(&states, "claude-fable-5-2"), used("b"));
        assert_eq!(pick(&states, "claude-opus-5-5"), used("a"));
    }

    #[test]
    fn a_run_refused_at_the_session_limit_blocks_the_whole_account_until_the_reset() {
        let mut a = AccountState::default();
        a.observe(&rejected(Some("five_hour"), 700), "claude-opus-5-5", NOW);
        assert_eq!(a.available_at("claude-haiku-5-5", NOW), NOW + 700);
        assert_eq!(a.available_at("claude-haiku-5-5", NOW + 701), 0);
        // A later successful run clears a block that has passed.
        let ok = RateLimit {
            status: "allowed".to_string(),
            resets_at: None,
            limit_type: None,
            windows: BTreeMap::from([("five_hour".to_string(), window(0.1, 18_000).unwrap())]),
        };
        a.observe(&ok, "claude-opus-5-5", NOW + 800);
        assert!(a.blocked.is_empty());
        assert_eq!(a.session_resets(NOW + 800), Some(NOW + 18_000));
        assert_eq!(a.source, "run");
    }

    #[test]
    fn times_are_read_with_their_offsets() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2000-03-01T00:00:00Z"), Some(951_868_800));
        let utc = parse_time("2026-11-14T23:30:00+00:00").unwrap();
        assert_eq!(parse_time("2026-11-14T18:30:00-05:00"), Some(utc));
        assert_eq!(parse_time("2026-11-14T23:30:00.250Z"), Some(utc));
        assert_eq!(parse_time("2026-11-15T00:30:00Z"), Some(utc + 3600));
        assert_eq!(parse_time("yesterday"), None);
    }

    #[test]
    fn the_usage_endpoints_answer_becomes_a_state() {
        let answer = serde_json::json!({"limits": [
            {"kind": "session", "percent": 11, "resets_at": "2026-11-15T01:00:00+00:00", "scope": null},
            {"kind": "weekly_all", "percent": 84, "resets_at": "2026-11-18T07:00:00+00:00", "scope": null},
            {"kind": "weekly_scoped", "percent": 100, "resets_at": "2026-11-18T07:00:00+00:00",
             "scope": {"model": {"display_name": "Fable"}}}
        ]});
        let now = parse_time("2026-11-14T20:00:00Z").unwrap();
        let state = state_from_usage(&answer, now).unwrap();
        assert_eq!(state.five_hour.as_ref().unwrap().utilization, 0.11);
        assert_eq!(
            state.session_resets(now),
            parse_time("2026-11-15T01:00:00Z")
        );
        assert_eq!(state.seven_day.as_ref().unwrap().utilization, 0.84);
        // Fable's own weekly limit is full: Fable must go elsewhere, Opus need not.
        assert!(state.available_at("claude-fable-5-2", now) > now);
        assert_eq!(state.available_at("claude-opus-5-5", now), 0);
        assert_eq!(
            state_from_usage(&serde_json::json!({"error": "x"}), now),
            None
        );
    }

    #[test]
    fn a_refresh_replaces_the_tokens_and_keeps_the_rest_of_the_login() {
        let stored = serde_json::json!({"claudeAiOauth": {
            "accessToken": "old-access", "refreshToken": "old-refresh", "expiresAt": 1000,
            "refreshTokenExpiresAt": 2000, "scopes": ["user:profile"], "subscriptionType": "max"
        }});
        let rotated = serde_json::json!({
            "access_token": "new-access", "refresh_token": "new-refresh",
            "expires_in": 3600, "refresh_token_expires_in": 86_400
        });
        let fresh = refreshed_login(&stored, &rotated, 10_000).unwrap();
        let login = &fresh["claudeAiOauth"];
        assert_eq!(login["accessToken"], "new-access");
        assert_eq!(login["refreshToken"], "new-refresh");
        assert_eq!(login["expiresAt"], 3_610_000);
        assert_eq!(login["refreshTokenExpiresAt"], 86_410_000);
        assert_eq!(login["scopes"], serde_json::json!(["user:profile"]));
        assert_eq!(login["subscriptionType"], "max");

        // A refresh that does not replace the refresh token keeps the old one.
        let kept = serde_json::json!({"access_token": "new-access", "expires_in": 60});
        let fresh = refreshed_login(&stored, &kept, 0).unwrap();
        assert_eq!(fresh["claudeAiOauth"]["refreshToken"], "old-refresh");
        assert_eq!(fresh["claudeAiOauth"]["refreshTokenExpiresAt"], 2000);
        // An answer with no access token changes nothing.
        assert_eq!(
            refreshed_login(&stored, &serde_json::json!({"error": "invalid_grant"}), 0),
            None
        );
        assert_eq!(refreshed_login(&serde_json::json!({}), &rotated, 0), None);
    }

    /// A stand-in for the token, profile and usage endpoints: it answers
    /// one request with each answer, in order, and returns what was asked.
    fn stand_in(answers: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for (status, answer) in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&request).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                stream.read_exact(&mut body).unwrap();
                asked.push(format!("{head}{}", String::from_utf8_lossy(&body)));
                let text = answer.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
            asked
        });
        (url, served)
    }

    /// An address nothing listens on: a request to it fails at once.
    fn nowhere() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    /// A directory of this test's own, and a login kept in a file in it.
    fn login_in_a_file(name: &str, expires_ms: u64) -> (PathBuf, PathBuf, Vault) {
        let dir = std::env::temp_dir().join(format!("factory-login-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("claude")).unwrap();
        let file = dir.join("login.json");
        let login = serde_json::json!({"claudeAiOauth": {
            "accessToken": "old-access", "refreshToken": "old-refresh", "expiresAt": expires_ms,
            "scopes": ["user:inference", "user:profile"], "subscriptionType": "max"
        }});
        std::fs::write(&file, login.to_string()).unwrap();
        let (reading, writing) = (file.clone(), file.clone());
        let vault = Vault::new(
            "the test login",
            move || std::fs::read_to_string(&reading).map_err(|err| err.to_string()),
            move |secret| std::fs::write(&writing, secret).map_err(|err| err.to_string()),
        );
        (dir, file, vault)
    }

    fn usage_answer() -> Value {
        serde_json::json!({"limits": [
            {"kind": "session", "percent": 10, "resets_at": "2026-11-15T15:00:00+00:00", "scope": null},
            {"kind": "weekly_all", "percent": 5, "resets_at": "2026-11-22T03:00:00+00:00", "scope": null},
        ]})
    }

    fn profile_of(account: &str) -> Value {
        serde_json::json!({"account": {"uuid": account}, "organization": {"uuid": "org"}})
    }

    #[test]
    fn the_refresh_lock_is_the_one_claude_code_keeps() {
        let dir = std::env::temp_dir().join(format!("factory-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("claude");
        std::fs::create_dir_all(&config).unwrap();
        let current = config.join(".oauth_refresh.lock");
        let legacy = std::fs::canonicalize(&dir).unwrap().join("claude.lock");
        let brief = Duration::from_millis(10);

        // Held: both of Claude Code's lock directories exist, and nobody
        // else gets it, Claude Code included.
        let lock = RefreshLock::acquire(&config, 1, brief).unwrap();
        assert!(current.is_dir() && legacy.is_dir());
        let busy = RefreshLock::acquire(&config, 2, brief).unwrap_err();
        assert!(
            busy.starts_with("Claude Code is refreshing this login right now"),
            "{busy}"
        );
        assert!(
            current.is_dir(),
            "a failed try leaves the holder's lock alone"
        );
        drop(lock);
        assert!(!current.exists() && !legacy.exists());

        // Held by Claude Code, in either form: the factory waits its turn,
        // and leaves nothing of its own behind.
        for held in [&current, &legacy] {
            std::fs::create_dir(held).unwrap();
            assert!(RefreshLock::acquire(&config, 2, brief).is_err());
            assert!(held.is_dir());
            std::fs::remove_dir(held).unwrap();
            assert!(!current.exists() && !legacy.exists());
        }

        // Left behind by a process that died more than a minute ago: taken over.
        std::fs::create_dir(&current).unwrap();
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(120);
        std::fs::File::open(&current)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let lock = RefreshLock::acquire(&config, 1, brief).unwrap();
        assert!(current.is_dir() && legacy.is_dir());
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_expired_login_is_refreshed_as_claude_code_would_and_asked_whose_it_is() {
        let now = parse_time("2026-11-15T12:00:00Z").unwrap();
        let (dir, file, vault) = login_in_a_file("refresh", (now - 60) * 1000);
        let (url, served) = stand_in(vec![
            (
                200,
                serde_json::json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600}),
            ),
            (200, profile_of("account-one")),
            (200, usage_answer()),
        ]);
        let token_url = format!("{url}/v1/oauth/token");
        let config = dir.join("claude");
        let refresh = Refresh {
            token_url: &token_url,
            safe_dir: &dir,
            lock_dir: Some(&config),
            lock_tries: 1,
        };
        let state =
            read_usage("claude", &vault, &url, Some(refresh), Known::default(), now).unwrap();
        let asked = served.join().unwrap();

        // The refresh is the one Claude Code makes, with the stored token.
        assert!(
            asked[0].starts_with("POST /v1/oauth/token "),
            "{}",
            asked[0]
        );
        let body: Value =
            serde_json::from_str(asked[0].rsplit("\r\n\r\n").next().unwrap()).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-refresh");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["scope"], "user:inference user:profile");
        // The new login is where the old one was, with the rest kept.
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let login = &stored["claudeAiOauth"];
        assert_eq!(login["accessToken"], "new-access");
        assert_eq!(login["refreshToken"], "new-refresh");
        assert_eq!(login["expiresAt"], (now + 3600) * 1000);
        assert_eq!(login["subscriptionType"], "max");
        // The new login was asked whose it is, then asked for the usage.
        assert!(asked[1].starts_with("GET /api/oauth/profile "));
        assert!(asked[1].contains("Bearer new-access") && asked[2].contains("Bearer new-access"));
        assert!(asked[2].starts_with("GET /api/oauth/usage "));
        assert_eq!(state.identity.as_deref(), Some("account-one"));
        assert_eq!(state.identity_login, (now + 3600) * 1000);
        assert_eq!(state.seven_day.as_ref().unwrap().utilization, 0.05);
        // Nothing is left behind: no lock, no copy of the login.
        assert!(!config.join(".oauth_refresh.lock").exists());
        assert!(!dir.join("login-refresh-claude.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_login_someone_else_refreshed_or_holds_the_lock_on_is_left_alone() {
        let now = parse_time("2026-11-15T12:00:00Z").unwrap();
        let nowhere = nowhere();
        // Still good for an hour: Claude Code got there first. No request
        // is made at all.
        let (dir, file, vault) = login_in_a_file("fresh", (now + 3600) * 1000);
        let config = dir.join("claude");
        let refresh = Refresh {
            token_url: &nowhere,
            safe_dir: &dir,
            lock_dir: Some(&config),
            lock_tries: 1,
        };
        let before = std::fs::read_to_string(&file).unwrap();
        let due = Renew::IfDue { refused: None };
        assert_eq!(
            refresh_login("claude", &vault, refresh, due, now).unwrap(),
            Refreshed::NotNeeded { minutes_left: 60 }
        );
        // Turned down by the service a moment ago, but no longer the stored
        // login: also left alone.
        let other = Renew::IfDue {
            refused: Some("an-older-access"),
        };
        assert!(matches!(
            refresh_login("claude", &vault, refresh, other, now).unwrap(),
            Refreshed::NotNeeded { .. }
        ));
        // The stored login is the one turned down: that needs a refresh,
        // which here cannot be reached.
        let same = Renew::IfDue {
            refused: Some("old-access"),
        };
        let failed = refresh_login("claude", &vault, refresh, same, now).unwrap_err();
        assert!(
            failed.to_string().contains("the refresh request failed"),
            "{failed}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert!(
            !config.join(".oauth_refresh.lock").exists(),
            "the lock is given back"
        );

        // Claude Code holds the lock: the factory puts its refresh off and
        // touches nothing.
        std::fs::create_dir(config.join(".oauth_refresh.lock")).unwrap();
        let busy = refresh_login("claude", &vault, refresh, Renew::Always, now).unwrap_err();
        assert!(
            busy.to_string()
                .contains("Claude Code is refreshing this login right now"),
            "{busy}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_refresh_changes_nothing_and_says_to_log_in() {
        let now = parse_time("2026-11-15T12:00:00Z").unwrap();
        let (dir, file, vault) = login_in_a_file("refused", (now - 60) * 1000);
        let before = std::fs::read_to_string(&file).unwrap();
        let (url, served) = stand_in(vec![(
            400,
            serde_json::json!({"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}),
        )]);
        let refresh = Refresh {
            token_url: &url,
            safe_dir: &dir,
            lock_dir: None,
            lock_tries: 1,
        };
        let refused =
            read_usage("claude", &vault, &url, Some(refresh), Known::default(), now).unwrap_err();
        served.join().unwrap();
        let text = refused.to_string();
        assert!(
            text.contains("HTTP 400 Refresh token not found or invalid")
                && text.contains("log in again under this account"),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        // Without leave to refresh, an expired login is only reported.
        let expired = read_usage("claude", &vault, &url, None, Known::default(), now).unwrap_err();
        assert!(
            expired
                .to_string()
                .contains("has expired and needs refreshing")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_login_that_has_become_another_accounts_is_not_used() {
        let now = parse_time("2026-11-15T12:00:00Z").unwrap();
        let expires = (now + 3600) * 1000;
        let (dir, _, vault) = login_in_a_file("identity", expires);

        // The login the account was confirmed with: only the usage is asked.
        let (url, served) = stand_in(vec![(200, usage_answer())]);
        let confirmed = Known {
            identity: Some("account-one"),
            login: expires,
        };
        let state = read_usage("claude", &vault, &url, None, confirmed, now).unwrap();
        assert!(served.join().unwrap()[0].starts_with("GET /api/oauth/usage "));
        assert_eq!(state.identity.as_deref(), Some("account-one"));
        assert_eq!(state.identity_login, expires);

        // A new login in its place, for the same account: asked, accepted.
        let earlier = Known {
            identity: Some("account-one"),
            login: 1,
        };
        let (url, served) = stand_in(vec![
            (200, profile_of("account-one")),
            (200, usage_answer()),
        ]);
        let state = read_usage("claude", &vault, &url, None, earlier, now).unwrap();
        assert_eq!(served.join().unwrap().len(), 2);
        assert_eq!(state.identity_login, expires);

        // A new login for somebody else: refused before its usage is read,
        // since those windows would be another subscription's.
        let (url, served) = stand_in(vec![(200, profile_of("account-two"))]);
        let refused = read_usage("claude", &vault, &url, None, earlier, now).unwrap_err();
        assert_eq!(served.join().unwrap().len(), 1);
        assert!(matches!(refused, AccountError::Identity { .. }));
        assert!(
            refused
                .to_string()
                .contains("now belongs to a different account")
                && refused.to_string().contains("factory accounts pin claude"),
            "{refused}"
        );

        // Whose it is cannot be learned just now. An account already known
        // goes on with the login; one never seen does not start with a guess.
        let (url, served) = stand_in(vec![(429, serde_json::json!({})), (200, usage_answer())]);
        let state = read_usage("claude", &vault, &url, None, earlier, now).unwrap();
        served.join().unwrap();
        assert_eq!(state.identity_login, 1, "still to be confirmed");
        let (url, served) = stand_in(vec![(429, serde_json::json!({}))]);
        let unknown = read_usage("claude", &vault, &url, None, Known::default(), now).unwrap_err();
        served.join().unwrap();
        assert!(
            unknown
                .to_string()
                .contains("whose login it is could not be read")
        );
        // The usage endpoint turns the factory away, as it sometimes does
        // for a while: whose login it is was still learned, and is kept.
        let (url, served) = stand_in(vec![
            (200, profile_of("account-one")),
            (429, serde_json::json!({})),
        ]);
        let turned_away =
            read_usage("claude", &vault, &url, None, Known::default(), now).unwrap_err();
        served.join().unwrap();
        assert!(
            turned_away
                .to_string()
                .ends_with("the endpoint answered HTTP 429")
        );
        assert!(matches!(
            turned_away,
            AccountError::Unanswered { identity: Some(ref who), identity_login, .. }
                if who == "account-one" && identity_login == expires
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_week_that_moves_and_two_accounts_that_are_one_are_noticed() {
        let week = |resets_in: i64| RateLimit {
            status: "allowed".to_string(),
            resets_at: None,
            limit_type: None,
            windows: BTreeMap::from([("seven_day".to_string(), window(0.3, resets_in).unwrap())]),
        };
        // The same week, reported a few seconds apart: nothing to say.
        let mut account = AccountState::default();
        account.observe(&week(500_000), "m", NOW);
        account.observe(&week(500_030), "m", NOW + 10);
        assert_eq!(account.concern, None);
        // An open week does not change when it ends: this is another account.
        account.observe(&week(200_000), "m", NOW + 20);
        let concern = account.concern.clone().unwrap();
        assert!(
            concern.contains("may have changed to another subscription"),
            "{concern}"
        );
        // A week that had ended is simply followed by the next one.
        let mut next = AccountState::default();
        next.observe(&week(100), "m", NOW);
        next.observe(&week(604_800), "m", NOW + 200);
        assert_eq!(next.concern, None);

        // Two accounts whose weeks end together, or whose logins are one
        // account's, are one subscription listed twice.
        let ids = ["a".to_string(), "b".to_string(), "c".to_string()];
        let mut states = BTreeMap::new();
        states.insert("a".to_string(), state(None, window(0.1, 500_000)));
        states.insert("b".to_string(), state(None, window(0.1, 500_002)));
        states.insert("c".to_string(), state(None, window(0.9, 90_000)));
        assert_eq!(
            same_subscription(&ids, &states, NOW),
            [("a".to_string(), "b".to_string())]
        );
        states.get_mut("b").unwrap().seven_day = window(0.1, 300_000);
        assert!(same_subscription(&ids, &states, NOW).is_empty());
        for id in ["b", "c"] {
            states.get_mut(id).unwrap().identity = Some("account-two".to_string());
        }
        assert_eq!(
            same_subscription(&ids, &states, NOW),
            [("b".to_string(), "c".to_string())]
        );
    }

    #[test]
    fn a_keychain_write_is_addressed_and_encoded_like_the_clis() {
        assert_eq!(hex(b"{\"a\":1}"), "7b2261223a317d");
        let listing = "keychain: \"login.keychain-db\"\nattributes:\n    \"acct\"<blob>=\"owner\"\n    \"svce\"<blob>=\"Claude Code-credentials\"\n";
        assert_eq!(account_of(listing).as_deref(), Some("owner"));
        assert_eq!(account_of("no such item"), None);
    }

    #[test]
    fn the_tracker_keeps_states_between_processes() {
        let dir = std::env::temp_dir().join(format!("factory-accounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tracker = Tracker::new(&dir);
        assert!(tracker.load().is_empty());
        tracker
            .update("a", |state| state.observe(&rejected(None, 60), "m", NOW))
            .unwrap();
        tracker.update("b", |state| state.updated_at = 5).unwrap();
        let states = Tracker::new(&dir).load();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(states["a"].available_at("m", NOW), NOW + 60);
        assert_eq!(states["b"].updated_at, 5);
    }

    #[test]
    fn a_token_comes_from_a_file_and_the_cli_login_needs_none() {
        let path = std::env::temp_dir().join(format!("factory-token-{}", std::process::id()));
        std::fs::write(&path, "tok-b\n").unwrap();
        let source = TokenSource {
            file: Some(&path),
            ..TokenSource::default()
        };
        let token = session_token("b", source).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(token.as_deref(), Some("tok-b"));
        assert_eq!(session_token("a", TokenSource::default()).unwrap(), None);
        let source = TokenSource {
            env: Some("NO_SUCH_FACTORY_TOKEN_VAR"),
            ..TokenSource::default()
        };
        let missing = session_token("b", source).unwrap_err();
        assert!(missing.to_string().contains("NO_SUCH_FACTORY_TOKEN_VAR"));
    }
}
