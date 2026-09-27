# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io),
certificate by [SignPath Foundation](https://signpath.org).

> **Status:** the application to SignPath Foundation is pending. Until it is
> approved, the Windows binaries on the release page are unsigned. This page
> describes the process that applies from the first signed build on.

## What is signed

Only the Windows binaries of this project, built from this repository:

| File | What it is |
| --- | --- |
| `dirsync.exe` | Windows build with the web GUI |
| `dirsync-cli.exe` | Windows command-line build without the GUI |

Both carry the product name `dirsync` and the version from `Cargo.toml` in
their Windows version resource. Nothing else is signed: no third-party
binaries, no installers, no builds made outside CI.

## How a binary gets signed

- **Source:** the official repository is
  https://github.com/AEAD1337/dirsync. Binaries are built from its `main`
  branch only.
- **Build:** only the GitHub Actions workflow
  [`.github/workflows/release.yml`](.github/workflows/release.yml) builds
  release binaries, on GitHub-hosted runners, from a clean checkout of the
  pushed commit, with `cargo build --locked`. A binary built on a developer
  machine is never submitted for signing.
- **Submission:** the workflow's `sign-windows` job uploads the unsigned build
  artifact of the same run to SignPath through the official
  [SignPath GitHub action](https://github.com/SignPath/github-action-submit-signing-request),
  pinned to a commit SHA. SignPath verifies that the artifact comes from
  this repository's workflow run.
- **Approval:** every signing request is approved manually by an approver
  (see below) in SignPath before it is signed. A request that is not
  approved in time is not signed, and the release then ships unsigned
  binaries.
- **Key:** the signing key is held by SignPath on a hardware security
  module. No project member has access to it.

The workflow and build scripts are part of the reviewed source: a change to
them goes through the same process as any other code change.

## Team roles

| Role | Responsibility | Members |
| --- | --- | --- |
| Authors | trusted to change the source code without further review | [AEAD1337](https://github.com/AEAD1337) |
| Reviewers | review and approve changes from anyone who is not an author | [AEAD1337](https://github.com/AEAD1337) |
| Approvers | approve each signing request | [AEAD1337](https://github.com/AEAD1337) |

All members use two-factor authentication on GitHub and SignPath.

## Privacy

This program will not transfer any information to other networked systems
unless specifically requested by the user.

The GUI build runs a web server bound to `127.0.0.1` (this computer only)
and opens it in the local browser; it serves nothing to other machines.
dirsync has no telemetry, no update checks and no network access of its
own beyond that. It reads and writes only the folders the user selects.

## Reporting

A binary signed as dirsync that behaves in a way this page does not
describe is a security issue: please report it as described in
[SECURITY.md](SECURITY.md).
