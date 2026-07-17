# Security Policy

MineRider connects to network services (Minecraft servers, and optionally
Microsoft/Xbox Live/Minecraft Services for premium login) and parses
untrusted server-controlled input. Security issues in the protocol decoder,
crypto, or authentication code are taken seriously.

## Reporting a vulnerability

Preferred: use [GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability)
on this repository (the "Security" tab → "Report a vulnerability"), if it is
enabled here. This lets you share details, proof-of-concept code, and
proposed fixes privately with maintainers before anything is public.

If private reporting is not available, open a regular issue that:

- describes the class of problem (e.g. "decoder can be made to allocate
  unbounded memory from a malicious server") without including exploit
  details or proof-of-concept code in the public issue, and
- asks a maintainer to follow up for a private channel to share specifics.

Please do not include real credentials, tokens, or session data in any
report, public or private.

## Scope

In scope: the wire protocol decoder/encoder (`minerider-protocol`), crypto
(RSA/AES-128-CFB8), compression handling, the login/configuration/play state
machines, world/chunk parsing, and the Microsoft/Xbox Live/Minecraft Services
auth flow (`src/auth`) — anything that processes attacker-controlled input
from a server or from the Microsoft identity platform.

Out of scope: issues that require a compromised or malicious *client*
environment (e.g. local malware with filesystem access to
`.minerider_msa_cache.json`), or third-party services (Mojang/Microsoft
infrastructure itself).

## What to expect

This is a volunteer-maintained alpha project without a formal SLA. Reports
will be acknowledged as soon as a maintainer sees them, and a fix or
mitigation will be prioritized based on severity.
