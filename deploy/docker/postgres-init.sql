-- Runs once, when the PostgreSQL container initialises an empty data volume (as the superuser).
-- The app's role `nbs`: a plain login role (no superuser, no CREATEDB, no CREATEROLE) owning the
-- database `nbs`. Its password comes from the mounted secret, never from this file.
\set ON_ERROR_STOP on
\set nbs_password `cat /run/secrets/db_password`
CREATE ROLE nbs LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD :'nbs_password';
CREATE DATABASE nbs OWNER nbs ENCODING 'UTF8';
