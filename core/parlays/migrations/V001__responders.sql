-- The parlay service's records (core/parlays/README.md, responders; core/store/decisions.md): the approved responder
-- accounts and every approval and revocation. Requests, quotes and subscriptions stay in memory (the README says why).

-- One row once the list has been started, from the catalogue's "responder": true accounts: after that the list, not
-- the catalogue, is the truth, so a revoked catalogue responder stays revoked.
CREATE TABLE responder_list (
  singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  started_at timestamptz NOT NULL
);

-- The accounts approved now.
CREATE TABLE responders (
  account_id bigint PRIMARY KEY CHECK (account_id >= 0),
  approved_at timestamptz NOT NULL
);

-- Every approval and revocation, in order: who made it (the catalogue's seed, or an operator) and when.
CREATE TABLE responder_changes (
  change_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id bigint NOT NULL,
  approved boolean NOT NULL,
  changed_by text NOT NULL CHECK (changed_by IN ('catalogue', 'operator', 'import')),
  at timestamptz NOT NULL
);

CREATE FUNCTION refuse_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% is append-only: % refused', TG_TABLE_NAME, TG_OP USING ERRCODE = 'insufficient_privilege';
END
$$;
CREATE TRIGGER responder_changes_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON responder_changes
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER responder_list_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON responder_list
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

REVOKE UPDATE, DELETE, TRUNCATE ON responder_list, responders, responder_changes FROM PUBLIC;
