-- Renumbered because stable already shipped migrations 0041 and 0042.
ALTER TABLE threads ADD COLUMN name TEXT;
