//! Which Claude account a run uses.
//!
//! Each account has a five-hour window and a weekly one. A five-hour window
//! that is open is use-it-or-lose-it, so the factory spends the one that
//! resets soonest first. An account at a limit is left alone until that
//! window resets, and when every account is at a limit the answer is to wait
//! for the earliest reset.
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
}

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

    /// Take in what a run's rate-limit event said. `model` is the model the
    /// run was on, which is what a limit of an unknown kind is taken to cover.
    pub fn observe(&mut self, limit: &RateLimit, model: &str, now: u64) {
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
    // An open five-hour window is lost at its reset: spend the one that
    // resets soonest. An account with no window open keeps until it is needed.
    let best = accounts
        .iter()
        .enumerate()
        .filter(|(_, id)| state(id).available_at(model, now) <= now)
        .min_by_key(|(order, id)| match state(id).session_resets(now) {
            Some(resets_at) => (0, resets_at, *order),
            None => (1, 0, *order),
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

/// Ask the subscription's usage endpoint about one account, with the full
/// login kept in the Keychain item `service`. No model is called and no
/// tokens are used. The endpoint is undocumented: every failure is an error
/// the caller can ignore, since runs report the same windows anyway.
pub fn read_usage(
    account: &str,
    service: &str,
    api_url: &str,
    now: u64,
) -> Result<AccountState, AccountError> {
    let fail = |reason: String| AccountError::Usage {
        account: account.to_string(),
        reason,
    };
    let credential: Value =
        serde_json::from_str(&keychain(service).map_err(fail)?).map_err(|_| {
            fail(format!(
                "the Keychain item `{service}` is not a Claude login"
            ))
        })?;
    let login = credential.get("claudeAiOauth").unwrap_or(&Value::Null);
    let token = login
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            fail(format!(
                "the Keychain item `{service}` holds no access token"
            ))
        })?;
    let expires = login.get("expiresAt").and_then(Value::as_u64).unwrap_or(0) / 1000;
    if expires != 0 && expires <= now {
        return Err(fail(
            "its stored login has expired and needs refreshing".to_string(),
        ));
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .get(&format!("{api_url}/api/oauth/usage"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|err| fail(format!("the request failed: {err}")))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(fail(format!("the endpoint answered HTTP {status}")));
    }
    let answer: Value = response
        .body_mut()
        .read_json()
        .map_err(|err| fail(format!("the answer was not JSON: {err}")))?;
    state_from_usage(&answer, now).ok_or_else(|| fail("the answer has no `limits`".to_string()))
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
