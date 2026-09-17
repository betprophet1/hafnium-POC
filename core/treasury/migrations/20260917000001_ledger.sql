-- The treasury's records (core/treasury/README.md, the ledger; core/store/decisions.md, S-6 onwards): the book, the
-- double-entry ledger, transfers and their history, the fake bank's own book, and reconciliation. Times are UTC.
-- Amounts are integer cents in bigint. Everything but a transfer's current state is append-only.

-- Refuses an UPDATE, DELETE or TRUNCATE on a table of record: a correction is a further row, never an edit.
CREATE FUNCTION refuse_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% is append-only: % refused (a correction is a compensating entry, never an edit)',
    TG_TABLE_NAME, TG_OP USING ERRCODE = 'insufficient_privilege';
END
$$;

-- One row: the book's nonce, which prefixes every command ID it sends, so no two books meet on a sequencer group.
CREATE TABLE book (
  singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  nonce bigint NOT NULL CHECK (nonce BETWEEN 0 AND 4294967295),
  created_at timestamptz NOT NULL
);

-- The ledger's accounts: customer:{id}:cash, bank:fbo_omnibus, transit:deposits, transit:withdrawals.
CREATE TABLE ledger_accounts (
  account text PRIMARY KEY,
  kind text NOT NULL CHECK (kind IN ('customer', 'bank', 'transit')),
  -- The venue account a customer ledger account belongs to; null for the venue's own.
  account_id bigint CHECK ((kind = 'customer') = (account_id IS NOT NULL)),
  opened_at timestamptz NOT NULL DEFAULT now()
);

-- A transfer as it now stands: the one table of record whose rows change, and each change is also a row of
-- transfer_events. (account_id, client_transfer_id) is the client's idempotency key.
CREATE TABLE transfers (
  transfer_id bigint PRIMARY KEY CHECK (transfer_id > 0),
  account_id bigint NOT NULL,
  client_transfer_id text NOT NULL CHECK (length(client_transfer_id) BETWEEN 1 AND 32),
  kind text NOT NULL CHECK (kind IN ('deposit', 'withdrawal')),
  amount_cents bigint NOT NULL CHECK (amount_cents > 0),
  method text NOT NULL,
  state text NOT NULL CHECK (state IN ('pending', 'settled', 'failed', 'cancelled')),
  reason text,
  sequence bigint,
  -- Where the treasury has it, finer than state: reclaiming, to_bank, at_bank (until due_at), granting (then
  -- then_state with then_reason), done.
  stage text NOT NULL CHECK (stage IN ('reclaiming', 'to_bank', 'at_bank', 'granting', 'done')),
  due_at timestamptz CHECK ((stage = 'at_bank') = (due_at IS NOT NULL)),
  then_state text CHECK ((stage = 'granting') = (then_state IS NOT NULL)),
  then_reason text,
  -- A reclaim or grant sent without an answer, since when: reconciliation finds out what happened.
  in_doubt_since timestamptz,
  created_at timestamptz NOT NULL,
  updated_at timestamptz NOT NULL,
  version integer NOT NULL CHECK (version > 0),
  UNIQUE (account_id, client_transfer_id)
);

-- What a transfer is never changes; only where it stands does.
CREATE FUNCTION transfers_keep_terms() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (NEW.transfer_id, NEW.account_id, NEW.client_transfer_id, NEW.kind, NEW.amount_cents, NEW.method, NEW.created_at)
     IS DISTINCT FROM
     (OLD.transfer_id, OLD.account_id, OLD.client_transfer_id, OLD.kind, OLD.amount_cents, OLD.method, OLD.created_at)
  THEN
    RAISE EXCEPTION 'transfer %: its terms cannot change', OLD.transfer_id USING ERRCODE = 'check_violation';
  END IF;
  IF NEW.version <> OLD.version + 1 THEN
    RAISE EXCEPTION 'transfer %: version % follows %', OLD.transfer_id, NEW.version, OLD.version
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER transfers_keep_terms BEFORE UPDATE ON transfers
  FOR EACH ROW EXECUTE FUNCTION transfers_keep_terms();
CREATE TRIGGER transfers_never_deleted BEFORE DELETE OR TRUNCATE ON transfers
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

-- Every state a transfer has been in, in order.
CREATE TABLE transfer_events (
  transfer_id bigint NOT NULL REFERENCES transfers,
  version integer NOT NULL,
  at timestamptz NOT NULL,
  state text NOT NULL,
  reason text,
  sequence bigint,
  stage text NOT NULL,
  due_at timestamptz,
  then_state text,
  then_reason text,
  in_doubt_since timestamptz,
  PRIMARY KEY (transfer_id, version)
);

-- A posting: one balanced movement between ledger accounts, written once. Its idempotency key says which step of
-- which transfer (or which opening balance) it is, so a write repeated after a lost acknowledgement posts nothing twice.
CREATE TABLE postings (
  posting_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  idempotency_key text NOT NULL UNIQUE,
  transfer_id bigint REFERENCES transfers,
  memo text NOT NULL,
  posted_at timestamptz NOT NULL,
  recorded_at timestamptz NOT NULL DEFAULT now()
);

-- A posting's lines: debits positive, credits negative, integer cents.
CREATE TABLE entries (
  posting_id bigint NOT NULL REFERENCES postings,
  line smallint NOT NULL CHECK (line >= 1),
  account text NOT NULL REFERENCES ledger_accounts,
  cents bigint NOT NULL CHECK (cents <> 0),
  PRIMARY KEY (posting_id, line)
);
CREATE INDEX entries_by_account ON entries (account);

-- Double entry, checked when the transaction commits: every posting written in it has at least two lines, and its
-- lines sum to zero. Deferred, so a posting and its lines can be inserted in any order within the transaction.
CREATE FUNCTION posting_balances() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  total numeric;
  lines integer;
BEGIN
  SELECT coalesce(sum(cents), 0), count(*) INTO total, lines FROM entries WHERE posting_id = NEW.posting_id;
  IF lines < 2 OR total <> 0 THEN
    RAISE EXCEPTION 'posting % does not balance: % lines summing to % cents', NEW.posting_id, lines, total
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NULL;
END
$$;
CREATE CONSTRAINT TRIGGER postings_balance AFTER INSERT ON postings
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION posting_balances();
CREATE CONSTRAINT TRIGGER entries_balance AFTER INSERT ON entries
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION posting_balances();

-- The fake bank's own book (core/treasury/README.md, the fake bank): it stands for another company's system, so it
-- shares no keys with the treasury's tables.
CREATE TABLE bank_book (
  singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  opening_cents bigint NOT NULL,
  opened_at timestamptz NOT NULL,
  -- The treasury book (its nonce) this bank book belongs to.
  treasury_book bigint NOT NULL
);

CREATE TABLE bank_receipts (
  transfer_id bigint PRIMARY KEY,
  account_id bigint NOT NULL,
  kind text NOT NULL CHECK (kind IN ('deposit', 'withdrawal')),
  amount_cents bigint NOT NULL CHECK (amount_cents > 0),
  received_at timestamptz NOT NULL,
  decided_at timestamptz NOT NULL,
  settles boolean NOT NULL,
  -- When each copy of its notice reaches the treasury, drawn on receipt; empty if lost.
  arrivals timestamptz[] NOT NULL
);

CREATE TABLE bank_cancellations (
  transfer_id bigint PRIMARY KEY REFERENCES bank_receipts,
  at timestamptz NOT NULL
);

-- Each copy of a notice the bank has sent.
CREATE TABLE bank_notices (
  transfer_id bigint NOT NULL REFERENCES bank_receipts,
  copy smallint NOT NULL CHECK (copy >= 0),
  sent_at timestamptz NOT NULL,
  PRIMARY KEY (transfer_id, copy)
);

-- Reconciliation (core/treasury/README.md, reconciliation): each run's report, each break as found, and each break's
-- resolution.
CREATE TABLE recon_runs (
  run_id bigint PRIMARY KEY,
  at timestamptz NOT NULL,
  status text NOT NULL CHECK (status IN ('clean', 'resolved', 'breaks', 'breach')),
  report jsonb NOT NULL
);

CREATE TABLE recon_breaks (
  break_id bigint PRIMARY KEY,
  kind text NOT NULL,
  transfer_id bigint,
  account_id bigint,
  detail text NOT NULL,
  found_at timestamptz NOT NULL,
  found_by_run bigint NOT NULL
);

CREATE TABLE recon_resolutions (
  break_id bigint PRIMARY KEY REFERENCES recon_breaks,
  at timestamptz NOT NULL,
  resolved_by text NOT NULL CHECK (resolved_by IN ('reconciliation', 'operator')),
  note text NOT NULL
);

-- Append-only, for every statement that would change a table of record, whether or not it would touch a row.
CREATE TRIGGER book_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON book
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER ledger_accounts_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON ledger_accounts
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER transfer_events_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON transfer_events
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER postings_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON postings
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER entries_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON entries
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER bank_book_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON bank_book
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER bank_receipts_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON bank_receipts
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER bank_cancellations_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON bank_cancellations
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER bank_notices_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON bank_notices
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER recon_runs_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON recon_runs
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER recon_breaks_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON recon_breaks
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER recon_resolutions_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON recon_resolutions
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

-- No other role may edit a record either. The owner keeps its privileges, because PostgreSQL's foreign key checks run
-- as the owner and need them; the triggers above are what hold the owner to append-only.
REVOKE UPDATE, DELETE, TRUNCATE ON book, ledger_accounts, transfers, transfer_events, postings, entries, bank_book,
  bank_receipts, bank_cancellations, bank_notices, recon_runs, recon_breaks, recon_resolutions FROM PUBLIC;

-- The trial balance of record, and one row saying whether the ledger balances, for operators and for the invariant.
CREATE VIEW trial_balance AS
  SELECT account, sum(cents)::bigint AS balance_cents, count(*) AS entries FROM entries GROUP BY account;
CREATE VIEW ledger_check AS
  SELECT coalesce(sum(cents), 0) AS total_cents, (SELECT count(*) FROM postings) AS postings,
         coalesce(sum(cents), 0) = 0 AS balanced
  FROM entries;
