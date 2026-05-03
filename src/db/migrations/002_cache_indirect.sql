-- Replace the unused fetch_cache table from 001 with one that points at
-- the canonical tab for each URL, so the markdown body lives once on disk
-- as `tabs/<tab_id>.md` and the cache row is just a pointer + revalidation
-- metadata.

DROP TABLE IF EXISTS fetch_cache;

CREATE TABLE fetch_cache (
    url           TEXT PRIMARY KEY,
    tab_id        TEXT NOT NULL REFERENCES tabs(id) ON DELETE CASCADE,
    fetched_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    etag          TEXT,
    last_modified TEXT
);
CREATE INDEX idx_fetch_cache_expires ON fetch_cache(expires_at);
