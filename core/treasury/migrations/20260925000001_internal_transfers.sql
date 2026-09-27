-- Internal transfers between two accounts sharing a master (spec/engine/decisions.md, D-34; interfaces/gateway/
-- decisions.md, G-22): the gateway checks the two accounts share a master before this is ever asked for, so what
-- this needs is only what makes the money move — a destination account, and no bank leg, so no method.
--
-- The `kind` and `method` CHECK constraints are dropped by name, looked up rather than assumed, since a name this
-- migration guessed wrong would silently do nothing (a dropped constraint that never existed is not an error) or
-- fail to compile at all; either way it is safer to ask Postgres what it called them.
DO $$
DECLARE
  name text;
BEGIN
  SELECT conname INTO name FROM pg_constraint
    WHERE conrelid = 'transfers'::regclass AND contype = 'c' AND pg_get_constraintdef(oid) LIKE '%kind%deposit%';
  IF name IS NOT NULL THEN
    EXECUTE format('ALTER TABLE transfers DROP CONSTRAINT %I', name);
  END IF;
END
$$;
ALTER TABLE transfers ADD CONSTRAINT transfers_kind_check CHECK (kind IN ('deposit', 'withdrawal', 'internal'));

ALTER TABLE transfers ALTER COLUMN method DROP NOT NULL;
ALTER TABLE transfers ADD CONSTRAINT transfers_method_only_off_internal CHECK ((kind = 'internal') = (method IS NULL));

ALTER TABLE transfers ADD COLUMN to_account_id bigint;
ALTER TABLE transfers ADD CONSTRAINT transfers_to_account_only_internal
  CHECK ((kind = 'internal') = (to_account_id IS NOT NULL));

-- What a transfer is never changes; only where it stands does. Replaces the trigger from 20260917000001 to cover
-- the new column.
CREATE OR REPLACE FUNCTION transfers_keep_terms() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (NEW.transfer_id, NEW.account_id, NEW.client_transfer_id, NEW.kind, NEW.amount_cents, NEW.method,
      NEW.to_account_id, NEW.created_at)
     IS DISTINCT FROM
     (OLD.transfer_id, OLD.account_id, OLD.client_transfer_id, OLD.kind, OLD.amount_cents, OLD.method,
      OLD.to_account_id, OLD.created_at)
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
