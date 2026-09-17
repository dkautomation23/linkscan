# linkscan

[![CI](https://github.com/dkautomation23/linkscan/actions/workflows/ci.yml/badge.svg)](https://github.com/dkautomation23/linkscan/actions/workflows/ci.yml)

Crawls a site and reports the links that are **actually** broken — with the page
each one was found on, and without calling a bot-protected host dead.

```bash
linkscan https://example.com --depth 2 --csv broken.csv
```

Single binary, no runtime. Wall clock is set by the hosts being checked rather
than by the tool: 220 links on one site took about 10 seconds at the default 16
concurrent requests, and a site full of slow third-party links will take longer
no matter what checks it.

## Why another link checker

Because the existing ones cry wolf. Run one against a normal site and the report
fills with 403s from Cloudflare, 999s from LinkedIn and timeouts from a slow CDN
— none of which are broken links. After the second false alarm nobody reads the
report again.

This one separates three things:

- **broken** — 404, 410, or a 5xx the visitor would see;
- **could not verify** — we were refused or timed out, a human with a browser is
  probably fine;
- **works, but leaves your domain** — a redirect that quietly hands your traffic
  to someone else.

Only the first group sets a non-zero exit code.

A real example from building this: `crates.io` answers `404` with an empty body
to anything that is not a browser. The first version reported the Rust
homepage as linking to a dead site. Two rules fixed it — a refusal followed by a
404 is a filter, and **a 404 on a domain's root is never a deleted page** — and
both are now covered by tests.

## What it does

- crawls internal pages breadth-first to `--depth`, capped by `--max-pages`;
- collects `<a href>`, `<img src>`, `<script src>` and stylesheet links;
- checks every unique URL **once**, no matter how many pages point at it, and
  remembers all of them so the report says where to go and fix it;
- HEAD first, GET only when HEAD is refused — many servers answer 405 to HEAD
  while serving GET perfectly;
- one page at a time while crawling (with `--crawl-delay`), fan-out only while
  checking, where the load is spread across many hosts;
- writes a CSV of everything worth fixing.

## Sample output

Against `rust-lang.org`, six pages deep enough to collect 220 unique links:

```console
$ linkscan https://rust-lang.org --depth 1 --max-pages 6 --concurrency 24

https://rust-lang.org/
  6 page(s) crawled, 220 unique link(s) checked in 10.5s
  219 fine, 0 broken, 1 unverified

Could not verify (1 - not counted as broken)
  refused (bot protection?)  https://crates.io/

Works, but redirects to another domain (3)
  https://foundation.rust-lang.org/members -> https://rustfoundation.org/members/
  https://foundation.rust-lang.org...ia-guide/ -> https://rustfoundation.org/polic...k-policy/
  https://foundation.rust-lang.org...y-policy/ -> https://rustfoundation.org/polic...y-policy/

Slowest response: 8369 ms  https://foundation.rust-lang.org/policies/logo-p...ia-guide/
```

When something is genuinely broken the report leads with it and names the pages:

```
Broken
  [404] link  https://example.com/old-pricing
        found on https://example.com/
        found on https://example.com/about
        ... and 4 more page(s)
```

## Install

A built binary for Linux, macOS (Apple silicon) and Windows is attached to
every [release](https://github.com/dkautomation23/linkscan/releases) — no toolchain,
no compile step:

```bash
curl -sSL https://github.com/dkautomation23/linkscan/releases/latest/download/linkscan-v1.0.0-x86_64-unknown-linux-gnu.tar.gz | tar xz
./linkscan-v1.0.0-x86_64-unknown-linux-gnu/linkscan --help
```

Each archive is built and tested on the platform it targets, not cross-compiled.

To build it yourself:

```bash
git clone https://github.com/dkautomation23/linkscan.git
cd linkscan
cargo build --release
./target/release/linkscan https://example.com
```

Stable Rust; CI builds and tests on 1.98.0, and the committed `Cargo.lock` is v4,
so anything older than Cargo 1.78 cannot read it. The binary is self-contained —
copy it to a server and it runs.

```bash
cargo test        # 15 tests, no network
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--depth` | 2 | levels of internal pages to follow |
| `--max-pages` | 100 | hard cap on pages crawled |
| `--concurrency` | 16 | links checked in parallel |
| `--timeout` | 15 | seconds per request |
| `--crawl-delay` | 200 | ms between page fetches |
| `--no-external` | off | skip links to other domains |
| `--csv` | – | write the findings to a file |
| `--user-agent` | linkscan/0.1 | override when a site treats the default badly |

Exit code is `1` when something is broken, so it fits in CI:

```bash
linkscan https://staging.example.com --no-external || exit 1
```

## Honest limits

- **No JavaScript.** Links that only exist after hydration are invisible here.
  That is a deliberate trade: the crawl stays fast and dependency-free, and a
  link a crawler cannot see is one Google cannot see either.
- **No sitemap parsing yet.** It follows links from the page you give it.
- **No `robots.txt` obedience.** You are expected to be scanning your own site;
  the crawl delay is there so you do not hurt it.
- **Bot protection stays unverifiable.** The tool tells you it could not check,
  it does not try to look like a browser to get around it.
- Redirect chains are followed up to 5 hops, then reported as unverifiable.

## License

MIT
