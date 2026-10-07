-- The live database before the library's protection against junk and floods (library.js):
-- npx wrangler d1 execute ferret --remote --file migrations/2026-10-07-protection.sql
-- (schema.sql's CREATE TABLE IF NOT EXISTS skips tables that exist, so new columns come here.)
ALTER TABLE packs ADD COLUMN net TEXT;
ALTER TABLE packs ADD COLUMN reports INTEGER NOT NULL DEFAULT 0;
ALTER TABLE packs ADD COLUMN reviewed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE votes ADD COLUMN net TEXT;
CREATE INDEX IF NOT EXISTS packs_net ON packs (net, uploaded);
CREATE INDEX IF NOT EXISTS votes_net ON votes (net);
CREATE TABLE IF NOT EXISTS downloads (
  pack INTEGER NOT NULL,
  install TEXT NOT NULL,
  PRIMARY KEY (pack, install)
);
CREATE TABLE IF NOT EXISTS pack_reports (
  pack INTEGER NOT NULL,
  install TEXT NOT NULL,
  net TEXT NOT NULL,
  PRIMARY KEY (pack, install)
);
CREATE INDEX IF NOT EXISTS pack_reports_net ON pack_reports (net);
