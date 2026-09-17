#!/usr/bin/env node
// DEV TOOL ONLY - a local stand-in for Pomerium in front of the GTFS editor.
// Never deploy it: it signs an identity for whatever email you ask it to.
//
// It does the two things the editor needs from Pomerium, and nothing else:
//   - generates an ES256 key on start and writes its JWKS to --jwks-out, for
//     GIMS's `gtfs_editor_pomerium_jwks_url = Some "file://<path>"`;
//   - forwards every request to --upstream with a freshly signed
//     `X-Pomerium-Jwt-Assertion` for the current identity and
//     `X-Forwarded-Host: <audience>`, dropping any assertion the client sent.
//
// The identity is per browser, like a Pomerium session: a `dev_pomerium_as`
// cookie set by /.dev-pomerium/as?email=... (empty email = signed out, no
// assertion at all), falling back to --as. /.dev-pomerium/ is a switch page.
//
//   node scripts/dev_pomerium_proxy.mjs --upstream http://127.0.0.1:18010 \
//     --audience gtfs.editor.localhost --jwks-out /tmp/dev-jwks.json --listen 18080
//   open http://localhost:18080/.dev-pomerium/
//
// Node 18+, standard library only.
import { createServer, request as httpRequest } from "node:http";
import { generateKeyPairSync, sign, randomUUID } from "node:crypto";
import { writeFileSync } from "node:fs";

const args = Object.fromEntries(process.argv.slice(2).reduce((acc, a, i, all) => {
  if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]?.startsWith("--") ? "" : all[i + 1] ?? ""]);
  return acc;
}, []));
const LISTEN = Number(args.listen || 18080);
const UPSTREAM = new URL(args.upstream || "http://127.0.0.1:18010");
const AUDIENCE = args.audience || "gtfs.editor.localhost";
const JWKS_OUT = args["jwks-out"];
const DEFAULT_AS = args.as ?? "";
const TTL = 300;
if (!JWKS_OUT) {
  console.error("--jwks-out <path> is required");
  process.exit(2);
}
if (!["127.0.0.1", "localhost", "::1"].includes(UPSTREAM.hostname)) {
  console.error("refusing: this dev proxy only forwards to a local GIMS");
  process.exit(2);
}

const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
const kid = `dev-${randomUUID().slice(0, 8)}`;
const jwk = publicKey.export({ format: "jwk" });
writeFileSync(JWKS_OUT, JSON.stringify({ keys: [{ kty: "EC", crv: "P-256", alg: "ES256", use: "sig", kid, x: jwk.x, y: jwk.y }] }));

const b64url = (buf) => Buffer.from(buf).toString("base64url");
function assertion(email) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "ES256", typ: "JWT", kid }));
  const claims = b64url(JSON.stringify({
    iss: "dev-pomerium", aud: AUDIENCE, sub: `dev|${email}`, email, iat: now, nbf: now, exp: now + TTL,
  }));
  const input = `${header}.${claims}`;
  const sig = sign("sha256", Buffer.from(input), { key: privateKey, dsaEncoding: "ieee-p1363" });
  return `${input}.${b64url(sig)}`;
}

function cookies(req) {
  const out = {};
  for (const part of (req.headers.cookie || "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = decodeURIComponent(part.slice(i + 1).trim());
  }
  return out;
}

const escapeHtml = (s) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

function devPage(res, current) {
  res.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store" });
  res.end(`<!doctype html><meta charset="utf-8"><title>DEV Pomerium</title>
<body style="font:15px system-ui;max-width:40rem;margin:3rem auto">
<p style="background:#6b21a8;color:#fff;padding:.4rem .8rem;border-radius:4px">DEV TOOL - stands in for Pomerium, never deploy</p>
<p>Signed in as <b>${current ? escapeHtml(current) : "(nobody - no SSO assertion)"}</b>, audience <code>${escapeHtml(AUDIENCE)}</code></p>
<form action="/.dev-pomerium/as"><input name="email" value="${escapeHtml(current)}" size="32">
<input type="hidden" name="next" value="/"><button>Act as</button></form>
<p><a href="/">Open the editor</a></p></body>`);
}

createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);
  const jar = cookies(req);
  const current = "dev_pomerium_as" in jar ? jar.dev_pomerium_as : DEFAULT_AS;

  if (url.pathname === "/.dev-pomerium/" || url.pathname === "/.dev-pomerium") return devPage(res, current);
  if (url.pathname === "/.dev-pomerium/as") {
    const email = (url.searchParams.get("email") || "").trim().toLowerCase();
    const next = url.searchParams.get("next") || "/";
    res.writeHead(302, {
      Location: next.startsWith("/") ? next : "/",
      "Set-Cookie": `dev_pomerium_as=${encodeURIComponent(email)}; Path=/; SameSite=Lax`,
      "Cache-Control": "no-store",
    });
    return res.end();
  }

  const headers = { ...req.headers };
  for (const h of Object.keys(headers)) {
    if (["x-pomerium-jwt-assertion", "x-forwarded-host", "host", "connection"].includes(h.toLowerCase())) delete headers[h];
  }
  // drop the dev identity cookie before it reaches GIMS
  if (headers.cookie) {
    headers.cookie = headers.cookie.split(";").filter((c) => !c.trim().startsWith("dev_pomerium_as=")).join(";");
    if (!headers.cookie.trim()) delete headers.cookie;
  }
  headers.host = UPSTREAM.host;
  headers["x-forwarded-host"] = AUDIENCE;
  headers["x-forwarded-proto"] = "https";
  if (current) headers["x-pomerium-jwt-assertion"] = assertion(current);

  const upstream = httpRequest({
    hostname: UPSTREAM.hostname, port: UPSTREAM.port, method: req.method, path: req.url, headers,
  }, (up) => {
    res.writeHead(up.statusCode, up.headers);
    up.pipe(res);
  });
  upstream.on("error", (e) => {
    res.writeHead(502, { "Content-Type": "text/plain" });
    res.end(`dev proxy: upstream ${UPSTREAM.origin} failed: ${e.message}`);
  });
  req.pipe(upstream);
}).listen(LISTEN, "127.0.0.1", () => {
  console.log(`DEV Pomerium stand-in on http://localhost:${LISTEN} -> ${UPSTREAM.origin} (aud ${AUDIENCE}, jwks ${JWKS_OUT})`);
});
