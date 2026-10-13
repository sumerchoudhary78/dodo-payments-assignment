# Engineering Decision Log

Working notes for the invoice service and mock PSP. These notes capture the main decisions, trade-offs, and issues found while implementing and testing the project.

## D1 — Invoice states

The invoice states are `draft`, `open`, `paid`, `void`, and `uncollectible`.

- A draft must be finalized before it can be paid.
- A failed payment leaves the invoice `open`, so the customer can retry.
- A payment that is still being processed belongs to the payment attempt, not the invoice.
- An `uncollectible` invoice can still become `paid` if a late payment succeeds.
- `paid` and `void` are terminal states.

A pending attempt must block another payment attempt for the same invoice. The database enforces this with a partial unique index on `invoice_id` for attempts with `status = 'pending'`. A second payment request returns `409 payment_in_progress`.

Voiding is rejected while a payment is pending. Overdue status is derived from an open invoice's due date rather than stored as another state.

State changes are checked in Rust and guarded in SQL so concurrent requests cannot quietly apply an invalid transition.

## D2 — Payment flow and PSP errors

Payment processing uses two database transactions with the PSP request between them:

1. Lock the invoice, validate it, create a pending attempt, and commit.
2. Call the PSP without holding a database transaction open.
3. Record the result, update the invoice if needed, and add any webhook event in a second transaction.

The PSP timeout is 5 seconds. Holding a database connection and row lock during a slow network request would tie up the pool.

A definite decline returns `402 Payment Required` and leaves the invoice open. If the request times out or the connection drops, the outcome is unknown, so the API returns `202 Accepted` with the attempt still pending. The client can check the attempt status rather than assuming the payment failed.

The mock PSP supports idempotency and charge-status lookup. A reconciler checks older pending attempts and records the result. The mock stores a charge as `processing` before responding, so a `not_found` result can be treated as a request the PSP never received. Its in-memory storage is only suitable for this mock; restarting it loses its records.

## D3 — Idempotency keys

`POST /invoices/{id}/pay` requires an `Idempotency-Key` header. Missing keys return `400 idempotency_key_missing`.

The key is stored with a hash of the parsed request body and URL path. Reusing a key with a different request returns `422 idempotency_key_reused`. The path is included so a key cannot be silently reused for another invoice.

The key row and payment attempt are created in the same transaction, before contacting the PSP. A retry can therefore find the original attempt even if the service crashes during payment processing. Replays return the attempt's current status, not a stale copy of the first response.

Keys do not expire. Expiring them could allow a delayed retry to create another charge. If the table grows significantly, old records can be archived or partitioned.

Requests rejected before an attempt is created, such as trying to pay an already-paid invoice, do not store an idempotency key.

## D4 — Webhooks

Webhook events use a transactional outbox: the event is written in the same transaction as the payment state change, and a background worker delivers it later. This avoids losing an event if the service crashes after committing the payment but before sending the webhook.

Requests are signed with HMAC-SHA256 over the timestamp and raw body. Receivers should reject timestamps more than five minutes away from the current time to reduce replay risk.

The worker retries failed deliveries on a fixed schedule, with a 10-second timeout per request. Any 2xx response counts as delivered. After the configured attempts are exhausted, the event is marked failed. Webhooks are notifications; the database remains the source of truth.

Delivery is at-least-once, so consumers should deduplicate using the event ID. Delivery order is not guaranteed. Endpoint secrets are stored for signing; encrypting them at rest would be a production improvement.

## D5 — API keys

API keys use the `sk_` prefix followed by 32 random bytes. The prefix is stored separately so a key can be identified without storing the full secret.

Only the SHA-256 hash is stored. These keys have high entropy, so a slow password hash is unnecessary and would make lookup on every request more expensive. Requests send the key in `Authorization: Bearer ...` over HTTPS.

A CLI command creates a business and prints its key once. For local development, Docker Compose can seed a demo business if `SEED_API_KEY` is set. Key creation and revocation are CLI-only for this project, so a leaked key cannot create or revoke other keys. Multiple keys can be active during rotation; the old one can be revoked after the new one is deployed.

Revocation is tracked with `revoked_at` and checked on each request. Each key is scoped to one business. A leaked key can access that business's API capabilities, but it cannot access other businesses or initiate refunds or payouts.

## D6 — Database schema

- UUID v7 is used for primary keys. Tenant scoping is still enforced by `business_id`; IDs are not treated as the security boundary.
- Invoice line items live in a separate table so the database can enforce quantity and amount constraints.
- Composite foreign keys include `business_id` to prevent cross-business references.
- Partial unique indexes prevent multiple pending attempts and multiple successful payments for one invoice.
- Text columns with `CHECK` constraints are used for state values.

Database tests check invalid line amounts, overflow, cross-tenant references, duplicate pending attempts, unknown states, and inconsistent failed attempts.

## D7 — Project layout and error handling

The code is split into two workspace crates: `invoice-service` and `mock-psp`. The mock PSP does not share the service's database dependencies, which keeps the external-service boundary clear.

The service uses runtime `sqlx::query_as` calls so a Docker build does not need a live database or a prepared `.sqlx` cache. SQL behavior is covered by integration tests.

Code is grouped by feature, with domain logic kept separate. SQL stays close to the handler that uses it rather than adding repository traits and extra service layers for this small project. The mock PSP is kept to a few files rather than split into many tiny modules.

The first implementation (written by Claude Code) had a problem with Axum's default extractors: malformed JSON and invalid path values did not use the API's error format. It was caught by deliberately sending malformed input during testing, and fixed with wrapper extractors and a JSON 404 fallback so these errors use the same envelope.

## D8 — Serializing invoice changes

Invoice state changes lock the invoice row first, check the transition, and then write within one transaction. Payment creation follows the same lock order.

A conditional update alone was not enough for the pending-payment check. Under `READ COMMITTED`, a statement can use a snapshot from before it waited for a lock, so it could miss an attempt committed while it was waiting. Locking the invoice first and then checking for pending attempts avoids that race. The update also includes the expected current state as a second guard.

Other input checks verified during implementation:
- Client-supplied `total_cents` is ignored.
- Decimal values for integer money fields are rejected.
- A due date in the past is rejected at invoice creation.
- Finalize, void, and mark-uncollectible do not emit webhooks in this scope.

## D9 — Payment implementation details

Testing led to a few changes in the payment flow:

- The reconciler does not hold a row lock while calling the PSP. It reads stale pending IDs, checks the PSP, and then records the result using the normal invoice-first lock order.
- The reconciliation delay must be at least three times the PSP timeout. Otherwise, the reconciler could check too early, mark an in-flight request failed, and then have the original charge succeed.
- Idempotency is checked after locking the invoice but before checking whether it is payable. This lets a retry of a successful payment return the existing success instead of a `409`.
- Database constraint errors are mapped to API errors, including `409 payment_in_progress` and `422 idempotency_key_reused`.
- Request hashes use the parsed body and path, so irrelevant JSON whitespace does not change the key's meaning.
- Webhook payloads include the invoice, and include the payment attempt for payment events.

## AI tool usage

I owned the key design decisions and overall implementation, using Claude Code (Opus) to explore trade-offs and accelerate development. I evaluated alternatives, made the final architectural choices, refined suggestions where needed, and reviewed the code and tests to ensure the solution met the requirements and behaved correctly.

