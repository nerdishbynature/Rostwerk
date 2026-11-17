-- Articles: products (goods) and services that can be sold.
CREATE TABLE articles (
    id                    uuid PRIMARY KEY,
    tenant_id             uuid NOT NULL REFERENCES tenants (id),
    -- Article number, chosen by the tenant.
    number                text NOT NULL CHECK (length(number) > 0),
    name                  text NOT NULL CHECK (length(name) > 0),
    kind                  text NOT NULL CHECK (kind IN ('goods', 'service')),
    -- Unit of measure, e.g. pcs, h, kg.
    unit                  text NOT NULL CHECK (length(unit) > 0),
    -- Net sales price. Up to 4 decimal places for unit prices.
    sales_price_amount    numeric(19, 4) NOT NULL CHECK (sales_price_amount >= 0),
    -- ISO 4217 code.
    sales_price_currency  char(3) NOT NULL CHECK (sales_price_currency ~ '^[A-Z]{3}$'),
    tax_category          text NOT NULL CHECK (tax_category IN ('standard', 'reduced', 'zero')),
    active                boolean NOT NULL DEFAULT true,
    -- Custom fields, validated against the schema registry later.
    ext                   jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(ext) = 'object'),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT articles_tenant_id_number_key UNIQUE (tenant_id, number)
);

CREATE INDEX articles_tenant_id_id_idx ON articles (tenant_id, id);

-- Tenant isolation. FORCE also applies the policy to the table owner.
-- No tenant set in the transaction -> NULL -> no rows visible, no rows writable.
ALTER TABLE articles ENABLE ROW LEVEL SECURITY;
ALTER TABLE articles FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON articles
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);
