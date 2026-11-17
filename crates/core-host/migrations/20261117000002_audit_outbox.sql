-- Audit log: append-only journal of every change to audited tables.
-- Rows are written only by the rostwerk_audit() trigger function.
CREATE TABLE audit_log (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id),
    occurred_at timestamptz NOT NULL DEFAULT now(),
    actor       text NOT NULL,
    entity_type text NOT NULL,
    entity_id   uuid NOT NULL,
    action      text NOT NULL CHECK (action IN ('insert', 'update', 'delete')),
    before      jsonb,
    after       jsonb,
    txid        bigint NOT NULL DEFAULT txid_current()
);

CREATE INDEX audit_log_entity_idx ON audit_log (tenant_id, entity_type, entity_id, id);

ALTER TABLE audit_log ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_log FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON audit_log
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

-- Append-only: no updates, no deletes, no truncation.
CREATE FUNCTION rostwerk_audit_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only';
END
$$;

CREATE TRIGGER audit_log_no_update_delete
    BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit_immutable();

CREATE TRIGGER audit_log_no_truncate
    BEFORE TRUNCATE ON audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION rostwerk_audit_immutable();

-- Only the audit trigger may insert. It runs as the function owner (SECURITY
-- DEFINER), so current_user differs from session_user; a direct INSERT does not.
CREATE FUNCTION rostwerk_audit_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF current_user = session_user THEN
        RAISE EXCEPTION 'audit_log is written by triggers only';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER audit_log_insert_guard
    BEFORE INSERT ON audit_log
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit_guard();

-- Generic row audit. Attach to a tenant table with:
--   CREATE TRIGGER <table>_audit AFTER INSERT OR UPDATE OR DELETE ON <table>
--       FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
-- The table needs `id uuid` and `tenant_id uuid` columns.
CREATE FUNCTION rostwerk_audit() RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    v_actor text := coalesce(nullif(current_setting('app.actor', true), ''), 'db:' || session_user);
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO audit_log (tenant_id, actor, entity_type, entity_id, action, after)
        VALUES (NEW.tenant_id, v_actor, TG_TABLE_NAME, NEW.id, 'insert', to_jsonb(NEW));
    ELSIF TG_OP = 'UPDATE' THEN
        INSERT INTO audit_log (tenant_id, actor, entity_type, entity_id, action, before, after)
        VALUES (NEW.tenant_id, v_actor, TG_TABLE_NAME, NEW.id, 'update', to_jsonb(OLD), to_jsonb(NEW));
    ELSE
        INSERT INTO audit_log (tenant_id, actor, entity_type, entity_id, action, before)
        VALUES (OLD.tenant_id, v_actor, TG_TABLE_NAME, OLD.id, 'delete', to_jsonb(OLD));
    END IF;
    RETURN NULL;
END
$$;

-- Transactional outbox: events are written in the same transaction as the
-- change and published afterwards by the relay (at-least-once).
CREATE TABLE outbox (
    id           uuid PRIMARY KEY,  -- UUIDv7, so ordered by creation
    tenant_id    uuid NOT NULL REFERENCES tenants (id),
    topic        text NOT NULL CHECK (length(topic) > 0),
    payload      jsonb NOT NULL,
    actor        text NOT NULL,
    occurred_at  timestamptz NOT NULL DEFAULT now(),
    published_at timestamptz,
    attempts     integer NOT NULL DEFAULT 0
);

CREATE INDEX outbox_unpublished_idx ON outbox (id) WHERE published_at IS NULL;

ALTER TABLE outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE outbox FORCE ROW LEVEL SECURITY;

-- Tenants write their own events; the relay (app.system = on) reads all.
CREATE POLICY tenant_or_system ON outbox
    USING (
        tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
        OR current_setting('app.system', true) = 'on'
    )
    WITH CHECK (
        tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
        OR current_setting('app.system', true) = 'on'
    );
