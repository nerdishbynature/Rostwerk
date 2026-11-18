-- Double-entry journal. Entries are drafts until posted; posted entries and
-- their lines can never be changed or deleted (GoBD). Corrections are
-- reversal entries. The triggers below enforce this for every role that is
-- not the table owner, whatever the application code does.

CREATE TABLE journal_entries (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants (id),
    status       text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'posted')),
    -- Gapless per tenant, assigned at posting (see journal_sequences).
    sequence_no  bigint CHECK (sequence_no > 0),
    booking_date date,
    -- Receipt or document the booking is based on (Belegnummer).
    document_ref text CHECK (length(document_ref) > 0),
    description  text NOT NULL DEFAULT '',
    -- Set on a reversal entry: the posted entry it reverses.
    reverses_id  uuid,
    posted_at    timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT journal_entries_tenant_id_id_key UNIQUE (tenant_id, id),
    CONSTRAINT journal_entries_tenant_id_sequence_no_key UNIQUE (tenant_id, sequence_no),
    CONSTRAINT journal_entries_reverses_fkey
        FOREIGN KEY (tenant_id, reverses_id) REFERENCES journal_entries (tenant_id, id),
    CONSTRAINT journal_entries_posted_complete CHECK (
        status = 'draft'
        OR (sequence_no IS NOT NULL AND booking_date IS NOT NULL
            AND document_ref IS NOT NULL AND posted_at IS NOT NULL)
    ),
    CONSTRAINT journal_entries_draft_unnumbered CHECK (
        status = 'posted' OR (sequence_no IS NULL AND posted_at IS NULL)
    )
);

-- An entry is reversed at most once.
CREATE UNIQUE INDEX journal_entries_reverses_id_key ON journal_entries (reverses_id)
    WHERE reverses_id IS NOT NULL;

CREATE TABLE journal_lines (
    id         uuid PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    entry_id   uuid NOT NULL,
    -- Position within the entry, from 1.
    line_no    integer NOT NULL CHECK (line_no > 0),
    account_id uuid NOT NULL,
    side       text NOT NULL CHECK (side IN ('debit', 'credit')),
    amount     numeric(19, 4) NOT NULL CHECK (amount > 0),
    -- ISO 4217 code.
    currency   char(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    CONSTRAINT journal_lines_entry_id_line_no_key UNIQUE (entry_id, line_no),
    CONSTRAINT journal_lines_entry_fkey
        FOREIGN KEY (tenant_id, entry_id) REFERENCES journal_entries (tenant_id, id),
    -- RESTRICT: an account that is used by a line cannot be deleted.
    CONSTRAINT journal_lines_account_fkey
        FOREIGN KEY (tenant_id, account_id) REFERENCES accounts (tenant_id, id)
);

CREATE INDEX journal_lines_tenant_id_account_id_idx ON journal_lines (tenant_id, account_id);

-- Last sequence number per tenant. Posting increments it under a row lock, so
-- concurrent postings wait for each other and a rolled-back posting leaves no gap.
CREATE TABLE journal_sequences (
    tenant_id uuid PRIMARY KEY REFERENCES tenants (id),
    last_no   bigint NOT NULL CHECK (last_no >= 0)
);

-- Tenant isolation. FORCE also applies the policy to the table owner.
-- No tenant set in the transaction -> NULL -> no rows visible, no rows writable.
ALTER TABLE journal_entries ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_entries FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON journal_entries
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

ALTER TABLE journal_lines ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_lines FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON journal_lines
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

ALTER TABLE journal_sequences ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_sequences FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON journal_sequences
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

CREATE TRIGGER journal_entries_audit
    AFTER INSERT OR UPDATE OR DELETE ON journal_entries
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
CREATE TRIGGER journal_lines_audit
    AFTER INSERT OR UPDATE OR DELETE ON journal_lines
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();

-- Posted entries are immutable. An entry is created as a draft; a draft may be
-- changed, deleted or posted. Posting (draft -> posted) is checked here as well
-- as in the application: balanced in a single currency, and numbered with the
-- tenant's next sequence number (taken from journal_sequences just before).
CREATE FUNCTION accounting_entry_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    v_currencies bigint;
    v_debit numeric;
    v_credit numeric;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.status <> 'draft' THEN
            RAISE EXCEPTION 'journal entries are created as drafts and posted afterwards'
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
        RETURN NEW;
    END IF;
    IF OLD.status = 'posted' THEN
        RAISE EXCEPTION 'journal entry % is posted and cannot be changed or deleted', OLD.id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    IF NEW.status = 'posted' THEN
        SELECT count(DISTINCT currency),
               coalesce(sum(amount) FILTER (WHERE side = 'debit'), 0),
               coalesce(sum(amount) FILTER (WHERE side = 'credit'), 0)
          INTO v_currencies, v_debit, v_credit
          FROM journal_lines WHERE entry_id = NEW.id;
        IF v_currencies <> 1 OR v_debit <> v_credit OR v_debit <= 0 THEN
            RAISE EXCEPTION 'journal entry % is not balanced in a single currency', NEW.id
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
        IF NEW.sequence_no IS DISTINCT FROM (
            SELECT last_no FROM journal_sequences WHERE tenant_id = NEW.tenant_id
        ) THEN
            RAISE EXCEPTION 'journal entry % must take the next sequence number', NEW.id
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER journal_entries_guard
    BEFORE INSERT OR UPDATE OR DELETE ON journal_entries
    FOR EACH ROW EXECUTE FUNCTION accounting_entry_guard();

-- Lines may be added, changed or removed only while their entry is a draft.
-- A new entry is always inserted as a draft, see the CHECK constraints.
CREATE FUNCTION accounting_line_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') AND EXISTS (
        SELECT 1 FROM journal_entries WHERE id = OLD.entry_id AND status = 'posted'
    ) THEN
        RAISE EXCEPTION 'lines of posted journal entry % cannot be changed or deleted', OLD.entry_id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') AND EXISTS (
        SELECT 1 FROM journal_entries WHERE id = NEW.entry_id AND status = 'posted'
    ) THEN
        RAISE EXCEPTION 'lines cannot be added to posted journal entry %', NEW.entry_id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER journal_lines_guard
    BEFORE INSERT OR UPDATE OR DELETE ON journal_lines
    FOR EACH ROW EXECUTE FUNCTION accounting_line_guard();

CREATE FUNCTION accounting_no_truncate() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME;
END
$$;

CREATE TRIGGER journal_entries_no_truncate
    BEFORE TRUNCATE ON journal_entries
    FOR EACH STATEMENT EXECUTE FUNCTION accounting_no_truncate();
CREATE TRIGGER journal_lines_no_truncate
    BEFORE TRUNCATE ON journal_lines
    FOR EACH STATEMENT EXECUTE FUNCTION accounting_no_truncate();

-- The sequence only moves forward by one, and by commit time the number it
-- moved to must belong to a posted entry. With the check in
-- accounting_entry_guard this keeps the numbering gapless in the database.
CREATE FUNCTION accounting_sequence_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE'
        OR (TG_OP = 'INSERT' AND NEW.last_no <> 1)
        OR (TG_OP = 'UPDATE' AND (NEW.last_no <> OLD.last_no + 1 OR NEW.tenant_id <> OLD.tenant_id))
    THEN
        RAISE EXCEPTION 'journal sequence numbers only advance by one'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER journal_sequences_guard
    BEFORE INSERT OR UPDATE OR DELETE ON journal_sequences
    FOR EACH ROW EXECUTE FUNCTION accounting_sequence_guard();

CREATE FUNCTION accounting_sequence_used() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM journal_entries
         WHERE tenant_id = NEW.tenant_id AND sequence_no = NEW.last_no AND status = 'posted'
    ) THEN
        RAISE EXCEPTION 'journal sequence number % was taken but not used by a posted entry', NEW.last_no
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER journal_sequences_used
    AFTER INSERT OR UPDATE ON journal_sequences
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION accounting_sequence_used();

CREATE TRIGGER journal_sequences_no_truncate
    BEFORE TRUNCATE ON journal_sequences
    FOR EACH STATEMENT EXECUTE FUNCTION accounting_no_truncate();
