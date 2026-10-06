//! TASK-021 performance test target.
//!
//! Slice 11.2b (FR-1108): the bounded-collections contract gate — every
//! collection endpoint in the merged OpenAPI document declares a bounded page
//! size or carries a reasoned exemption. Slice 11.2d added
//! `explain_assertions` (EXPLAIN (ANALYZE, BUFFERS) plan gate for the
//! profiled read models); slice 11.2e added `statement_counts`
//! (pg_stat_statements diff per use case — the N+1 gate).
//!
//! Scoping convention follows `test-idor`/`rls-matrix`: the workspace suite
//! runs this target too, and `scripts`-equivalent invocations scope to it
//! with `cargo nextest run -p sanvi-api --test perf`.

mod bounded_collections;
mod explain_assertions;
mod statement_counts;
