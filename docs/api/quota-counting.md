# Quota accounting

The usage endpoint and commercial enforcement read the same PostgreSQL
`platform.quota_counters` rows. Redis projects committed values with a version
check: delayed workers cannot overwrite a newer value, including a smaller
gauge after deletion. Decimal-string comparisons preserve integer precision
beyond Lua's 2^53 limit. PostgreSQL remains authoritative after worker restart,
Redis flush, expiration, or outage. There is no local counter fallback.

API requests, conversions, and notification sends accumulate in UTC calendar
months. Storage bytes, active seats, and nonremoved custom domains are current
resource gauges under a fixed epoch key: month rollover never forgets live
resources. The usage response reports the current calendar month with each
dimension's current usage and plan limit.

| Dimension | Starter | Professional | Enterprise |
| --- | ---: | ---: | ---: |
| API requests / month | 2,470,000 | 14,820,000 | 74,100,000 |
| Tracked conversions / month | 10,000 | 100,000 | 1,000,000 |
| Storage bytes | 1,073,741,824 | 10,737,418,240 | 107,374,182,400 |
| Custom domains | 1 | 10 | 100 |
| Active seats | 5 | 25 | 250 |
| Notification sends / month | 10,000 | 100,000 | 1,000,000 |

API budgets derive from TASK-021's [capacity model](../runbooks/capacity.md):
494,000 requests/tenant/day divided by six and multiplied by thirty for Starter;
Professional gets the measured tenant share, Enterprise five shares. This is lab
arithmetic rather than a production capacity claim. Other dimensions are initial
commercial policy. The active/trialing/past-due subscription selects the billing
tier; inactive or grace-expired subscriptions fall back to Starter. The HTTP
boundary refreshes the tenant's policy before writes, reflecting plan changes
and the `platform.quotas` feature flag. Enforcement initially remains disabled.

Memberships, domains, and conversions count through producer transaction triggers.
Migrations backfill existing gauges and the current month's conversions. Resource
and counter changes roll back together. Policy and counter locks serialize
enforcement, preventing concurrent oversubscription. Retention deletion never
refunds conversion usage. Nonconsuming updates and resource release remain possible
after a downgrade. External media and notification adapters use the shared quota
service; external gauge snapshots must serialize with producer changes, and send
reservations precede external sends.

Flag-off counting keeps accumulating above limits. Enforced source reservations
raise `quota_exceeded|dimension|attempted_usage|limit|upgrade_path` for the HTTP
entitlement-aware problem mapping. Breaches name the next plan's checkout path;
Enterprise names `contact_sales`. Redis is a disposable read projection behind a
PostgreSQL durability fence. Usage and enforcement first read the committed
counter version and value, then use the Redis value when both fields match.
Missing, outdated, or corrupted cache entries reconcile from that same PostgreSQL
snapshot. Cache values alone cannot prove another worker's committed write is
visible, so PostgreSQL remains part of every decision. No latency improvement is
claimed without measurement. Redis outages never discard or reset the authoritative
PostgreSQL counter.

Roll back application composition before reversing migrations: reversal removes
producer triggers before dropping counter and policy tables. Integration tests
exercise restart, isolated cache deletion, concurrent reservations, flag-off
accumulation, resource release, and rollback of a failed producer transaction.
