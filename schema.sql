CREATE TABLE portals (
    id VARCHAR(40) NOT NULL,
    revision BIGINT NOT NULL,
    timestamp BIGINT NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL,
    faction CHAR(1) NOT NULL,
    level INTEGER NOT NULL,
    health INTEGER NOT NULL,
    res_count INTEGER NOT NULL,
    image TEXT,
    title TEXT NOT NULL,
    owner VARCHAR(255),
    PRIMARY KEY (id, revision)
);
CREATE TABLE portal_mods (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    portal_id VARCHAR(40) NOT NULL,
    portal_revision BIGINT NOT NULL,
    mod_owner VARCHAR(255) NOT NULL,
    mod_name VARCHAR(255) NOT NULL,
    mod_rarity VARCHAR(255) NOT NULL
);
CREATE INDEX portal_mod_id ON portal_mods (portal_id, portal_revision);
CREATE TABLE portal_resonators (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    portal_id VARCHAR(40) NOT NULL,
    portal_revision BIGINT NOT NULL,
    reso_owner VARCHAR(255) NOT NULL,
    reso_level INTEGER NOT NULL,
    reso_energy INTEGER NOT NULL
);
CREATE INDEX portal_resonator_id ON portal_resonators (portal_id, portal_revision);
