-- Run once, outside of a transaction, then connect to the new database:
-- CREATE DATABASE p0sticcio_bot;
-- \c p0sticcio_bot

BEGIN;

CREATE SCHEMA p0sticcio_bot;
-- queries are unqualified, make them resolve to this schema
ALTER DATABASE p0sticcio_bot SET search_path TO p0sticcio_bot, public;

CREATE TABLE p0sticcio_bot.portals (
    id BIGSERIAL PRIMARY KEY,
    -- byte order collation, keeps the index independent from the OS locale (glibc vs musl)
    portal_id VARCHAR(40) COLLATE "C" NOT NULL,
    latest_revision_id BIGINT NOT NULL,
    latitude DOUBLE PRECISION NOT NULL,
    longitude DOUBLE PRECISION NOT NULL,
    image TEXT,
    title TEXT NOT NULL
);
CREATE INDEX idx_16505_portal_id ON p0sticcio_bot.portals (portal_id);
CREATE INDEX portals_coords ON p0sticcio_bot.portals (latitude, longitude);

CREATE TABLE p0sticcio_bot.portal_revisions (
    id BIGSERIAL PRIMARY KEY,
    portal_id BIGINT NOT NULL,
    revision BIGINT NOT NULL,
    "timestamp" BIGINT NOT NULL,
    faction CHAR(1) NOT NULL,
    level BIGINT NOT NULL,
    health BIGINT NOT NULL,
    res_count BIGINT NOT NULL,
    owner VARCHAR(255)
);
CREATE INDEX idx_16524_portal_mod_id ON p0sticcio_bot.portal_revisions (portal_id, revision);

CREATE TABLE p0sticcio_bot.portal_mods (
    id BIGSERIAL PRIMARY KEY,
    revision_id BIGINT NOT NULL,
    mod_owner VARCHAR(255) NOT NULL,
    mod_name VARCHAR(255) NOT NULL,
    mod_rarity VARCHAR(255) NOT NULL
);
CREATE INDEX idx_16512_portal_mod_id ON p0sticcio_bot.portal_mods (revision_id);

CREATE TABLE p0sticcio_bot.portal_resonators (
    id BIGSERIAL PRIMARY KEY,
    revision_id BIGINT NOT NULL,
    reso_owner VARCHAR(255) NOT NULL,
    reso_level BIGINT NOT NULL,
    reso_energy BIGINT NOT NULL
);
CREATE INDEX idx_16519_portal_resonator_id ON p0sticcio_bot.portal_resonators (revision_id);

CREATE TABLE p0sticcio_bot.locations (
    user_id BIGINT PRIMARY KEY,
    latitude DOUBLE PRECISION NOT NULL,
    longitude DOUBLE PRECISION NOT NULL
);

COMMIT;
