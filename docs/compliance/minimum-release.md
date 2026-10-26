# Minimum privacy release controls

Status: implementation baseline, 15 October 2026. This is not a worldwide legal certification. The company is intended to be US registered, provide organizational SaaS worldwide, admit users aged 16+, and exclude health data. The applicability register must distinguish platform account data (Sanvi as controller) from tenant customer data (usually the tenant as controller and Sanvi as processor). Future commerce needs a separate release assessment.

## Deployment disclosures

Organization details are deployment configuration under `privacy.disclosures`. Defaults identify Sanvi as a draft and leave unknown facts empty. Both public marketing and storefront notices render this configuration. Do not invent a registered entity, office, representative, country, certification, or transfer agreement.

Example configuration below is intentionally **not publishable**. Replace each placeholder with verified information, configure actual hosting and support-access countries, and review the complete notice before setting `reviewed_for_publication = true`.

```toml
[privacy]
enforce_release_checks = true
retention_dry_run = false # only after a reviewed dry run and verified deletion exercise
erasure_ledger_dir = "/srv/sanvi-erasure-ledger"
erasure_ledger_shared_and_durable = true # an infrastructure assertion, not volume provisioning

[privacy.disclosures]
controller_name = "Sanvi (draft; legal entity not yet supplied)"
business_address = "Pending registered business address"
privacy_email = "privacy@example.com"
hosting_countries = []
transfer_safeguards = "Pending executed transfer arrangements and review"
reviewed_for_publication = false
targeted_advertising = false
sale_or_share = false
# eu_representative and uk_representative: identify appointed representatives,
# or record the reviewed reason an appointment is not required in the register.
```

Supply `SANVI__PRIVACY__KEK` through secret management as a base64-encoded 32-byte key. The production overlay enables release checks: missing keys, dry-run retention, incomplete disclosures, or an unacknowledged durable ledger prevent startup. Keep advertising disclosures aligned with actual enabled processing and subprocessor records. Changing notices requires a new version, approval diff, and appropriate subject communication.

## Contracts and operating evidence

Before live personal data, assign a privacy owner and incident deputy; execute tenant processing terms and vendor processing agreements; maintain processing records and a subprocessor list; document security, retention, deletion, and transfer assessments. Use the companion draft DPA as a completion checklist, not an executed agreement. Select and execute the applicable EU/UK transfer instruments and Japan arrangements, assess destination/access risks, and determine EU/UK representation and DPO requirements. Configuration alone does not do these things.

Record for each market: territorial and business applicability, controller/processor roles, age rules, categories and purposes, lawful bases, rights and exceptions, marketing/tracking rules, transfers, breach triggers/deadlines, local representative/contact duties, effective dates, reviewer, and next review. The global profile uses short internal deadlines and disables sale/sharing, targeted advertising, significant profiling and sensitive-data use. It is a safe operating restriction, not a substitute for country law. Add reviewed profiles and country/state mappings through the platform jurisdiction API. Do not infer statutory breach duties from comprehensive-privacy-law thresholds.

Keep promotional email disabled until sender identification, jurisdiction-specific permission, unsubscribe and suppression propagation are verified. Service/security notices must not carry unrelated promotion. Retain existing accessibility gates and review any covered consumer commerce service before activation.

## Rights, complaints, and restrictions

Monitor the published privacy mailbox and the platform request queue every business day. Confirm identity proportionately. Route tenant-controlled requests to the tenant while assisting under its instructions. Requests for unsupported rights, explanations, correction assistance and complaints can use `other`; rectification and `other` remain open until an operator records the actual response and case evidence. The public request status displays that response. Do not record fulfillment merely because a ticket was assigned. Use a reasoned refusal where applicable, including appeal/regulator information and deadlines.

For UK complaints, record receipt, acknowledge within 30 days, investigate without undue delay, record the decision and send the outcome through the verified channel. The general case workflow is the minimum manual mechanism; it is not a US appeal. The acknowledgement and final delivery require a monitored operational process. See [ICO complaint guidance](https://ico.org.uk/about-the-ico/media-centre/news-and-blogs/2026/06/new-data-protection-complaints-law-now-in-force/).

Verified restriction/objection requests persist a processing restriction that denies consentable purposes and cannot be bypassed by a later grant. The worker records downstream refusals, then parks the request on hold for manual assessment. Record fulfillment and evidence only after reviewing the remaining scope and performing the required actions; automatic suppression alone cannot close it. Review any processing outside those purposes, downstream processors and legal exceptions manually. Before lifting, verify the instruction or lawful basis, inform the subject, and record evidence in the operator release reason. Release does not restore old permission automatically; obtain any required new permission. Export/security/rights processing remains available for its appropriate purpose.

EU/UK response deadlines use calendar months, including month-end clipping. Extensions need a recorded reason before the original deadline and a notification; delivery must be monitored. Do not extend a missed deadline. Legacy calls to the extension endpoint without a reason are still parsed, but receive a conflict response and do not change the deadline; deploy the updated clients with their reason field before use. Breach obligations are conservative review tasks: immediate tasks represent qualitative deadlines, not automatic legal conclusions. Assess harm, exposed keys, local affected counts, local US reporting rules, and actual awareness/discovery timestamps. Japan gets initial and 30-day final tasks; document any applicable 60-day malicious-act rule rather than delaying the initial report. Tenant controller notices require immediate attention.

## Age and data-use limits

Registration requires the 16+ assertion through the Kratos identity schema. Migrate existing identities and complete a re-attestation exercise; test each configured social-login registration flow before launch. This is an assertion, not verified age assurance. Publish 16+ and prohibited-data terms for tenant users and hosted services; investigate known violations and erase inappropriate data under the applicable process. Do not permit ordinary consent to override mandatory minor protections. Review 16–17-year-olds separately per country; assess under-18 regimes and phased effective dates before enabling affected uses. Do not collect full birth dates or identity documents by default.

## Media rollout and erasure

Deploy the media service owner-list API before the backend provider. New uploads require ownership and persist the canonical subject/namespace/owner index before calling the service. Backfill existing assets with verified tenant and subject ownership; assets lacking reliable ownership must be quarantined for manual resolution before accepting erasure requests as complete. An empty test subject does not prove a populated media deletion.

Exports include metadata, original-download links and captions with seven-day expiry; deliver them promptly, within both the export and link lifetime. Erasure queues deletion and remains pending until list-by-owner returns no assets. Verify purge of originals, renditions, captions, temporary objects and provider copies; test a failed purge and retry. Operational errors cannot be reported by this media provider as successful retention exceptions.

Other providers can disclose retained records in participant reports (for example tenant account records or statutory financial records). Review every exception and communicate the scope and reason. The external notification service currently exposes no recipient search or erasure API: delivery payloads rely on its configured retention. Verify that retention and contract, handle necessary subject requests with its operator, and do not claim those payloads were synchronously erased or exported. Enabling a new external processor requires a populated-data rights drill and propagation evidence.

## Durable erasure and disaster recovery

Provision a shared durable POSIX filesystem outside the database rollback boundary, mounted at the same configured absolute path for every API and worker replica. Initialize it once with a `.sanvi-erasure-ledger-v1` marker file containing `v1`. Restrict directory access to service operators (0700 recommended), protect backups and availability, and test file sync/atomic hard-link semantics. Configuration does not provision this volume. Missing or uninitialized storage fails closed; never recreate an empty ledger during a restore to bypass a failure.

Each key destruction durably writes an encrypted subject tombstone before changing the database key. Readers consult durable revocation state and the database rather than trusting cached DEKs. Keep tombstones at least until every recoverable backup/provider copy has expired and the retention decision is documented. Treat the ledger as protected personal data. Back it up independently and monitor missing mounts, corruption, permissions, capacity and replay failures. KEK rotation must rewrap the tombstones as well as database/archive keys; retain the old key until every protected item has been migrated and recovery tested.

For full or isolated recovery:

1. Stop public traffic and downstream personal-data processing. Restore the database and object stores in isolation; attach the **current** independent ledger, not a snapshot-era copy.
2. Start the worker with the same KEK and ledger. Its startup queues verified erasure replay requests from tombstones not satisfied by the restored request ledger. Monitor all provider participants, holds, failures and pending object purges until complete.
3. Start the API. Its startup refuses traffic until replay is complete. Verify deleted subject rows/assets and perform the encrypted-export recovery drill before routing traffic.
4. Reconcile external processors and manually handled retained data; record evidence and only then resume other consumers and traffic.

Online tenant restore is refused when the configured ledger contains an erasure. Use isolated maintenance recovery and replay; there is no safe bypass flag. The deployment must enforce the traffic isolation above, including workers outside this application. Test volume loss, a warmed replica, restored database DEKs and restored media. A database-only crypto-shred cannot revoke plaintext already delivered to a third party.

## Release evidence still supplied by operations

Record actual entity/contact/countries, executed contracts and transfers, representation decisions, applicability reviews, deployed retention settings and job health, notification retention, legacy-media backfill, registration re-attestation, populated-data rights drills, backup/ledger restore drills, incident/complaint owner exercises, security and access reviews. Confirm production uses the reviewed configuration. These tasks cannot be truthfully completed by source edits alone.
