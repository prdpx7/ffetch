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
ffetch search "robot gripper" --sources web --deep --engines google,bing,duckduckgo,yandex,baidu,brave
```

`--deep` expands engine coverage. It does not plan follow-up queries. Use
`--search-mode fast` for the first validated search response. `--limit` applies
per engine or source, so the merged result count can exceed it.

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
8. `--max-bytes` limits each decoded response to 16 MiB by default.

## Research sources

| Source | Operations |
| --- | --- |
| `hn` | Search, feeds, stories, bounded comment trees |
| `reddit` | Community search, feeds, posts, bounded comment trees |
| `youtube` | Video search, metadata, available captions, API popular rankings |
| `arxiv` | Preprint search, versioned metadata, abstracts, categories, PDF links |
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

arXiv search supports field queries and pagination. URL fetching returns versioned
metadata and the abstract, not parsed PDF content. Requests to its API are
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
universal access are not guaranteed. HTML search adapters currently read the
initial results page. Browser support has been tested on controlled local pages.
Live caption retrieval and authenticated platform access need broader validation.

X search, PDF/OCR extraction, measured trends, and prompt-injection detection are
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
