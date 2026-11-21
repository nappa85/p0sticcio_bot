-- portals hit by a virus: after the 1h virus cooldown they're rescanned every minute,
-- to record resonators upgraded by other faction accounts before a second virus hides them
-- (IF NOT EXISTS: it could have been created by hand before migrations existed)
CREATE TABLE IF NOT EXISTS portal_watches (
    id BIGSERIAL PRIMARY KEY,
    latitude DOUBLE PRECISION NOT NULL,
    longitude DOUBLE PRECISION NOT NULL,
    -- comm time of the first virus, milliseconds
    virus_time BIGINT NOT NULL,
    -- portal faction when polling started, a change ends the watch
    faction CHAR(1),
    -- when the watch ended early, by a second virus or a faction change, milliseconds
    ended_at BIGINT
);
CREATE UNIQUE INDEX IF NOT EXISTS portal_watches_coords ON portal_watches (latitude, longitude);
