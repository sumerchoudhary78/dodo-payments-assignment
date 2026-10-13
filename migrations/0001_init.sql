-- Invoice & Payment Service — initial schema.
--
-- Conventions:
--   * Primary keys are UUID v7 (Postgres 18 built-in): unguessable, but time-ordered so
--     inserts append to the end of the B-tree like a serial would.
--   * Money is BIGINT minor units (cents). No NUMERIC/REAL/DOUBLE anywhere.
--   * Every tenant-owned row carries business_id. Child rows use composite foreign keys
--     (id, business_id) so the database itself refuses cross-tenant references.
--   * States are TEXT + CHECK instead of Postgres ENUM types: adding a value to a CHECK is a
--     plain migration, while ALTER TYPE ... ADD VALUE has transaction restrictions.

-- ─── Tenants & auth ────────────────────────────────────────────────────────────────

CREATE TABLE businesses (
    id          UUID        PRIMARY KEY DEFAULT uuidv7(),
    name        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE api_keys (
    id           UUID        PRIMARY KEY DEFAULT uuidv7(),
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    key_prefix   TEXT        NOT NULL,          -- first chars, for humans to identify a key
    key_hash     BYTEA       NOT NULL UNIQUE,   -- SHA-256 of the full key; auth looks up by this
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at   TIMESTAMPTZ                    -- NULL = active
);
CREATE INDEX api_keys_business_idx ON api_keys (business_id);

-- ─── Customers ─────────────────────────────────────────────────────────────────────

CREATE TABLE customers (
    id           UUID        PRIMARY KEY DEFAULT uuidv7(),
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    name         TEXT        NOT NULL,
    email        TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (id, business_id)                    -- target for composite FKs below
);
-- List customers for a business, newest first (UUID v7 sorts by creation time).
CREATE INDEX customers_business_idx ON customers (business_id, id DESC);

-- ─── Invoices ──────────────────────────────────────────────────────────────────────

CREATE TABLE invoices (
    id           UUID        PRIMARY KEY DEFAULT uuidv7(),
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    customer_id  UUID        NOT NULL,
    state        TEXT        NOT NULL DEFAULT 'draft'
                 CHECK (state IN ('draft', 'open', 'paid', 'void', 'uncollectible')),
    currency     TEXT        NOT NULL DEFAULT 'USD' CHECK (currency = 'USD'),
    total_cents  BIGINT      NOT NULL CHECK (total_cents > 0),  -- computed by server, never client
    due_date     DATE        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    paid_at      TIMESTAMPTZ,
    UNIQUE (id, business_id),
    -- Customer must belong to the same business as the invoice.
    FOREIGN KEY (customer_id, business_id) REFERENCES customers (id, business_id)
);
-- List invoices for a business filtered by state, newest first.
CREATE INDEX invoices_business_state_idx ON invoices (business_id, state, id DESC);
CREATE INDEX invoices_customer_idx ON invoices (customer_id);

CREATE TABLE invoice_line_items (
    id                 UUID   PRIMARY KEY DEFAULT uuidv7(),
    invoice_id         UUID   NOT NULL REFERENCES invoices (id),
    position           INT    NOT NULL,          -- preserves the order the client sent
    description        TEXT   NOT NULL,
    quantity           BIGINT NOT NULL CHECK (quantity > 0),
    unit_amount_cents  BIGINT NOT NULL CHECK (unit_amount_cents >= 0),
    -- Postgres raises an error on BIGINT overflow, so this can never silently wrap.
    amount_cents       BIGINT NOT NULL CHECK (amount_cents = quantity * unit_amount_cents),
    UNIQUE (invoice_id, position)
);

-- ─── Payments ──────────────────────────────────────────────────────────────────────

CREATE TABLE payment_attempts (
    id              UUID        PRIMARY KEY DEFAULT uuidv7(),  -- also sent to PSP as its idempotency key
    business_id     UUID        NOT NULL,
    invoice_id      UUID        NOT NULL,
    amount_cents    BIGINT      NOT NULL CHECK (amount_cents > 0),
    payment_method  TEXT        NOT NULL,        -- mock card token; a real system stores a PSP token, never a card number
    status          TEXT        NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'succeeded', 'failed')),
    failure_code    TEXT,                        -- e.g. card_declined, insufficient_funds
    psp_ref         TEXT,                        -- PSP's charge id on success
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (invoice_id, business_id) REFERENCES invoices (id, business_id),
    CHECK ((status = 'failed') = (failure_code IS NOT NULL)),
    CHECK (status <> 'succeeded' OR psp_ref IS NOT NULL)
);
-- D1/D2: at most one in-flight attempt per invoice. A second concurrent /pay fails on this index.
CREATE UNIQUE INDEX payment_attempts_one_pending
    ON payment_attempts (invoice_id) WHERE status = 'pending';
-- Belt and braces: an invoice can never have two successful charges recorded.
CREATE UNIQUE INDEX payment_attempts_one_success
    ON payment_attempts (invoice_id) WHERE status = 'succeeded';
-- Reconciler scans old pending attempts.
CREATE INDEX payment_attempts_pending_age
    ON payment_attempts (created_at) WHERE status = 'pending';

-- D3: Idempotency-Key → payment attempt. Created in the same transaction as the attempt.
-- No expiry by design (see decisions.md D3).
CREATE TABLE idempotency_keys (
    business_id   UUID        NOT NULL REFERENCES businesses (id),
    key           TEXT        NOT NULL CHECK (length(key) BETWEEN 1 AND 255),
    request_hash  BYTEA       NOT NULL,          -- SHA-256 of method + path + body
    attempt_id    UUID        NOT NULL REFERENCES payment_attempts (id),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (business_id, key)
);

-- ─── Webhooks (transactional outbox) ───────────────────────────────────────────────

CREATE TABLE webhook_endpoints (
    id           UUID        PRIMARY KEY DEFAULT uuidv7(),
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    url          TEXT        NOT NULL,
    secret       TEXT        NOT NULL,           -- plaintext: HMAC needs it. Prod: encrypt via KMS.
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at  TIMESTAMPTZ
);
CREATE INDEX webhook_endpoints_business_idx ON webhook_endpoints (business_id);

-- One row per thing that happened. Inserted in the same transaction as the state change.
CREATE TABLE events (
    id           UUID        PRIMARY KEY DEFAULT uuidv7(),  -- receivers dedupe on this
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    type         TEXT        NOT NULL
                 CHECK (type IN ('invoice.created', 'invoice.paid', 'invoice.payment_failed')),
    payload      JSONB       NOT NULL,           -- snapshot sent to receivers
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per (event, endpoint). This is the work queue the delivery worker polls.
CREATE TABLE webhook_deliveries (
    id                UUID        PRIMARY KEY DEFAULT uuidv7(),
    event_id          UUID        NOT NULL REFERENCES events (id),
    endpoint_id       UUID        NOT NULL REFERENCES webhook_endpoints (id),
    status            TEXT        NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'delivered', 'failed')),
    attempt_count     INT         NOT NULL DEFAULT 0,
    next_attempt_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_status_code  INT,
    last_error        TEXT,
    delivered_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (event_id, endpoint_id)
);
-- Worker query: pending deliveries that are due, oldest first.
CREATE INDEX webhook_deliveries_due
    ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';
