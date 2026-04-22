# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Purpose

`rust-utils` is a personal library crate intended as a home for reusable data structures and helpers that can be depended on from other Rust projects. It is **not** a binary and has no runtime entry point.

The crate grows module-by-module as concrete needs arise in downstream projects. Avoid inventing architecture that does not yet exist in the tree.

## Toolchain

- Rust edition: **2024** (see `Cargo.toml`). Some older patterns/lints behave differently — check edition-specific behavior before suggesting idioms.
- Current runtime dependencies: `reqwest`, `governor`, `tokio` (narrow feature set), `thiserror`, `error-stack`. Before adding another dependency, confirm it is justified for a utility library (keep the dependency surface small so downstream consumers aren't forced to pull in heavy trees).

## Error handling

All fallible functions in this crate MUST return `crate::error::UtilsResult<T>` (alias for `Result<T, error_stack::Report<UtilsError>>`). Never expose `reqwest::Error`, `std::io::Error`, or other third-party error types in a public signature — classify them into a `UtilsError` variant and attach the original via `Report::new(err).change_context(UtilsError::…)` or `attach_printable`.

When matching errors in retry/recovery logic, match on `report.current_context()` against a `UtilsError` variant. Do not downcast to the underlying dependency error.

## Common commands

```bash
cargo build                 # Compile the library
cargo check                 # Fast type-check without codegen (preferred during editing)
cargo test                  # Run all unit + doc tests
cargo test <name>           # Run a single test by substring match (e.g. `cargo test it_works`)
cargo test -- --nocapture   # Show println!/dbg! output from tests
cargo test --doc            # Run only documentation tests
cargo clippy --all-targets -- -D warnings   # Lint (treat warnings as errors)
cargo fmt                   # Format
cargo doc --open            # Build and open rustdoc locally
```

## Conventions for this repo

- **Docs are mandatory on public items.** Every `pub` fn/struct/enum/trait gets a rustdoc comment (`///`) with a short description, `# Arguments`, `# Returns`, and — when relevant — `# Panics` / `# Errors` / `# Safety` sections. For trivial helpers, a one-line summary is enough; do not pad with empty sections.
- Per the user's global rule, place comments (including rustdoc) **above** `#[derive(...)]` / other attributes, not between the attribute and the item.
- All in-code annotations (comments, docstrings, error messages) must be written in **English**, even when the user prompt is in Spanish.
- Because this crate is meant to be consumed by other projects, treat the public API as a stability surface: prefer adding items over renaming them, and re-export deliberately from `lib.rs` rather than exposing deep module paths by accident.

## Keep README.md in sync — MANDATORY

Whenever a change touches the public API (new pub item, renamed item, changed signature, changed error type, new module, new feature flag) **update `README.md` in the same change**. The README's module tables are the curated summary downstream consumers read first — if they drift, the crate becomes harder to trust than the one people were avoiding by depending on this. Concretely:

- New `pub` item → add a row to the matching module's table with its kind and one-line summary.
- Removed/renamed item → remove or rename the row; do not leave stale references.
- Changed error surface (e.g. a function now returns `UtilsResult`) → update the table entry and any code example below it so the `?` / match still makes sense.
- New module → add a new `###` section with a short rationale and at least one table.

Do not defer README updates to "a later pass". A PR that changes public API without updating the README is incomplete.

## Adding a new utility module

1. Create `src/<topic>/mod.rs` (or `src/<topic>.rs` for a single-file module).
2. Declare it in `src/lib.rs` with `pub mod <topic>;` and re-export the intended public items if they should live at the crate root.
3. Co-locate unit tests in a `#[cfg(test)] mod tests { ... }` block at the bottom of the module file.
4. Add rustdoc examples where useful — they double as `cargo test --doc` coverage.
5. Add a section to `README.md` as described above.

## Not yet present

There is currently no CI config, no `rustfmt.toml`/`clippy.toml`, and no workspace. If one of these is added later, update this file so future sessions know where to look.
