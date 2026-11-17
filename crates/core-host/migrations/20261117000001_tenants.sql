-- Tenant registry. Owned by the core, not tenant-scoped.
CREATE TABLE tenants (
    id         uuid PRIMARY KEY,
    name       text NOT NULL CHECK (length(name) > 0),
    modules    text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);
