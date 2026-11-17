-- Which OIDC identities may act in which tenant, and as what.
-- Owned by the core, not tenant-scoped (like `tenants`): it is read before a
-- tenant context exists, to decide which tenant a request runs in.
-- `subject` is the `sub` claim of the configured issuer.
CREATE TABLE tenant_members (
    tenant_id  uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    subject    text NOT NULL CHECK (length(subject) > 0),
    kind       text NOT NULL CHECK (kind IN ('user', 'agent')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, subject)
);

CREATE INDEX tenant_members_subject_idx ON tenant_members (subject);
