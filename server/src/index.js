// Ferret's server: takes feedback and bug reports from the app's Send Feedback dialog and
// keeps them in a D1 database for the developer to read (reports.sh), and holds the shared
// library of saved values (library.js). Nothing is sent anywhere from here, and no email is
// involved.

import { library } from "./library.js";

const LIMITS = {
  body: 640 * 1024,
  message: 5000,
  contact: 200,
  game: 200,
  version: 40,
  log: 400 * 1024,
  profile: 128 * 1024,
};
// Reports one install can send in an hour, and all installs together in a day (keeps a
// flood within the free tier; a real player never gets near either).
const PER_INSTALL_HOUR = 5;
const ALL_DAY = 300;

const PRIVACY = `Ferret: what its server keeps

When you send feedback or a bug report from Ferret, the server keeps what the dialog showed
you before you sent it: your message, the contact you typed (if any), Ferret's version, the
game's program name, and, if you left them on, Ferret's log (your home folder's path removed)
and the game's saved values. It also keeps a random id Ferret made for this install, to stop
floods and to tell reports apart. Nothing else is collected and nothing is sent anywhere.

The server runs on Cloudflare, which sees your IP address like any website does. Ferret's
server doesn't store it.

Reports are read only by Ferret's developer, to fix bugs and answer questions.

Shared values: when you share a game's values, the server keeps exactly the text Ferret
showed you before uploading (the values' names and how to find them in the game), the game's
program name and Steam id, Ferret's version and the install's random id (so you can delete
your uploads). Anyone using Ferret can download it. When you confirm a downloaded value (or
remove one you never confirmed), Ferret tells the server it worked (or didn't) for that upload,
with the same random id, so each install counts once. The server notes which installs
downloaded an upload (only they can say whether it worked), and keeps reports players send
about an upload (rude names, junk) for the developer to look at.

Against floods and fake votes, the library counts uploads, votes and reports per network too.
For that it keeps a code made from your IP address and the day: it changes every day and can't
be turned back into the address. The address itself is never stored.
`;

function reply(status, body) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** The field if it is a string no longer than `max` characters, else undefined. */
function text(value, max, required = false) {
  if (value === undefined || value === null || value === "") {
    return required ? undefined : null;
  }
  return typeof value === "string" && value.length <= max ? value : undefined;
}

async function feedback(request, env) {
  if (Number(request.headers.get("content-length") || 0) > LIMITS.body) {
    return reply(413, { error: "too big" });
  }
  const raw = await request.text();
  if (raw.length > LIMITS.body) {
    return reply(413, { error: "too big" });
  }
  let body;
  try {
    body = JSON.parse(raw);
  } catch {
    return reply(400, { error: "not JSON" });
  }
  const report = {
    id: typeof body.id === "string" && /^[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}$/.test(body.id) ? body.id : undefined,
    install: typeof body.install === "string" && /^[0-9a-f]{32}$/.test(body.install) ? body.install : undefined,
    version: text(body.version, LIMITS.version, true),
    message: text(body.message?.trim(), LIMITS.message, true),
    contact: text(body.contact?.trim(), LIMITS.contact),
    game: text(body.game, LIMITS.game),
    log: text(body.log, LIMITS.log),
    profile: text(body.profile, LIMITS.profile),
  };
  const bad = Object.entries(report).find(([, v]) => v === undefined);
  if (bad) {
    return reply(400, { error: `missing or too long: ${bad[0]}` });
  }
  const [mine, all] = await env.DB.batch([
    env.DB.prepare("SELECT count(*) AS n FROM reports WHERE install = ? AND received > datetime('now', '-1 hour')").bind(report.install),
    env.DB.prepare("SELECT count(*) AS n FROM reports WHERE received > datetime('now', '-1 day')"),
  ]);
  if (mine.results[0].n >= PER_INSTALL_HOUR || all.results[0].n >= ALL_DAY) {
    return reply(429, { error: "too many reports, try again later" });
  }
  const insert = env.DB.prepare(
    "INSERT INTO reports (ref, install, version, message, contact, game, log, profile) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (ref) DO NOTHING",
  ).bind(report.id, report.install, report.version, report.message, report.contact, report.game, report.log, report.profile);
  const { meta } = await insert.run();
  if (meta.changes === 0) {
    return reply(409, { error: "a report with this id exists" });
  }
  return reply(200, { ok: true, id: report.id });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const { pathname } = url;
    const shared = await library(request, url, env);
    if (shared) {
      return shared;
    }
    if (pathname === "/v1/feedback") {
      return request.method === "POST" ? feedback(request, env) : reply(405, { error: "POST only" });
    }
    if (pathname === "/privacy" && request.method === "GET") {
      return new Response(PRIVACY, { headers: { "content-type": "text/plain; charset=utf-8" } });
    }
    return reply(404, { error: "not found" });
  },
};
