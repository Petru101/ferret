-- npx wrangler d1 execute ferret --remote --file schema.sql
CREATE TABLE IF NOT EXISTS reports (
  id INTEGER PRIMARY KEY,
  -- The report's id as Ferret showed it to the player ("K7Q2-9XMB"); row numbers are reused.
  ref TEXT NOT NULL,
  received TEXT NOT NULL DEFAULT (datetime('now')),
  -- A random id Ferret makes once per install: rate limits, and telling reports apart.
  install TEXT NOT NULL,
  version TEXT NOT NULL,
  message TEXT NOT NULL,
  -- How the player would like an answer, if at all (free text, never used to send anything).
  contact TEXT,
  game TEXT,
  log TEXT,
  profile TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS reports_ref ON reports (ref);
CREATE INDEX IF NOT EXISTS reports_install ON reports (install, received);
