#!/usr/bin/env node
// `cargo update` with the 7-day minimum release age from CLAUDE.md
// ("Dependencies"). Cargo has no stable age gate (`-Z min-publish-age` is
// nightly-only), so this runs a normal `cargo update`, looks up every crate
// version it moved to on crates.io, and holds each one younger than 7 days
// back at the version the lockfile had before. It repeats until nothing young
// is left, because holding one crate back can change what the others resolve
// to.
//
// Fails closed: if a lookup fails, a young crate cannot be held back, or the
// update brings in a package from outside crates.io (git, another registry:
// nothing to date it by), the original Cargo.lock is restored and the script
// exits non-zero.
//
// Usage: node scripts/cargo-cooldown.js [--days N]
// No npm dependencies: pure Node.js (18+ for fetch).

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const DAYS = Number(process.argv[process.argv.indexOf('--days') + 1]) || 7;
const ROOT = path.resolve(__dirname, '..');
const LOCK = path.join(ROOT, 'Cargo.lock');
const UA = 'dirsync-make-update (https://github.com/AEAD1337/dirsync)';
const MAX_ROUNDS = 5;

// The exact source ids Cargo writes for crates.io (the git index id, which
// Cargo also records for the sparse protocol, and the sparse id itself).
// Compared whole: a substring test would take `git+https://evil.example/crates.io`
// or `registry+https://crates.io.evil.example/` for crates.io.
const CRATES_IO = new Set([
  'registry+https://github.com/rust-lang/crates.io-index',
  'sparse+https://index.crates.io/',
]);

// versions: name -> Set(version) for crates.io packages, the only ones whose
// age can be looked up. foreign: "name@version (source)" for every package
// from anywhere else (git, another registry), which no age gate covers.
function readLock(text) {
  const versions = new Map();
  const foreign = new Set();
  for (const block of text.split('[[package]]').slice(1)) {
    const name = /^name = "([^"]+)"/m.exec(block);
    const version = /^version = "([^"]+)"/m.exec(block);
    const source = /^source = "([^"]+)"/m.exec(block);
    // No source: a workspace member, built from this repository.
    if (!name || !version || !source) continue;
    if (!CRATES_IO.has(source[1])) {
      foreign.add(`${name[1]}@${version[1]} (${source[1]})`);
      continue;
    }
    if (!versions.has(name[1])) versions.set(name[1], new Set());
    versions.get(name[1]).add(version[1]);
  }
  return { versions, foreign };
}

function numeric(v) {
  return v.split(/[.+-]/).map((p) => (/^\d+$/.test(p) ? Number(p) : -1));
}

function compare(a, b) {
  const x = numeric(a);
  const y = numeric(b);
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d !== 0) return d;
  }
  return 0;
}

// Versions present now that were not in the original lockfile.
function moved(before, now) {
  const out = [];
  for (const [name, versions] of now) {
    for (const v of versions) {
      if (!before.get(name)?.has(v)) out.push({ name, version: v });
    }
  }
  return out;
}

async function publishedAt(name, version) {
  const url = `https://crates.io/api/v1/crates/${encodeURIComponent(name)}/${encodeURIComponent(version)}`;
  const res = await fetch(url, { headers: { 'User-Agent': UA } });
  if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
  const body = await res.json();
  return new Date(body.version.created_at);
}

function cargo(args) {
  console.log(`>> cargo ${args.join(' ')}`);
  execFileSync('cargo', args, { cwd: ROOT, stdio: 'inherit' });
}

async function main() {
  const original = fs.readFileSync(LOCK, 'utf8');
  const { versions: before, foreign: foreignBefore } = readLock(original);
  const restore = (why) => {
    fs.writeFileSync(LOCK, original);
    console.error(`cargo-cooldown: ${why}\nCargo.lock restored to its state before the update.`);
    process.exit(1);
  };

  cargo(['update']);

  // A package from anywhere but crates.io has no publish date to check, so
  // the cooldown cannot vouch for it: fail closed instead of letting it by.
  const newForeign = [...readLock(fs.readFileSync(LOCK, 'utf8')).foreign].filter((f) => !foreignBefore.has(f));
  if (newForeign.length > 0) {
    restore(`the update brought in packages from outside crates.io, which the cooldown cannot date: ${newForeign.join(', ')}`);
  }

  const ages = new Map(); // "name@version" -> Date
  const cutoff = Date.now() - DAYS * 86400 * 1000;
  for (let round = 1; round <= MAX_ROUNDS; round++) {
    const changes = moved(before, readLock(fs.readFileSync(LOCK, 'utf8')).versions);
    const young = [];
    for (const c of changes) {
      const key = `${c.name}@${c.version}`;
      if (!ages.has(key)) {
        try {
          ages.set(key, await publishedAt(c.name, c.version));
        } catch (e) {
          restore(`could not date ${key} (${e.message})`);
        }
      }
      if (ages.get(key).getTime() > cutoff) young.push(c);
    }
    if (young.length === 0) {
      for (const c of changes) {
        console.log(`  ok    ${c.name} ${c.version} (published ${ages.get(`${c.name}@${c.version}`).toISOString().slice(0, 10)})`);
      }
      console.log(`cargo-cooldown: ${changes.length} crate version(s) updated, all at least ${DAYS} days old.`);
      return;
    }
    for (const c of young) {
      // An earlier hold in this round can already have moved this crate:
      // jiff pins jiff-static exactly, so holding jiff back takes jiff-static
      // with it. `cargo update -p` would then name a version that is gone;
      // the next round re-checks whatever the crate resolved to instead.
      if (!readLock(fs.readFileSync(LOCK, 'utf8')).versions.get(c.name)?.has(c.version)) continue;
      const published = ages.get(`${c.name}@${c.version}`);
      const eligible = new Date(published.getTime() + DAYS * 86400 * 1000).toISOString().slice(0, 10);
      // The newest version the lockfile had before that is older than this one.
      const previous = [...(before.get(c.name) ?? [])].filter((v) => compare(v, c.version) < 0).sort(compare).pop();
      if (!previous) {
        restore(`${c.name} ${c.version} is younger than ${DAYS} days (eligible ${eligible}) and is new to the lockfile, so there is no older version to hold it at`);
      }
      console.log(`  hold  ${c.name} ${c.version} -> ${previous} (published ${published.toISOString().slice(0, 10)}, eligible ${eligible})`);
      try {
        cargo(['update', '-p', `${c.name}@${c.version}`, '--precise', previous]);
      } catch {
        restore(`cargo could not hold ${c.name} back at ${previous}; a dependent may require ${c.version}`);
      }
    }
  }
  restore(`still finding young versions after ${MAX_ROUNDS} rounds`);
}

if (require.main === module) {
  main().catch((e) => {
    console.error(e);
    process.exit(1);
  });
} else {
  module.exports = { readLock };
}
