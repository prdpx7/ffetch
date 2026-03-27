# ffetch

A quick hack to make Claude Code (and other AI agents) actually useful at web research.

Claude's `WebFetch` fails on half the internet -- Reddit blocks it, SPAs return empty divs, Cloudflare pages give you a "checking your browser" wall. ffetch is a wrapper around [curl_cffi](https://github.com/lexiforest/curl_cffi), [Lightpanda](https://github.com/lightpanda-io/browser), and [Botasaurus](https://github.com/omkarcloud/botasaurus) that auto-escalates through them until it gets the content. Returns clean markdown. Just a CLI.

## Install

```bash
uv tool install git+https://github.com/prdpx7/ffetch
ffetch setup
```

That's it. `ffetch setup` creates `~/.config/ffetch/config.yaml` and installs [Lightpanda](https://github.com/lightpanda-io/browser) for JS rendering.

Then just tell your AI agent to use it:

```
use ffetch <url> to fetch web pages
```

Works with Claude Code, Codex, opencode, aider -- any agent that can run shell commands.

To make it permanent in Claude Code:
```bash
ffetch setup --install-agent-prompt   # appends instructions to ~/.claude/CLAUDE.md
```

## Usage

```bash
ffetch <url>                    # auto-escalate, markdown output
ffetch <url> -t fast            # force fast tier (curl_cffi)
ffetch <url> -t render          # force JS rendering (lightpanda)
ffetch <url> --json             # structured JSON output
ffetch <url> --text             # plain text output
ffetch <url> --html             # raw HTML output
ffetch <url> --cache            # enable response caching
ffetch <url> | llm "summarize"  # pipe to any LLM
```

```bash
ffetch setup                    # one-time setup
ffetch config show              # print resolved config
ffetch config init              # create config file
ffetch config path              # print config file location
```

## How it works

```
ffetch <url>
  1. curl_cffi with browser TLS fingerprint       (fast, no browser)
  2. Lightpanda headless browser                   (if blocked or JS needed)
  3. Botasaurus anti-detection                     (if still blocked, optional)

  -> clean markdown via trafilatura + readability fallback
```

| Tier | Tool | When |
|-|-|-|
| fast | curl_cffi | 90% of sites. Impersonates browser TLS/JA3 fingerprint. |
| render | Lightpanda / Playwright | SPAs, JS-heavy pages, Reddit, anything that needs JS. |
| stealth | Botasaurus | Cloudflare challenges, aggressive bot detection. Optional. |

Auto-escalation detects blocked responses (Cloudflare walls, 403s, "checking your browser") and JS-required pages (empty `<div id="root">`) and moves to the next tier automatically.

## Config

Config lives at `~/.config/ffetch/config.yaml`. This is the primary tuning interface -- edit it to fix things, no code changes needed.

```yaml
defaults:
  timeout: 30
  impersonate: chrome

sites:
  - pattern: "*.reddit.com"
    tier: render
  - pattern: "*.twitter.com"
    tier: stealth

cache:
  enabled: false
  ttl_seconds: 3600
```

Site not working? Add a rule. Extraction bad? Change the fallback. An AI agent can edit this config and re-run ffetch to self-correct.

Run `ffetch config show` to see all defaults.

## Optional: MCP server

If you prefer MCP over CLI (e.g., for tool-native integrations), ffetch also ships an MCP server:

```bash
claude mcp add ffetch ffetch-mcp
```

This exposes `ffetch` as a native tool over stdio. But the CLI approach (`Bash(ffetch <url>)`) is simpler and works everywhere.

## Dependencies

- [curl_cffi](https://github.com/lexiforest/curl_cffi) -- HTTP with browser TLS fingerprint impersonation
- [Lightpanda](https://github.com/lightpanda-io/browser) -- headless browser built for AI (installed via `ffetch setup`)
- [trafilatura](https://github.com/adbar/trafilatura) -- HTML to markdown extraction
- [readability-lxml](https://github.com/buriy/python-readability) -- fallback content extraction for pages trafilatura misses
- [Playwright](https://playwright.dev/) -- fallback JS renderer if Lightpanda unavailable
- [Botasaurus](https://github.com/omkarcloud/botasaurus) -- anti-detection browser (optional: `uv tool install 'ffetch[stealth]'`)

## Development

```bash
git clone https://github.com/prdpx7/ffetch.git
cd ffetch
uv sync
uv run ffetch https://example.com
```

## License

MIT
