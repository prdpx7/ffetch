# ffetch

A Rust CLI for web search, page retrieval, and research data. It returns JSON for
agents and Markdown or text for terminal use. HTTP requests, extraction, jq,
and JMESPath run inside the executable. JavaScript rendering uses an optional
Chrome or Chromium installation.

## Install

Install a recent Rust toolchain, then run this command from the repository:

```sh
cargo install --path . --locked
```

Linux is tested. macOS and Windows have not been verified.

## Search

```sh
ffetch search "open source robot gripper" --format markdown
```

The default sources are web search and Hacker News. Balanced web search gathers
Brave, Bing, and DuckDuckGo concurrently. Duplicate URLs retain each engine's
rank, snippet, and provenance. Final web ranking uses reciprocal rank fusion.

```sh
ffetch search "robot gripper" --query "underactuated tendon gripper" --query "open source gripper CAD" --sources web,papers --engines brave,bing --max-searches 15
```

Repeat `--query` to search related queries together. Exact duplicates are removed
after trimming outer whitespace. `--dry-run` prints the query/source plan without
network requests or saved files. `--max-searches` limits query/source pairs and
defaults to 32. Paging and HTTP retries are separate from that count.

Results retain query provenance. Repeated discoveries use stable IDs and trigger
updates in JSONL. A shared URL is read once with `--read`. Web ranking uses the
best observed rank from each engine; repeated queries through one engine do not
increase engine agreement. `query_count` and `observation_count` report query
coverage separately. Native search records retain query observations; DOI records
preserve their original provider records. Query-specific errors remain in
`meta.coverage`.

```sh
ffetch search "robot gripper" --sources web --deep --engines google,bing,duckduckgo,yandex,baidu,brave
```

`--deep` expands engine coverage. It does not plan follow-up queries. Use
`--search-mode fast` for the first validated search response per query. `--limit`
applies per query and source, so the merged result count can exceed it. Query
variants are explicit; automatic query expansion and iterative research planning
are not implemented.

```sh
ffetch search "robot gripper" --sources web --engines brave,bing --search-pages 3 --limit 50
```

`--search-pages` allows up to 20 pages per engine, with one page by default.
Bing, Brave, and Google support explicit next-page links. Other web engines
currently read one page. Retrieval stops at the result limit, page budget,
deadline, missing next link, or repeated results. Coverage metadata records the
stop reason. A missing next link does not prove that all results were searched.
Completed pages survive later failures. Named HTML engines emit completed pages
immediately in JSONL, and `--read` can fetch their documents while later result
pages are still loading. Page and position metadata accompany results. Origin
ranks count recognized results before cross-page deduplication. The progress
queue is bounded; full run evidence is still collected in memory.

```sh
ffetch search "robot gripper" --sources web --limit 3 --read --format jsonl
```

`--read` fetches discovered pages while other sources are still searching. JSONL
emits records, updates to existing records, and a final completion event. Check
completion status and coverage before treating a run as finished.

## Fetch and extract

```sh
ffetch fetch https://example.com --format markdown
```

```sh
ffetch fetch https://example.com --extract links --fields title,url
```

```sh
ffetch fetch https://example.com --extract tables --css '#specifications' --format csv
```

Extraction modes are article, text, html, links, tables, and json. `fetch -` reads
one URL per line from stdin. Multiple URLs run concurrently.

Failed public-page retrieval can fall back to Wayback captures. Recovery checks
recent captures and skips recognized error pages. Historical results include the
capture time and archive URL. This does not guarantee the newest capture across
all archives. Use `--archive off` to disable recovery.

`--timeout` sets a per-target or per-provider deadline. `--concurrency` defaults to
8. `--origin-concurrency` defaults to 4 requests per origin within that global
limit. Redirects use the destination's limit. `--max-bytes` limits each decoded
response to 16 MiB by default.

HTTP GET requests retry at most once after a valid `Retry-After` on 429 or 503.
The delay must fit the remaining operation budget. Cooldowns are shared by HTTP
requests to the same origin during the current invocation. Other origins can
continue while one is waiting. Redirect delays also respect `Retry-After`.
Queue waits, redirects, and retry delays share `--timeout`; each HTTP attempt is
capped at 15 seconds. arXiv uses its own spacing gate without an automatic retry.

`meta.http` reports HTTP dispatches, retries, backoff responses, redirects, and
remaining origin cooldowns. It records origins without paths, queries, or bearer
tokens. These counters cover ffetch's HTTP client; browser network traffic and
other CLI processes use separate state.

## Research sources

```sh
ffetch pdf 'https://example.org/paper.pdf' --page 1 --limit 20
```

`pdf` extracts text-layer content by physical page number. The page limit is
1-500, with 20 by default. Records include page locators and the original PDF
artifact. Pages outside the selection and pages without usable text are reported.
This does not perform OCR or reconstruct columns, table cells, images, or formulas.

Extraction uses an embedded Rust parser in a child process of the same binary.
The retrieval/extraction deadline cancels that process. Unix workers have a
512 MiB address-space limit; streams and output also have byte limits. The
Windows memory limit is not implemented. Password-protected PDFs are unsupported.
No separate PDF tool is required. `fetch` with article or text extraction and
`search --read` select this parser when response bytes or the MIME type identify
a PDF. This works without a `.pdf` URL suffix. Each document is downloaded once;
workers reuse the saved artifact when storage is enabled. Automatic reads return
up to the first 20 pages. Use `pdf --page N --limit N` to select another range.
CSS selectors and HTML, JSON, links, or tables extraction are unsupported for PDFs.

```sh
ffetch fulltext PMC11762333
```

`fulltext` retrieves available open-access JATS XML from Europe PMC by PMCID or
a supported PMC article URL. It checks article identity and returns body text,
blocks with source locators, table cells, references, authors, and publication
date parts. The original XML is saved as an artifact unless `--no-store` is used.
DTD files are never loaded, and internal entity declarations are rejected.
MathML layout and images are not fully represented in the text output. This
command does not parse PDFs; unavailable or restricted XML returns a structured
failure.

Search `--read` selects JATS when a result exposes a PMCID or a supported PMC
article URL. Repeated discoveries share one full-text retrieval. Later metadata
with a PMCID can upgrade an earlier ordinary page read. When an explicit DOI is
available, the XML must also establish that DOI before being linked to the search
result. A failed JATS retrieval can fall back to ordinary HTTP/browser/archive
retrieval, keeping partial status and marking full-text completeness unverified.
`meta.read_coverage` records the chosen representation and any errors.

```sh
ffetch references '10.1038/nphys1170' --depth 2 --limit 20
```

`references` follows explicit DOI references reported by Crossref. Depth is 1-3,
with 1 by default. The limit bounds reference entries inspected across the graph,
including duplicates and entries without a usable DOI. At most one seed plus
that many unique referenced works can be looked up. Breadth first waves resolve
works concurrently within each wave and avoid repeated lookups and cycles.

Work metadata is returned as records. `meta.edges` retains reference entries,
provenance, and fetched/failed/pending/unresolved status. Missing lists and DOIs
remain explicit. These are provider-reported references, not independently
verified citations or parsed full text. Other DOI agencies and non-DOI references
may remain unresolved. Output is collected until traversal completes.

| Source | Operations |
| --- | --- |
| `hn` | Search, feeds, stories, bounded comment trees |
| `reddit` | Community search, feeds, posts, bounded comment trees |
| `youtube` | Video search, metadata, available captions, API popular rankings |
| `arxiv` | Preprint search, versioned metadata, abstracts, categories, PDF text |
| `crossref` | DOI metadata, available abstracts, references, resource links |
| `europepmc` | Publication metadata, available abstracts, full-text links |
| `pubmed` | PubMed-indexed records through Europe PMC |
| `papers` | Concurrent arXiv, Crossref, and Europe PMC search |

```sh
ffetch search "robotic gripper" --sources papers --limit 10
```

Crossref and Europe PMC records merge by DOI. arXiv versions stay separate from
each other and from published records, even when a publication DOI is linked. Original provider records, field sources, and
conflicting metadata remain available. Metadata and abstracts are distinguished
from full text.

```sh
ffetch search 'cat:cs.RO AND all:gripper' --sources arxiv --limit 10
```

```sh
ffetch fetch https://arxiv.org/abs/2501.12345v2
```

arXiv search supports field queries and pagination. Fetching an `/abs/` URL returns
versioned metadata and the abstract. Fetching a `/pdf/` URL extracts PDF pages.
Search `--read` fetches the PDF for each discovered version without another
metadata lookup. Requests to its API are
serialized at three-second intervals within one process. Avoid parallel arXiv
CLI processes; cross-process rate coordination is not implemented.

```sh
ffetch search "robot gripper" --sources reddit --community robotics
```

```sh
ffetch trends --sources reddit --community robotics --feed top --period week
```

```sh
ffetch thread 'https://news.ycombinator.com/item?id=8863' --limit 100
```

HN threads schedule comments as request slots become free. `--limit` bounds
unique comment requests. Thread metadata reports returned, failed, queued, and
unfinished requests, with complete or incomplete coverage.

Reddit thread URLs also work. Public Reddit API access may be blocked. Search can
return scoped web results with partial status and native metrics unavailable.
`FFETCH_REDDIT_TOKEN` supplies an optional approved OAuth token. Further comment
expansion and token refresh are not implemented.

```sh
ffetch search "MIT robotics lecture" --sources youtube
```

```sh
ffetch transcript 'https://www.youtube.com/watch?v=Z8oMbOj9IWM' --language en
```

Caption retrieval is best effort. Missing or blocked tracks retain video metadata
and return partial status. A listed track can still return an empty response.
Successful segments include timestamps. YouTube popular rankings require `FFETCH_YOUTUBE_API_KEY`. Platform feeds and popular rankings do
not measure momentum over time.

## Query and sample

```sh
ffetch search SQLite --sources hn --jq '.items[] | select(.metrics.comments >= 50) | {title,url}'
```

```sh
ffetch query results.json --jmespath 'items[].{title:title,url:url}'
```

jq uses embedded jaq. Complete jq compatibility is not promised. Whole-result
queries wait for collection. `--fields` projects record fields; `-r` prints
query-produced strings without JSON quotes.

```sh
ffetch search robotics --sources papers --sample 10 --sample-seed 42
```

Sampling uses Algorithm R to select output records uniformly without replacement.
The full saved run remains available. Sampling does not reduce retrieval work or
total run memory. A seed reproduces selection only for the same input order.

## Browser sessions

```sh
ffetch browser open https://example.com --session research
```

```sh
ffetch browser inspect --session research
```

Use the returned element references with `browser click` or `browser fill`.
References expire after navigation or a new inspection. Other operations include
extract, press, tabs, switch, close-tab, screenshot, status, and stop.

For page and HTML search-engine retrieval, `--browser auto` tries a browser after
eligible HTTP or parsing failures.
`--browser off` disables rendering; `always` requests browser rendering. Select
an executable with `--browser-path` or `FFETCH_BROWSER_PATH`. Browser installation
is separate. Rendered DOM artifacts are labeled separately from HTTP responses.

If Chromium cannot start, ffetch reports `browser_unavailable` with a startup
diagnosis. Automatic rendering shares startup failures across providers in the
current run, avoiding repeated launches after a failed startup.
Other successful providers remain available with partial status. On hosts where
Chromium reports an unusable sandbox, configure sandbox support or select a
working Chrome installation. `--browser off` keeps retrieval on HTTP routes.
`--browser-no-sandbox` is an explicit option for trusted isolated environments.

## Output and storage

JSON output uses schema `ffetch/1` with items, errors, attempts, warnings, and
coverage metadata. Other formats are jsonl, markdown, text, and csv.

Runs and available response artifacts are saved under `$XDG_DATA_HOME/ffetch` or
`~/.local/share/ffetch`. Use `--data-dir` to change the location. `inspect RUN_UUID`
reads a saved run; `artifact SHA256` writes a stored artifact to stdout.

`--no-store` disables evidence writes. Named browser sessions still write their
profiles and session state. Screenshots write to the requested path.

| Exit code | Meaning |
| --- | --- |
| 0 | Success, including valid empty results |
| 1 | Retrieval or provider failure |
| 2 | Usage, query, or storage failure |
| 3 | Partial results |

## Current limits

Search engines can block requests or change their HTML. CAPTCHA recovery and
universal access are not guaranteed. Web pagination depends on recognized engine
markup and can be blocked independently of the initial page. Browser support has
been tested on controlled local pages.
Live caption retrieval and authenticated platform access need broader validation.

X search, OCR, measured trends, and prompt-injection detection are
not implemented. Retrieved content is
untrusted source data. Use `ffetch capabilities` to inspect the operations
exposed by a build.

## Development

```sh
cargo test --locked
```

```sh
cargo clippy --locked --all-targets -- -D warnings
```

```sh
cargo fmt --all --check
```

Browser tests require `FFETCH_TEST_BROWSER` to point to a Chrome or Chromium
executable. Without it, those tests do not launch a browser.
