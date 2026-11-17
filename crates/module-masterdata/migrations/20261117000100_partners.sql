-- Business partners (customers, suppliers).
CREATE TABLE partners (
    id         uuid PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    name       text NOT NULL CHECK (length(name) > 0),
    kind       text NOT NULL CHECK (kind IN ('customer', 'supplier', 'both')),
    email      text,
    -- Custom fields, validated against the schema registry later.
    ext        jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(ext) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX partners_tenant_id_id_idx ON partners (tenant_id, id);

-- Tenant isolation. FORCE also applies the policy to the table owner.
-- No tenant set in the transaction -> NULL -> no rows visible, no rows writable.
ALTER TABLE partners ENABLE ROW LEVEL SECURITY;
ALTER TABLE partners FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON partners
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);
