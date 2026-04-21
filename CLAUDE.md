# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Purpose

`rust-utils` is a personal library crate intended as a home for reusable data structures and helpers that can be depended on from other Rust projects. It is **not** a binary and has no runtime entry point.

At the time of writing, the crate is a fresh `cargo new --lib` scaffold: `src/lib.rs` still contains only the default `add` placeholder. Expect the shape of the crate to grow module-by-module; avoid inventing architecture that does not yet exist in the tree.

## Toolchain

- Rust edition: **2024** (see `Cargo.toml`). Some older patterns/lints behave differently — check edition-specific behavior before suggesting idioms.
- No external dependencies yet. Before adding one, confirm it is justified for a utility library (keep the dependency surface small so downstream consumers aren't forced to pull in heavy trees).

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

## Adding a new utility module

The expected pattern (once the first real module lands):

1. Create `src/<topic>/mod.rs` (or `src/<topic>.rs` for a single-file module).
2. Declare it in `src/lib.rs` with `pub mod <topic>;` and re-export the intended public items if they should live at the crate root.
3. Co-locate unit tests in a `#[cfg(test)] mod tests { ... }` block at the bottom of the module file.
4. Add rustdoc examples where useful — they double as `cargo test --doc` coverage.

## Not yet present

There is currently no README, no CI config, no `rustfmt.toml`/`clippy.toml`, and no workspace. If one of these is added later, update this file so future sessions know where to look.
