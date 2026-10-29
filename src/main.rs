use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ffetch::{
    browser::{self, Renderer},
    discovery::{self, Fusion, SearchMode},
    extract::{self, Extract, Options},
    fetch::{Archive, Fetcher},
    model::{self, Issue, Run},
    net::{self, Http},
    providers::{self, Providers},
    query,
    store::Store,
};
use futures::{FutureExt, StreamExt, stream};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};
use url::Url;

#[derive(Parser)]
#[command(
    version,
    about = "Discover, retrieve, recover, and query the web without writing scrapers"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Output representation. JSON is the stable agent interface.
    #[arg(long, global = true, value_enum, default_value = "json")]
    format: Format,
    /// Apply an embedded jq expression to the complete JSON result.
    #[arg(long, global = true, conflicts_with = "jmespath")]
    jq: Option<String>,
    /// Apply a JMESPath expression to the complete JSON result.
    #[arg(long, global = true)]
    jmespath: Option<String>,
    /// Project record fields (supports dotted paths).
    #[arg(long, global = true, value_delimiter = ',')]
    fields: Vec<String>,
    /// Print strings without JSON quotes after a query.
    #[arg(short = 'r', long, global = true)]
    raw_output: bool,
    /// Per-target or per-provider deadline in seconds, including recovery.
    #[arg(long, global=true, default_value="45", value_parser=parse_positive)]
    timeout: usize,
    /// Balanced gathers three engines; deep broadens coverage; fast returns first success.
    #[arg(long, global = true, value_enum, default_value = "balanced")]
    search_mode: SearchMode,
    /// Broaden search to all built-in web engines (alias for --search-mode deep).
    #[arg(long, global = true, conflicts_with = "search_mode")]
    deep: bool,
    /// Ordered engines for deep web search; the first is the preferred engine.
    #[arg(long, global = true, value_delimiter = ',')]
    engines: Vec<String>,
    /// Maximum result pages per named HTML engine (1-20); stops at --limit.
    #[arg(long, global = true, default_value="1", value_parser=clap::value_parser!(u32).range(1..=20))]
    search_pages: u32,
    /// Reservoir-sample at most N output records; original saved run retains all.
    #[arg(long, global = true, value_parser = parse_positive)]
    sample: Option<usize>,
    /// Reproducible reservoir seed (same seed requires the same input order).
    #[arg(long, global = true, default_value = "0", requires = "sample")]
    sample_seed: u64,
    /// Browser rendering policy for document retrieval.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    browser: browser::Mode,
    #[arg(long, global = true, env = "FFETCH_BROWSER_PATH")]
    browser_path: Option<PathBuf>,
    /// Disable Chromium sandbox explicitly (isolated trusted test environments only).
    #[arg(long, global = true)]
    browser_no_sandbox: bool,
    /// Named browser profile for authenticated/persistent browsing.
    #[arg(long, global = true)]
    session: Option<String>,
    /// Country code for YouTube API search and popular rankings.
    #[arg(long, global = true, default_value = "US")]
    region: String,
    /// Maximum concurrent requests within a batch/provider.
    #[arg(long, global=true, default_value="8", value_parser=parse_positive)]
    concurrency: usize,
    /// Maximum simultaneous HTTP requests per origin, within --concurrency.
    #[arg(long, global=true, default_value="4", value_parser=parse_positive)]
    origin_concurrency: usize,
    /// Maximum decompressed bytes per response.
    #[arg(long, global=true, default_value="16777216", value_parser=parse_positive)]
    max_bytes: usize,
    /// Run/artifact directory (default: XDG_DATA_HOME/ffetch or ~/.local/share/ffetch).
    #[arg(long, global = true, env = "FFETCH_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Do not persist this run or its original response bytes.
    #[arg(long, global = true)]
    no_store: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Jsonl,
    Text,
    Markdown,
    Csv,
}

#[derive(Subcommand)]
enum Command {
    /// Extract text-layer content from selected PDF pages.
    Pdf {
        url: String,
        #[arg(long,default_value="1",value_parser=clap::value_parser!(u32).range(1..))]
        page: u32,
        #[arg(long,default_value="20",value_parser=clap::value_parser!(u32).range(1..=500))]
        limit: u32,
    },
    #[command(name = "__pdf-worker", hide = true)]
    PdfWorker {
        input: PathBuf,
        output: PathBuf,
        url: String,
        page: u32,
        limit: u32,
    },
    /// Retrieve open-access JATS article text by PMCID.
    Fulltext { input: String },
    /// Follow DOI references through bounded Crossref metadata lookups.
    References {
        doi: String,
        /// Maximum reference entries inspected across the traversal.
        #[arg(long, default_value="20", value_parser=parse_positive)]
        limit: usize,
        #[arg(long, default_value="1", value_parser=clap::value_parser!(u32).range(1..=3))]
        depth: u32,
    },
    /// Fetch URLs; '-' reads one URL per line from stdin.
    Fetch(FetchArgs),
    /// Search multiple sources concurrently; returns evidence and provider failures.
    Search {
        query: String,
        /// Additional query variant; repeat this flag to search several together.
        #[arg(long = "query")]
        queries: Vec<String>,
        /// Maximum query/source pairs; pagination and HTTP retries are separate.
        #[arg(long, default_value="32", value_parser=parse_positive)]
        max_searches: usize,
        /// Print the query/source plan without network requests or saved files.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_delimiter = ',', default_value = "web,hn")]
        sources: Vec<String>,
        #[arg(long, default_value="20", value_parser=parse_positive)]
        limit: usize,
        /// Exact time filter for providers supporting it: 24h, 7d, or RFC3339.
        #[arg(long)]
        since: Option<String>,
        /// Fetch linked documents after search; retains search records and links document IDs.
        #[arg(long)]
        read: bool,
        /// Subreddit or subreddit names joined by + (Reddit source).
        #[arg(long)]
        community: Option<String>,
    },
    /// Retrieve a platform ranking. Measured momentum is not yet implemented.
    Trends {
        #[arg(long, value_delimiter = ',', default_value = "hn")]
        sources: Vec<String>,
        #[arg(long, default_value="top", value_parser=["top","new","best","ask","show","hot","rising","popular"])]
        feed: String,
        #[arg(long, default_value="20", value_parser=parse_positive)]
        limit: usize,
        #[arg(long)]
        community: Option<String>,
        /// Reddit ranking period; HN feed ranking is unchanged.
        #[arg(long, default_value="day", value_parser=["hour","day","week","month","year","all"])]
        period: String,
    },
    /// Read a Hacker News, Reddit, or GitHub issue conversation.
    Thread {
        /// Numeric HN item ID or an HN item URL.
        input: String,
        #[arg(long, default_value="100", value_parser=parse_positive)]
        limit: usize,
    },
    /// Retrieve an available YouTube caption track, retaining metadata on failure.
    Transcript {
        input: String,
        #[arg(long, default_value = "en")]
        language: String,
    },
    /// Navigate, inspect, and interact through a persistent Chromium session.
    Browser {
        #[command(subcommand)]
        action: browser::Action,
    },
    /// Query a saved run, a JSON/JSONL file, or stdin ('-').
    Query { input: String },
    /// Inspect a saved run, including attempts, errors, and provenance.
    Inspect { run_id: String },
    /// Write original bytes for a stored SHA-256 artifact to stdout.
    Artifact { hash: String },
    /// Describe available sources, operations, schema, and limitations.
    Capabilities,
}

#[derive(Args)]
struct FetchArgs {
    #[arg(required = true)]
    urls: Vec<String>,
    #[arg(long, value_enum, default_value = "article")]
    extract: Extract,
    #[arg(long)]
    css: Option<String>,
    #[arg(long, value_enum, default_value = "auto")]
    archive: Archive,
    #[arg(long, default_value="25", value_parser=parse_positive)]
    archive_limit: usize,
    /// Native enrichment. Comments support HN, Reddit, and GitHub issues.
    #[arg(long, value_delimiter=',', value_parser=["article","comments","transcript"])]
    include: Vec<String>,
    /// Caption language when --include transcript is used for YouTube.
    #[arg(long, default_value = "en")]
    language: String,
    /// Maximum comments per native thread.
    #[arg(long, default_value="100", value_parser=parse_positive)]
    limit: usize,
}

fn parse_positive(value: &str) -> std::result::Result<usize, String> {
    let n = value
        .parse::<usize>()
        .map_err(|_| "expected a positive integer")?;
    if n == 0 {
        Err("value must be greater than zero".into())
    } else {
        Ok(n)
    }
}

fn data_dir(cli: &Cli) -> Result<PathBuf> {
    if let Some(path) = &cli.data_dir {
        return Ok(path.clone());
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("ffetch"));
    }
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("set --data-dir when HOME is unavailable")?)
            .join(".local/share/ffetch"),
    )
}

fn endpoint(name: &str, default: &str) -> Result<Url> {
    net::normalize(&std::env::var(name).unwrap_or_else(|_| default.into()))
        .with_context(|| format!("invalid {name}"))
}

fn since_timestamp(input: Option<&str>) -> Result<Option<i64>> {
    input
        .map(|input| {
            if let Some(raw) = input.strip_suffix('h').or_else(|| input.strip_suffix('d')) {
                let count = raw
                    .parse::<i64>()
                    .context("--since expects 24h, 7d, or an RFC3339 timestamp")?;
                ensure!(count > 0, "--since duration must be positive");
                let multiplier = if input.ends_with('d') { 86400 } else { 3600 };
                let seconds = count
                    .checked_mul(multiplier)
                    .context("--since duration is too large")?;
                chrono::Utc::now()
                    .timestamp()
                    .checked_sub(seconds)
                    .context("--since duration is too large")
            } else {
                Ok(chrono::DateTime::parse_from_rfc3339(input)?.timestamp())
            }
        })
        .transpose()
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|s| {
        let s = s.to_string_lossy();
        !s.starts_with('-') && (s.contains("://") || s.contains('.'))
    }) {
        args.insert(1, "fetch".into());
    }
    let cli = Cli::parse_from(args);
    let result = (|| {
        if let Command::Fulltext { input } = &cli.command {
            ensure!(
                ffetch::fulltext::pmcid(input).is_some(),
                "fulltext requires a PMCID or supported PMC article URL"
            );
        }
        if let Command::References { doi, .. } = &cli.command {
            ensure!(
                ffetch::citations::doi(doi).is_some(),
                "references requires a DOI or DOI resolver URL"
            );
        }
        if let Command::Search {
            query,
            queries,
            sources,
            max_searches,
            ..
        } = &cli.command
        {
            let mode = if cli.deep {
                SearchMode::Deep
            } else {
                cli.search_mode
            };
            let sources = discovery::plan(
                sources,
                &cli.engines,
                mode,
                std::env::var("FFETCH_SEARXNG_URL").is_ok(),
            )?;
            discovery::query_plan(query, queries, &sources, *max_searches)?;
        }
        ensure!(
            cli.search_pages == 1 || matches!(cli.command, Command::Search { .. }),
            "--search-pages applies to search"
        );
        ensure!(
            cli.search_pages == 1 || cli.deep || cli.search_mode != SearchMode::Fast,
            "--search-pages requires balanced or deep search mode"
        );
        ensure!(
            cli.sample.is_none()
                || matches!(
                    cli.command,
                    Command::Fetch(_)
                        | Command::Search { .. }
                        | Command::Trends { .. }
                        | Command::Thread { .. }
                        | Command::Transcript { .. }
                        | Command::References { .. }
                        | Command::Fulltext { .. }
                        | Command::Pdf { .. }
                ),
            "--sample applies to fetch, search, trends, thread, transcript, references, or PDF output"
        );
        let query = query::Query::compile(cli.jq.as_deref(), cli.jmespath.as_deref())?;
        if let Some(code) = execute_local(&cli, &query)? {
            return Ok(code);
        }
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(execute(&cli, &query))
    })();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            // Broken pipes are normal for Unix pipelines such as `ffetch ... | head`.
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("ffetch: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn execute_local(cli: &Cli, query: &query::Query) -> Result<Option<u8>> {
    if let Command::PdfWorker {
        input,
        output,
        url,
        page,
        limit,
    } = &cli.command
    {
        ffetch::pdf::worker(input, output, url, *page, *limit)?;
        return Ok(Some(0));
    }
    if matches!(cli.command, Command::Capabilities) {
        output(cli, query, capabilities())?;
        return Ok(Some(0));
    }
    let store = Store::new(data_dir(cli)?);
    match &cli.command {
        Command::Search {
            query: primary,
            queries,
            sources,
            max_searches,
            dry_run: true,
            limit,
            ..
        } => {
            let mode = if cli.deep {
                SearchMode::Deep
            } else {
                cli.search_mode
            };
            let sources = discovery::plan(
                sources,
                &cli.engines,
                mode,
                std::env::var("FFETCH_SEARXNG_URL").is_ok(),
            )?;
            let plan = discovery::query_plan(primary, queries, &sources, *max_searches)?;
            let mut value = serde_json::to_value(plan)?;
            value["schema"] = json!(model::SCHEMA_VERSION);
            value["operation"] = json!("search_plan");
            value["max_searches"] = json!(max_searches);
            value["limit_per_query_source"] = json!(limit);
            value["page_budget"] = json!(cli.search_pages);
            output(cli, query, value)?;
            return Ok(Some(0));
        }
        Command::Query { input } => {
            let value = if uuid::Uuid::parse_str(input).is_ok() {
                serde_json::to_value(store.load(input)?)?
            } else {
                let mut bytes = Vec::new();
                if input == "-" {
                    io::stdin().read_to_end(&mut bytes)?;
                } else {
                    bytes = std::fs::read(input)?;
                }
                let mut values = serde_json::Deserializer::from_slice(&bytes)
                    .into_iter::<Value>()
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                ensure!(!values.is_empty(), "query input is empty");
                if values.len() == 1 {
                    values.remove(0)
                } else {
                    Value::Array(values)
                }
            };
            output(cli, query, value)?;
            return Ok(Some(0));
        }
        Command::Inspect { run_id } => {
            output(cli, query, serde_json::to_value(store.load(run_id)?)?)?;
            return Ok(Some(0));
        }
        Command::Artifact { hash } => {
            io::stdout().lock().write_all(&store.read_blob(hash)?)?;
            return Ok(Some(0));
        }
        _ => (),
    }
    Ok(None)
}

async fn execute(cli: &Cli, query: &query::Query) -> Result<u8> {
    let store = Store::new(data_dir(cli)?);
    let timeout = Duration::from_secs(u64::try_from(cli.timeout)?);
    let http = Http::with_limits(
        timeout,
        cli.max_bytes,
        cli.concurrency,
        cli.origin_concurrency,
    )?;
    let browser_config = browser::Config {
        root: data_dir(cli)?,
        executable: cli.browser_path.clone(),
        timeout,
        max_bytes: cli.max_bytes,
        no_sandbox: cli.browser_no_sandbox,
    };
    let renderer = Renderer::new(browser_config.clone(), cli.session.clone(), None);
    let fetcher = Fetcher {
        pdf_worker: Some(std::env::current_exe()?),
        http: http.clone(),
        browser: (cli.browser != browser::Mode::Off).then_some(renderer.clone()),
        browser_mode: cli.browser,
        store: (!cli.no_store).then_some(store.clone()),
        budget: timeout,
        cdx: endpoint(
            "FFETCH_WAYBACK_CDX",
            "https://web.archive.org/cdx/search/cdx",
        )?,
        replay: endpoint("FFETCH_WAYBACK_REPLAY", "https://web.archive.org/")?,
        archive_limit: match &cli.command {
            Command::Fetch(args) => args.archive_limit,
            _ => 25,
        },
    };
    let providers = Providers {
        http,
        browser: (cli.browser != browser::Mode::Off).then_some(renderer),
        browser_only: cli.browser == browser::Mode::Always,
        hn_base: endpoint("FFETCH_HN_BASE", "https://hacker-news.firebaseio.com/v0/")?,
        hn_search: endpoint("FFETCH_HN_SEARCH", "https://hn.algolia.com/api/v1/search")?,
        web_search: endpoint("FFETCH_WEB_SEARCH", "https://html.duckduckgo.com/html/")?,
        brave_search: endpoint("FFETCH_BRAVE_SEARCH", "https://search.brave.com/search")?,
        searxng: std::env::var("FFETCH_SEARXNG_URL")
            .ok()
            .map(|s| net::normalize(&s))
            .transpose()?,
        concurrency: cli.concurrency,
        web_pages: cli.search_pages as usize,
        budget: timeout,
        youtube_region: cli.region.clone(),
        reddit_community: match &cli.command {
            Command::Search { community, .. } | Command::Trends { community, .. } => {
                community.clone()
            }
            _ => None,
        },
    };
    let start = Instant::now();
    let mut live = LiveOutput::new(cli);
    let mut run = match &cli.command {
        Command::Pdf { url, page, limit } => {
            ffetch::pdf::fetch(
                &providers.http,
                url,
                *page,
                *limit,
                (!cli.no_store).then_some(&store),
                timeout,
                &std::env::current_exe()?,
            )
            .await
        }
        Command::Fulltext { input } => {
            ffetch::fulltext::fetch(
                &providers.http,
                input,
                (!cli.no_store).then_some(&store),
                timeout,
            )
            .await
        }
        Command::References { doi, limit, depth } => {
            ffetch::citations::references(
                &providers.http,
                doi,
                *depth,
                *limit,
                cli.concurrency,
                timeout,
            )
            .await
        }
        Command::Browser { action } => {
            browser_config
                .execute(
                    cli.session.as_deref().unwrap_or("default"),
                    action.clone(),
                    (!cli.no_store).then_some(&store),
                )
                .await
        }
        Command::Fetch(args) => {
            ensure!(
                !args.include.iter().any(|s| s == "transcript")
                    || (matches!(args.extract, Extract::Article)
                        && args.css.is_none()
                        && args.archive != Archive::LatestValid),
                "Transcript enrichment requires article mode without CSS scope or archive-only lookup"
            );
            if let Some(css) = &args.css {
                extract::selector(css)?;
            }
            let options = Options {
                mode: args.extract,
                css: args.css.clone(),
            };
            let mut urls = args.urls.clone();
            if urls.iter().any(|u| u == "-") {
                urls.retain(|u| u != "-");
                let mut text = String::new();
                io::stdin().read_to_string(&mut text)?;
                urls.extend(
                    text.lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && !s.starts_with('#'))
                        .map(str::to_owned),
                );
            }
            ensure!(!urls.is_empty(), "no URLs were provided");
            let mut run = Run::new("fetch");
            run.meta = json!({"requested_urls":urls,"concurrency":cli.concurrency});
            let tasks = urls.iter().map(|url| {
                let fetcher = &fetcher;
                let providers = &providers;
                let options = &options;
                async move {
                    let archive = ffetch::github::archive_policy(url, args.archive);
                    if args.include.iter().any(|s| s == "transcript")
                        && ffetch::youtube::video_id(url).is_none()
                    {
                        let mut result = fetcher.fetch(url, options, archive).await;
                        result.errors.push(Issue::new(
                            "web",
                            "unsupported_enrichment",
                            "Transcript enrichment currently supports YouTube video URLs only",
                        ));
                        return result;
                    }
                    let id = net::normalize(url)
                        .ok()
                        .and_then(|url| providers::hn_id(&url));
                    if let Some(target) = ffetch::github::body_target(url).filter(|_| {
                        cli.browser != browser::Mode::Always
                            && cli.session.is_none()
                            && options.css.is_none()
                            && matches!(options.mode, Extract::Article)
                            && archive != Archive::LatestValid
                    }) {
                        ffetch::reading::github_read(
                            fetcher,
                            url,
                            &target,
                            args.include.iter().any(|s| s == "comments"),
                            args.limit,
                            archive,
                        )
                        .await
                    } else if let Some(id) = ffetch::arxiv::paper_id(url).filter(|_| {
                        !net::normalize(url).is_ok_and(|url| url.path().starts_with("/pdf/"))
                            && options.css.is_none()
                            && matches!(options.mode, Extract::Article)
                            && archive != Archive::LatestValid
                    }) {
                        providers.arxiv_paper(&id).await
                    } else if let Some(id) = ffetch::youtube::video_id(url).filter(|_| {
                        options.css.is_none()
                            && matches!(options.mode, Extract::Article)
                            && archive != Archive::LatestValid
                    }) {
                        let transcript = args.include.iter().any(|s| s == "transcript");
                        let mut video = providers
                            .youtube_video(&id, transcript.then_some(args.language.as_str()))
                            .await;
                        if args.include.iter().any(|s| s == "comments") {
                            video.errors.push(Issue::new(
                                "youtube",
                                "unsupported_enrichment",
                                "YouTube comments are not implemented",
                            ));
                        }
                        if video.items.is_empty() && !transcript {
                            let mut fallback = fetcher.fetch(url, options, archive).await;
                            if args.include.iter().any(|s| s == "comments") {
                                fallback.errors.extend(video.errors);
                            } else {
                                fallback.warnings.extend(
                                    video
                                        .errors
                                        .into_iter()
                                        .map(|e| format!("YouTube metadata failed: {}", e.message)),
                                );
                            }
                            fallback.attempts.extend(video.attempts);
                            fallback
                        } else {
                            video
                        }
                    } else if let Some(id) = ffetch::reddit::thread_id(url).filter(|_| {
                        options.css.is_none()
                            && matches!(options.mode, Extract::Article)
                            && archive != Archive::LatestValid
                    }) {
                        let comments = args.include.iter().any(|s| s == "comments");
                        let mut thread = providers.reddit_thread(&id, comments, args.limit).await;
                        if thread.items.is_empty() {
                            let mut fallback = fetcher.fetch(url, options, archive).await;
                            fallback.meta["reddit_native_available"] = json!(false);
                            let mut attempts = thread.attempts;
                            attempts.append(&mut fallback.attempts);
                            fallback.attempts = attempts;
                            if comments {
                                fallback.errors.extend(thread.errors);
                            } else {
                                fallback.warnings.extend(
                                    thread.errors.into_iter().map(|e| {
                                        format!("Reddit native route failed: {}", e.message)
                                    }),
                                );
                            }
                            fallback
                        } else {
                            if args.include.iter().any(|s| s == "article")
                                && let Some(link) = thread
                                    .items
                                    .first()
                                    .and_then(|r| r.content["article_url"].as_str())
                                    .map(str::to_owned)
                            {
                                thread.merge(fetcher.fetch(&link, options, archive).await);
                            }
                            thread
                        }
                    } else if let Some(id) = id.filter(|_| {
                        options.css.is_none()
                            && matches!(options.mode, Extract::Article)
                            && archive != Archive::LatestValid
                    }) {
                        let mut thread = providers
                            .hn_thread(id, args.include.iter().any(|s| s == "comments"), args.limit)
                            .await;
                        if args.include.iter().any(|s| s == "article")
                            && let Some(url) = thread
                                .items
                                .first()
                                .and_then(|r| r.content["article_url"].as_str())
                                .map(str::to_owned)
                        {
                            thread.merge(fetcher.fetch(&url, options, archive).await);
                        }
                        thread
                    } else {
                        fetcher.fetch(url, options, archive).await
                    }
                }
            });
            let mut pending = stream::iter(tasks).buffer_unordered(cli.concurrency);
            while let Some(child) = pending.next().await {
                run.meta["targets"]
                    .as_array_mut()
                    .map(|v| v.push(child.meta.clone()))
                    .unwrap_or_else(|| {
                        run.meta["targets"] = json!([child.meta]);
                    });
                live.records(&run.run_id, &child.items, &cli.fields)?;
                run.merge(child);
            }
            run
        }
        Command::Search {
            query,
            queries,
            max_searches,
            dry_run: _,
            sources,
            limit,
            since,
            read,
            community: _,
        } => {
            let since = since_timestamp(since.as_deref())?;
            let mut run = Run::new("search");
            run.meta = json!({"query":query,"sources":sources,"limit_per_source":limit});
            let search_mode = if cli.deep {
                SearchMode::Deep
            } else {
                cli.search_mode
            };
            let planned = discovery::plan(
                sources,
                &cli.engines,
                search_mode,
                providers.searxng.is_some(),
            )?;
            run.meta["engine_plan"] = json!(planned);
            run.meta["preferred_engine"] =
                json!(planned.iter().find_map(|s| s.strip_prefix("engine:")));
            run.meta["search_mode"] = json!(format!("{search_mode:?}").to_lowercase());
            let query_plan = discovery::query_plan(query, queries, &planned, *max_searches)?;
            run.meta["query_plan"] = json!(query_plan);
            run.meta["max_searches"] = json!(max_searches);
            let mut fusion = Fusion::default();
            let stream_pages = *read || live.enabled();
            let (progress, mut pages) = tokio::sync::mpsc::channel(cli.concurrency.clamp(1, 32));
            let tasks = query_plan
                .jobs
                .iter()
                .map(|job| {
                    let source = &job.source;
                    let query = &query_plan.queries[job.query_index];
                    let progress = stream_pages.then(|| progress.clone());
                    let providers = &providers;
                    async move {
                        if let Some(progress) = progress {
                            providers
                                .search_with_progress(source, query, *limit, since, progress)
                                .await
                        } else {
                            providers.search(source, query, *limit, since).await
                        }
                    }
                })
                .collect::<Vec<_>>();
            drop(progress);
            let mut searches = stream::iter(tasks).buffer_unordered(cli.concurrency);
            let mut documents = futures::stream::FuturesUnordered::new();
            let mut targets: std::collections::VecDeque<ffetch::reading::Target> =
                std::collections::VecDeque::new();
            let mut read_plan = ffetch::reading::Planner::default();
            let mut coverage = Vec::new();
            let mut searching = true;
            let mut receiving_pages = stream_pages;
            let mut received_ids = std::collections::HashSet::new();
            loop {
                while documents.len() < cli.concurrency {
                    let Some(target) = targets.pop_front() else {
                        break;
                    };
                    let fetcher = &fetcher;
                    let providers = &providers;
                    documents.push(
                        async move {
                            let fetched = if target.pmcid.is_some() {
                                ffetch::reading::fulltext_read(fetcher, &target).await
                            } else if let Some(github) = ffetch::github::body_target(&target.url)
                                .filter(|_| {
                                    cli.browser != browser::Mode::Always && cli.session.is_none()
                                })
                            {
                                ffetch::reading::github_read(
                                    fetcher,
                                    &target.url,
                                    &github,
                                    false,
                                    100,
                                    Archive::Auto,
                                )
                                .await
                            } else if ffetch::arxiv::paper_id(&target.url).is_some() {
                                ffetch::reading::arxiv_read(fetcher, &target.url).await
                            } else if let Some(id) = ffetch::youtube::video_id(&target.url) {
                                providers.youtube_video(&id, None).await
                            } else {
                                fetcher
                                    .fetch(
                                        &target.url,
                                        &Options::default(),
                                        ffetch::github::archive_policy(&target.url, Archive::Auto),
                                    )
                                    .await
                            };
                            (target.parent, fetched)
                        }
                        .boxed_local(),
                    );
                }
                if !searching && !receiving_pages && documents.is_empty() && targets.is_empty() {
                    break;
                }
                let mut discovered = Vec::new();
                tokio::select! {
                    child = searches.next(), if searching => {
                        let Some(mut child) = child else { searching = false; continue; };
                        coverage.push(json!({"status":child.status,"meta":child.meta,"errors":child.errors}));
                        discovered = std::mem::take(&mut child.items);
                        run.merge(child);
                    }
                    page = pages.recv(), if receiving_pages => {
                        match page {
                            Some(records) => discovered = records,
                            None => receiving_pages = false,
                        }
                    }
                    document = documents.next(), if !documents.is_empty() => {
                        if let Some((parent, mut fetched)) = document {
                            run.meta["read_coverage"].as_array_mut().map(|entries|entries.push(json!({"parent":parent,"status":fetched.status,"meta":fetched.meta,"errors":fetched.errors}))).unwrap_or_else(||run.meta["read_coverage"]=json!([{"parent":parent,"status":fetched.status,"meta":fetched.meta,"errors":fetched.errors}]));
                            for record in &mut fetched.items {
                                if !record.native.is_object() { record.native = json!({}); }
                                record.native["discovered_from"] = json!(parent);
                            }
                            live.records(&run.run_id, &fetched.items, &cli.fields)?;
                            run.merge(fetched);
                        }
                    }
                }
                // The terminal provider run is authoritative and also contains
                // pages already delivered. Stable IDs prevent duplicate events,
                // while retaining any page whose send was cut short by timeout.
                if stream_pages {
                    discovered.retain(|r| {
                        r.kind != "search_result"
                            || r.source != "web"
                            || received_ids.insert(r.id.clone())
                    });
                }
                let changed = fusion.merge_records(&mut run, discovered);
                for &index in &changed {
                    live.upserts(&run.run_id, &run.items[index..index + 1], &cli.fields)?;
                }
                if *read {
                    for index in changed {
                        let record = &run.items[index];
                        if let Some(target) = read_plan.consider(record) {
                            targets.push_back(target);
                        }
                    }
                }
            }
            run.meta["coverage"] = json!(coverage);
            fusion.finish(&mut run);
            run
        }
        Command::Trends {
            sources,
            feed,
            limit,
            community: _,
            period,
        } => {
            let mut run = Run::new("trends");
            run.meta = json!({"sources":sources,"feed":feed,"ranking":"platform","momentum_measured":false});
            for source in sources {
                if source == "hn" {
                    let child = providers.hn_feed(feed, *limit).await;
                    live.records(&run.run_id, &child.items, &cli.fields)?;
                    run.merge(child);
                } else if source == "youtube" {
                    if !matches!(feed.as_str(), "top" | "popular") {
                        run.errors.push(Issue::new(
                            "youtube",
                            "unsupported_feed",
                            "YouTube supports the mostPopular chart via --feed popular or top",
                        ));
                    } else {
                        let child = providers.youtube_popular(*limit).await;
                        live.records(&run.run_id, &child.items, &cli.fields)?;
                        run.meta["youtube"] = child.meta.clone();
                        run.merge(child);
                    }
                } else if source == "reddit" {
                    let child = providers.reddit_feed(feed, period, *limit).await;
                    live.records(&run.run_id, &child.items, &cli.fields)?;
                    run.meta["reddit"] = child.meta.clone();
                    run.merge(child);
                } else {
                    run.errors.push(Issue::new(
                        source,
                        "unsupported_source",
                        "Supported ranking sources: hn, reddit",
                    ));
                }
            }
            run
        }
        Command::Transcript { input, language } => {
            let id = ffetch::youtube::video_id(input)
                .context("transcript requires a supported YouTube video URL")?;
            let mut video = providers.youtube_video(&id, Some(language)).await;
            video.operation = "transcript".into();
            video
        }
        Command::Thread { input, limit } => {
            if let Some(github) = ffetch::github::target(input).filter(|t| t.number.is_some()) {
                ffetch::github::fetch(
                    &providers.http,
                    &github,
                    true,
                    *limit,
                    fetcher.store.as_ref(),
                    timeout,
                )
                .await
            } else if let Some(id) = ffetch::reddit::thread_id(input) {
                providers.reddit_thread(&id, true, *limit).await
            } else {
                let id = input
                    .parse::<u64>()
                    .ok()
                    .or_else(|| {
                        net::normalize(input)
                            .ok()
                            .and_then(|url| providers::hn_id(&url))
                    })
                    .context("thread expects an HN ID/URL, a Reddit URL, reddit:POST_ID, or a GitHub issue/pull URL")?;
                providers.hn_thread(id, true, *limit).await
            }
        }
        _ => unreachable!(),
    };
    run.meta["http"] = serde_json::to_value(providers.http.stats())?;
    run.finish(start.elapsed().as_millis() as u64);
    if !cli.no_store {
        store.save(&run)?;
    }
    let code = run.exit_code();
    if let Some(size) = cli.sample {
        let mut sample = ffetch::sampling::Reservoir::new(size, cli.sample_seed)?;
        let population = run.items.len();
        for (index, record) in run.items.iter().enumerate() {
            sample.push((index, record.id.clone()))?;
        }
        let mut selected = sample.into_items();
        selected.sort_by_key(|(index, _)| *index);
        let ids = selected
            .iter()
            .map(|(_, id)| id.clone())
            .collect::<std::collections::HashSet<_>>();
        run.items.retain(|r| ids.contains(&r.id));
        run.meta["sampling"] = json!({"algorithm":"reservoir-r", "prng":"splitmix64-rejection-v1", "seed":cli.sample_seed, "population_records":population,"selected_records":run.items.len(),"requested":size,"scope":"gathered output records; not the web population","full_run_saved":!cli.no_store});
        if let Some(ranking) = run.meta.get_mut("ranking") {
            ranking["ordered_record_ids"] =
                json!(run.items.iter().map(|r| &r.id).collect::<Vec<_>>());
        }
    }
    if code != 0 {
        eprintln!(
            "ffetch: {} ({} records, {} errors); run {}",
            run.status,
            run.items.len(),
            run.errors.len(),
            run.run_id
        );
    }
    if live.enabled() {
        live.records(&run.run_id, &run.items, &cli.fields)?;
        live.complete(&run)?;
    } else {
        output(cli, query, serde_json::to_value(run)?)?;
    }
    Ok(code)
}

/// Synchronous bounded writes naturally backpressure the coordinator: producers
/// are polled here rather than spawned into an unbounded background queue.
struct LiveOutput {
    writer: Option<io::BufWriter<io::Stdout>>,
    emitted: std::collections::HashSet<String>,
}

impl LiveOutput {
    fn new(cli: &Cli) -> Self {
        Self {
            writer: (matches!(cli.format, Format::Jsonl)
                && cli.jq.is_none()
                && cli.jmespath.is_none()
                && cli.sample.is_none())
            .then(|| io::BufWriter::new(io::stdout())),
            emitted: Default::default(),
        }
    }
    fn enabled(&self) -> bool {
        self.writer.is_some()
    }
    fn records(
        &mut self,
        run_id: &str,
        records: &[model::Record],
        fields: &[String],
    ) -> Result<()> {
        self.write_records(run_id, records, fields, false)
    }
    fn upserts(
        &mut self,
        run_id: &str,
        records: &[model::Record],
        fields: &[String],
    ) -> Result<()> {
        self.write_records(run_id, records, fields, true)
    }
    fn write_records(
        &mut self,
        run_id: &str,
        records: &[model::Record],
        fields: &[String],
        updates: bool,
    ) -> Result<()> {
        let Some(writer) = &mut self.writer else {
            return Ok(());
        };
        #[derive(serde::Serialize)]
        struct Event<'a, T> {
            r#type: &'static str,
            run_id: &'a str,
            item: T,
        }
        for record in records {
            let fresh = self.emitted.insert(record.id.clone());
            if !fresh && !updates {
                continue;
            }
            let event_type = if fresh { "record" } else { "record_update" };
            if fields.is_empty() {
                serde_json::to_writer(
                    &mut *writer,
                    &Event {
                        r#type: event_type,
                        run_id,
                        item: record,
                    },
                )?;
            } else {
                let mut projected = json!({"items":[record]});
                query::project(&mut projected, fields);
                serde_json::to_writer(
                    &mut *writer,
                    &Event {
                        r#type: event_type,
                        run_id,
                        item: &projected["items"][0],
                    },
                )?;
            }
            writeln!(writer)?;
        }
        // Flush completed groups immediately: don't hide a useful record behind
        // a slow sibling or a buffer that has not filled yet.
        writer.flush()?;
        Ok(())
    }
    fn complete(&mut self, run: &Run) -> Result<()> {
        let Some(writer) = &mut self.writer else {
            return Ok(());
        };
        #[derive(serde::Serialize)]
        struct Summary<'a> {
            schema: &'a str,
            run_id: &'a str,
            operation: &'a str,
            status: &'a str,
            started_at: &'a str,
            elapsed_ms: u64,
            errors: &'a [Issue],
            warnings: &'a [String],
            attempts: &'a [model::Attempt],
            meta: &'a Value,
        }
        #[derive(serde::Serialize)]
        struct Complete<'a> {
            r#type: &'static str,
            result: Summary<'a>,
        }
        serde_json::to_writer(
            &mut *writer,
            &Complete {
                r#type: "complete",
                result: Summary {
                    schema: &run.schema,
                    run_id: &run.run_id,
                    operation: &run.operation,
                    status: &run.status,
                    started_at: &run.started_at,
                    elapsed_ms: run.elapsed_ms,
                    errors: &run.errors,
                    warnings: &run.warnings,
                    attempts: &run.attempts,
                    meta: &run.meta,
                },
            },
        )?;
        writeln!(writer)?;
        writer.flush()?;
        Ok(())
    }
}

fn output(cli: &Cli, query: &query::Query, mut value: Value) -> Result<()> {
    if !cli.fields.is_empty() {
        query::project(&mut value, &cli.fields);
    }
    let values = query.run(value)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for value in values {
        if cli.raw_output
            && let Some(s) = value.as_str()
        {
            writeln!(out, "{s}")?;
            continue;
        }
        match cli.format {
            Format::Json => {
                serde_json::to_writer_pretty(&mut out, &value)?;
                writeln!(out)?;
            }
            Format::Jsonl => {
                if let Some(items) = value["items"].as_array() {
                    for item in items {
                        serde_json::to_writer(
                            &mut out,
                            &json!({"type":"record","run_id":value["run_id"],"item":item}),
                        )?;
                        writeln!(out)?;
                    }
                    let mut meta = value.clone();
                    meta.as_object_mut().unwrap().remove("items");
                    serde_json::to_writer(&mut out, &json!({"type":"complete","result":meta}))?;
                    writeln!(out)?;
                } else {
                    serde_json::to_writer(&mut out, &value)?;
                    writeln!(out)?;
                }
            }
            Format::Text | Format::Markdown => {
                if let Some(items) = value["items"].as_array() {
                    for item in items {
                        if let Some(t) = item["title"].as_str() {
                            writeln!(out, "{t}")?;
                        }
                        if let Some(url) = item["url"].as_str() {
                            writeln!(out, "{url}")?;
                        }
                        if let Some(depth) = item["native"]["citation_depth"].as_u64() {
                            writeln!(out, "Citation depth: {depth}")?;
                        }
                        if item["native"]["fulltext_fallback"] == true {
                            writeln!(out, "[Fallback: full-text completeness unverified]")?;
                        }
                        if let Some(origins) = item["native"]["origins"].as_array() {
                            let engines = origins
                                .iter()
                                .filter_map(|o| o["engine"].as_str())
                                .collect::<std::collections::BTreeSet<_>>();
                            if !engines.is_empty() {
                                writeln!(
                                    out,
                                    "Found by: {}",
                                    engines.into_iter().collect::<Vec<_>>().join(", ")
                                )?;
                            }
                        }
                        if let Some(queries) = item["native"]["discovery_queries"]
                            .as_array()
                            .filter(|q| q.len() > 1)
                        {
                            writeln!(out, "Queries: {}", serde_json::to_string(queries)?)?;
                        }
                        if let Some(time) = item["provenance"]["captured_at"].as_str() {
                            writeln!(out, "[Historical capture: {time}]")?;
                        }
                        if let Some(text) = item["text"].as_str() {
                            writeln!(out, "{text}\n")?;
                        } else if !item["content"].is_null() {
                            writeln!(out, "{}\n", serde_json::to_string_pretty(&item["content"])?)?;
                        }
                    }
                    for key in ["warnings", "errors"] {
                        if let Some(items) = value[key].as_array() {
                            for item in items {
                                writeln!(out, "{key}: {item}")?;
                            }
                        }
                    }
                } else if let Some(s) = value.as_str() {
                    writeln!(out, "{s}")?;
                } else {
                    writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
                }
            }
            Format::Csv => write_csv(&mut out, &value)?,
        }
    }
    out.flush()?;
    Ok(())
}

fn write_csv(out: &mut impl Write, value: &Value) -> Result<()> {
    let records = value
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .context("CSV requires an array of objects or a result envelope; use --jq or --fields")?;
    let cell = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    if records.iter().any(|r| r["kind"] == "table") {
        ensure!(
            records.len() == 1,
            "CSV table export needs one table; use --css to select it"
        );
        let rows = records[0]["content"]["rows"]
            .as_array()
            .context("table missing rows")?;
        for row in rows {
            let cells = row.as_array().context("table row is not an array")?;
            writeln!(
                out,
                "{}",
                cells
                    .iter()
                    .map(|v| cell(v["text"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
        }
        return Ok(());
    }
    let mut headers = std::collections::BTreeSet::new();
    for record in records {
        headers.extend(
            record
                .as_object()
                .context("CSV rows must be objects")?
                .keys()
                .cloned(),
        );
    }
    writeln!(
        out,
        "{}",
        headers
            .iter()
            .map(|s| cell(s))
            .collect::<Vec<_>>()
            .join(",")
    )?;
    for record in records {
        writeln!(
            out,
            "{}",
            headers
                .iter()
                .map(|h| {
                    let v = &record[h];
                    cell(&if v.is_null() {
                        String::new()
                    } else if let Some(s) = v.as_str() {
                        s.into()
                    } else {
                        v.to_string()
                    })
                })
                .collect::<Vec<_>>()
                .join(",")
        )?;
    }
    Ok(())
}

fn capabilities() -> Value {
    json!({
        "schema":model::SCHEMA_VERSION,"version":env!("CARGO_PKG_VERSION"),
        "sources":{"web":{"fetch":true,"search":true,"search_providers":["brave-html","duckduckgo-html","google-html","bing-html","yandex-html","baidu-html","searxng (configured)"],"pagination":{"engines":["brave","bing","google"],"flag":"--search-pages","default_pages":1,"maximum_pages":20,"page_streaming":true,"progress_queue_bounded":true,"full_run_collected":true,"coverage":"bounded; missing next link does not prove exhaustion"},"default_mode":"balanced","deep":"concurrent engine fanout with reciprocal-rank fusion and origins; HTML availability varies","fast":"Brave starts first; DuckDuckGo hedges after 300ms; first validated response wins"},"reddit":{"search":true,"community":true,"feeds":["hot","new","top","rising"],"threads":true,"morechildren_expansion":false,"oauth_bearer_optional":true,"public_access_may_be_blocked":true,"blocked_search_fallback":"scoped web discovery with partial status and no native metrics"},"arxiv":{"search":true,"pagination":true,"versioned_metadata":true,"abstracts":true,"pdf_links":true,"pdf_text":true,"search_read":"versioned PDF; no repeated Atom lookup"},"crossref":{"search":true,"cursor_pagination":true,"metadata":true,"full_text":false},"europepmc":{"search":true,"cursor_pagination":true,"abstracts_when_available":true},"pubmed":{"search":true,"adapter":"europepmc SRC:MED","abstracts_when_available":true},"hn":{"search":true,"feeds":["top","new","best","ask","show"],"threads":true,"search_pagination":true},"github":{"repository_search":true,"issue_search_source":"github-issues","code_search_source":"github-code","code_search_authentication_required":true,"code_search_full_files":false,"repository_metadata":true,"readme_text":true,"parallel_metadata_readme":true,"issue_conversations":true,"pull_request_review_comments":false,"pagination":true,"search_page_streaming":true,"api_search_result_cap":1000,"search_read_native":true,"since":{"repositories":"pushed_at","issues":"created_at","code":false},"optional_token_env":"FFETCH_GITHUB_TOKEN","credential_origin":"https://api.github.com","local_gh_credentials":false,"live_authenticated_access_validated":false,"api_version":"2022-11-28","ordinary_read_fallback":"HTTP/browser/archive under the same deadline; API error retained"},"wayback":{"recovery":true,"validation":true,"global_latest_guarantee":false}},
        "pdf":{"command":"pdf","parser":"lopdf 0.46","external_runtime":false,"isolated_worker":true,"unix_address_space_limit_bytes":536870912,"default_pages":20,"maximum_pages":500,"ocr":false,"layout_verified":false,"automatic_fetch":true,"automatic_modes":["article","text"],"detection":"response signature or MIME type; not URL suffix","single_download":true,"reuse_stored_artifact":true,"search_read_automatic":true},
        "fulltext":{"command":"fulltext","provider":"europepmc","input":"PMCID or supported PMC article URL","format":"JATS XML","body_text":true,"blocks":true,"tables":true,"original_artifact":true,"pdf":false,"images":false,"math_layout_complete":false,"external_entities":false,"search_read_automatic":true,"trigger":"PMCID metadata or supported PMC URL","doi_assertion_check":true},
        "citations":{"command":"references","provider":"crossref","direction":"references","maximum_depth":3,"default_reference_budget":20,"lookup_bound":"one seed plus at most the reference-entry budget","fuzzy_matching":false,"full_text":false,"graph":"meta.edges","streaming":false},
        "search_planning":{"explicit_query_variants":true,"query_flag":"--query","dry_run":true,"default_max_searches":32,"limit_scope":"per query and source","automatic_expansion":false,"iterative_planning":false,"engine_agreement":"best observed rank per engine across queries"},
        "http":{"https_protocols":["HTTP/2 (ALPN)","HTTP/1.1 fallback"],"http2_multiplexing":true,"global_concurrency_default":8,"origin_concurrency_default":4,"origin_flag":"--origin-concurrency","attempt_timeout_seconds":15,"redirect_limit":10,"redirect_origin_limits":true,"retry_after":{"statuses":[429,503],"max_retries_per_get":1,"valid_header_required":true,"remaining_budget":true,"redirect_delays":true,"cross_process_coordination":false},"telemetry":"meta.http","protocol_telemetry":"responses_by_protocol; client-side response headers observed, not body completion or upstream proxy protocol","duckduckgo_request_profile":"Chrome 152 User-Agent only, scoped to the request origin; no TLS impersonation or JavaScript","browser_network_scheduled":false},
        "browser":{"backend":"chromium-cdp","continuous_dispatch":true,"retained_notifications":"navigation and session lifecycle; subresource data events are not persisted","event_queue":{"maximum_events":ffetch::browser::cdp::EVENT_COUNT,"maximum_wire_bytes":ffetch::browser::cdp::EVENT_BYTES,"wire_byte_limit":"--max-bytes clamped to 64 KiB through 4 MiB","overflow":"explicit failure"},"readiness":"document committed, then visible body text or requested CSS selector; event driven","client_side_redirects":true,"ephemeral_contexts":{"isolated":true,"dispose_on_disconnect":true},"shared_render_deadline":true,"optional_runtime":true,"sessions":true,"navigation":true,"inspection_refs":true,"click_fill_press":true,"screenshots":true,"document_fallback":true,"search_engine_fallback":true},
        "research_identity":{"doi_merge":true,"ascii_case_folding":true,"original_provider_records":true,"field_sources":true,"conflicts":true,"title_based_identity":false,"independent_corroboration_claim":false},
        "query":{"jq":"embedded jaq; not a promise of complete jq compatibility","jmespath":true,"css":true,"fields":true},
        "extract":["article","text","html","links","tables","json"],
        "formats":["json","jsonl","text","markdown","csv"],
        "exit_codes":{"0":"success (possibly an empty result)","1":"retrieval/provider failure","2":"usage/query/storage failure","3":"partial results"},
        "sampling":{"algorithm":"reservoir-r","seeded":true,"stage":"output records after retrieval; full saved run retained","constant_memory_whole_operation":false},
        "limitations":["HTTP transport currently uses reqwest without browser impersonation","Browser rendering requires optional Chrome/Chromium; CAPTCHA/session/network restrictions can still prevent retrieval","Additional social/scientific sources and archive providers are planned; Crossref and Europe PMC metadata search are available","PDF text layers are supported; OCR, audio and CAD semantic parsers are not implemented","Trend output is a platform ranking, not measured momentum","JSONL streams completed targets/sources without whole-run queries; providers may still collect internally","No guarantee of universal access or globally newest archive capture"],
        "storage":"HTTP/DOM artifacts addressed by SHA-256; runs addressed by UUID; --no-store disables evidence writes, not browser profile/session state"
    })
}
