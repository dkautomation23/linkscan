//! linkscan - crawl a site and report the links that are actually broken.
//!
//!     linkscan https://example.com
//!     linkscan https://example.com --depth 3 --concurrency 32 --csv broken.csv
//!     linkscan https://example.com --no-external
//!
//! Design notes worth knowing before reading the code:
//!
//! * pages are crawled depth by depth, links are checked concurrently behind a
//!   semaphore, and every URL is checked exactly once no matter how many pages
//!   point at it;
//! * the crawler is polite to the site being crawled (one page at a time,
//!   optional delay) while link *checking* fans out - the load lands on many
//!   different hosts, not on one;
//! * a 403 is never called broken. See `check.rs`.

mod check;
mod extract;

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use tokio::sync::Semaphore;
use url::Url;

use check::{Outcome, Verdict};
use extract::{Found, Kind};

const DEFAULT_AGENT: &str =
    "linkscan/0.1 (+https://github.com/dkautomation23/linkscan)";

#[derive(Parser, Debug)]
#[command(name = "linkscan", about = "Find the broken links on a site, without the false alarms")]
struct Args {
    /// Site to crawl, e.g. https://example.com
    url: String,

    /// How many levels of internal pages to follow (0 = only the page given)
    #[arg(long, default_value_t = 2)]
    depth: usize,

    /// Maximum internal pages to crawl
    #[arg(long, default_value_t = 100)]
    max_pages: usize,

    /// How many links to check at the same time
    #[arg(long, default_value_t = 16)]
    concurrency: usize,

    /// Per-request timeout, seconds
    #[arg(long, default_value_t = 15)]
    timeout: u64,

    /// Skip links pointing to other domains
    #[arg(long)]
    no_external: bool,

    /// Milliseconds to wait between page fetches while crawling
    #[arg(long, default_value_t = 200)]
    crawl_delay: u64,

    /// Write every problem to this CSV
    #[arg(long)]
    csv: Option<String>,

    /// Overwrite --csv if it already exists
    #[arg(long)]
    force: bool,

    /// Override the User-Agent
    #[arg(long, default_value = DEFAULT_AGENT)]
    user_agent: String,

    /// Allow crawling and checking loopback, private and link-local addresses
    /// (use when you are deliberately scanning your own internal network)
    #[arg(long)]
    allow_internal: bool,
}

/// A link plus every page it was found on - without the sources a report is
/// just a list of URLs nobody can act on.
#[derive(Debug, Default)]
struct Sources {
    kind: Option<Kind>,
    pages: BTreeSet<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let start = Instant::now();

    // Only draw progress when someone is watching; piping to a file should
    // produce a clean report, not a wall of carriage returns.
    let progress = std::io::stderr().is_terminal();

    let root = Url::parse(&args.url).or_else(|_| Url::parse(&format!("https://{}", args.url)))?;
    let client = check::client(args.timeout, &args.user_agent, args.allow_internal)?;

    // ---- crawl -----------------------------------------------------------
    let mut queue: VecDeque<(Url, usize)> = VecDeque::from([(root.clone(), 0)]);
    let mut visited: HashSet<String> = HashSet::new();
    let mut targets: BTreeMap<String, Sources> = BTreeMap::new();
    let mut pages_crawled = 0usize;
    let mut unreadable_pages: Vec<(String, String)> = Vec::new();

    while let Some((page, depth)) = queue.pop_front() {
        if pages_crawled >= args.max_pages || !visited.insert(page.as_str().to_string()) {
            continue;
        }

        if let Err(reason) = check::host_guard(&page, args.allow_internal) {
            unreadable_pages.push((page.to_string(), reason));
            continue;
        }
        let body = match client.get(page.clone()).send().await {
            // The body is read through a cap, not `.text()` directly - gzip
            // decompression means Content-Length bears no relation to what a
            // hostile server can make this allocate. See check::read_capped.
            Ok(response) if response.status().is_success() => match check::read_capped(response, check::MAX_BODY_BYTES).await {
                Ok(body) => body,
                Err(error) => {
                    unreadable_pages.push((page.to_string(), error));
                    continue;
                }
            },
            Ok(response) => {
                unreadable_pages.push((page.to_string(), format!("HTTP {}", response.status().as_u16())));
                continue;
            }
            Err(error) => {
                unreadable_pages.push((page.to_string(), error.to_string()));
                continue;
            }
        };
        pages_crawled += 1;
        if progress {
            eprint!("\rcrawling: {pages_crawled} page(s), {} link(s) found", targets.len());
        }

        // `links` is synchronous on purpose - see extract.rs.
        let found: Vec<Found> = extract::links(&body, &page, args.allow_internal);
        for item in found {
            let internal = extract::same_site(&root, &item.url);
            if !internal && args.no_external {
                continue;
            }
            let entry = targets.entry(item.url.as_str().to_string()).or_default();
            entry.kind.get_or_insert(item.kind);
            entry.pages.insert(page.as_str().to_string());

            if internal && item.kind == Kind::Anchor && depth < args.depth {
                queue.push_back((item.url, depth + 1));
            }
        }

        if args.crawl_delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(args.crawl_delay)).await;
        }
    }
    if progress {
        eprintln!();
    }

    // ---- check -----------------------------------------------------------
    let semaphore = Arc::new(Semaphore::new(args.concurrency.max(1)));
    let client = Arc::new(client);
    let mut tasks = Vec::with_capacity(targets.len());

    let allow_internal = args.allow_internal;
    for raw in targets.keys() {
        let Ok(url) = Url::parse(raw) else { continue };
        let permit_source = semaphore.clone();
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit_source.acquire_owned().await.expect("semaphore is never closed");
            check::check(&client, url, allow_internal).await
        }));
    }

    let total = tasks.len();
    let mut outcomes: Vec<Outcome> = Vec::with_capacity(total);
    for (index, task) in tasks.into_iter().enumerate() {
        if let Ok(outcome) = task.await {
            outcomes.push(outcome);
        }
        if progress && index % 10 == 0 {
            eprint!("\rchecking: {}/{total}", index + 1);
        }
    }
    if progress {
        eprintln!("\rchecking: {total}/{total}   ");
    }

    report(&root, &outcomes, &targets, pages_crawled, &unreadable_pages, start);

    if let Some(path) = args.csv.as_deref() {
        write_csv(path, &outcomes, &targets, args.force)?;
    }

    let problems = outcomes.iter().filter(|o| o.verdict.is_problem()).count();
    std::process::exit(if problems > 0 { 1 } else { 0 });
}

fn short(url: &str, width: usize) -> String {
    if url.chars().count() <= width {
        return url.to_string();
    }
    let head: String = url.chars().take(width - 12).collect();
    let tail: String = url.chars().skip(url.chars().count() - 9).collect();
    format!("{head}...{tail}")
}

fn report(
    root: &Url,
    outcomes: &[Outcome],
    targets: &BTreeMap<String, Sources>,
    pages: usize,
    unreadable: &[(String, String)],
    start: Instant,
) {
    let mut by_verdict: BTreeMap<&str, Vec<&Outcome>> = BTreeMap::new();
    for outcome in outcomes {
        by_verdict.entry(outcome.verdict.label()).or_default().push(outcome);
    }

    let ok = outcomes.iter().filter(|o| o.verdict == Verdict::Ok).count();
    let problems: Vec<&Outcome> = outcomes.iter().filter(|o| o.verdict.is_problem()).collect();
    let slowest = outcomes.iter().max_by_key(|o| o.millis);

    println!("\n{}", root.as_str());
    println!(
        "  {pages} page(s) crawled, {} unique link(s) checked in {:.1}s",
        outcomes.len(),
        start.elapsed().as_secs_f32()
    );
    println!("  {ok} fine, {} broken, {} unverified", problems.len(),
             outcomes.len() - ok - problems.len());

    if !problems.is_empty() {
        println!("\nBroken");
        for outcome in &problems {
            let sources = targets.get(outcome.url.as_str());
            let kind = sources.and_then(|s| s.kind).map(Kind::label).unwrap_or("link");
            let status = outcome.status.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
            println!("  [{status}] {kind}  {}", short(outcome.url.as_str(), 88));
            if let Some(sources) = sources {
                for page in sources.pages.iter().take(3) {
                    println!("        found on {}", short(page, 80));
                }
                if sources.pages.len() > 3 {
                    println!("        ... and {} more page(s)", sources.pages.len() - 3);
                }
            }
        }
    }

    let unverified: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Refused | Verdict::Unreachable))
        .collect();
    if !unverified.is_empty() {
        println!("\nCould not verify ({} - not counted as broken)", unverified.len());
        for outcome in unverified.iter().take(10) {
            println!("  {:<26} {}", outcome.verdict.label(), short(outcome.url.as_str(), 70));
        }
        if unverified.len() > 10 {
            println!("  ... and {} more", unverified.len() - 10);
        }
    }

    let offsite: Vec<&Outcome> = outcomes.iter().filter(|o| o.redirected_offsite()).collect();
    if !offsite.is_empty() {
        println!("\nWorks, but redirects to another domain ({})", offsite.len());
        for outcome in offsite.iter().take(5) {
            println!(
                "  {} -> {}",
                short(outcome.url.as_str(), 44),
                short(outcome.final_url.as_ref().map(|u| u.as_str()).unwrap_or(""), 44)
            );
        }
    }

    if !unreadable.is_empty() {
        println!("\nPages that could not be crawled ({})", unreadable.len());
        for (page, why) in unreadable.iter().take(5) {
            println!("  {}  {why}", short(page, 70));
        }
    }

    if let Some(slow) = slowest {
        if slow.millis > 3000 {
            println!("\nSlowest response: {} ms  {}", slow.millis, short(slow.url.as_str(), 60));
        }
    }
    println!();
}

fn write_csv(
    path: &str,
    outcomes: &[Outcome],
    targets: &BTreeMap<String, Sources>,
    force: bool,
) -> std::io::Result<()> {
    use std::io::Write;

    // A rerun must never quietly eat an earlier report - refuse instead of
    // overwriting unless the caller explicitly asked for that.
    if !force && std::path::Path::new(path).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{path} already exists; overwrite only with --force"),
        ));
    }

    fn escape(value: &str) -> String {
        if value.contains(',') || value.contains('"') {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value.to_string()
        }
    }

    let mut file = std::fs::File::create(path)?;
    writeln!(file, "url,verdict,status,kind,found_on,detail")?;
    let mut written = 0;
    for outcome in outcomes {
        if outcome.verdict == Verdict::Ok && !outcome.redirected_offsite() {
            continue;
        }
        let sources = targets.get(outcome.url.as_str());
        let kind = sources.and_then(|s| s.kind).map(Kind::label).unwrap_or("link");
        let pages = sources
            .map(|s| s.pages.iter().cloned().collect::<Vec<_>>().join(" | "))
            .unwrap_or_default();
        let verdict = if outcome.redirected_offsite() { "offsite redirect" } else { outcome.verdict.label() };
        writeln!(
            file,
            "{},{},{},{},{},{}",
            escape(outcome.url.as_str()),
            escape(verdict),
            outcome.status.map(|s| s.to_string()).unwrap_or_default(),
            kind,
            escape(&pages),
            escape(&outcome.detail),
        )?;
        written += 1;
    }
    println!("{written} row(s) -> {path}");
    Ok(())
}
