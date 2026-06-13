//! The static command registry (MVP-2 P0): the single source of truth for which
//! verbs the command palette exposes, plus the `:`-line parser that turns a typed
//! command into either a `Command` to dispatch or a client-side `LocalAction`.

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
}
