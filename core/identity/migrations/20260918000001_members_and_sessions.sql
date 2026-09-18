-- The people behind the accounts, and the sessions they log in with (core/identity/README.md, members; interface 6).
-- The accounts table said a person, and what identifies them, belong in tables of their own that refer to it: these
-- are those tables. They hold the only personal data the venue keeps, and nothing here reaches the journal,
-- ClickHouse or Valkey (blueprint §12). Times are UTC.

-- A member: one person, one account. The email is what they log in with, kept lower-cased so it is unique whatever
-- case it was typed in, and the password is never stored — only its Argon2id hash, which carries its own salt and
-- parameters in the PHC string.
CREATE TABLE members (
  member_id text PRIMARY KEY CHECK (member_id ~ '^m_[0-9a-f]{16}$'),
  account_id bigint NOT NULL UNIQUE REFERENCES accounts,
  email text NOT NULL UNIQUE CHECK (email = lower(email) AND position('@' IN email) > 1),
  password_phc text NOT NULL CHECK (password_phc LIKE '$argon2id$%'),
  display_name text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 64),
  created_at timestamptz NOT NULL,
  -- Set rather than deleted: an account outlives every record that refers to it, and so does the member.
  disabled_at timestamptz
);
COMMENT ON TABLE members IS 'People, their email and their password hash: the venue''s only personal data.';

-- A session is what an app holds after logging in. The token is never stored, only
-- SHA-256("venue-session/1:" + sessionId + ":" + secret), as an API key's secret is.
CREATE TABLE member_sessions (
  session_id text PRIMARY KEY CHECK (session_id ~ '^vs_[0-9a-f]{16}$'),
  member_id text NOT NULL REFERENCES members,
  token_sha256 bytea NOT NULL CHECK (octet_length(token_sha256) = 32),
  created_at timestamptz NOT NULL,
  expires_at timestamptz NOT NULL CHECK (expires_at > created_at),
  -- When the member logged out, or something ended the session for them.
  ended_at timestamptz,
  -- What the session was last used from, for a member to recognise their own devices later. No address is kept yet.
  last_used_at timestamptz
);
CREATE INDEX member_sessions_by_member ON member_sessions (member_id, created_at DESC);

-- A session's terms never change: it is ended, not edited, and a session that ended stays ended.
CREATE FUNCTION refuse_session_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.session_id <> OLD.session_id OR NEW.member_id <> OLD.member_id
     OR NEW.token_sha256 <> OLD.token_sha256 OR NEW.created_at <> OLD.created_at
     OR NEW.expires_at <> OLD.expires_at THEN
    RAISE EXCEPTION 'a session cannot be changed, only ended' USING ERRCODE = 'insufficient_privilege';
  END IF;
  IF OLD.ended_at IS NOT NULL AND NEW.ended_at IS DISTINCT FROM OLD.ended_at THEN
    RAISE EXCEPTION 'a session that ended stays ended' USING ERRCODE = 'insufficient_privilege';
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER member_sessions_are_ended_not_edited
  BEFORE UPDATE ON member_sessions FOR EACH ROW EXECUTE FUNCTION refuse_session_change();
