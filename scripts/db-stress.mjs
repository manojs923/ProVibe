#!/usr/bin/env node
/**
 * Multi-node write stress + failover probe.
 *
 * Fires N image-sized inserts at every configured node, then counts what
 * actually landed. Reports per-node success rate and latency so a node that
 * accepts rows and drops them is distinguishable from one that rejects.
 *
 *   node scripts/db-stress.mjs --count 20 --size 300 --concurrency 3
 *   node scripts/db-stress.mjs --count 20 --size 300 --parallel
 *   node scripts/db-stress.mjs --count 5  --size 300 --break 2
 *   node scripts/db-stress.mjs --count 3  --size 300 --real
 *
 * Flags:
 *   --count N        iterations (default 20)
 *   --size KB        synthetic payload size (default 300)
 *   --interval MS    gap between iterations (default 0)
 *   --parallel       fire every node at once
 *   --concurrency N  cap simultaneous nodes (default: all if --parallel, else 1)
 *   --break N        replace node N with a dead host, to prove failover
 *   --real           use a real JPEG instead of random bytes
 *   --keep           skip cleanup
 */

import fs from "node:fs";
import path from "node:path";
import { randomBytes } from "node:crypto";
import { createClient } from "@supabase/supabase-js";

const args = process.argv.slice(2);
const val = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i === -1 ? fallback : args[i + 1];
};
const has = (name) => args.includes(`--${name}`);

const COUNT = Number(val("count", 20));
const SIZE_KB = Number(val("size", 300));
const INTERVAL = Number(val("interval", 0));
const BREAK = Number(val("break", 0));
const PARALLEL = has("parallel");
const REAL = has("real");
const KEEP = has("keep");

const die = (msg) => { console.error(`\n  ${msg}\n`); process.exit(1); };

function loadEnv() {
  const out = {};
  for (const name of [".env", ".env.local", ".env.development.local"]) {
    const file = path.resolve(process.cwd(), name);
    if (!fs.existsSync(file)) continue;
    const raw = fs.readFileSync(file, "utf8").replace(/^\uFEFF/, "");
    for (const line of raw.split(/\r?\n/)) {
      const m = line.match(/^\s*(?:export\s+)?([A-Za-z0-9_]+)\s*=\s*(.*)$/);
      if (!m) continue;
      out[m[1]] = m[2].trim().replace(/^["']|["']$/g, "");
    }
  }
  return out;
}

const env = loadEnv();
const rawUrls = (env.VITE_SUPABASE_URLS || env.VITE_SUPABASE_URL || "").split(",").map((s) => s.trim()).filter(Boolean);
const rawKeys = (env.VITE_SUPABASE_KEYS || env.VITE_SUPABASE_ANON_KEY || "").split(",").map((s) => s.trim()).filter(Boolean);

if (rawUrls.length === 0) die("no node URLs found in .env or .env.local");
if (rawUrls.length !== rawKeys.length) die(`url/key count mismatch: ${rawUrls.length} urls, ${rawKeys.length} keys`);
if (rawUrls.length < 2) die(`only ${rawUrls.length} node found — set VITE_SUPABASE_URLS, not VITE_SUPABASE_URL`);

const urls = [...rawUrls];
const labels = urls.map((u, i) => {
  try { return `node${i + 1} ${new URL(u).host.split(".")[0]}`; } catch { return `node${i + 1}`; }
});

if (BREAK > 0) {
  if (BREAK > urls.length) die(`--break ${BREAK} but only ${urls.length} nodes`);
  urls[BREAK - 1] = "https://dead-node-0000.invalid.co";
  labels[BREAK - 1] += " (broken)";
}

const CONCURRENCY = Math.max(1, Math.min(
  Number(val("concurrency", PARALLEL ? urls.length : 1)),
  urls.length,
));

const clients = urls.map((u, i) => createClient(u, rawKeys[i], { auth: { persistSession: false } }));
const SESSION = `STRESS-${Date.now().toString(36).toUpperCase()}`;
const BATCH = `stress_${Date.now()}`;

let realB64 = null;
if (REAL) {
  const res = await fetch("https://picsum.photos/1280/720");
  if (!res.ok) die("could not fetch the sample photo for --real");
  realB64 = Buffer.from(await res.arrayBuffer()).toString("base64");
}

const payload = () =>
  `data:image/jpeg;base64,${realB64 ?? randomBytes(SIZE_KB * 1024).toString("base64")}`;

const stats = urls.map(() => ({ ok: 0, fail: 0, lat: [], errors: new Map() }));

async function shoot(node, batchId) {
  const t0 = performance.now();
  try {
    const { error } = await clients[node].from("copilot_sessions").insert({
      session_code: SESSION,
      batch_id: batchId,
      kind: "frame",
      sender: "copilot",
      image_url: payload(),
    });
    const dt = performance.now() - t0;
    const s = stats[node];
    if (error) {
      s.fail += 1;
      const k = error.code || error.message || "unknown";
      s.errors.set(k, (s.errors.get(k) || 0) + 1);
    } else {
      s.ok += 1;
      s.lat.push(dt);
    }
  } catch (e) {
    const s = stats[node];
    s.fail += 1;
    const k = e?.cause?.code || e?.name || "throw";
    s.errors.set(k, (s.errors.get(k) || 0) + 1);
  }
}

// Worker pool: CONCURRENCY nodes in flight at any moment.
async function pool(batchId) {
  let next = 0;
  const workers = Array.from({ length: CONCURRENCY }, async () => {
    while (next < clients.length) await shoot(next++, batchId);
  });
  await Promise.all(workers);
}

const pct = (arr, p) => {
  if (!arr.length) return 0;
  const sorted = [...arr].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))];
};

console.log(`\n  session ${SESSION}`);
console.log(`  ${COUNT} frames x ${SIZE_KB} KB across ${urls.length} node(s), concurrency ${CONCURRENCY}\n`);

const wall = performance.now();
for (let i = 0; i < COUNT; i++) {
  await pool(BATCH);
  if (INTERVAL) await new Promise((r) => setTimeout(r, INTERVAL));
}
const elapsed = ((performance.now() - wall) / 1000).toFixed(1);

// What the database actually holds — the only number that matters.
const landed = [];
for (let n = 0; n < clients.length; n++) {
  try {
    const { count, error } = await clients[n]
      .from("copilot_sessions")
      .select("*", { count: "exact", head: true })
      .eq("batch_id", BATCH);
    landed.push(error ? `ERR ${error.code || error.message}` : count ?? 0);
  } catch (e) {
    landed.push(`ERR ${e?.cause?.code || "throw"}`);
  }
}

console.log(`  ${"node".padEnd(22)} ${"ok".padStart(5)} ${"fail".padStart(5)} ${"stored".padStart(7)} ${"p50".padStart(7)} ${"p95".padStart(7)}`);
console.log(`  ${"-".repeat(60)}`);

let worst = 100;
for (let n = 0; n < clients.length; n++) {
  const s = stats[n];
  const rate = COUNT ? (s.ok / COUNT) * 100 : 0;
  worst = Math.min(worst, rate);
  console.log(
    `  ${labels[n].padEnd(22)} ${String(s.ok).padStart(5)} ${String(s.fail).padStart(5)} ` +
    `${String(landed[n]).padStart(7)} ${pct(s.lat, 0.5).toFixed(0).padStart(7)} ${pct(s.lat, 0.95).toFixed(0).padStart(7)}`
  );
}

for (let n = 0; n < clients.length; n++) {
  if (stats[n].errors.size) {
    console.log(`\n  node${n + 1} errors: ${[...stats[n].errors].map(([k, v]) => `${k} x${v}`).join(", ")}`);
  }
}

const mismatched = stats.map((s, n) => s.ok !== landed[n]).filter(Boolean).length;
const healthy = stats.filter((s) => s.ok > 0).length;

console.log(`\n  ${COUNT * clients.length} attempts in ${elapsed}s`);
console.log(`  nodes accepting: ${healthy}/${clients.length}   worst success: ${worst.toFixed(1)}%`);
if (mismatched) console.log(`  WARNING: ${mismatched} node(s) reported success but stored a different count`);

if (!KEEP) {
  let removed = 0;
  await Promise.all(clients.map(async (c) => {
    try {
      const { error } = await c.from("copilot_sessions").delete().eq("batch_id", BATCH);
      if (!error) removed += 1;
    } catch { /* node down */ }
  }));
  console.log(`  cleanup: ${removed}/${clients.length} nodes (rows may remain if delete policy is missing)`);
}

console.log("");
process.exit(worst === 100 && mismatched === 0 ? 0 : 1);