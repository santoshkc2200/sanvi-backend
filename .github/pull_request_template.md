## Summary

<!-- What does this change and why? Link the issue. -->

## Checklist

- [ ] Follows the layering rules in `docs/architecture-overview.md` (no `sqlx`/`actix`/`reqwest` in `domain`)
- [ ] Tests added/updated and green (`cargo nextest run --workspace`)
- [ ] `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` pass
- [ ] OpenAPI regenerated when handlers changed (`cargo run -p sanvi-cli -- openapi --check`)
- [ ] Migrations are reversible (down migration provided)
- [ ] Secrets never added to config files; `.env.example` updated only with placeholders
- [ ] Threat model reviewed when security-relevant (`docs/security/threat-model.md`)

## Rollback

<!-- How is this change reverted if it breaks staging? -->
