# API Reference

Base URL (docker compose): `http://localhost:8080`. All bodies are JSON. All money is integer
**cents** (USD); decimals in money fields are rejected. IDs are UUIDs (v7). Timestamps are RFC 3339 UTC.

## Authentication

Every endpoint except `/health` requires an API key:

```
Authorization: Bearer sk_...
```

The docker compose stack seeds a demo key: `sk_demo_local_only`. Create more with
`docker compose exec api invoice-service create-business "Acme"` (the key is printed once).
Missing, unknown or revoked key → `401 unauthorized`. Resources of another business → `404`.

## Errors

Every error has the same shape:

```json
{ "error": { "code": "invalid_state_transition", "message": "cannot move invoice from paid (a terminal state) to void" } }
```

`code` is stable and machine-readable; `message` is for humans and may change.

| HTTP | code | When |
|---|---|---|
| 400 | `validation_error` | Bad JSON, missing field, wrong type (e.g. `19.99` for cents), bad UUID, invalid value |
| 400 | `idempotency_key_missing` | `POST /pay` without an `Idempotency-Key` header (1–255 chars) |
| 401 | `unauthorized` | Missing, invalid or revoked API key |
| 402 | `card_declined`, `insufficient_funds`, `processing_error`, … | Payment declined (body also contains `payment_attempt`, see below) |
| 404 | `not_found` | Resource doesn't exist or belongs to another business; unknown route |
| 409 | `invalid_state_transition` | Transition not allowed from the invoice's current state |
| 409 | `invoice_not_payable` | `/pay` on an invoice that isn't `open` or `uncollectible` |
| 409 | `payment_in_progress` | Another attempt for this invoice is still `pending` (`/pay` or `/void`) |
| 422 | `idempotency_key_reused` | Idempotency key already used with a different request |
| 500 | `internal_error` | Unexpected server error (details are logged, never returned) |

## Pagination

List endpoints take `limit` (1–100, default 20) and `starting_after` (an ID from the previous page),
and return newest first:

```json
{ "data": [ ... ], "has_more": true }
```

---

## Customers

### `POST /customers`
```json
{ "name": "Jane Doe", "email": "jane@example.com" }
```
`201`:
```json
{ "id": "01a13625-40c4-7521-bf45-e3cacab6b026", "name": "Jane Doe", "email": "jane@example.com", "created_at": "2026-10-13T16:42:53.508250Z" }
```

### `GET /customers/{id}` → `200` customer
### `GET /customers?limit=&starting_after=` → `200` list of customers

---

## Invoices

States: `draft` → `open` → `paid` / `void` / `uncollectible`. See DESIGN.md §2 for the state machine.

### `POST /invoices`
```json
{
  "customer_id": "01a13625-40c4-7521-bf45-e3cacab6b026",
  "due_date": "2026-12-31",
  "line_items": [
    { "description": "Pro plan", "quantity": 3, "unit_amount_cents": 1999 },
    { "description": "Setup fee", "quantity": 1, "unit_amount_cents": 500 }
  ]
}
```
- The server computes every `amount_cents` and the `total_cents`. A client-sent total is ignored.
- 1–100 line items; `quantity > 0`; `unit_amount_cents >= 0`; total must be `> 0`; `due_date` not in the past.
- Creates the invoice in `draft` and emits `invoice.created`.

`201`:
```json
{
  "id": "01a13625-4134-795a-ad23-ca34edf4d0ea",
  "customer_id": "01a13625-40c4-7521-bf45-e3cacab6b026",
  "state": "draft",
  "currency": "USD",
  "total_cents": 6497,
  "due_date": "2026-12-31",
  "line_items": [
    { "description": "Pro plan", "quantity": 3, "unit_amount_cents": 1999, "amount_cents": 5997 },
    { "description": "Setup fee", "quantity": 1, "unit_amount_cents": 500, "amount_cents": 500 }
  ],
  "created_at": "2026-10-13T16:42:53.619927Z",
  "updated_at": "2026-10-13T16:42:53.619927Z",
  "paid_at": null
}
```

### `GET /invoices/{id}` → `200` invoice
### `GET /invoices?state=open&limit=&starting_after=` → `200` list of invoices
`state` is optional; one of `draft`, `open`, `paid`, `void`, `uncollectible`.

### State transitions: `POST /invoices/{id}/finalize` · `/void` · `/mark-uncollectible`
No body. `200` with the updated invoice, or `409 invalid_state_transition`.
`/void` is also rejected with `409 payment_in_progress` while a payment attempt is pending.

| Endpoint | Allowed from |
|---|---|
| `finalize` | `draft` |
| `void` | `draft`, `open`, `uncollectible` |
| `mark-uncollectible` | `open` |

---

## Payments

### `POST /invoices/{id}/pay`
Headers: `Idempotency-Key: <1–255 chars>` (**required**)
```json
{ "card_token": "tok_success" }
```
Mock tokens: `tok_success`, `tok_insufficient_funds`, `tok_card_declined`, `tok_timeout`, `tok_network_error`.

| Outcome | Status | Body |
|---|---|---|
| Charged | `200` | payment attempt, `status: "succeeded"`; invoice is now `paid` |
| Declined | `402` | `{"error": {...}, "payment_attempt": {...}}`; invoice unchanged |
| Outcome unknown (PSP timeout / error) | `202` | payment attempt, `status: "pending"`; poll it or wait for a webhook |

`200`:
```json
{
  "id": "01a13625-420b-716f-a85c-1be933edf4c4",
  "invoice_id": "01a13625-4134-795a-ad23-ca34edf4d0ea",
  "amount_cents": 6497,
  "status": "succeeded",
  "failure_code": null,
  "psp_ref": "5166bc45-fab3-4c6a-9ba2-d4b9bfc25919",
  "created_at": "2026-10-13T16:42:53.833104Z",
  "updated_at": "2026-10-13T16:42:53.946210Z"
}
```
`402`:
```json
{
  "error": { "code": "card_declined", "message": "the payment was declined" },
  "payment_attempt": {
    "id": "01a13625-417a-7ba4-86ff-be7d6a1dd5c5",
    "invoice_id": "01a13625-4134-795a-ad23-ca34edf4d0ea",
    "amount_cents": 6497,
    "status": "failed",
    "failure_code": "card_declined",
    "psp_ref": null,
    "created_at": "2026-10-13T16:42:53.690510Z",
    "updated_at": "2026-10-13T16:42:53.805129Z"
  }
}
```

**Idempotency:** retrying with the same key and the same body never charges again; it returns the
attempt's **current** state (so a retry after a `202` may return `200`). The same key with a
different body or invoice → `422 idempotency_key_reused`. Keys don't expire.

### `GET /payment_attempts/{id}` → `200` payment attempt (any status)

---

## Webhook endpoints

### `POST /webhook_endpoints`
```json
{ "url": "https://example.com/hooks" }
```
`201`. The `secret` is returned **only here**; store it:
```json
{ "id": "01a13625-42f8-7698-aabe-7739ea9eba19", "url": "https://example.com/hooks", "created_at": "2026-10-13T16:42:54.072252Z", "secret": "whsec_416d…" }
```

### `GET /webhook_endpoints` → `200` array of endpoints (without secrets)

### Deliveries

`POST <your url>` with:
```
Content-Type: application/json
Dodo-Signature: t=1760373774,v1=5f2c…   (hex HMAC-SHA256(secret, "<t>.<raw body>"))
```
```json
{
  "id": "01a13625-4248-7c1e-9d0f-3b1d2c9a1e77",
  "type": "invoice.paid",
  "created_at": "2026-10-13T16:42:53.946210Z",
  "data": { "invoice": { ... }, "payment_attempt": { ... } }
}
```
- Types: `invoice.created` (`data.invoice`), `invoice.paid` and `invoice.payment_failed`
  (`data.invoice` + `data.payment_attempt`).
- Verify: recompute the HMAC over the **raw** body, compare in constant time, reject if
  `|now − t| > 300s`. See `webhook-receiver/src/main.rs` for a working example.
- Respond with any `2xx` within 10s. Otherwise we retry after 30s, 2m, 10m, 30m, 1h, 3h, 6h
  (8 attempts, 10h42m30s in total), then mark the delivery failed.
- At-least-once and unordered: deduplicate on `id`; rely on `data.invoice.state`, not arrival order.

---

## Health

### `GET /health` → `200 ok` (no auth)
