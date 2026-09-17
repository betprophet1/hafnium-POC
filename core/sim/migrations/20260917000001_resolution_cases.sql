-- The dummy oracle's records (core/sim/README.md, the dummy oracle; core/store/decisions.md): resolution cases with
-- their source reports, proposals, sign-offs, disputes and history, and what operators did by hand that the journal
-- does not say (postponements, suspensions by hand). Times are the sequencer's clock, in nanoseconds since the Unix
-- epoch, as the oracle works in them.

-- A case as it now stands: the one row per case that changes.
CREATE TABLE cases (
  event_id bigint PRIMARY KEY,
  states bigint NOT NULL CHECK (states > 0),
  opened_at_ns bigint NOT NULL,
  stage text NOT NULL CHECK (stage IN ('collecting', 'tiebreak', 'escalated', 'proposed', 'sequencing', 'settled')),
  -- Tiebreak and escalated: since when.
  stage_since_ns bigint CHECK ((stage IN ('tiebreak', 'escalated')) = (stage_since_ns IS NOT NULL)),
  escalation_reason text CHECK ((stage = 'escalated') = (escalation_reason IS NOT NULL)),
  -- Proposed, sequencing and settled: the proposal (0: settled outside the oracle).
  proposal_revision bigint CHECK ((stage IN ('proposed', 'sequencing', 'settled')) = (proposal_revision IS NOT NULL)),
  -- How many proposals the case has signed.
  revisions bigint NOT NULL CHECK (revisions >= 0),
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cases_settled ON cases (updated_at) WHERE stage = 'settled';

-- Each report a source made, in order.
CREATE TABLE case_reports (
  event_id bigint NOT NULL REFERENCES cases,
  n integer NOT NULL CHECK (n >= 0),
  source text NOT NULL CHECK (source IN ('alpha', 'beta', 'referee', 'operator', 'auto-approver')),
  outcome text NOT NULL CHECK (outcome IN ('state', 'void')),
  winning_state bigint CHECK ((outcome = 'state') = (winning_state IS NOT NULL)),
  evidence text NOT NULL,
  at_ns bigint NOT NULL,
  PRIMARY KEY (event_id, n)
);

-- Each proposal, by revision: the outcome, the sources backing it and their evidence, and its dispute window.
CREATE TABLE case_proposals (
  event_id bigint NOT NULL REFERENCES cases,
  revision bigint NOT NULL CHECK (revision >= 0),
  outcome text NOT NULL CHECK (outcome IN ('state', 'void')),
  winning_state bigint CHECK ((outcome = 'state') = (winning_state IS NOT NULL)),
  sources text[] NOT NULL,
  evidence text[] NOT NULL,
  signed_at_ns bigint NOT NULL,
  window_ends_ns bigint NOT NULL,
  PRIMARY KEY (event_id, revision)
);

-- Each signature on a proposal (two of three, blueprint §11).
CREATE TABLE case_signoffs (
  event_id bigint NOT NULL,
  revision bigint NOT NULL,
  signer text NOT NULL,
  signed_at_ns bigint NOT NULL,
  PRIMARY KEY (event_id, revision, signer),
  FOREIGN KEY (event_id, revision) REFERENCES case_proposals
);

-- Each dispute of a proposal in its window, which escalated the case.
CREATE TABLE case_disputes (
  event_id bigint NOT NULL REFERENCES cases,
  -- The entry of the case's history that records it.
  n integer NOT NULL,
  revision bigint NOT NULL,
  reason text NOT NULL,
  at_ns bigint NOT NULL,
  PRIMARY KEY (event_id, n)
);

-- What happened to the case, in order, as the page shows it.
CREATE TABLE case_history (
  event_id bigint NOT NULL REFERENCES cases,
  n integer NOT NULL CHECK (n >= 0),
  at_ns bigint NOT NULL,
  what text NOT NULL,
  PRIMARY KEY (event_id, n)
);

-- Rounds an operator postponed, until when; rounds an operator suspended by hand. These change as operators act.
CREATE TABLE holds (
  event_id bigint PRIMARY KEY,
  until_ns bigint NOT NULL
);
CREATE TABLE suspended_by_hand (
  event_id bigint PRIMARY KEY,
  since timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION refuse_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% is append-only: % refused', TG_TABLE_NAME, TG_OP USING ERRCODE = 'insufficient_privilege';
END
$$;
CREATE TRIGGER case_reports_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON case_reports
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER case_proposals_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON case_proposals
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER case_signoffs_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON case_signoffs
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER case_disputes_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON case_disputes
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER case_history_append_only BEFORE UPDATE OR DELETE OR TRUNCATE ON case_history
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();
CREATE TRIGGER cases_never_deleted BEFORE DELETE OR TRUNCATE ON cases
  FOR EACH STATEMENT EXECUTE FUNCTION refuse_change();

REVOKE UPDATE, DELETE, TRUNCATE ON cases, case_reports, case_proposals, case_signoffs, case_disputes, case_history
  FROM PUBLIC;
