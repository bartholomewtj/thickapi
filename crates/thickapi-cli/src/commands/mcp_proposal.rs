use std::path::Path;

use thickapi_core::thin::mcp_proposal::{serve, validate_paths, EXIT_STARTUP};

/// `thickapi mcp-proposal --catalog --ready --violation` (SPEC §19.7).
pub fn run(catalog: &Path, ready: &Path, violation: &Path) -> i32 {
    let paths = match validate_paths(catalog, ready, violation) {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("mcp-proposal: {err:#}");
            return EXIT_STARTUP;
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&paths, stdin.lock(), stdout.lock())
}
