//! Tracing filter for `thickapi` (SPEC §1.6 / §19.8): `THICKAPI_LOG`, else `RUST_LOG`, else
//! `thickapi=info`, then HTTP crates pinned to `warn` so the env cannot raise them.

use tracing_subscriber::EnvFilter;

const PINNED: &str = "hyper=warn,h2=warn,reqwest=warn,rustls=warn";

pub fn filter_spec(thickapi_log: Option<&str>, rust_log: Option<&str>) -> String {
    let base = nonempty(thickapi_log)
        .or_else(|| nonempty(rust_log))
        .unwrap_or("thickapi=info");
    format!("{base},{PINNED}")
}

pub fn env_filter() -> EnvFilter {
    let spec = filter_spec(
        std::env::var("THICKAPI_LOG").ok().as_deref(),
        std::env::var("RUST_LOG").ok().as_deref(),
    );
    EnvFilter::try_new(&spec).unwrap_or_else(|_| EnvFilter::new(format!("thickapi=info,{PINNED}")))
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn pins_http_crates_after_env_so_they_cannot_be_raised() {
        let spec = filter_spec(
            None,
            Some("h2=trace,hyper=debug,reqwest=trace,rustls=trace"),
        );
        assert!(
            spec.ends_with("hyper=warn,h2=warn,reqwest=warn,rustls=warn"),
            "{spec}"
        );
        assert!(spec.contains("h2=trace"), "{spec}");
        let filter = EnvFilter::try_new(&spec).expect("parse");
        let rendered = filter.to_string();
        for pin in ["hyper=warn", "h2=warn", "reqwest=warn", "rustls=warn"] {
            assert!(rendered.contains(pin), "{rendered} missing {pin}");
        }
    }

    #[test]
    fn thickapi_log_wins_over_rust_log() {
        assert_eq!(
            filter_spec(Some("thickapi=debug"), Some("thickapi=trace")),
            "thickapi=debug,hyper=warn,h2=warn,reqwest=warn,rustls=warn"
        );
        assert_eq!(
            filter_spec(None, None),
            "thickapi=info,hyper=warn,h2=warn,reqwest=warn,rustls=warn"
        );
    }
}
