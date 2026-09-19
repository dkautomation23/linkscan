# Security Policy

linkscan crawls a site you point it at and fetches whatever links it finds.
That means it's built to make outbound HTTP requests to arbitrary,
attacker-influenceable URLs by design — the security question is not
"can it reach the network" but "can a target make it do something you didn't
ask for."

## What counts as a vulnerability here

- **Server-side request forgery beyond the intended target.** A page that
  redirects linkscan to a loopback, link-local, or private-network address
  (for example a cloud metadata endpoint) in a way that the result then
  exposes in the report. Following normal HTTP redirects to other public
  hosts is expected behavior, not this.
- **CSV/formula injection.** A link or page title that, once written into
  `--csv` output, opens as a spreadsheet formula (a cell starting with `=`,
  `+`, `-`, or `@`) rather than plain text.
- **Unbounded resource use from a single response.** A response (via gzip
  decompression, an extreme redirect chain, or a pathological HTML payload)
  that causes memory or CPU use wildly out of proportion to the response
  size — a decompression bomb, in effect.
- A crash, memory-safety issue, or hang triggered by a malformed HTTP
  response or HTML/URL that a normal crawl target would never send.

Report these.

## What is not a vulnerability

- A bot-protected host reported as "could not verify" instead of "broken" —
  that's the documented three-way split (broken / could not verify /
  redirected), working as intended.
- Links that only exist after JavaScript runs being invisible to the crawl —
  documented; linkscan does not execute JavaScript.
- linkscan not honoring `robots.txt` — documented; the tool assumes you are
  scanning a site you're allowed to crawl, and the crawl delay exists so it
  doesn't hurt that site.
- Slowness against a genuinely slow or link-heavy site.

## Reporting a vulnerability

Preferred: open a report through
[GitHub Private vulnerability reporting](https://github.com/dkautomation23/linkscan/security/advisories/new)
on this repository.

Alternative: email **hello@dkautomation.dev** with `linkscan` in the subject
line.

Please include:
- the linkscan version (`linkscan --version`) and OS,
- the exact command and flags you ran,
- the URL or a minimal reproduction site/page (a `data:` URL or a small
  local HTML file is fine if the target site is private).

**First response within 3 business days.** After triage we'll tell you the
expected timeline for a fix and credit you in the release notes, if you want
that.

## Supported versions

Only the latest release is supported. If you're on an older tag, please
upgrade before reporting — the issue may already be fixed.
