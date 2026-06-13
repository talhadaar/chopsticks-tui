//! Saved sessions (spec §8) and time-travel specs (spec §7.2) — ticket P4.
//!
//! A `Session` is a *replayable script*: fork config + pins + baseline + an
//! ordered `RecordedAction` log, persisted as TOML. Restore re-forks and replays
//! the log. `TimeSpec` is the parsed argument to `dev_timeTravel`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::contracts::{BuildMode, DevAccount, ForkConfig, PinnedItem, PreparedTx, Result, TxSigner};

/// A parsed `dev_timeTravel` argument, resolved to absolute unix epoch ms.
///
/// Accepts (see plan §"TimeSpec formats"): unix ms, unix seconds, `YYYY-MM-DD`,
/// `YYYY-MM-DDThh:mm:ss` (UTC), and relative `+<n>{s,m,h,d}`. Always stored as the
/// resolved absolute ms so a saved session replays identically regardless of when
/// it is restored. `source` keeps the original text for display.
///
/// This replaces the P0 stub `contracts::TimeSpec` enum: `Command::TimeTravel`
/// carries this refined payload type (a freeze-allowed payload refinement).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSpec {
    pub epoch_ms: u64,
    pub source: String,
}

impl TimeSpec {
    /// Build directly from a resolved instant (used by tests and by replay).
    pub fn from_epoch_ms(epoch_ms: u64, source: &str) -> Self {
        Self {
            epoch_ms,
            source: source.to_string(),
        }
    }

    /// Parse user input into an absolute instant. `now_ms` is the base for
    /// relative offsets (the wall clock at dispatch time).
    pub fn parse(input: &str, now_ms: u64) -> Result<Self> {
        let s = input.trim();
        if s.is_empty() {
            bail!("empty time spec");
        }
        let epoch_ms = if let Some(rest) = s.strip_prefix('+') {
            now_ms + parse_relative_ms(rest)?
        } else if s.contains('-') {
            parse_iso(s)?
        } else {
            // Bare integer: ms if it looks like ms-precision, else seconds.
            let n: u64 = s
                .parse()
                .map_err(|_| anyhow!("`{s}` is not a date, timestamp, or +offset"))?;
            if n >= 1_000_000_000_000 { n } else { n * 1000 }
        };
        Ok(Self {
            epoch_ms,
            source: s.to_string(),
        })
    }
}

/// Parse a `+`-stripped relative offset like `1d`, `90m`, `30s`, `2h` → ms.
fn parse_relative_ms(rest: &str) -> Result<u64> {
    let (num, unit) = rest.split_at(
        rest.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| anyhow!("relative offset `{rest}` needs a unit (s/m/h/d)"))?,
    );
    let n: u64 = num.parse().map_err(|_| anyhow!("`{num}` is not a number"))?;
    let unit_ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        other => bail!("unsupported offset unit `{other}` (use s/m/h/d)"),
    };
    Ok(n * unit_ms)
}

/// Parse `YYYY-MM-DD` or `YYYY-MM-DDThh:mm:ss` as UTC → epoch ms. Hand-rolled
/// (no chrono dep): days-from-civil for the date, plus the time-of-day seconds.
fn parse_iso(s: &str) -> Result<u64> {
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dit = date_part.split('-');
    let year: i64 = dit
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| anyhow!("bad year in `{s}`"))?;
    let month: i64 = dit
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| anyhow!("bad month in `{s}`"))?;
    let day: i64 = dit
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| anyhow!("bad day in `{s}`"))?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        bail!("out-of-range month/day in `{s}`");
    }
    let days = days_from_civil(year, month, day);
    let mut secs = days * 86_400;
    if let Some(t) = time_part {
        let mut tit = t.split(':');
        let h: i64 = tit
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| anyhow!("bad hour in `{s}`"))?;
        let m: i64 = tit
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| anyhow!("bad minute in `{s}`"))?;
        let sec: i64 = tit.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        secs += h * 3_600 + m * 60 + sec;
    }
    if secs < 0 {
        bail!("`{s}` is before the unix epoch");
    }
    Ok(secs as u64 * 1000)
}

/// Days since 1970-01-01 for a civil (proleptic Gregorian) date.
/// Howard Hinnant's `days_from_civil` algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Action log (freeze §4)
// ---------------------------------------------------------------------------

/// Serializable description of a transaction's *signer* (mirror of `TxSigner`,
/// which is not serde-friendly): an impersonated account is stored as its SS58
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignerRecord {
    Dev(DevAccount),
    /// Impersonated account as an SS58 string.
    Impersonate(String),
}

impl SignerRecord {
    fn from_signer(signer: &TxSigner) -> Self {
        match signer {
            TxSigner::Dev(a) => SignerRecord::Dev(*a),
            TxSigner::Impersonate(id) => SignerRecord::Impersonate(id.to_string()),
        }
    }
}

/// Serializable, *replayable* description of a staged extrinsic. Stores the
/// user-level inputs (the original typed-arg strings), not the encoded SCALE
/// values — so a restore can rebuild the tx through the same builder path. See
/// plan §"why a serializable mirror".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedTxRecord {
    pub pallet: String,
    pub call: String,
    /// Original typed-arg strings, in declared order.
    pub args: Vec<String>,
    pub signer: SignerRecord,
}

impl PreparedTxRecord {
    /// Build a record from a `PreparedTx` plus the user-level arg strings the
    /// builder captured (the `Value`s in `PreparedTx` are not reversible to text).
    pub fn from_parts(tx: &PreparedTx, arg_strings: Vec<String>) -> Self {
        Self {
            pallet: tx.pallet.clone(),
            call: tx.call.clone(),
            args: arg_strings,
            signer: SignerRecord::from_signer(&tx.signer),
        }
    }
}

/// The replayable action log entry (freeze §4). Every state-changing dev command
/// appends one of these on success; restore replays them in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordedAction {
    /// A `dev_setStorage` edit (P2). Stored as a JSON string for forward-compat
    /// with P2's payload, since P2 owns `SetStorageReq`.
    SetStorage { edits_json: String },
    /// One block built from a (possibly empty) staged queue (P3).
    BuiltBlock {
        extrinsics: Vec<PreparedTxRecord>,
        timestamp: Option<u64>,
        author: Option<String>,
    },
    /// Head re-forked to this block (P4, §7.1).
    SetHead(u32),
    /// Chain timestamp set (P4, §7.2).
    TimeTravel(TimeSpec),
    /// Build mode switched (P3).
    SetBuildMode(BuildMode),
}

// ---------------------------------------------------------------------------
// Session persistence (freeze §5)
// ---------------------------------------------------------------------------

/// Where a session came from: a manual save or an auto set-head snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSource {
    Manual,
    /// Auto-created when `set-head` rewound the head (amber `⏳ auto-snap`).
    AutoSnapshot,
}

/// A persisted, replayable fork session (spec §8, freeze §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub fork: ForkConfig,
    pub pins: Vec<PinnedItem>,
    pub baseline: Option<u32>,
    pub actions: Vec<RecordedAction>,
    /// Head height at save time — for display + drift detection.
    pub head: u32,
    pub source: SessionSource,
}

/// A lightweight row for the sessions list view (no full action log).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub name: String,
    pub head: u32,
    pub pin_count: usize,
    pub source: SessionSource,
    /// Seconds since the file was last modified (for the age column).
    pub age_secs: u64,
}

/// Resolve the sessions directory (see plan §"Sessions directory"):
/// `$CHOPSTICKS_TUI_SESSIONS_DIR`, else `$XDG_CONFIG_HOME/chopsticks-tui/sessions`,
/// else `$HOME/.config/chopsticks-tui/sessions`, else `./.chopsticks-sessions`.
pub fn sessions_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CHOPSTICKS_TUI_SESSIONS_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("chopsticks-tui").join("sessions");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("chopsticks-tui")
            .join("sessions");
    }
    PathBuf::from("./.chopsticks-sessions")
}

/// Auto-snapshot name for the abandoned future when set-head rewinds (§7.1).
pub fn auto_snapshot_name(old_tip: u32) -> String {
    format!("timeline-#{old_tip}")
}

/// Make a session name safe to use as a filename (no path separators).
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == std::path::MAIN_SEPARATOR {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn session_path(name: &str) -> PathBuf {
    sessions_dir().join(format!("{}.toml", sanitize_name(name)))
}

/// Persist a session as `<name>.toml`, creating the directory if needed.
pub fn save_session(session: &Session) -> Result<()> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("creating sessions dir {}: {e}", dir.display()))?;
    let text = toml::to_string_pretty(session)
        .map_err(|e| anyhow!("serializing session `{}`: {e}", session.name))?;
    let path = session_path(&session.name);
    std::fs::write(&path, text).map_err(|e| anyhow!("writing {}: {e}", path.display()))?;
    Ok(())
}

/// Load a session by name.
pub fn load_session(name: &str) -> Result<Session> {
    let path = session_path(name);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("reading session `{name}` ({}): {e}", path.display()))?;
    let session: Session =
        toml::from_str(&text).map_err(|e| anyhow!("parsing session `{name}`: {e}"))?;
    Ok(session)
}

/// Delete a session file by name.
pub fn delete_session(name: &str) -> Result<()> {
    let path = session_path(name);
    std::fs::remove_file(&path).map_err(|e| anyhow!("deleting {}: {e}", path.display()))?;
    Ok(())
}

/// List all saved sessions as summaries, newest first. Unreadable files are
/// skipped (never panics the UI).
pub fn list_sessions() -> Result<Vec<SessionSummary>> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // Missing dir = no sessions yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(anyhow!("reading sessions dir {}: {e}", dir.display())),
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = toml::from_str::<Session>(&text) else {
            continue;
        };
        let age_secs = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(SessionSummary {
            name: session.name,
            head: session.head,
            pin_count: session.pins.len(),
            source: session.source,
            age_secs,
        });
    }
    // Newest (smallest age) first.
    out.sort_by_key(|s| s.age_secs);
    Ok(out)
}

/// Format an age in seconds compactly for the list view (`now`, `2m`, `1h`, `3d`).
pub fn format_age(age_secs: u64) -> String {
    match age_secs {
        0..=4 => "now".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Capture the current wall clock as unix epoch ms (base for relative TimeSpecs).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed base instant for relative-offset tests: 2025-06-15T00:00:00Z in ms.
    const BASE_MS: u64 = 1_749_945_600_000;

    #[test]
    fn parses_unix_millis() {
        let ts = TimeSpec::parse("1750000000000", BASE_MS).unwrap();
        assert_eq!(ts.epoch_ms, 1_750_000_000_000);
    }

    #[test]
    fn parses_unix_seconds_and_scales_to_ms() {
        let ts = TimeSpec::parse("1750000000", BASE_MS).unwrap();
        assert_eq!(ts.epoch_ms, 1_750_000_000_000);
    }

    #[test]
    fn parses_iso_date_as_utc_midnight() {
        let ts = TimeSpec::parse("1970-01-02", BASE_MS).unwrap();
        assert_eq!(ts.epoch_ms, 86_400_000); // exactly one day in ms
    }

    #[test]
    fn parses_iso_datetime_utc() {
        let ts = TimeSpec::parse("1970-01-01T00:00:01", BASE_MS).unwrap();
        assert_eq!(ts.epoch_ms, 1_000);
    }

    #[test]
    fn parses_relative_offsets_against_base() {
        assert_eq!(
            TimeSpec::parse("+1d", BASE_MS).unwrap().epoch_ms,
            BASE_MS + 86_400_000
        );
        assert_eq!(
            TimeSpec::parse("+90m", BASE_MS).unwrap().epoch_ms,
            BASE_MS + 90 * 60_000
        );
        assert_eq!(
            TimeSpec::parse("+30s", BASE_MS).unwrap().epoch_ms,
            BASE_MS + 30_000
        );
        assert_eq!(
            TimeSpec::parse("+2h", BASE_MS).unwrap().epoch_ms,
            BASE_MS + 2 * 3_600_000
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(TimeSpec::parse("", BASE_MS).is_err());
        assert!(TimeSpec::parse("not-a-date", BASE_MS).is_err());
        assert!(TimeSpec::parse("+1y", BASE_MS).is_err()); // unsupported unit
        assert!(TimeSpec::parse("1970-13-01", BASE_MS).is_err()); // bad month
    }

    #[test]
    fn round_trips_through_toml() {
        let ts = TimeSpec::from_epoch_ms(1_750_000_000_000, "+1d");
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrap {
            ts: TimeSpec,
        }
        let text = toml::to_string(&Wrap { ts: ts.clone() }).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.ts.epoch_ms, ts.epoch_ms);
        assert_eq!(back.ts.source, ts.source);
    }

    #[test]
    fn recorded_action_round_trips_through_toml() {
        use crate::contracts::{BuildMode, DevAccount};
        let actions = vec![
            RecordedAction::SetHead(1030),
            RecordedAction::TimeTravel(TimeSpec::from_epoch_ms(1_750_000_000_000, "+1d")),
            RecordedAction::SetBuildMode(BuildMode::Instant),
            RecordedAction::SetStorage {
                edits_json: "[[\"0x26aa\",\"0x0100\"]]".to_string(),
            },
            RecordedAction::BuiltBlock {
                extrinsics: vec![PreparedTxRecord {
                    pallet: "Balances".to_string(),
                    call: "transfer_keep_alive".to_string(),
                    args: vec!["//Bob".to_string(), "50".to_string()],
                    signer: SignerRecord::Dev(DevAccount::Alice),
                }],
                timestamp: Some(1_750_000_000_000),
                author: Some("//Alice".to_string()),
            },
        ];
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wrap {
            actions: Vec<RecordedAction>,
        }
        let text = toml::to_string(&Wrap {
            actions: actions.clone(),
        })
        .unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.actions, actions);
    }

    #[test]
    fn prepared_tx_record_captures_user_inputs_from_prepared_tx() {
        use crate::contracts::{DevAccount, PreparedTx, TxSigner};
        use scale_value::Value;
        let tx = PreparedTx {
            pallet: "System".to_string(),
            call: "remark".to_string(),
            args: vec![Value::string("hi".to_string())],
            signer: TxSigner::Dev(DevAccount::Bob),
            encoded_preview: "0x00".to_string(),
        };
        let rec = PreparedTxRecord::from_parts(&tx, vec!["hi".to_string()]);
        assert_eq!(rec.pallet, "System");
        assert_eq!(rec.call, "remark");
        assert_eq!(rec.args, vec!["hi".to_string()]);
        assert_eq!(rec.signer, SignerRecord::Dev(DevAccount::Bob));
    }

    #[test]
    fn session_round_trips_through_disk() {
        use crate::contracts::{BuildMode, ForkConfig, KeyArg, PinnedItem, PinnedItemId};
        let dir = std::env::temp_dir().join(format!("ctui-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test; we set the override for this process only.
        unsafe {
            std::env::set_var("CHOPSTICKS_TUI_SESSIONS_DIR", &dir);
        }

        let session = Session {
            name: "before-upgrade".to_string(),
            fork: ForkConfig::Spawn {
                chain_or_path: "polkadot".to_string(),
                build_mode: BuildMode::Manual,
                mock_signature_host: false,
            },
            pins: vec![PinnedItem {
                id: PinnedItemId(1),
                pallet: "System".to_string(),
                entry: "Account".to_string(),
                keys: vec![KeyArg::U(1)],
                path: vec![],
                label: "System.Account(1)".to_string(),
            }],
            baseline: Some(1042),
            actions: vec![RecordedAction::SetHead(1030)],
            head: 1042,
            source: SessionSource::Manual,
        };

        save_session(&session).unwrap();
        let loaded = load_session("before-upgrade").unwrap();
        assert_eq!(loaded.name, session.name);
        assert_eq!(loaded.head, 1042);
        assert_eq!(loaded.baseline, Some(1042));
        assert_eq!(loaded.actions, session.actions);
        assert_eq!(loaded.pins, session.pins);

        let summaries = list_sessions().unwrap();
        assert!(
            summaries
                .iter()
                .any(|s| s.name == "before-upgrade" && s.head == 1042)
        );

        delete_session("before-upgrade").unwrap();
        assert!(load_session("before-upgrade").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_snapshot_name_uses_old_tip() {
        assert_eq!(auto_snapshot_name(1045), "timeline-#1045");
    }

    #[test]
    fn sanitize_name_strips_path_separators() {
        assert_eq!(sanitize_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_name("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(2), "now");
        assert_eq!(format_age(30), "30s");
        assert_eq!(format_age(120), "2m");
        assert_eq!(format_age(7_200), "2h");
        assert_eq!(format_age(172_800), "2d");
    }
}
