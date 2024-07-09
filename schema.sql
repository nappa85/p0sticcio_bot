BEGIN;
CREATE DATABASE p0sticcio_bot;
USE p0sticcio_bot;
CREATE TABLE portals (
    id BIGINT PRIMARY KEY AUTO_INCREMENT,
    portal_id VARCHAR(40) NOT NULL,
    latest_revision_id BIGINT NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL,
    image TEXT,
    title TEXT NOT NULL
);
CREATE INDEX portal_id ON portals (portal_id);
CREATE TABLE portal_revisions (
    id BIGINT PRIMARY KEY AUTO_INCREMENT,
    portal_id BIGINT NOT NULL,
    revision BIGINT NOT NULL,
    timestamp BIGINT NOT NULL,
    faction CHAR(1) NOT NULL,
    level INTEGER NOT NULL,
    health INTEGER NOT NULL,
    res_count INTEGER NOT NULL,
    owner VARCHAR(255)
);
CREATE INDEX portal_mod_id ON portal_revisions (portal_id, revision);
CREATE TABLE portal_mods (
    id BIGINT PRIMARY KEY AUTO_INCREMENT,
    revision_id BIGINT NOT NULL,
    mod_owner VARCHAR(255) NOT NULL,
    mod_name VARCHAR(255) NOT NULL,
    mod_rarity VARCHAR(255) NOT NULL
);
CREATE INDEX portal_mod_id ON portal_mods (revision_id);
CREATE TABLE portal_resonators (
    id BIGINT PRIMARY KEY AUTO_INCREMENT,
    revision_id BIGINT NOT NULL,
    reso_owner VARCHAR(255) NOT NULL,
    reso_level INTEGER NOT NULL,
    reso_energy INTEGER NOT NULL
);
CREATE INDEX portal_resonator_id ON portal_resonators (revision_id);
COMMIT;
