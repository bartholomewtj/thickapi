//! `thickapi-cli` as a library: the command implementations `main.rs` dispatches to, the
//! capture tooling they drive (`capture/`, SPEC §19.11) and the tracing filter they share,
//! split out so `tests/*.rs` can exercise a command module without `#[path]`-including the
//! binary's source.

pub mod capture;
pub mod commands;
pub mod log_filter;
