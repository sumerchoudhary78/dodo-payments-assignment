# Invoice & Payment Service

A small invoicing backend in Rust: businesses create customers and invoices, customers pay invoices through a payment processor (PSP), and businesses get signed webhooks about what happened. The interesting parts are the invoice state machine, payment correctness under concurrency and PSP failures, and webhook delivery. Those are covered in [**DESIGN.md**](DESIGN.md).

| Doc | What's in it |
| --- | --- |
| [DESIGN.md](DESIGN.md) | Data model, state machine, failure modes, webhooks, API keys, what was cut |
| [API.md](API.md) | Every endpoint, request/response shapes, the error format |
| [AI_USAGE.md](AI_USAGE.md) | How AI tools were used, and where I decided differently |

## Demo Video

**▶ [Watch the walkthrough](https://drive.google.com/file/d/1Se9npS5JeLkolPM5czHpSg9_83OTts_g/view?usp=sharing)**

## Run it

Requires Docker (with Compose v2). Nothing else.

```bash
docker compose up --build
```

This starts four containers:

| Service | Port | What it is |
| --- | --- | --- |
| `api` | 8080 | The invoice service (runs migrations on start) |
| `db` | 5433 | Postgres 18 (host port 5433, so it doesn't clash with a local Postgres) |
| `mock-psp` | 8081 | Mock payment processor; outcome decided by the card token |
| `webhook-receiver` | — | Demo endpoint that verifies webhook signatures and logs each event |

On first start the API seeds a **demo business** with API key `sk_demo_local_only` and registers the webhook receiver as its endpoint, so the examples below work with no setup. To start over with an empty database: `docker compose down -v && docker compose up --build`.

Watch webhooks arrive in a second terminal:

```bash
docker compose logs -f webhook-receiver
```

## Try it (curl)

The examples use `jq` to capture IDs. Set the key once:

```bash
KEY="Authorization: Bearer sk_demo_local_only"
```

**1. Create a customer**

```bash
CUSTOMER=$(curl -s -X POST localhost:8080/customers -H "$KEY" -H 'Content-Type: application/json' \
  -d '{"name": "Jane Doe", "email": "jane@example.com"}' | jq -r .id)
```

**2. Create an invoice and finalize it.** The server computes the total (3 × 1999 + 500 = 6497):

```bash
INVOICE=$(curl -s -X POST localhost:8080/invoices -H "$KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\": \"$CUSTOMER\", \"due_date\": \"2026-12-31\", \"line_items\": [
        {\"description\": \"Pro plan\",  \"quantity\": 3, \"unit_amount_cents\": 1999},
        {\"description\": \"Setup fee\", \"quantity\": 1, \"unit_amount_cents\": 500}]}" | jq -r .id)

curl -s -X POST localhost:8080/invoices/$INVOICE/finalize -H "$KEY" | jq '{state, total_cents}'
# { "state": "open", "total_cents": 6497 }
```

**3. A failing payment** (`tok_card_declined` → `402`, invoice stays `open`):

```bash
curl -s -X POST localhost:8080/invoices/$INVOICE/pay -H "$KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: decline-$INVOICE" -d '{"card_token": "tok_card_declined"}' | jq
# { "error": { "code": "card_declined", ... }, "payment_attempt": { "status": "failed", ... } }
```

**4. A successful payment** (`tok_success` → `200`, invoice becomes `paid`):

```bash
curl -s -X POST localhost:8080/invoices/$INVOICE/pay -H "$KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: pay-$INVOICE" -d '{"card_token": "tok_success"}' | jq '{status, psp_ref}'

curl -s localhost:8080/invoices/$INVOICE -H "$KEY" | jq .state
# "paid"
```

Run step 4 again (same `Idempotency-Key`) and you get the same attempt back, with no second charge. Change the key and you get `409 invoice_not_payable`. Keys never expire, which is why the examples derive them from the invoice ID.

**5. A PSP timeout** (`tok_timeout`: the mock takes 30s; the API answers `202` after 5s):

```bash
INVOICE2=$(curl -s -X POST localhost:8080/invoices -H "$KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\": \"$CUSTOMER\", \"due_date\": \"2026-12-31\", \"line_items\": [
        {\"description\": \"Plan\", \"quantity\": 1, \"unit_amount_cents\": 1000}]}" | jq -r .id)
curl -s -o /dev/null -X POST localhost:8080/invoices/$INVOICE2/finalize -H "$KEY"

ATTEMPT=$(curl -s -X POST localhost:8080/invoices/$INVOICE2/pay -H "$KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: pay-$INVOICE2" -d '{"card_token": "tok_timeout"}' | jq -r .id)   # 202, status "pending"

# ~40s later the reconciler has asked the PSP and recorded the result:
curl -s localhost:8080/payment_attempts/$ATTEMPT -H "$KEY" | jq .status   # "succeeded"
```

Other tokens: `tok_insufficient_funds` (402), `tok_network_error` (202; resolves to failed).

## Tests

```bash
docker compose up -d db                  # the tests need a Postgres server
cargo test                               # 20 tests, about 5 seconds
```

The tests start the real service and the real mock PSP in-process, each test with its own freshly migrated database. Set `TEST_DATABASE_URL` to use a different Postgres server.

| Required test | File |
| --- | --- |
| Concurrency: 20 parallel `/pay` → exactly one success, one PSP charge | `invoice-service/tests/concurrency.rs` |
| Idempotency: same key → same response, no second PSP call | `invoice-service/tests/idempotency.rs` |
| PSP failure: `tok_timeout` / `tok_network_error` don't leave the invoice stuck | `invoice-service/tests/psp_failure.rs` |

`psp_failure.rs` also covers failure mode (c): the PSP charged the card, but the service crashed before recording it. No required test was skipped.

## Repository layout

```
invoice-service/   the API (lib + binary); tests/ holds the integration tests
  src/domain/      state machine and money math (no DB, no HTTP)
  src/payments/    /pay flow, idempotency, PSP client
  src/webhooks/    endpoint registration, outbox, signing
  src/workers/     reconciler, webhook sender
mock-psp/          mock payment processor (separate binary)
webhook-receiver/  demo webhook endpoint that verifies signatures
migrations/        SQL schema (applied automatically on startup)
```

Create another business and API key (the key is printed once):

```bash
docker compose exec api invoice-service create-business "Acme Inc"
```

## Time spent

About **16 hours** in total over 2 days. This is more than the suggested 4–6 hours, partly because I used the time to make sure I understood every decision. The remaining work I'd do is listed in DESIGN.md §6–7.