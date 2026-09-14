ALTER TABLE eta_roots
    ADD COLUMN freshness_minimum_seconds INTEGER NOT NULL DEFAULT 900;
