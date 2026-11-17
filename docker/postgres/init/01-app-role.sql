-- The application role: no superuser, no BYPASSRLS, so row-level security applies.
-- Tables are created by rostwerk_admin (migrations); the app gets DML rights on them.
CREATE ROLE rostwerk_app LOGIN PASSWORD 'rostwerk_app' NOSUPERUSER NOBYPASSRLS;

GRANT CONNECT ON DATABASE rostwerk TO rostwerk_app;
GRANT USAGE ON SCHEMA public TO rostwerk_app;

ALTER DEFAULT PRIVILEGES FOR ROLE rostwerk_admin IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO rostwerk_app;
