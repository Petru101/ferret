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

-- The shared library (src/library.js): one row per upload, a game's saved values as a
-- .ferret file.
CREATE TABLE IF NOT EXISTS packs (
  id INTEGER PRIMARY KEY,
  -- Its id as players see it ("K7Q2-9XMB").
  ref TEXT NOT NULL,
  install TEXT NOT NULL,
  -- The network it came from, as library.js's daily-salted hash (never the address).
  net TEXT,
  -- The game's program, lowercase, and its Steam app id when Ferret knew it.
  game TEXT NOT NULL,
  steam TEXT,
  build TEXT,
  -- "coins, health": shown in the list.
  names TEXT NOT NULL,
  body TEXT NOT NULL,
  -- Of the body: the same upload twice is refused.
  hash TEXT NOT NULL,
  version TEXT NOT NULL,
  uploaded TEXT NOT NULL DEFAULT (datetime('now')),
  worked INTEGER NOT NULL DEFAULT 0,
  failed INTEGER NOT NULL DEFAULT 0,
  -- Players (one per network) who reported it; at 3 it leaves the list until looked at.
  reports INTEGER NOT NULL DEFAULT 0,
  -- 1: the developer looked at it and kept it (reports no longer hide it).
  reviewed INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS packs_ref ON packs (ref);
CREATE UNIQUE INDEX IF NOT EXISTS packs_hash ON packs (game, hash);
CREATE INDEX IF NOT EXISTS packs_install ON packs (install, uploaded);
CREATE INDEX IF NOT EXISTS packs_net ON packs (net, uploaded);
-- Whether an upload worked for an install, once each.
CREATE TABLE IF NOT EXISTS votes (
  pack INTEGER NOT NULL,
  install TEXT NOT NULL,
  net TEXT,
  worked INTEGER NOT NULL,
  PRIMARY KEY (pack, install)
);
CREATE INDEX IF NOT EXISTS votes_net ON votes (net);
-- Installs that downloaded an upload: only they can vote on it.
CREATE TABLE IF NOT EXISTS downloads (
  pack INTEGER NOT NULL,
  install TEXT NOT NULL,
  PRIMARY KEY (pack, install)
);
-- Reports on uploads (rude names, junk), once per install.
CREATE TABLE IF NOT EXISTS pack_reports (
  pack INTEGER NOT NULL,
  install TEXT NOT NULL,
  net TEXT NOT NULL,
  PRIMARY KEY (pack, install)
);
CREATE INDEX IF NOT EXISTS pack_reports_net ON pack_reports (net);
-- Games whose values can't be shared (online play, anti-cheat): a program name, lowercase, or
-- "steam:<app id>", and why.
CREATE TABLE IF NOT EXISTS blocked (
  game TEXT PRIMARY KEY,
  why TEXT NOT NULL
);
