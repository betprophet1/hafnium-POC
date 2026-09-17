-- The identity service's records (core/identity/README.md; core/store/decisions.md, S-13 onwards): the environment's
-- accounts and their API keys. Times are UTC.

-- Each account of the environment, by its pseudonymous ID: the one place the venue will map an account to a person
-- (blueprint §12: no personal data in the journal, ClickHouse or Valkey; the mapping lives only in the accounts
-- database). No personal data is kept yet: a person, and what identifies them, belong in tables of their own that
-- refer to this one, with retention of their own. An account is never deleted while anything refers to it.
CREATE TABLE accounts (
  account_id bigint PRIMARY KEY CHECK (account_id >= 0),
  display_name text NOT NULL,
  -- Test environments only: SHA-256("venue-dev-token/1:" + token) of the dev token that acts as the account.
  dev_token_sha256 bytea CHECK (octet_length(dev_token_sha256) = 32),
  created_at timestamptz NOT NULL,
  updated_at timestamptz NOT NULL
);
COMMENT ON TABLE accounts IS 'Pseudonymous accounts; the future home of the account-to-person mapping, in tables that refer to this one.';

-- Each API key (interfaces/gateway/decisions.md, G-1 to G-12). The secret is never stored, only
-- SHA-256("venue-api-key/1:" + keyId + ":" + secret).
CREATE TABLE api_keys (
  key_id text PRIMARY KEY CHECK (key_id ~ '^ak_[0-9a-f]{16}$'),
  -- The order keys were made in, for listing.
  made bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  account_id bigint NOT NULL REFERENCES accounts,
  label text NOT NULL CHECK (length(label) BETWEEN 1 AND 64),
  permissions text[] NOT NULL CHECK (permissions <@ ARRAY['read', 'trade', 'transfers', 'keys', 'respond']
                                     AND 'read' = ANY (permissions)),
  secret_sha256 bytea NOT NULL CHECK (octet_length(secret_sha256) = 32),
  created_at timestamptz NOT NULL,
  expires_at timestamptz CHECK (expires_at > created_at),
  created_by text REFERENCES api_keys,
  revoked_at timestamptz,
  last_used_at timestamptz
);
CREATE INDEX api_keys_by_account ON api_keys (account_id, made);

-- A key cannot be changed, only revoked (G-6): its terms stay, a revocation is never undone or moved, and only its
-- last use moves.
CREATE FUNCTION api_keys_keep_terms() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (NEW.key_id, NEW.made, NEW.account_id, NEW.label, NEW.permissions, NEW.secret_sha256, NEW.created_at,
      NEW.expires_at, NEW.created_by)
     IS DISTINCT FROM
     (OLD.key_id, OLD.made, OLD.account_id, OLD.label, OLD.permissions, OLD.secret_sha256, OLD.created_at,
      OLD.expires_at, OLD.created_by)
  THEN
    RAISE EXCEPTION 'key %: a key cannot be changed, only revoked', OLD.key_id USING ERRCODE = 'check_violation';
  END IF;
  IF OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
    RAISE EXCEPTION 'key %: a revocation is never undone or moved', OLD.key_id USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER api_keys_keep_terms BEFORE UPDATE ON api_keys FOR EACH ROW EXECUTE FUNCTION api_keys_keep_terms();

CREATE FUNCTION refuse_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% rows are never deleted: % refused', TG_TABLE_NAME, TG_OP USING ERRCODE = 'insufficient_privilege';
END
$$;
CREATE TRIGGER api_keys_never_deleted BEFORE DELETE OR TRUNCATE ON api_keys
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_delete();
CREATE TRIGGER accounts_never_deleted BEFORE DELETE OR TRUNCATE ON accounts
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_delete();

REVOKE UPDATE, DELETE, TRUNCATE ON accounts, api_keys FROM PUBLIC;
