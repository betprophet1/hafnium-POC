-- A key may be restricted to the addresses it is expected to be used from (interfaces/gateway/decisions.md, G-23).
-- Each entry is an address or a CIDR network, in the same textual form the gateway's --trusted-proxies takes
-- (core/gateway/src/forwarded.rs): "192.0.2.7" or "10.42.0.0/16". Empty means unrestricted, matching every key made
-- before this existed.
ALTER TABLE api_keys ADD COLUMN allowed_ips text[] NOT NULL DEFAULT '{}';

-- A key's allow-list is a term like its permissions or its expiry: changed only by revoking and remaking (G-6, G-23),
-- never in place. Replaces the trigger from 20260917000001 to cover the new column.
CREATE OR REPLACE FUNCTION api_keys_keep_terms() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (NEW.key_id, NEW.made, NEW.account_id, NEW.label, NEW.permissions, NEW.secret_sha256, NEW.created_at,
      NEW.expires_at, NEW.created_by, NEW.allowed_ips)
     IS DISTINCT FROM
     (OLD.key_id, OLD.made, OLD.account_id, OLD.label, OLD.permissions, OLD.secret_sha256, OLD.created_at,
      OLD.expires_at, OLD.created_by, OLD.allowed_ips)
  THEN
    RAISE EXCEPTION 'key %: a key cannot be changed, only revoked', OLD.key_id USING ERRCODE = 'check_violation';
  END IF;
  IF OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
    RAISE EXCEPTION 'key %: a revocation is never undone or moved', OLD.key_id USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END
$$;
