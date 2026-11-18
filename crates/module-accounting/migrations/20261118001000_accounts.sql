-- Chart of accounts of a tenant.
CREATE TABLE accounts (
    id         uuid PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    -- Account number, chosen by the tenant (e.g. 1200).
    number     text NOT NULL CHECK (length(number) > 0),
    name       text NOT NULL CHECK (length(name) > 0),
    type       text NOT NULL CHECK (type IN ('asset', 'liability', 'equity', 'revenue', 'expense')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT accounts_tenant_id_number_key UNIQUE (tenant_id, number),
    -- Target of the journal lines' (tenant_id, account_id) foreign key, so a
    -- line can never point to another tenant's account.
    CONSTRAINT accounts_tenant_id_id_key UNIQUE (tenant_id, id)
);

-- Tenant isolation. FORCE also applies the policy to the table owner.
-- No tenant set in the transaction -> NULL -> no rows visible, no rows writable.
ALTER TABLE accounts ENABLE ROW LEVEL SECURITY;
ALTER TABLE accounts FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON accounts
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

CREATE TRIGGER accounts_audit
    AFTER INSERT OR UPDATE OR DELETE ON accounts
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
