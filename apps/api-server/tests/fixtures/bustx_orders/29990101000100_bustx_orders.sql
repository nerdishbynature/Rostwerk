-- Test module `bustxorders` (tests/bus_transactions.rs). Set up like a real module table.
CREATE TABLE bustx_orders (
    id        uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    item      text NOT NULL
);

ALTER TABLE bustx_orders ENABLE ROW LEVEL SECURITY;
ALTER TABLE bustx_orders FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON bustx_orders
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

CREATE TRIGGER bustx_orders_audit
    AFTER INSERT OR UPDATE OR DELETE ON bustx_orders
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
