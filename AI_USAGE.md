# AI Usage

## Tools and what they did

**Claude Code (Opus 5.5)** was my main tool, used throughout.

- **Design:** I worked through each decision with it before any code was written. For every
  decision I made the final call. Often I answered first and it stress-tested my answer; sometimes it
  laid out options and I chose. My decision notes are in `decisions.md`.
- **Code:** Claude Code wrote nearly all of the Rust code, the SQL migration, the tests, the Dockerfile
  and the compose file. I set the direction (for example, I asked for the code to be split into
  feature modules instead of large files) and reviewed each module before moving on. That included
  working through review questions such as why the idempotency check runs before the payable check,
  and why the unique index, not the row lock, is what rejects a concurrent payment.
- **Verification:** I ran smoke tests against real Postgres and the mock PSP after every
  step, and mutation-checked the required tests (it removed each safety mechanism and confirmed the
  matching test failed). I reviewed the results.
- **Docs:** Claude Code wrote API.md and the README, and wrote DESIGN.md's text from my decision
  notes. The decisions and reasoning in DESIGN.md are the ones I made and recorded during the design
  phase; I reviewed the final text.

**ChatGPT** helped me phrase some of my technical answers more clearly (idempotency, database schema).

## Three decisions I made myself

**1. Idempotency keys never expire (against the AI's suggestion).**
Claude Code proposed a 24-hour expiry, as Stripe does. I chose no expiry: a retry at hour 25 that
creates a second charge is exactly the bug the key exists to prevent. The rows are tiny, and no expiry
means no cleanup job. If the table grows, it can be partitioned by `created_at` and archived.

**2. The invoice state model (decided before the AI's input).**
I decided to keep `draft`, to leave a failed payment's invoice `open` so the customer can retry, to
track an in-flight payment on the payment attempt rather than as an invoice state, and to let an
`uncollectible` invoice still become `paid`. Claude Code then pointed out that tracking in-flight state
only on the attempt would let a second `/pay` start during a pending one. I kept my model and closed
the gap in the database, with a partial unique index allowing one pending attempt per invoice.

**3. API key management is CLI-only.**
My starting point was a CLI to create businesses and keys. Claude Code raised the conflict with
"`docker compose up` with no manual steps" and asked whether keys should also be manageable over HTTP.
I chose to keep the CLI, seed a demo key from an environment variable for local runs, and expose
**no** key endpoints over HTTP, so a leaked key can't create new keys or revoke the owner's. The
trade-off is that rotation needs an operator rather than self-service, which I consider acceptable at
this scope.

Other decisions where Claude Code's suggestion matched my own view, and I chose it for that reason:
separate crates for the service and the mock PSP; runtime SQL queries to keep the Docker build simple;
UUID v7 keys and a separate line-items table; `402` for declines; `422` for a reused key (I first argued
for 409, and switched after reading the IETF Idempotency-Key draft).

## What the AI got wrong

The clearest case: **Claude Code's first version returned plain-text errors for malformed input.**
It used Axum's default `Json`/`Path`/`Query` extractors, so broken JSON or a bad UUID came back as
plain text, and a missing field returned 422 while everything else returned 400. That broke the
"consistent error format" requirement. Sending malformed input on purpose exposed it, and I
fixed it with wrapper extractors (`invoice-service/src/extract.rs`).

Other mistakes in Claude Code's work, all caught by the testing done at every step:
- **A race in its first design for void:** a single conditional `UPDATE … AND NOT EXISTS (pending
  attempt)` can miss an attempt committed while it waits for the row lock (READ COMMITTED snapshot).
  Replaced with lock-then-check.
- **Reconciler timing:** a reconcile threshold shorter than the PSP timeout could mark an in-flight
  charge failed and allow a double charge. Now enforced at startup (`RECONCILE_AFTER_MS ≥ 3 × PSP_TIMEOUT_MS`).
- **`GET /invoices?limit=1` returned 400** (`serde(flatten)` with query strings). Found while
  writing API.md; fixed, with a regression test that fails without the fix.
- **Arithmetic:** it stated the webhook retry budget as about 10h43m; a unit test showed 10h42m30s.
- **Reconciler locking:** its original plan held row locks while calling the PSP, which contradicts
  the design rule of never holding locks across network calls. Replaced with an unlocked scan and an
  update that only applies while the attempt is still `pending`.

What I took from that: AI-written payment code isn't trustworthy until it has been attacked with
concurrent requests, retries, bad input and failure cases.

## How correctness was verified

- 20 automated tests (12 unit, 8 integration), including the three required ones. Each safety
  mechanism was removed in turn to confirm that a test fails without it.
- Every PSP token was run against the full `docker compose up` stack, checking database state and
  signed webhook deliveries.
- I reviewed each module before the next one was built, and worked through every failure mode in
  DESIGN.md §3 against the code.
