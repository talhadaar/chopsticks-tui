//! The static command registry (MVP-2 P0): the single source of truth for which
//! verbs the command palette exposes, plus the `:`-line parser that turns a typed
//! command into either a `Command` to dispatch or a client-side `LocalAction`.

use crate::contracts::{BuildMode, Command};
use crate::session::{TimeSpec, now_ms};

/// The shape of a single command-line argument. Distinct from
/// `tx_builder::ArgKind` (which parses call args into `scale_value`); this enum
/// describes the small set of *command* arg shapes the palette accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// No argument (the command is a bare verb).
    None,
    /// A block number, e.g. `1042`.
    Block,
    /// Free text (session name, etc.).
    Text,
    /// A timestamp / date / relative offset (parser owned by P4).
    TimeSpec,
    /// One of `manual` / `instant` / `batch`.
    BuildMode,
}

/// One named, possibly-optional argument of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandArg {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
}

/// The underlying RPC a command maps to, for the right-aligned palette label.
/// `Dev(_)` shows the `dev_*` method name in purple; `Local` is client-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcLabel {
    Dev(&'static str),
    Local,
}

/// A registered command: how it shows in the palette and what args it takes.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub rpc: RpcLabel,
    pub args: &'static [CommandArg],
}

const NO_ARGS: &[CommandArg] = &[];

/// The static verb table. The single source of truth for the palette and parser.
/// Ordered roughly by frequency / spec §2.2 grouping.
pub fn registry() -> &'static [CommandSpec] {
    &[
        // --- MVP-1 verbs surfaced in the palette ---
        CommandSpec {
            name: "pin",
            description: "Pin a storage item to the grid",
            rpc: RpcLabel::Local,
            args: NO_ARGS,
        },
        CommandSpec {
            name: "tx",
            description: "Open the transaction builder",
            rpc: RpcLabel::Local,
            args: NO_ARGS,
        },
        CommandSpec {
            name: "reconnect",
            description: "Reconnect to the fork",
            rpc: RpcLabel::Local,
            args: NO_ARGS,
        },
        CommandSpec {
            name: "quit",
            description: "Quit the application",
            rpc: RpcLabel::Local,
            args: NO_ARGS,
        },
        // --- MVP-2 verbs (spec §2.2) ---
        CommandSpec {
            name: "set-storage",
            description: "Write a storage value at the tip",
            rpc: RpcLabel::Dev("dev_setStorage"),
            args: NO_ARGS, // selection happens via the picker (spec §5)
        },
        CommandSpec {
            name: "set-head",
            description: "Re-fork to a chosen block",
            rpc: RpcLabel::Dev("dev_setHead"),
            args: &[CommandArg { name: "block", kind: ArgKind::Block, required: true }],
        },
        CommandSpec {
            name: "set-baseline",
            description: "Pin the current (or given) block as baseline",
            rpc: RpcLabel::Local,
            args: &[CommandArg { name: "block", kind: ArgKind::Block, required: false }],
        },
        CommandSpec {
            name: "clear-baseline",
            description: "Revert to vs-previous diffing",
            rpc: RpcLabel::Local,
            args: NO_ARGS,
        },
        CommandSpec {
            name: "sessions",
            description: "Save or load a fork session",
            rpc: RpcLabel::Local,
            args: NO_ARGS,
        },
        CommandSpec {
            name: "build",
            description: "Build a block from the staged queue",
            rpc: RpcLabel::Dev("dev_newBlock"),
            args: NO_ARGS,
        },
        CommandSpec {
            name: "build-mode",
            description: "Switch Chopsticks build mode",
            rpc: RpcLabel::Dev("dev_setBlockBuildMode"),
            args: &[CommandArg { name: "mode", kind: ArgKind::BuildMode, required: true }],
        },
        CommandSpec {
            name: "time-travel",
            description: "Set the chain timestamp",
            rpc: RpcLabel::Dev("dev_timeTravel"),
            args: &[CommandArg { name: "when", kind: ArgKind::TimeSpec, required: true }],
        },
    ]
}

/// A `:`-line successfully matched to a registered command, with its raw args.
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    pub spec: &'static CommandSpec,
    pub args: Vec<String>,
}

/// Where a parsed command goes: over the `Command` channel (RPC) or applied to
/// `AppState` directly (client-side).
#[derive(Debug)]
pub enum CommandRoute {
    Dispatch(Command),
    Local(LocalAction),
}

/// A client-side action applied in the UI loop via `AppState::apply_local`.
/// P1 (baseline diff) implements the effect; P0 only routes here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    /// Pin a baseline (None = current tip).
    SetBaseline(Option<u32>),
    /// Revert to vs-previous diffing.
    ClearBaseline,
    /// Open the storage picker (`:pin` entry point).
    OpenPicker,
    /// Open the set-storage editor (`:set-storage` entry point; P2).
    OpenSetStorage,
    /// Open the transaction builder (`:tx`).
    OpenTxBuilder,
    /// Open the build staging panel (`:build`). P3 fills the UI.
    OpenBuildPanel,
    /// Open the sessions modal (`:sessions`). P4 fills the UI.
    OpenSessions,
}

/// Why a `:`-line failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Nothing but whitespace was typed.
    Empty,
    /// The verb is not in the registry.
    UnknownCommand(String),
    /// A required argument was not supplied.
    MissingArg(&'static str),
    /// An argument was supplied but failed to parse.
    BadArg { name: &'static str, value: String, reason: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => write!(f, "type a command"),
            ParseError::UnknownCommand(c) => write!(f, "unknown command `{c}`"),
            ParseError::MissingArg(a) => write!(f, "missing required argument `{a}`"),
            ParseError::BadArg { name, value, reason } => {
                write!(f, "bad `{name}` `{value}`: {reason}")
            }
        }
    }
}

/// Match a typed `:`-line against the registry. A leading `:` and surrounding
/// whitespace are ignored. The first whitespace-delimited token is the verb; the
/// rest are positional args (kept as raw strings; `to_route` parses them).
pub fn parse_line(line: &str) -> std::result::Result<ParsedCommand, ParseError> {
    let line = line.trim().strip_prefix(':').unwrap_or(line.trim()).trim();
    let mut toks = line.split_whitespace();
    let verb = toks.next().ok_or(ParseError::Empty)?;
    let spec = registry()
        .iter()
        .find(|c| c.name == verb)
        .ok_or_else(|| ParseError::UnknownCommand(verb.to_string()))?;
    let args: Vec<String> = toks.map(str::to_string).collect();

    // Required-arg arity check (positional).
    for (i, arg) in spec.args.iter().enumerate() {
        if arg.required && args.get(i).is_none() {
            return Err(ParseError::MissingArg(arg.name));
        }
    }
    Ok(ParsedCommand { spec, args })
}

/// Turn a parsed command into its route. RPC commands become `Command`s; local
/// verbs become `LocalAction`s. Arg parsing (block numbers, build modes) happens
/// here so the palette can surface a `BadArg` error before dispatch.
pub fn to_route(parsed: &ParsedCommand) -> std::result::Result<CommandRoute, ParseError> {
    let arg = |i: usize| parsed.args.get(i).map(String::as_str);
    let parse_block = |name: &'static str, s: &str| {
        s.parse::<u32>().map_err(|_| ParseError::BadArg {
            name,
            value: s.to_string(),
            reason: "expected a block number".to_string(),
        })
    };

    let route = match parsed.spec.name {
        // RPC commands.
        // `:set-storage` opens the set-storage editor overlay (target picker →
        // value editor). The editor dispatches `Command::SetStorage` itself once
        // the user confirms, so this routes to a local overlay-open, not a direct
        // dispatch (P2).
        "set-storage" => CommandRoute::Local(LocalAction::OpenSetStorage),
        "set-head" => {
            let n = parse_block("block", arg(0).ok_or(ParseError::MissingArg("block"))?)?;
            CommandRoute::Dispatch(Command::SetHead(n))
        }
        // `:build` opens the staging panel (P3); the panel's ↵ emits the actual
        // `Command::BuildWithQueue`. (Previously a direct empty-queue dispatch.)
        "build" => CommandRoute::Local(LocalAction::OpenBuildPanel),
        "build-mode" => {
            let raw = arg(0).ok_or(ParseError::MissingArg("mode"))?;
            let mode = match raw.to_ascii_lowercase().as_str() {
                "manual" => BuildMode::Manual,
                "instant" => BuildMode::Instant,
                "batch" => BuildMode::Batch,
                _ => {
                    return Err(ParseError::BadArg {
                        name: "mode",
                        value: raw.to_string(),
                        reason: "expected manual|instant|batch".to_string(),
                    });
                }
            };
            CommandRoute::Dispatch(Command::SetBuildMode(mode))
        }
        "time-travel" => {
            let raw = arg(0).ok_or(ParseError::MissingArg("when"))?;
            // P4 parses the spec to an absolute instant here so the palette can
            // surface a `BadArg` error before dispatch. Relative offsets resolve
            // against the wall clock at parse time.
            let spec = TimeSpec::parse(raw, now_ms()).map_err(|e| ParseError::BadArg {
                name: "when",
                value: raw.to_string(),
                reason: e.to_string(),
            })?;
            CommandRoute::Dispatch(Command::TimeTravel(spec))
        }
        // Local actions.
        "set-baseline" => {
            let block = match arg(0) {
                Some(s) => Some(parse_block("block", s)?),
                None => None,
            };
            CommandRoute::Local(LocalAction::SetBaseline(block))
        }
        "clear-baseline" => CommandRoute::Local(LocalAction::ClearBaseline),
        "pin" => CommandRoute::Local(LocalAction::OpenPicker),
        "tx" => CommandRoute::Local(LocalAction::OpenTxBuilder),
        "sessions" => CommandRoute::Local(LocalAction::OpenSessions),
        "reconnect" => CommandRoute::Dispatch(Command::Reconnect),
        "quit" => CommandRoute::Dispatch(Command::Quit),
        // Unreachable: `parse_line` only returns specs from `registry()`.
        other => return Err(ParseError::UnknownCommand(other.to_string())),
    };
    Ok(route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_mvp2_verb() {
        let names: Vec<&str> = registry().iter().map(|c| c.name).collect();
        // MVP-2 verbs (spec §2.2).
        for v in [
            "set-storage", "set-head", "set-baseline", "clear-baseline",
            "sessions", "build", "build-mode", "time-travel",
        ] {
            assert!(names.contains(&v), "registry missing MVP-2 verb `{v}`");
        }
        // MVP-1 verbs surfaced in the palette.
        for v in ["pin", "tx", "quit", "reconnect"] {
            assert!(names.contains(&v), "registry missing MVP-1 verb `{v}`");
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = registry().iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate command name in registry");
    }

    #[test]
    fn rpc_labels_match_spec() {
        let find = |n: &str| registry().iter().find(|c| c.name == n).unwrap();
        assert!(matches!(find("set-storage").rpc, RpcLabel::Dev("dev_setStorage")));
        assert!(matches!(find("set-head").rpc, RpcLabel::Dev("dev_setHead")));
        assert!(matches!(find("set-baseline").rpc, RpcLabel::Local));
        assert!(matches!(find("clear-baseline").rpc, RpcLabel::Local));
    }

    #[test]
    fn parse_line_matches_verb_and_collects_args() {
        let p = parse_line("set-head 1030").expect("parses");
        assert_eq!(p.spec.name, "set-head");
        assert_eq!(p.args, vec!["1030".to_string()]);
    }

    #[test]
    fn parse_line_ignores_leading_colon_and_extra_whitespace() {
        let p = parse_line(":set-baseline   1042").expect("parses");
        assert_eq!(p.spec.name, "set-baseline");
        assert_eq!(p.args, vec!["1042".to_string()]);
    }

    #[test]
    fn parse_line_unknown_verb_errors() {
        assert!(matches!(parse_line("frobnicate"), Err(ParseError::UnknownCommand(_))));
    }

    #[test]
    fn parse_line_empty_errors() {
        assert!(matches!(parse_line("   "), Err(ParseError::Empty)));
    }

    #[test]
    fn parse_line_missing_required_arg_errors() {
        assert!(matches!(parse_line("set-head"), Err(ParseError::MissingArg("block"))));
    }

    #[test]
    fn route_set_head_dispatches_command() {
        let p = parse_line("set-head 1030").unwrap();
        match to_route(&p).unwrap() {
            CommandRoute::Dispatch(crate::contracts::Command::SetHead(n)) => assert_eq!(n, 1030),
            other => panic!("expected SetHead dispatch, got {other:?}"),
        }
    }

    #[test]
    fn route_set_baseline_with_block_is_local() {
        let p = parse_line("set-baseline 1042").unwrap();
        match to_route(&p).unwrap() {
            CommandRoute::Local(LocalAction::SetBaseline(Some(1042))) => {}
            other => panic!("expected SetBaseline(Some(1042)), got {other:?}"),
        }
    }

    #[test]
    fn route_set_baseline_no_arg_pins_current_tip() {
        let p = parse_line("set-baseline").unwrap();
        match to_route(&p).unwrap() {
            CommandRoute::Local(LocalAction::SetBaseline(None)) => {}
            other => panic!("expected SetBaseline(None), got {other:?}"),
        }
    }

    #[test]
    fn route_set_storage_opens_editor_overlay() {
        let p = parse_line("set-storage").unwrap();
        assert!(matches!(
            to_route(&p).unwrap(),
            CommandRoute::Local(LocalAction::OpenSetStorage)
        ));
    }

    #[test]
    fn route_clear_baseline_is_local() {
        let p = parse_line("clear-baseline").unwrap();
        assert!(matches!(to_route(&p).unwrap(), CommandRoute::Local(LocalAction::ClearBaseline)));
    }

    #[test]
    fn route_build_mode_parses_each_variant() {
        use crate::contracts::BuildMode;
        for (s, want) in [
            ("manual", BuildMode::Manual),
            ("instant", BuildMode::Instant),
            ("batch", BuildMode::Batch),
        ] {
            let p = parse_line(&format!("build-mode {s}")).unwrap();
            match to_route(&p).unwrap() {
                CommandRoute::Dispatch(crate::contracts::Command::SetBuildMode(m)) => {
                    assert_eq!(m, want)
                }
                other => panic!("expected SetBuildMode, got {other:?}"),
            }
        }
    }

    #[test]
    fn route_bad_block_arg_errors() {
        let p = parse_line("set-head notanumber").unwrap();
        assert!(matches!(to_route(&p), Err(ParseError::BadArg { .. })));
    }

    #[test]
    fn route_bad_build_mode_errors() {
        let p = parse_line("build-mode turbo").unwrap();
        assert!(matches!(to_route(&p), Err(ParseError::BadArg { .. })));
    }
}
