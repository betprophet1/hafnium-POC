-- Whether a member is one of the venue's own people, which is what decides whether they are told that any venue
-- exists but the live one (interfaces/gateway/README.md, choosing an environment). Deliberately not a permission:
-- a permission is what a credential may do with an account, checked on every trading call, and this is a fact about
-- a person that no account has anything to do with.
--
-- It is not a way in. A test environment keeps its own members and its own sessions, and this flag is never sent to
-- one; all it does is decide what the live venue is willing to name. See decision G-20.
ALTER TABLE members ADD COLUMN staff boolean NOT NULL DEFAULT false;

-- Few rows are ever true, so the index is on those and nothing else.
CREATE INDEX members_staff ON members (member_id) WHERE staff;
COMMENT ON COLUMN members.staff IS 'One of the venue''s own people: told which test environments exist, nothing more.';
