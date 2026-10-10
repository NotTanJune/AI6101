# Contributing

## Workflow

1. Work on the current branch in this repository. Keep changes focused.
2. Make the change with tests. No test may use the network, a paid provider, a live MCP service,
   or a real credential. Use the mock provider and the fixtures.
3. Run the three gates from the repository root; all must exit 0 before you hand work over:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   ```

4. For changes to the CLI or pipeline, also run:

   ```sh
   cargo run -p harness-cli -- --help
   cargo run -p harness-cli -- validate --config fixtures/smoke.toml
   cargo run -p harness-cli -- run --config fixtures/smoke.toml --output target/harness-smoke
   ```

CI runs the same commands (`.github/workflows/ci.yml`).

## Code conventions

- Rust 1.98, edition 2024, pinned in `rust-toolchain.toml`.
- Library errors use `thiserror`. Avoid `unwrap()` and `expect()` outside tests.
- `unsafe` is forbidden by the workspace lints.
- Keep TUI state and rendering separate. The TUI and CLI must call the same `harness-core`
  pipeline.
- Do not reimplement Python-owned logic (task execution, completion verification, judging,
  scoring, failure analysis). Rust validates and displays those events.
- Treat the metric ids `tf`, `pe`, `ecs` as opaque identifiers.
- No Python files or Python dependencies in this repository.

## Changing a contract

The Tool KB (`tool-kb/1`) and run events (`run-event/1`) are the only Rust/Python interchange
formats. A field or enum change is a contract change:

1. Version the schema (a new schema version and id, not a silent edit).
2. Update both language validators: the Rust schema embedding and validation, and the Python
   side, which the Python owner maintains. Coordinate with them before merging.
3. Update the shared fixtures in `fixtures/contracts/` (valid and malformed) and the contract
   tests.
4. Update `docs/experiment-protocol.md`.

Unsupported schema versions must keep failing closed. The same applies to the proposed runner
argv and exit codes: they are not agreed with the Python owner yet, so change them only together
with the docs and the runner tests.

## Data and secrets

- Never commit the supplied MCP catalog, evaluation data, benchmark judge, or any derived copy.
  Inputs are read from `MCP_DATA_DIR` and `EVAL_DATA_DIR` and stay outside Git.
- Never commit API keys or tokens. Configs carry environment variable names only.
- Never put a secret in argv, config, fixtures, events, or logs.
- Do not invent benchmark data. If an input is missing, mark it BLOCKED in
  `docs/experiment-protocol.md` and say why.
- Generated run output (`target/`) is ignored. Raw result artifacts that are part of a deliverable
  are not.

## Experiments

Benchmark release, task selection, server versions, model, decoding, judge, and rate-card date
are experiment inputs. Record them per run. Never compare runs whose settings differ without
labeling the difference. Do not start paid runs from CI or tests.
