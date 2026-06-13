//! Input modes and mode-driven key routing (MVP-2 P0).
//!
//! `Mode` is pure UI-loop state (like `phase`/`follow`); it never crosses the
//! `Command`/`Event` channel. `route_key` replaces the ad-hoc phase/overlay
//! branching the grid used in MVP-1, without changing any leaf behavior.
