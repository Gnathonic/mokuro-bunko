# CLAUDE.md

Guidance for coding agents working in this repository.

## NEVER NAME A DEPLOYMENT. EVER.

**This is the one rule with no exceptions and no judgement calls. No deployment
is ever named in this repository.** That means:

- nobody's public mokuro-bunko instance, and no community or third-party server;
- none of the maintainer's own servers, NAS boxes or containers;
- no test, staging or rehearsal host.

Never write a real instance's domain, hostname, URL, IP address (public or
LAN), machine name, container name, port mapping or admin path. Nowhere means
nowhere:

- code, comments, tests and fixtures;
- docs, the deploy templates and compose files, the README and this file;
- the CHANGELOG, commit messages and branch names;
- PR titles and bodies, issue comments and release notes.

Use reserved names instead:

- the domains `example.com`, `example.net` and `*.test`, plus `localhost`;
- the documentation addresses `192.0.2.x`, `198.51.100.x` and `203.0.113.x`.

A made-up private address in a test (`192.168.1.5`) is nobody's machine. A real
one never goes in, not even in a test.

- A user's bug report may quote their server. Leave it in their text, and never
  repeat it in ours.
- Before every commit and every push, check the diff, the commit messages and
  the PR body for hostnames and IP addresses.
- If a deployment's name turns up anywhere in the tree, removing it is the first
  job, before anything else.
- This rule itself names none. Keep it that way: never add a "for example" here.

The only real addresses the repo carries are Mokuro Reader's own origins. They
are the default client, CORS origin and reader link (`crates/bunko-core/src/config.rs`).
