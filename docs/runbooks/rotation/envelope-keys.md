# Envelope KEK and DEK rotation

Owner: platform engineering. Rehearse: `just rotate-credentials --rehearse`.

Privacy and advertising use separate AES-256-GCM KEKs wrapping per-subject and
per-tenant DEKs. The local drill rewraps and persists/reloads a DEK, verifies
replacement-key access and retired-key rejection using production crypto.
The advertising unit proof also asserts the plaintext DEK is unchanged.
This is a local primitive/persistence rehearsal, not a deployment-wide migration.

1. Record start time; retain an encrypted recovery backup with its KEK separately
   secured. A prior backup still needs its historical KEK; destroying it makes
   that backup unrecoverable. Restrict retired KEKs to offline recovery.
2. Quiesce API/worker writers. Deploying a new KEK before rewrapping all rows
   breaks reads: current adapters carry one KEK and cache unwrapped DEKs.
3. Generate a 32-byte replacement KEK and unique KEK id for each context.
   In a transaction, lock the live key rows, unwrap with the old KEK, wrap the
   **same DEK** with the replacement, and update wrapped bytes + KEK id together.
   Privacy: `privacy.subject_keys`, exclude destroyed keys. Advertising:
   `advertising.tenant_secret_keys`. Never resurrect a destroyed subject key.
4. Verify each live DEK using a fresh replacement crypto instance. Verify
   representative existing DSR/export and ad-token ciphertext still decrypts.
   Verify a fresh old-key instance cannot unwrap the migrated rows. Do not use
   cached DEKs for this proof. Roll back on any failure before publishing config.
5. Switch the KEK/id environment settings and restart **all** API/worker
   instances to clear DEK caches. Resume traffic only after smoke tests pass.
   Record finish time including the quiescence and deployment window.

DEKs are preserved during KEK rotation. Subject erasure destroys its DEK row;
advertising DEKs belong to tenants and must not be shredded by subject erasure.
Rotating a DEK itself requires re-encrypting all its ciphertext atomically and
is separate from this rehearsal. Export passphrases are per-artifact secrets.
