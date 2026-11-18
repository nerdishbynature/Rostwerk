-- Test module `bustxstock` (tests/bus_transactions.rs). Set up like a real module table.
CREATE TABLE bustx_reservations (
    id        uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    item      text NOT NULL
);

ALTER TABLE bustx_reservations ENABLE ROW LEVEL SECURITY;
ALTER TABLE bustx_reservations FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON bustx_reservations
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

CREATE TRIGGER bustx_reservations_audit
    AFTER INSERT OR UPDATE OR DELETE ON bustx_reservations
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
