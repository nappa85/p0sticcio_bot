-- the schema as it was before migrations, databases created back then already have all of it

CREATE TABLE IF NOT EXISTS portals (
    id BIGSERIAL PRIMARY KEY,
    -- byte order collation, keeps the index independent from the OS locale (glibc vs musl)
    portal_id VARCHAR(40) COLLATE "C" NOT NULL,
    latest_revision_id BIGINT NOT NULL,
    latitude DOUBLE PRECISION NOT NULL,
    longitude DOUBLE PRECISION NOT NULL,
    image TEXT,
    title TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_16505_portal_id ON portals (portal_id);
CREATE INDEX IF NOT EXISTS portals_coords ON portals (latitude, longitude);

CREATE TABLE IF NOT EXISTS portal_revisions (
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
CREATE INDEX IF NOT EXISTS idx_16524_portal_mod_id ON portal_revisions (portal_id, revision);

CREATE TABLE IF NOT EXISTS portal_mods (
    id BIGSERIAL PRIMARY KEY,
    revision_id BIGINT NOT NULL,
    mod_owner VARCHAR(255) NOT NULL,
    mod_name VARCHAR(255) NOT NULL,
    mod_rarity VARCHAR(255) NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_16512_portal_mod_id ON portal_mods (revision_id);

CREATE TABLE IF NOT EXISTS portal_resonators (
    id BIGSERIAL PRIMARY KEY,
    revision_id BIGINT NOT NULL,
    reso_owner VARCHAR(255) NOT NULL,
    reso_level BIGINT NOT NULL,
    reso_energy BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_16519_portal_resonator_id ON portal_resonators (revision_id);

CREATE TABLE IF NOT EXISTS locations (
    user_id BIGINT PRIMARY KEY,
    latitude DOUBLE PRECISION NOT NULL,
    longitude DOUBLE PRECISION NOT NULL
);
