-- Standard charts (SKR03/SKR04) and Rostwerk tax codes.
--
-- Journal lines carry a Rostwerk tax code, not DATEV's BU-Schlüssel, so the
-- ledger does not depend on DATEV; the DATEV export maps codes to keys.

-- Tax codes are reference data shared by all tenants, so this table has no
-- tenant_id and no row-level security. It is filled by migrations only.
CREATE TABLE accounting_tax_codes (
    code        text PRIMARY KEY CHECK (code ~ '^[A-Z0-9_]+$'),
    name        text NOT NULL CHECK (length(name) > 0),
    -- Percent, e.g. 19.00.
    rate        numeric(5, 2) NOT NULL CHECK (rate >= 0 AND rate < 100),
    -- output: sales (Umsatzsteuer), allowed on revenue accounts.
    -- input: purchases (Vorsteuer), allowed on expense and asset accounts.
    direction   text NOT NULL CHECK (direction IN ('output', 'input')),
    treatment   text NOT NULL CHECK (treatment IN ('standard', 'reverse_charge', 'intra_eu'))
);

INSERT INTO accounting_tax_codes (code, name, rate, direction, treatment) VALUES
    ('VAT19_OUT', 'Umsatzsteuer 19 %', 19, 'output', 'standard'),
    ('VAT7_OUT', 'Umsatzsteuer 7 %', 7, 'output', 'standard'),
    ('VAT0_OUT', 'Umsatzsteuer 0 % (§ 12 Abs. 3 UStG)', 0, 'output', 'standard'),
    ('VAT19_IN', 'Vorsteuer 19 %', 19, 'input', 'standard'),
    ('VAT7_IN', 'Vorsteuer 7 %', 7, 'input', 'standard'),
    ('VAT0_IN', 'Vorsteuer 0 % (§ 12 Abs. 3 UStG)', 0, 'input', 'standard'),
    ('RC19_IN', 'Steuerschuldnerschaft des Leistungsempfängers § 13b UStG, 19 % Vorsteuer und 19 % Umsatzsteuer', 19, 'input', 'reverse_charge'),
    ('RC7_IN', 'Steuerschuldnerschaft des Leistungsempfängers § 13b UStG, 7 % Vorsteuer und 7 % Umsatzsteuer', 7, 'input', 'reverse_charge'),
    ('RC_OUT', 'Leistung, für die der Leistungsempfänger die Steuer nach § 13b UStG schuldet', 0, 'output', 'reverse_charge'),
    ('IGL_OUT', 'Steuerfreie innergemeinschaftliche Lieferung § 4 Nr. 1b UStG', 0, 'output', 'intra_eu'),
    ('IGE19_IN', 'Innergemeinschaftlicher Erwerb, 19 % Vorsteuer und 19 % Umsatzsteuer', 19, 'input', 'intra_eu'),
    ('IGE7_IN', 'Innergemeinschaftlicher Erwerb, 7 % Vorsteuer und 7 % Umsatzsteuer', 7, 'input', 'intra_eu');

CREATE FUNCTION accounting_tax_codes_readonly() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- Migrations run as the table owner; the application role may only read.
    IF session_user <> (SELECT tableowner FROM pg_tables
                         WHERE schemaname = TG_TABLE_SCHEMA AND tablename = TG_TABLE_NAME) THEN
        RAISE EXCEPTION 'tax codes are changed by migrations only';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER accounting_tax_codes_readonly
    BEFORE INSERT OR UPDATE OR DELETE ON accounting_tax_codes
    FOR EACH ROW EXECUTE FUNCTION accounting_tax_codes_readonly();

-- Automatic account (Automatikkonto): every line on it is taxed with this
-- code, and no other code may be given.
ALTER TABLE accounts ADD COLUMN tax_code text REFERENCES accounting_tax_codes (code);

-- Optional tax code of a line.
ALTER TABLE journal_lines ADD COLUMN tax_code text REFERENCES accounting_tax_codes (code);

-- The standard chart a tenant was set up with. At most one per tenant; set up once.
CREATE TABLE accounting_charts (
    id        uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    chart     text NOT NULL CHECK (chart IN ('skr03', 'skr04')),
    -- Version of the shipped chart template that was applied.
    version   text NOT NULL CHECK (length(version) > 0),
    set_up_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT accounting_charts_tenant_id_key UNIQUE (tenant_id)
);

ALTER TABLE accounting_charts ENABLE ROW LEVEL SECURITY;
ALTER TABLE accounting_charts FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON accounting_charts
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

CREATE TRIGGER accounting_charts_audit
    AFTER INSERT OR UPDATE OR DELETE ON accounting_charts
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
