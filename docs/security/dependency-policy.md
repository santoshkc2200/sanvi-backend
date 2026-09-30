# Dependency review policy

**Task:** TASK-024 (slice 11.5f) · **Applies to:** every crate in the
workspace, direct and transitive · **Enforced by:** `just audit`
(`cargo audit`, `cargo deny check`, `scripts/check-audit-acceptances.sh`)
and `just sbom` — all wired into CI (`.github/workflows/pr.yml`,
`release.yml`).

The one-sentence rule: **every dependency is chosen, known, current within
reason, and inventoried — and every exception is a loan with a due date.**

## Adding a dependency

1. **Justify the crate in the PR description**: what it replaces, why an
   in-house or already-present crate cannot do it, and its maintenance
   signal (release cadence, maintainer count, advisory history).
2. `deny.toml` gates run on the PR: an unlisted license, a duplicate
   version of an existing dependency (`multiple-versions = "warn"`), a
   wildcard version (`wildcards = "deny"`), or a non-crates.io source
   (`[sources]`) fails or flags the build.
3. Prefer crates already in the lockfile — a second version of `h2`,
   `rustls`, or `tokio` in one binary is attack surface and binary size for
   no benefit. A new duplicate version requires a sentence in the PR saying
   why the existing one is unreachable.
4. Path-only workspace members (our own unpublished crates) are expected to
   be wildcard-declared; `allow-wildcard-paths = true` keeps that distinct
   from a real unpinned registry dependency.

## Licenses

The allowlist in `deny.toml` is permissive-only (MIT, Apache-2.0, BSD-2/3,
ISC, Zlib, Unicode-3.0, MPL-2.0, CC0-1.0, CDLA-Permissive-2.0, BSL-1.0,
OpenSSL, Apache-2.0 WITH LLVM-exception) with per-entry comments for the
less obvious ones (e.g. webpki-roots' bundled CA data). Adding a new
license to the list is a policy decision, not a mechanical one: copyleft
(AGPL, GPL, LGPL) and source-available (BSL non-1.0, SSPL, Elastic) terms
are incompatible with the product's distribution model and stay out.

## Advisories: the acceptance lifecycle

`cargo audit` and `cargo deny check advisories` both fail the build on a
known vulnerability or a yanked crate in the lockfile. When a finding
cannot be fixed in the PR that discovered it (no fixed release exists, or
the fix is a framework migration), it is **accepted, never ignored**:

1. The acceptance is a structured entry in `deny.toml`:

   ```toml
   ignore = [
     { id = "RUSTSEC-YYYY-NNNN", reason = "…two sentences of context…; expires:YYYY-MM-DD" },
   ]
   ```

2. The `reason` must state **why the exposure is bounded** (dev-dependency
   only, not reachable from a request path, mitigated at the edge) — the
   reason is the audit trail.
3. The `reason` must **end with `expires:YYYY-MM-DD`**. Windows are short:
   weeks for anything reachable from a request path, a quarter for
   dev-dependencies. `scripts/check-audit-acceptances.sh` fails the build
   on a missing, unparsable, or expired date — so an acceptance re-surfaces
   on its own, instead of being remembered by no one.
4. `cargo-audit` does not read `deny.toml`; accepted ids are mirrored in
   `.cargo/audit.toml` with a pointer back. **deny.toml is the ledger**;
   if the two disagree, fix the mirror.
5. Renewal is allowed but visible: a new expiry date is a diff on the
   ledger that a reviewer has to ack, in the same way as the route-matrix
   regeneration rule from TASK-024 11.5a.

The current ledger: one entry — RUSTSEC-2026-0258 (h2 0.3 via actix-http
3.x; bounded because HTTP/2 terminates at the edge; expires 2026-11-15).

## Update cadence

- **Routine:** `cargo update` for patch-level bumps rides along with normal
  PRs; dependency-only PRs say so in the title and run the full gate set.
- **Monthly-ish:** run `cargo audit` + `cargo deny check` on a clean tree
  even with no changes — the advisory DB updates under us; CI catches this
  on the next PR regardless, but finding out before a release branch is
  cut is cheaper.
- **Minor/major upgrades of the load-bearing frame** (actix-web, sqlx,
  tokio, tonic, reqwest): one PR per framework, full `just ci` before
  merge, and a release-candidate perf run (`just perf-compare`) when the
  framework touches the request path — the latency budgets in
  `docs/slo/budgets.md` are regression guards, and a framework bump is
  exactly the kind of change they exist to catch.

## SBOM and provenance

- `just sbom` (locally) and the `sbom` CI job produce a CycloneDX JSON
  document for every PR; the artifact is retained with the run.
- On every `v*` tag, `.github/workflows/release.yml` rebuilds the SBOM
  **from the tagged commit**, checksums it, re-runs the advisory gates on
  that same tree, and attaches SBOM + checksum to the GitHub release. A
  release's SBOM therefore always names the exact commit it shipped — the
  property TASK-029's evidence packs also depend on.
- Container images are signed at build (main.yml, keyless); the SBOM and
  the signature both key off the commit SHA, so a shipped artifact is
  traceable end to end.

## Scanner set and what each one owns

| Scanner | Surface | Where |
| --- | --- | --- |
| cargo audit | known vulnerabilities + yanked crates in `Cargo.lock` | pr.yml `audit`, `just audit` |
| cargo deny | licenses, bans/duplicates, sources, advisories | pr.yml `deny`, `just audit` |
| acceptance expiry | dated acceptances in `deny.toml` | pr.yml `audit`, release.yml |
| gitleaks | committed secrets (full history) | pr.yml `secrets` |
| SAST (clippy security groups + semgrep `p/rust` vendored) | source-level vulnerability patterns | pr.yml `sast` (TASK-031) |
| trivy (fs, CRITICAL/HIGH) | filesystem/IaC vulnerabilities | pr.yml `trivy` |
| cargo-cyclonedx | dependency inventory | pr.yml `sbom`, release.yml |
| clippy `-D warnings` + `cargo fmt` | whole-workspace hygiene | pr.yml `clippy`/`fmt` |

All of them block. A scanner that only warns is a scanner that has already
been turned off.

## Out of scope (recorded, not forgotten)

- Automated dependency-update bots (Dependabot/renovate) — needs a decision
  on who owns the update queue; parked in `docs/release/needs-humans.md`.
- Vendored-dependency pinning beyond what `deny.toml`'s source policy
  already enforces.
- Frontend dependency scanning — `sanvi-frontend` owns its policy.
