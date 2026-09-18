-- What a member needs beyond an email and a password (core/identity/README.md, members): proving the email, getting
-- back in after forgetting the password, a second factor, a stop on guessing, and the mail that tells them any of it
-- happened. Times are UTC.

-- Proving the address, and what repeated failures have done to this member.
ALTER TABLE members ADD COLUMN email_verified_at timestamptz;
ALTER TABLE members ADD COLUMN failed_logins integer NOT NULL DEFAULT 0 CHECK (failed_logins >= 0);
-- Set while a member is locked out; a login before this is refused whatever the password is.
ALTER TABLE members ADD COLUMN locked_until timestamptz;

-- A token emailed to a member: to prove their address, or to set a new password. Kept as its SHA-256, like every
-- other secret here, so the records hold nothing that could be used.
CREATE TABLE member_tokens (
  token_sha256 bytea PRIMARY KEY CHECK (octet_length(token_sha256) = 32),
  member_id text NOT NULL REFERENCES members,
  purpose text NOT NULL CHECK (purpose IN ('verify', 'reset')),
  created_at timestamptz NOT NULL,
  expires_at timestamptz NOT NULL CHECK (expires_at > created_at),
  used_at timestamptz
);
CREATE INDEX member_tokens_by_member ON member_tokens (member_id, purpose, created_at DESC);
COMMENT ON TABLE member_tokens IS 'Emailed one-time tokens, hashed: proving an address, and setting a new password.';

-- The second factor: a shared secret an authenticator app computes six-digit codes from (RFC 6238). Unconfirmed
-- until the member has typed a code it produced, so a mistyped secret cannot lock them out of their own account.
CREATE TABLE member_totp (
  member_id text PRIMARY KEY REFERENCES members,
  secret bytea NOT NULL CHECK (octet_length(secret) = 20),
  created_at timestamptz NOT NULL,
  confirmed_at timestamptz,
  -- The last code accepted, so the same one cannot be used twice inside its window.
  last_step bigint
);

-- Codes to get in with when the authenticator is gone. Single use, hashed, and made as a set.
CREATE TABLE member_recovery_codes (
  member_id text NOT NULL REFERENCES members,
  code_sha256 bytea NOT NULL CHECK (octet_length(code_sha256) = 32),
  created_at timestamptz NOT NULL,
  used_at timestamptz,
  PRIMARY KEY (member_id, code_sha256)
);

-- What a session was opened from, so a member can recognise it in a list of their own sessions and end the one they
-- do not know. A name the app gives, never a fingerprint of the device.
ALTER TABLE member_sessions ADD COLUMN device text;

-- Mail the venue means to send. A test environment has no mail provider, so the messages sit here and the dummy
-- sender marks them sent (core/identity/README.md, mail): the demo can then show what would have been sent, the way
-- the treasury's fake bank stands in for a real one.
CREATE TABLE outbox_mail (
  mail_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  to_email text NOT NULL,
  subject text NOT NULL,
  body text NOT NULL,
  created_at timestamptz NOT NULL,
  sent_at timestamptz
);
CREATE INDEX outbox_mail_unsent ON outbox_mail (created_at) WHERE sent_at IS NULL;
COMMENT ON TABLE outbox_mail IS 'Mail to be sent. A real sender replaces the dummy one; the rows are the same.';
