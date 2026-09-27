-- Subaccounts (spec/engine/decisions.md, D-33; interfaces/gateway/decisions.md, G-21): a way for a firm to organise
-- trading into accounts that share an owner, its master, but cannot trade against each other. Set once, when the
-- subaccount is made, and never reassigned.
ALTER TABLE accounts ADD COLUMN master_account_id bigint REFERENCES accounts (account_id)
  CHECK (master_account_id IS NULL OR master_account_id <> account_id);
CREATE INDEX accounts_by_master ON accounts (master_account_id) WHERE master_account_id IS NOT NULL;

-- A family is two levels deep at most: a master may not itself be a subaccount, and an account that already has
-- subaccounts of its own may not become one either. Together with the trigger below, master_account_id can only ever
-- move from null to one value, on an account nothing else has named as a master.
CREATE FUNCTION accounts_master_is_a_master() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.master_account_id IS NULL THEN
    RETURN NEW;
  END IF;
  IF EXISTS (SELECT 1 FROM accounts WHERE account_id = NEW.master_account_id AND master_account_id IS NOT NULL) THEN
    RAISE EXCEPTION 'account %: master % is itself a subaccount', NEW.account_id, NEW.master_account_id
      USING ERRCODE = 'check_violation';
  END IF;
  IF EXISTS (SELECT 1 FROM accounts WHERE master_account_id = NEW.account_id) THEN
    RAISE EXCEPTION 'account %: already a master, so it cannot become a subaccount', NEW.account_id
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER accounts_master_is_a_master BEFORE INSERT OR UPDATE ON accounts
  FOR EACH ROW EXECUTE FUNCTION accounts_master_is_a_master();

CREATE FUNCTION accounts_master_is_set_once() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.master_account_id IS NOT NULL AND NEW.master_account_id IS DISTINCT FROM OLD.master_account_id THEN
    RAISE EXCEPTION 'account %: a master account is set once and never reassigned', OLD.account_id
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER accounts_master_is_set_once BEFORE UPDATE ON accounts
  FOR EACH ROW EXECUTE FUNCTION accounts_master_is_set_once();
