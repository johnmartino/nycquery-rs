use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use regex::Regex;
use reqwest::Client;
use scraper::{Html, Selector};
use serde_json::{json, Value};

// --- Spinner ---

struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Spinner {
    fn new() -> Self {
        Spinner {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    fn start(&mut self, text: &str) {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let running = Arc::clone(&self.running);
        let text = text.to_string();
        running.store(true, Ordering::SeqCst);
        self.handle = Some(thread::spawn(move || {
            let mut i = 0usize;
            while running.load(Ordering::SeqCst) {
                let frame = frames[i % frames.len()];
                print!("\r{} {} ", text, frame);
                io::stdout().flush().ok();
                thread::sleep(Duration::from_millis(100));
                i += 1;
            }
        }));
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
        print!("\r\x1B[K");
        io::stdout().flush().ok();
    }
}

// --- Article ---

#[derive(Clone)]
struct Article {
    id: String,
    title: String,
}

// --- Fetch and rank NYC 311 articles ---

async fn fetch_nyc_knowledge_article(
    client: &Client,
    issue: &str,
    alternative_phrasings: &[String],
) -> anyhow::Result<String> {
    let html = client
        .get("https://portal.311.nyc.gov/all-articles/")
        .send()
        .await?
        .text()
        .await?;

    let doc = Html::parse_document(&html);
    let ka_re = Regex::new(r"KA-\d+").unwrap();
    let a_sel = Selector::parse("a[href]").unwrap();

    let mut articles: Vec<Article> = Vec::new();
    for el in doc.select(&a_sel) {
        let href = el.value().attr("href").unwrap_or("");
        let title: String = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
        let Some(m) = ka_re.find(href) else { continue };
        articles.push(Article {
            id: m.as_str().to_string(),
            title,
        });
    }

    // Dedupe by KA ID, preferring entries with a non-empty title
    let mut by_id: HashMap<String, Article> = HashMap::new();
    for a in articles {
        let entry = by_id.entry(a.id.clone()).or_insert_with(|| a.clone());
        if entry.title.is_empty() && !a.title.is_empty() {
            *entry = a;
        }
    }

    let generic: HashSet<&str> = ["311 Service Request", "Service Request", "NYC 311", "311"]
        .iter()
        .copied()
        .collect();
    let mut candidates: Vec<Article> = by_id
        .into_values()
        .filter(|a| {
            !a.title.is_empty()
                && a.title.len() > 6
                && !generic.contains(a.title.trim())
        })
        .collect();

    // Rank by word-overlap score (substitute for NLEmbedding)
    let queries: Vec<String> = std::iter::once(issue.to_string())
        .chain(alternative_phrasings.iter().cloned())
        .map(|q| q.to_lowercase())
        .collect();

    candidates.sort_by_key(|a| {
        let context = a.title.to_lowercase();
        let score: usize = queries
            .iter()
            .map(|q| {
                q.split_whitespace()
                    .filter(|w| context.contains(*w))
                    .count()
            })
            .sum();
        Reverse(score)
    });

    let top: Vec<&Article> = candidates.iter().take(20).collect();
    Ok(top
        .iter()
        .map(|a| format!("{} — {}", a.id, a.title))
        .collect::<Vec<_>>()
        .join("\n"))
}

// --- Ollama API (simple two-step: fetch articles, then ask model to pick) ---

const OLLAMA_URL: &str = "http://localhost:11434/v1/chat/completions";
const OLLAMA_MODEL: &str = "llama3.2";

/// Generate naive alternative phrasings from the raw prompt by pulling
/// meaningful words out — good enough to improve the keyword-ranking step.
fn naive_phrasings(prompt: &str) -> Vec<String> {
    let stopwords = ["i", "my", "the", "a", "an", "is", "are", "was", "has",
                     "have", "been", "not", "out", "in", "on", "at", "to",
                     "for", "of", "and", "or", "it", "its", "there", "from"];
    let words: Vec<&str> = prompt
        .split_whitespace()
        .filter(|w| !stopwords.contains(&w.to_lowercase().as_str()) && w.len() > 2)
        .collect();
    // Pairs and the full condensed phrase
    let mut phrasings: Vec<String> = words
        .windows(2)
        .map(|w| w.join(" "))
        .collect();
    if words.len() > 2 {
        phrasings.push(words.join(" "));
    }
    phrasings.into_iter().take(3).collect()
}

async fn run_session(client: &Client, prompt: &str) -> anyhow::Result<String> {
    // Step 1: fetch and rank articles locally — no LLM needed for this
    let phrasings = naive_phrasings(prompt);
    let articles = fetch_nyc_knowledge_article(client, prompt, &phrasings).await?;

    // Step 2: ask Ollama to pick the single best KA ID from the ranked list
    let user_message = format!(
        "A New York City resident says: \"{prompt}\"\n\n\
         Here are the 20 most relevant NYC 311 knowledge articles:\n\
         {articles}\n\n\
         Reply with ONLY the single KA identifier (e.g. KA-03641) that best matches \
         the resident's issue. No other text."
    );

    let resp = client
        .post(OLLAMA_URL)
        .json(&json!({
            "model": OLLAMA_MODEL,
            "stream": false,
            "messages": [
                {
                    "role": "system",
                    "content": "You are a precise NYC 311 classifier. \
                                When given a list of knowledge articles and a resident's issue, \
                                you reply with exactly one KA identifier and nothing else."
                },
                { "role": "user", "content": user_message }
            ]
        }))
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        anyhow::bail!("{}", err);
    }

    Ok(resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

// --- Open article URL ---

fn open_article(ka: &str) {
    let url = format!("https://portal.311.nyc.gov/article/?kanumber={ka}");
    std::process::Command::new("open").arg(&url).spawn().ok();
}

// --- CLI ---

fn usage(name: &str) -> ! {
    eprintln!(
        "Usage:\n  {name} '<prompt>'\n  {name} --stdin\n\n\
         Use single quotes so shell metacharacters like ! are not expanded.\n\
         Example: {name} 'There is loud banging coming from next door!'\n\
         Stdin:   printf '%s\\n' 'Loud music upstairs' | {name} --stdin"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = env::args().collect();
    let name = std::path::Path::new(&argv[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("nycquery")
        .to_string();

    let cli: Vec<&str> = argv[1..].iter().map(String::as_str).collect();

    let prompt: String = if cli == ["--stdin"] || cli == ["-"] {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).expect("failed to read stdin");
        buf.trim().to_string()
    } else if cli.len() == 1 {
        cli[0].to_string()
    } else {
        usage(&name)
    };

    if prompt.is_empty() {
        usage(&name);
    }

    let client = Client::new();
    let mut spinner = Spinner::new();
    spinner.start("Thinking");

    match run_session(&client, &prompt).await {
        Ok(content) => {
            spinner.stop();
            let ka_re = Regex::new(r"KA-\d+").unwrap();
            if let Some(m) = ka_re.find(&content) {
                let ka = m.as_str();
                println!("{ka}");
                open_article(ka);
            } else {
                println!("{content}");
            }
        }
        Err(e) => {
            spinner.stop();
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
