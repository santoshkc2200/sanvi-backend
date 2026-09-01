# Runbook — advertising platform API version upgrade drill

Every future Google Ads or Meta API version bump runs this drill **before** production.
It turns "we hope the new ad platform API version is compatible" into a verified
statement: recorded platform capability fixtures, schema snapshots, and adapter test
corpora are replayed against the candidate build's handlers and ports.

Sanvi pins external ad platform API versions in `configs/base.toml`:
- `advertising.google_ads_api_version = "v22"`
- `advertising.meta_api_version = "v24.0"`

> [!NOTE]
> **Adapter credential status**: Live Google Ads and Meta API adapters are currently
> blocked on upstream developer-token approvals and app review (see `docs/tasks/backlog.md`
> TASK-011/012/013/015 rows). The primary fixture mechanism used for API version drills
> is `FakeAdPlatform` (`crates/contexts/advertising/src/adapters/stubs.rs`) and the
> canonical capability fixture dataset in `crates/contexts/advertising/fixtures/capability-matrices.json`.
> Once live credentials are wired, live sandbox recorded corpora will supplement these fixtures.

## The drill

### 0. Preconditions

- A clean branch with the candidate version bump.
- The capability matrix fixture `crates/contexts/advertising/fixtures/capability-matrices.json`
  is present and passing round-trip equality against `FakeAdPlatform::fixtures()`.
- OpenAPI schema generator (`just openapi`) and invariant gates (`scripts/check-ads-invariants.sh`)
  are green on the baseline build.

### 1. Snapshot the corpus (current version)

Ensure the current capability matrix and platform descriptors are captured:

```bash
# Verify baseline capability matrices fixture
cargo test -p sanvi-advertising fixture_file_is_the_frontend_export
```

Record the current catalog snapshot:

```bash
curl -s http://localhost:8080/api/v1/tenant/ads/platforms | jq . > capability-matrix-baseline.json
```

### 2. Build the candidate

1. Bump the pinned API version in `configs/base.toml` (and `configs/staging.toml` / `configs/production.toml`):
   ```toml
   [advertising]
   google_ads_api_version = "<new-google-version>"
   meta_api_version = "<new-meta-version>"
   ```
2. Update the platform adapters under `crates/contexts/advertising/src/adapters/` and the
   capability matrix in `crates/contexts/advertising/fixtures/capability-matrices.json`
   if the upstream platform introduced new objectives, placements, or deprecated fields.

### 3. Replay the corpus against candidate adapters

Run the full advertising integration test suite to replay all campaign lifecycles,
budget calculations, conversion uploads, audience syncs, and metrics ingestions against
the candidate platform adapters:

```bash
# Run advertising crate integration test corpus
cargo nextest run -p sanvi-advertising

# Run invariant gate (verifies no raw PII in adapters and proper ConsentSnapshot types)
./scripts/check-ads-invariants.sh

# Run layering and architecture verification
just layering
just ads-invariants
just id-policy
just openapi
```

### 4. Diff & Judge the result

A pass requires that:
1. **Zero regression in capability matrices**:
   Compare the candidate exported capability matrices against baseline:
   ```bash
   diff -u capability-matrix-baseline.json <(cargo test -p sanvi-advertising fixture_file_is_the_frontend_export)
   ```
2. **Deterministic metrics & ROAS ingestion**:
   `RunMetricsIngestion` and `SqlxMetricsRepository::replace_window` correctly parse
   synthetic and recorded responses without rounding or currency drift.
3. **Conversion upload safety**:
   Upload ports continue to enforce `ConsentSnapshot` and `HashedIds` checks without
   leaking identifiers.

- **Pass**: All tests green, matrix diffs are intentional and documented in
  `crates/contexts/advertising/fixtures/capability-matrices.json`, zero invariant errors.
- **Fail**: Any contract error, broken schema serialization, or invariant failure.
  Revert the version bump, update adapter translation mappings, and re-run.

### 5. Clean up & Sign-off

1. If changes were made to platform capabilities, synchronize the exported JSON fixture
   to `sanvi-frontend` if required.
2. Record the upgraded versions, fixture corpus SHA, drill timestamp, and operator in
   the release changelog.
3. Roll the upgrade through staging deployment.

## Why fixture replays and not live traffic

Live ad traffic arrives once, unrepeatable, and API mismatches can silently drop
conversions or corrupt attribution metrics. The recorded capability matrix corpus
ensures deterministic, counted, and repeatable verification across every supported
objective, placement, and budget type.
