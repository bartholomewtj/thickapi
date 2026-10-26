set shell := ["pwsh", "-NoProfile", "-Command"]

# Same four checks CI runs on every PR (.github/workflows/ci.yml, B2), in the
# same order. `just gate` is the local-equivalent subset named in
# scripts/verify-all.ts's header.
gate:
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo clippy --workspace --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo test --workspace --no-fail-fast
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    # One line: just runs each recipe line in its own shell, so a Push-Location on its
    # own line is gone by the next one and `bun test` hit the repo root (fixtures/soak/).
    Push-Location extensions/pi-thin; bun test; $c = $LASTEXITCODE; Pop-Location; exit $c

ci-local: gate
