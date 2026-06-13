//! The static command registry (MVP-2 P0): the single source of truth for which
//! verbs the command palette exposes, plus the `:`-line parser that turns a typed
//! command into either a `Command` to dispatch or a client-side `LocalAction`.
