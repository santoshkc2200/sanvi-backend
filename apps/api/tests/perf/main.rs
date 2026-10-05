//! TASK-021 performance test target.
//!
//! Slice 11.2b (FR-1108): the bounded-collections contract gate — every
//! collection endpoint in the merged OpenAPI document declares a bounded page
//! size or carries a reasoned exemption. Slices 11.2d/e will add
//! `explain_assertions` and `statement_counts` modules here.
//!
//! Scoping convention follows `test-idor`/`rls-matrix`: the workspace suite
//! runs this target too, and `scripts`-equivalent invocations scope to it
//! with `cargo nextest run -p sanvi-api --test perf`.

mod bounded_collections;
