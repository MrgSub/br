-- Initial schema. See docs/plan for the full design.

CREATE TABLE agents (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    metadata     TEXT
);
CREATE INDEX idx_agents_last_seen ON agents(last_seen_at DESC);

CREATE TABLE tabs (
    id              TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL REFERENCES agents(id),
    url             TEXT NOT NULL,
    canonical_url   TEXT,
    title           TEXT,
    status          TEXT NOT NULL,
    source          TEXT,
    quality_hint    TEXT,
    error           TEXT,
    token_count     INTEGER,
    bytes_html      INTEGER,
    bytes_md        INTEGER,
    opened_at       INTEGER NOT NULL,
    fetched_at      INTEGER,
    closed_at       INTEGER
);
CREATE INDEX idx_tabs_agent  ON tabs(agent_id, opened_at DESC);
CREATE INDEX idx_tabs_url    ON tabs(url);
CREATE INDEX idx_tabs_status ON tabs(status);

CREATE TABLE tab_content (
    tab_id      TEXT PRIMARY KEY REFERENCES tabs(id) ON DELETE CASCADE,
    markdown    BLOB,
    raw_html    BLOB,
    screenshot  BLOB,
    headers     TEXT
);

CREATE TABLE tab_history (
    tab_id      TEXT NOT NULL REFERENCES tabs(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    detail      TEXT,
    PRIMARY KEY (tab_id, seq)
);

CREATE TABLE fetch_cache (
    url         TEXT NOT NULL,
    source      TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL,
    expires_at  INTEGER,
    etag        TEXT,
    last_mod    TEXT,
    markdown    BLOB,
    PRIMARY KEY (url, source)
);
