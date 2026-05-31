# NYCquery Architecture

## Overview

`nycquery` is a Rust CLI tool that accepts a natural language complaint from a New York City resident and returns the most relevant NYC 311 knowledge article(s) for that complaint. The best match is opened automatically in the default browser.

The program combines web scraping, a local LLM (via Ollama), and HTTP calls to the NYC 311 portal to classify an open-ended complaint into a concrete service article — all in a single command. Complaints in any language are supported: a translation step converts non-English input to English before keyword matching. Articles whose titles directly match the complaint bypass LLM reasoning entirely.

---

## Usage

```
nycquery [-list] [-no-open] [-debug] '<complaint>'
nycquery [-list] [-no-open] [-debug] --stdin
```

- Without `-list`: prints only the top result and opens it in the browser.
- With `-list`: prints all 5 ranked results and opens the top result in the browser.
- `-no-open`: suppresses opening the result in the browser.
- `-debug`: prints the keyword search terms used for ranking to stderr.
- `--stdin` (or `-`): reads the complaint from standard input instead of a command-line argument.

---

## Pipeline

Each run executes the following stages in order:

```
1. Fetch all article titles       (HTTP scrape of NYC 311 portal)
   ↕ runs in parallel with ↕
   Extract search terms            (LLM call — maps complaint to formal 311 terms)
   ↕ runs in parallel with ↕
   Translate complaint to English  (LLM call — no-op if already English)
         │
         ▼
2. Merge & keyword-rank articles  (in memory — word-boundary, length-weighted scoring)
         │
         ▼
3. Near-exact title match?
   ├─ yes → return as top result(s), skip LLM
   └─ no  → Fetch bodies for candidates  (concurrent HTTP requests)
                    │
                    ▼
4. Select best 5                  (LLM call)
         │
         ▼
5. Print results + open browser
```

Stages 1's three operations (article fetch, search term extraction, translation) all run concurrently via `tokio::join!`.

Stage 3's HTTP requests all fire concurrently using `tokio::spawn`.

---

## Data Structures

### `Article`

```rust
struct Article {
    id: String,    // e.g. "KA-01234"
    title: String, // e.g. "Noise - Residential"
}
```

A single NYC 311 knowledge article, identified by its KA number and display title. Used throughout the pipeline from initial scrape through final output.

---

## Functions

### `main`

The program entry point. Handles argument parsing, starts the spinner, drives `run_session`, and prints results.

**Argument parsing:**
- Collects raw CLI args and strips the `-list`, `-no-open`, and `-debug` flags if present.
- The remaining args must be exactly one prompt string, `--stdin`, or `-`.
- Reads from stdin when `--stdin` or `-` is passed.

**Output:**
- Without `-list`: prints `KA-XXXXX — Title` for the top result only.
- With `-list`: prints all five results in ranked order.
- Opens the top result in the browser via `open_article` unless `-no-open` is set.
- Passes the `debug` flag to `run_session` to enable search term logging.

---

### `run_session`

```rust
async fn run_session(client: &Client, prompt: &str, debug: bool) -> anyhow::Result<Vec<(String, String)>>
```

Orchestrates the entire matching pipeline. Returns up to 5 `(KA-id, title)` pairs ranked best-first.

**Steps:**
1. Runs `fetch_all_articles`, `extract_search_terms`, and `translate_to_english` concurrently via `tokio::join!`.
2. Merges LLM-generated search terms with English words extracted from the translated complaint (ASCII-alphabetic words only, length > 2).
3. Sorts all articles by `(is_near_exact, score)` descending, where score is the sum of matched term lengths using word-boundary matching (longer, more specific terms outweigh short function words naturally).
4. Takes the top 15 ranked articles.
5. Partitions into near-exact title matches and the rest. Near-exact matches go directly into results — no LLM needed.
6. If fewer than 5 results so far, spawns concurrent `tokio` tasks to fetch bodies for the remaining candidates, then calls `select_best`.
7. Resolves KA IDs back to titles using a `HashMap` and returns the combined result.

---

### `fetch_all_articles`

```rust
async fn fetch_all_articles(client: &Client) -> anyhow::Result<Vec<Article>>
```

Scrapes the NYC 311 "all articles" index page and returns every unique knowledge article with a non-generic title.

**How it works:**
1. Fetches `https://portal.311.nyc.gov/all-articles/` as raw HTML.
2. Parses the document with `scraper` and selects every `<a href>` element.
3. Applies a regex (`KA-\d+`) to the `href` attribute to extract the KA identifier. Links without a KA number are skipped.
4. Collects the visible link text as the article title.
5. De-duplicates by KA ID using a `HashMap`, preferring entries with a non-empty title.
6. Filters out articles with empty titles, titles shorter than 7 characters, or generic placeholder titles (`"311"`, `"NYC 311"`, `"Service Request"`, `"311 Service Request"`).

The result is typically around 500 articles.

---

### `fetch_article_body`

```rust
async fn fetch_article_body(client: &Client, ka_id: &str) -> String
```

Fetches the content page for a single knowledge article and extracts a compact text summary.

**How it works:**
1. Constructs the URL: `https://portal.311.nyc.gov/article/?kanumber={ka_id}`.
2. Fetches the page and parses it with `scraper`.
3. Selects all `<p>` and `<li>` elements (paragraphs and list items contain the substantive content).
4. Normalises whitespace in each element's text.
5. Discards elements with fewer than 10 characters (navigation fragments, labels, etc.).
6. Takes at most 8 elements.
7. Joins them and truncates to 600 characters.

Returns an empty string on any network or parse failure — the caller continues without the body rather than failing the whole request.

---

### `extract_search_terms`

```rust
async fn extract_search_terms(client: &Client, prompt: &str) -> anyhow::Result<Vec<String>>
```

Sends the resident's complaint to the local LLM and asks it to produce 3–5 English words or short phrases that would appear in the title of the correct NYC 311 article. Its primary value is mapping conversational language ("music next door is driving me crazy") to the formal terminology used in article titles ("noise", "residential", "neighbor").

**LLM call:**
- Model: `llama3.1:8b` via Ollama at `http://localhost:11434/v1/chat/completions`
- Temperature: `0` (deterministic output)
- System prompt instructs the model to respond with space-separated lowercase terms only, regardless of the input language.

**Post-processing:**
- Splits the response on whitespace.
- Lowercases every token.
- Discards tokens shorter than 3 characters.

The returned tokens are merged with words from `translate_to_english` by `run_session` before keyword scoring.

---

### `translate_to_english`

```rust
async fn translate_to_english(client: &Client, prompt: &str) -> String
```

Translates the resident's complaint to English. If the complaint is already in English, the LLM returns it unchanged.

**Purpose:** Provides the raw English words of the complaint for keyword matching. This ensures that specific nouns from the complaint (e.g. "graffiti", "neighbor") are always present in the term list, regardless of what `extract_search_terms` produces. Runs concurrently with `fetch_all_articles` and `extract_search_terms`.

**LLM call:**
- Temperature: `0`
- System prompt instructs the model to return only the translation, no other text.

**Failure handling:** Returns the original prompt on any network or parse error, so the pipeline continues with at least the raw complaint words.

---

### `looks_english`

```rust
fn looks_english(w: &str) -> bool
```

Returns `true` if every character in the word is ASCII alphabetic. Used to filter translated complaint words before adding them to the keyword term list — words containing non-ASCII characters (CJK, Arabic, Cyrillic, accented Latin, etc.) are excluded because they cannot match English article titles.

---

### `is_near_exact`

```rust
fn is_near_exact(title: &str, complaint: &str) -> bool
```

Returns `true` if the article title and the complaint are close enough to be considered a direct match, without needing LLM reasoning. The three conditions checked are:
- `title == complaint` — identical strings
- `title.contains(complaint)` — the complaint phrase appears inside the title
- `complaint.contains(title)` — the title phrase appears inside the complaint (e.g. complaint "graffiti on the wall", title "Graffiti")

Used in `run_session` both to sort near-exact articles to the top of the candidate list and to partition them out before the `select_best` LLM call.

---

### `select_best`

```rust
async fn select_best(
    client: &Client,
    prompt: &str,
    enriched: &[(Article, String)],
) -> anyhow::Result<Vec<String>>
```

Sends the original complaint together with the non-exact-match candidate articles (title + body excerpt) to the LLM and asks it to pick and rank the top 5 matches. Only called when near-exact title matches have not already filled all 5 result slots.

**Input construction:**
Each article is formatted as:
```
KA-XXXXX — Title
<body excerpt up to 600 chars>
```
All articles are joined with blank lines and prepended with the complaint text.

**LLM call:**
- Temperature: `0`
- System prompt instructs the model to return only KA identifiers, one per line, ranked best-first.

**Post-processing:**
- Applies the `KA-\d+` regex to the raw response text.
- Takes at most 5 matches in the order they appear.

Returns a `Vec<String>` of KA IDs. The caller (`run_session`) resolves each ID to its title.

---

### `open_article`

```rust
fn open_article(ka: &str)
```

Constructs the full article URL and launches it in the system default browser using the macOS `open` command. Failures are silently ignored.

---

### `usage`

```rust
fn usage(name: &str) -> !
```

Prints usage instructions to stderr and exits with code 2. The return type `!` (never) tells the Rust compiler this function does not return, allowing it to be used in positions that expect a value.

---

### `Spinner`

A terminal progress indicator that runs on a dedicated OS thread while the async pipeline executes.

**Fields:**
- `running: Arc<AtomicBool>` — shared flag between the main thread and the spinner thread. `Arc` allows both threads to hold a reference; `AtomicBool` allows lock-free reads and writes across threads.
- `handle: Option<thread::JoinHandle<()>>` — the OS thread handle, held so the thread can be joined on stop.

**`Spinner::new`** — creates a spinner in the stopped state.

**`Spinner::start(text)`** — sets the `running` flag to `true`, then spawns an OS thread that cycles through 10 Braille spinner frames every 100ms, printing `\r` to overwrite the current line in place.

**`Spinner::stop`** — sets `running` to `false`, joins the spinner thread (waits for it to exit its loop), then prints `\r\x1B[K` (carriage return + ANSI "erase to end of line") to clear the spinner from the terminal before results are printed.

---

## External Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime; powers `async/await`, `tokio::spawn`, and `tokio::join!` |
| `reqwest` | Async HTTP client used for all web requests |
| `scraper` | HTML parser built on `html5ever`; used to extract article titles and body text |
| `regex` | Regular expression matching; extracts `KA-\d+` identifiers from HTML and LLM output |
| `serde_json` | JSON serialisation for Ollama API requests and deserialisation of responses |
| `anyhow` | Ergonomic error handling; propagates errors up the call stack with `?` |

---

## External Services

| Service | URL | Purpose |
|---|---|---|
| NYC 311 portal (index) | `https://portal.311.nyc.gov/all-articles/` | Source of all article titles and KA IDs |
| NYC 311 portal (article) | `https://portal.311.nyc.gov/article/?kanumber={id}` | Source of article body content |
| Ollama | `http://localhost:11434/v1/chat/completions` | Local LLM inference (llama3.1:8b) — used for `extract_search_terms`, `translate_to_english`, and `select_best` |
