-- Custom field definitions per tenant (see core_host::ext).
-- Identified by (entity, key), never by a server-generated id, so a definition
-- is the same on Dev, QAS and PROD. `id` exists only for the audit trigger.
CREATE TABLE ext_field_definitions (
    tenant_id   uuid NOT NULL REFERENCES tenants (id),
    -- `<module>.<entity>`, e.g. `masterdata.partner`, as declared by the module.
    entity      text NOT NULL CHECK (length(entity) > 0),
    key         text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{0,62}$'),
    type        text NOT NULL CHECK (type IN ('string', 'number', 'decimal', 'boolean', 'date', 'enum')),
    required    boolean NOT NULL DEFAULT false,
    -- Allowed values, only and always for type `enum`.
    enum_values text[] CHECK ((type = 'enum') = (enum_values IS NOT NULL AND cardinality(enum_values) > 0)),
    label       text NOT NULL CHECK (length(label) > 0),
    id          uuid NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, entity, key)
);

ALTER TABLE ext_field_definitions ENABLE ROW LEVEL SECURITY;
ALTER TABLE ext_field_definitions FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON ext_field_definitions
    USING (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid);

CREATE TRIGGER ext_field_definitions_audit
    AFTER INSERT OR UPDATE OR DELETE ON ext_field_definitions
    FOR EACH ROW EXECUTE FUNCTION rostwerk_audit();
