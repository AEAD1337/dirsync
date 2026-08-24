# Security Policy

## Supported versions

dirsync ships as a rolling release built from `main`. Only the current
`main` and the latest published release receive fixes; there are no
backports to older tags.

| Version                    | Supported |
| -------------------------- | --------- |
| latest release / `rolling` | yes       |
| any earlier tag or build   | no        |

## Reporting a vulnerability

**Please do not open a public issue for a suspected vulnerability.**

Report it privately through GitHub Security Advisories:

https://github.com/AEAD1337/dirsync/security/advisories/new

That form is private between you and the maintainer until an advisory is
published. If it is unavailable for you, open a minimal public issue that
says only "security report, please open a private channel" and leaves out
the details.

For anything that is clearly not a vulnerability - a crash on bad input, a
confusing log line, a UI bug - a normal issue is the right place:

https://github.com/AEAD1337/dirsync/issues/new

## What to include

The more of this you can provide, the faster a fix lands:

- dirsync version (`dirsync --version`) and build (GUI or CLI-only)
- operating system and filesystem involved (NTFS, ext4, exFAT, network share)
- the exact command line or GUI configuration used
- reproduction steps, ideally against a throwaway directory tree
- what you expected and what actually happened
- for anything touching the GUI: the request or WebSocket frame that triggers it

## Response expectations

This is a single-maintainer hobby project, so timelines are best effort
rather than a contractual SLA:

- acknowledgement: within 15 days
- initial assessment: within 30 days
- fix or documented decision not to fix: within 90 days for anything
  confirmed as exploitable

There is no bug bounty. Reporters are credited in the published advisory
and the release notes unless they ask not to be.

## Scope

dirsync is a local one-way directory mirror. It has two attack surfaces:
the sync engine, which deletes and overwrites files in the destination,
and the GUI, which runs an HTTP and WebSocket server on the local machine.

In scope:

- **Destructive behavior outside the configured destination.** Any input
  that makes dirsync write to, delete, or overwrite a path outside the
  destination root: path traversal via crafted names, symlink or junction
  or reparse-point following, drive-relative or UNC path handling, `..`
  components surviving normalization.
- **Silent data loss inside the destination.** A plan that deletes or
  overwrites a file the planner did not report, or a rename/move match
  (head+tail fingerprinting) that maps a file onto the wrong target and
  destroys content.
- **GUI server issues.** Bypassing the `Host` and `Origin` same-origin
  middleware in `src/gui/server.rs`, DNS rebinding, CSRF against the
  state-changing `POST`/`PUT` routes, XSS in the Svelte frontend, path or
  filesystem disclosure through `/api/v1/browse`, `/api/v1/complete`, or
  `/api/v1/stat` beyond what the UI is meant to expose, or a request that
  reaches `/api/v1/shutdown` or `/api/v1/run` from another origin.
- **Static asset serving.** Traversal out of the rust-embed bundle in
  `src/gui/assets.rs`.
- **Memory safety or panics reachable from untrusted filesystem input**,
  such as a filename or directory structure that aborts the process
  mid-write and leaves the destination inconsistent.
- **Vulnerable dependencies** that are actually reachable in a shipped
  binary. The published SBOMs (`sbom-*.json` on the rolling release) list
  exactly which crates and frontend packages each build contains.
- **Release integrity**: a problem with how the CI workflow builds,
  signs, or publishes the artifacts.

Out of scope:

- Pointing dirsync at the wrong destination and losing the files that were
  there. Mirroring is destructive by design; that is what dry-run mode and
  the plan preview are for.
- Attacks that require an attacker who already has your user account or
  administrator rights on the machine.
- Anything in `frontend/node_modules` that is a build-time devDependency
  and does not end up in the Vite bundle. Check `sbom-frontend.json`
  before reporting: whatever is listed there does ship.
- Missing hardening headers, TLS, or rate limiting on a server bound to
  `127.0.0.1` for a single desktop user, absent a concrete exploit.
- Findings from an automated scanner with no working reproduction.

## Known limitations

These are understood and accepted, not vulnerabilities. They are listed so
nobody spends time rediscovering them:

- **The GUI server has no authentication.** It binds `127.0.0.1` only, and
  the `Host`/`Origin` middleware blocks browser-driven cross-origin access
  and unattributed `POST`/`PUT`. It does not, and cannot, stop another
  process running as the same user on the same machine: such a process can
  already read and write the same files directly.
- **The port is discoverable.** A local process can scan `127.0.0.1` and
  find the GUI. The middleware limits what it can do, but the server's
  existence is not secret.
- **No integrity signatures on release artifacts.** Binaries on the
  rolling release are not code-signed, and the tag is force-moved on every
  push to `main`, so a downloaded `rolling` artifact is not a stable,
  verifiable point in time. Build from source if you need that.
- **Third-party GitHub Actions are pinned to commit SHAs**; GitHub-owned
  actions are pinned to major version tags, which the vendor can move.
