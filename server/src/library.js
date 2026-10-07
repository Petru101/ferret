// The shared library: players upload a game's saved values (a `.ferret` file, the same text
// as Ferret's Copy Values), others list and download them for the game they have open, and
// say whether they worked. Uploads are data, never code: every line is checked here with the
// same rules Ferret's import uses (src/share.rs), and Ferret checks them again on download.
//
// Against junk and floods (anyone can make up install ids, edit a file or skip Ferret): limits
// count by network as well as by install (`network`: a daily-salted hash of the IP, the address
// itself never stored); a vote counts only from an install that downloaded the upload, once per
// network; an upload most of whose takers said it didn't work, or that three networks
// reported, leaves the list (reported ones until the developer looks: library.sh).

const MAX_BODY = 128 * 1024;
const MAX_VALUES = 200;
const MAX_WAYS = 64;
// Uploads one install can make in a day, one network, and all installs together.
const PER_INSTALL_DAY = 10;
const PER_NETWORK_DAY = 10;
const ALL_DAY = 1000;
const LISTED = 50;
// Votes and reports one network can make in a day.
const VOTES_NETWORK_DAY = 200;
const REPORTS_NETWORK_DAY = 30;
// Reports (from different networks) that take an upload off the list until it's looked at.
const REPORTS_TO_HIDE = 3;
// "Didn't work" answers that take an upload off the list, when they outnumber "worked".
const FAILED_TO_HIDE = 2;

const ID_CHARS = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const ID = /^[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}$/;
const INSTALL = /^[0-9a-f]{32}$/;

const hex = (s) => /^-?[0-9a-fA-F]{1,16}$/.test(s);
// Value names: short, so they leave little room for messages in the list.
const goodName = (s) => /^[\p{L}\p{N}_-]{1,32}$/u.test(s);
const goodBuild = (s) => /^[\p{L}\p{N}_-]{1,64}$/u.test(s);
const KINDS = ["i32", "f32", "f64", "xor", "u16"];

function goodSite(s) {
  const f = s.split(/\s+/);
  if (f.length !== 4) return false;
  const [pattern, offset, reg, disp] = f;
  return (
    pattern.length % 2 === 0 &&
    pattern.length >= 8 &&
    pattern.length <= 256 &&
    /^([0-9a-fA-F]{2}|\?\?)+$/.test(pattern) &&
    /^\d+$/.test(offset) &&
    Number(offset) < pattern.length / 2 &&
    /^[a-zA-Z0-9]{1,5}$/.test(reg) &&
    hex(disp)
  );
}

function goodPath(s) {
  const f = s.split(/\s+/);
  const m = /^(.+)[+-]([0-9a-fA-F]{1,16})$/.exec(f[0]);
  return m !== null && f.length <= 17 && f.slice(1).every(hex);
}

function goodNamed(s) {
  return s.length <= 512 && ["gd:", "mono:", "il2cpp:", "ue:", "{", '"'].some((p) => s.startsWith(p)) && !/[\s\p{Cc}]/u.test(s);
}

function goodLimit(s) {
  const f = s.split(/\s+/);
  return f.length === 2 && f.every((b) => b === "-" || Number.isFinite(Number(b)));
}

/**
 * Checks an upload. Unlike an import, which skips what it can't take, an upload with anything
 * wrong is refused whole: it comes from Ferret's own export, so a bad line means it didn't.
 * Returns {game, steam, build, names, text} or {error}.
 */
export function parse(body) {
  if (typeof body !== "string" || body.length > MAX_BODY) return { error: "too big" };
  const lines = body
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l !== "" && !l.startsWith("```"));
  if (lines.shift() !== "ferret 1") return { error: "not a Ferret file" };
  const file = { game: null, steam: null, build: null, names: [] };
  let value = null;
  const done = () => {
    if (value && value.ways === 0) throw `${value.name}: no way to find it`;
  };
  try {
    for (const line of lines) {
      if (line.length > 1024) throw "a line too long";
      const space = line.indexOf(" ");
      const [key, rest] = space < 0 ? [line, ""] : [line.slice(0, space), line.slice(space + 1).trim()];
      if (key === "value") {
        done();
        if (!goodName(rest)) throw `a name that can't be used (letters, digits, _ and -, 32 at most): ${rest.slice(0, 64)}`;
        if (file.names.includes(rest)) throw `${rest} is in the file twice`;
        if (file.names.length >= MAX_VALUES) throw "too many values";
        file.names.push(rest);
        value = { name: rest, ways: 0 };
        continue;
      }
      if (value === null) {
        if (key === "game" && rest.length > 0 && rest.length <= 200 && !/\p{Cc}/u.test(rest)) file.game = rest;
        else if (key === "steam" && /^\d{1,12}$/.test(rest)) file.steam = rest;
        else if (key === "build" && goodBuild(rest)) file.build = rest;
        else throw `a broken line: ${key}`;
        continue;
      }
      const way = { named: goodNamed, site: goodSite, path: goodPath }[key];
      if (way) {
        if (!way(rest)) throw `${value.name}: a broken ${key} line`;
        if (++value.ways > MAX_WAYS) throw `${value.name}: too many ways to find it`;
      } else if (key === "type") {
        if (!KINDS.includes(rest)) throw `${value.name}: an unknown value type`;
      } else if (key === "decimals") {
        if (!/^[0-6]$/.test(rest)) throw `${value.name}: a broken decimals line`;
      } else if (key === "build") {
        if (!goodBuild(rest)) throw `${value.name}: a broken build line`;
      } else if (key === "limit") {
        if (!goodLimit(rest)) throw `${value.name}: a broken limit line`;
      } else {
        throw `${value.name}: an unknown line (${key.slice(0, 20)})`;
      }
    }
    done();
  } catch (why) {
    return { error: why };
  }
  if (!file.game) return { error: "no game line" };
  if (file.names.length === 0) return { error: "no values" };
  return { ...file, game: file.game.toLowerCase(), text: ["ferret 1", ...lines].join("\n") + "\n" };
}

function json(status, body) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

function text(body) {
  return new Response(body, { headers: { "content-type": "text/plain; charset=utf-8" } });
}

async function sha256(s) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** The network a request comes from, as a hash that changes every day (UTC): enough to count
 *  per network, useless for finding anyone. IPv6 counts by its /64 (one home's addresses).
 *  A local test server (`TEST_NETWORK`) takes `x-test-ip`, else the install, as the address. */
async function network(request, install, env) {
  let ip = request.headers.get("cf-connecting-ip") || "";
  if (env.TEST_NETWORK === "1") ip = request.headers.get("x-test-ip") || install || ip;
  if (ip.includes(":")) {
    const [head, tail] = ip.split("::");
    const h = head ? head.split(":") : [];
    const t = tail ? tail.split(":") : [];
    const groups = tail === undefined ? h : [...h, ...Array(Math.max(0, 8 - h.length - t.length)).fill("0"), ...t];
    ip = groups.slice(0, 4).map((g) => parseInt(g || "0", 16).toString(16)).join(":") + "::/64";
  }
  const day = new Date().toISOString().slice(0, 10);
  return (await sha256(`${env.NETWORK_SALT || "ferret"}|${day}|${ip}`)).slice(0, 32);
}

function newId() {
  const bytes = crypto.getRandomValues(new Uint8Array(8));
  const s = [...bytes].map((b) => ID_CHARS[b % 32]).join("");
  return `${s.slice(0, 4)}-${s.slice(4)}`;
}

/** Uploads for a game, best first, one per line (Ferret reads them without a JSON parser):
 *  id, day uploaded, worked, didn't work, game build or -, 1 if the asker's own, value names. */
async function list(url, install, env) {
  const game = (url.searchParams.get("game") || "").toLowerCase();
  const steam = url.searchParams.get("steam") || null;
  if (!game || game.length > 200) return json(400, { error: "which game?" });
  // Not listed: uploads most takers said didn't work, and reported ones not looked at yet. The
  // uploader still sees theirs.
  const { results } = await env.DB.prepare(
    `SELECT ref, date(uploaded) AS day, worked, failed, build, install = ?1 AS mine, names FROM packs
     WHERE game = ?2 AND (steam IS NULL OR ?3 IS NULL OR steam = ?3)
       AND (install = ?1 OR ((reviewed = 1 OR reports < ${REPORTS_TO_HIDE}) AND NOT (failed >= ${FAILED_TO_HIDE} AND failed > worked)))
     ORDER BY worked - failed DESC, uploaded DESC LIMIT ${LISTED}`,
  )
    .bind(install || "", game, steam)
    .all();
  return text(results.map((r) => [r.ref, r.day, r.worked, r.failed, r.build || "-", r.mine, r.names].join("\t")).join("\n"));
}

async function upload(request, install, net, env) {
  let body;
  try {
    body = JSON.parse(await request.text());
  } catch {
    return json(400, { error: "not JSON" });
  }
  const version = typeof body.version === "string" && body.version.length <= 40 ? body.version : null;
  if (!version) return json(400, { error: "which Ferret?" });
  const file = parse(body.body);
  if (file.error) return json(400, { error: file.error });
  const blocked = await env.DB.prepare("SELECT why FROM blocked WHERE game = ? OR game = ?")
    .bind(file.game, file.steam ? `steam:${file.steam}` : "")
    .first();
  if (blocked) return json(403, { error: `this game can't be shared: ${blocked.why}` });
  const [mine, here, all] = await env.DB.batch([
    env.DB.prepare("SELECT count(*) AS n FROM packs WHERE install = ? AND uploaded > datetime('now', '-1 day')").bind(install),
    env.DB.prepare("SELECT count(*) AS n FROM packs WHERE net = ? AND uploaded > datetime('now', '-1 day')").bind(net),
    env.DB.prepare("SELECT count(*) AS n FROM packs WHERE uploaded > datetime('now', '-1 day')"),
  ]);
  if (mine.results[0].n >= PER_INSTALL_DAY || here.results[0].n >= PER_NETWORK_DAY || all.results[0].n >= ALL_DAY) {
    return json(429, { error: "too many uploads today, try again tomorrow" });
  }
  const hash = await sha256(file.text);
  const same = await env.DB.prepare("SELECT ref FROM packs WHERE game = ? AND hash = ?").bind(file.game, hash).first();
  if (same) return json(409, { error: "already shared", id: same.ref });
  for (let tries = 0; tries < 3; tries++) {
    const ref = newId();
    const { meta } = await env.DB.prepare(
      `INSERT INTO packs (ref, install, net, game, steam, build, names, body, hash, version) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT DO NOTHING`,
    )
      .bind(ref, install, net, file.game, file.steam, file.build, file.names.join(", "), file.text, hash, version)
      .run();
    if (meta.changes === 1) return json(200, { ok: true, id: ref });
  }
  return json(500, { error: "try again" });
}

/** Worked or not, once per install and upload; "worked" stays once said (a player who got one
 *  value working and removed another still had it work). Counted only from an install that
 *  downloaded the upload, once per network, and not on one's own uploads. */
async function vote(request, ref, install, net, env) {
  let worked;
  try {
    worked = JSON.parse(await request.text()).worked;
  } catch {
    return json(400, { error: "not JSON" });
  }
  if (typeof worked !== "boolean") return json(400, { error: "worked: true or false" });
  const pack = await env.DB.prepare("SELECT id, install FROM packs WHERE ref = ?").bind(ref).first();
  if (!pack) return json(404, { error: "no such upload" });
  if (pack.install === install) return json(200, { ok: true, counted: false });
  const [took, sameNet, today] = await env.DB.batch([
    env.DB.prepare("SELECT 1 AS yes FROM downloads WHERE pack = ? AND install = ?").bind(pack.id, install),
    env.DB.prepare("SELECT 1 AS yes FROM votes WHERE pack = ? AND net = ? AND install != ?").bind(pack.id, net, install),
    env.DB.prepare("SELECT count(*) AS n FROM votes WHERE net = ?").bind(net),
  ]);
  if (took.results.length === 0 || sameNet.results.length > 0 || today.results[0].n >= VOTES_NETWORK_DAY) {
    return json(200, { ok: true, counted: false });
  }
  await env.DB.batch([
    env.DB.prepare(
      `INSERT INTO votes (pack, install, net, worked) VALUES (?, ?, ?, ?)
       ON CONFLICT (pack, install) DO UPDATE SET worked = max(worked, excluded.worked)`,
    ).bind(pack.id, install, net, worked ? 1 : 0),
    env.DB.prepare(
      `UPDATE packs SET worked = (SELECT count(*) FROM votes WHERE pack = ?1 AND worked = 1),
       failed = (SELECT count(*) FROM votes WHERE pack = ?1 AND worked = 0) WHERE id = ?1`,
    ).bind(pack.id),
  ]);
  return json(200, { ok: true, counted: true });
}

/** A player reports an upload (rude names, junk): once per install and network. Enough reports
 *  from different networks take it off the list until the developer looks (library.sh). */
async function report(ref, install, net, env) {
  const pack = await env.DB.prepare("SELECT id, install FROM packs WHERE ref = ?").bind(ref).first();
  if (!pack) return json(404, { error: "no such upload" });
  if (pack.install === install) return json(400, { error: "that's your own upload" });
  const [sameNet, today] = await env.DB.batch([
    env.DB.prepare("SELECT 1 AS yes FROM pack_reports WHERE pack = ? AND net = ? AND install != ?").bind(pack.id, net, install),
    env.DB.prepare("SELECT count(*) AS n FROM pack_reports WHERE net = ?").bind(net),
  ]);
  if (sameNet.results.length > 0 || today.results[0].n >= REPORTS_NETWORK_DAY) return json(200, { ok: true, counted: false });
  await env.DB.batch([
    env.DB.prepare("INSERT INTO pack_reports (pack, install, net) VALUES (?, ?, ?) ON CONFLICT DO NOTHING").bind(pack.id, install, net),
    env.DB.prepare("UPDATE packs SET reports = (SELECT count(*) FROM pack_reports WHERE pack = ?1) WHERE id = ?1").bind(pack.id),
  ]);
  return json(200, { ok: true, counted: true });
}

/** Routes /v1/packs...; `null` for other paths. */
export async function library(request, url, env) {
  const parts = url.pathname.split("/").filter(Boolean); // v1, packs, [ref], [vote]
  if (parts[1] !== "packs") return null;
  const install = request.headers.get("x-ferret-install");
  const known = install !== null && INSTALL.test(install);
  const ref = parts[2];
  if (ref !== undefined && !ID.test(ref)) return json(404, { error: "no such upload" });
  const method = request.method;
  if (parts.length === 2 && method === "GET") return list(url, known ? install : null, env);
  if (!known && method !== "GET") return json(400, { error: "which install?" });
  const net = await network(request, known ? install : null, env);
  if (parts.length === 2 && method === "POST") return upload(request, install, net, env);
  if (parts.length === 3 && method === "GET") {
    const pack = await env.DB.prepare("SELECT id, body FROM packs WHERE ref = ?").bind(ref).first();
    if (!pack) return json(404, { error: "no such upload" });
    // Who took it: only they can say whether it worked.
    if (known) {
      await env.DB.prepare("INSERT INTO downloads (pack, install) VALUES (?, ?) ON CONFLICT DO NOTHING").bind(pack.id, install).run();
    }
    return text(pack.body);
  }
  if (parts.length === 3 && method === "DELETE") {
    const pack = await env.DB.prepare("SELECT id FROM packs WHERE ref = ? AND install = ?").bind(ref, install).first();
    if (!pack) return json(404, { error: "no upload of yours with this id" });
    await env.DB.batch([
      env.DB.prepare("DELETE FROM votes WHERE pack = ?").bind(pack.id),
      env.DB.prepare("DELETE FROM downloads WHERE pack = ?").bind(pack.id),
      env.DB.prepare("DELETE FROM pack_reports WHERE pack = ?").bind(pack.id),
      env.DB.prepare("DELETE FROM packs WHERE id = ?").bind(pack.id),
    ]);
    return json(200, { ok: true });
  }
  if (parts.length === 4 && parts[3] === "vote" && method === "POST") return vote(request, ref, install, net, env);
  if (parts.length === 4 && parts[3] === "report" && method === "POST") return report(ref, install, net, env);
  return json(405, { error: "not here" });
}
