# Security policy

railway-tui runs with your Railway login and can change environment variables, redeploy
services, deploy a service's latest commit and switch the branch a service deploys from on your
account, so security reports are welcome.

## Reporting a vulnerability

Please report privately through GitHub's
[private vulnerability reporting](https://github.com/nplayer89/railway-tui/security/advisories/new)
rather than opening a public issue. Include what you found, how to reproduce it, and what an
attacker could do with it. You should get a reply within a week.

## Scope

Of particular interest:

- anything that writes to Railway without the user confirming it, or writes to the wrong
  environment or service;
- a variable value or token reaching the screen unrevealed, an error message, or a log;
- the on-disk files (`pending.toml`, the remembered state) being readable by other users.

## Supported versions

Fixes go into the latest release and `main`; older releases are not patched.
