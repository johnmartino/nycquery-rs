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

// --- Fetch all NYC 311 article titles ---

async fn fetch_all_articles(client: &Client) -> anyhow::Result<Vec<Article>> {
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
        articles.push(Article { id: m.as_str().to_string(), title });
    }

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

    Ok(by_id
        .into_values()
        .filter(|a| !a.title.is_empty() && a.title.len() > 6 && !generic.contains(a.title.trim()))
        .collect())
}

// --- Fetch article body ---

async fn fetch_article_body(client: &Client, ka_id: &str) -> String {
    let url = format!("https://portal.311.nyc.gov/article/?kanumber={ka_id}");
    let Ok(resp) = client.get(&url).send().await else { return String::new() };
    let Ok(html) = resp.text().await else { return String::new() };
    let doc = Html::parse_document(&html);
    let sel = Selector::parse("p, li").unwrap();
    let text: String = doc
        .select(&sel)
        .map(|el| el.text().collect::<String>())
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| s.len() > 10)
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    text.chars().take(600).collect()
}

fn looks_english(w: &str) -> bool {
    w.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_near_exact(title: &str, complaint: &str) -> bool {
    title == complaint || title.contains(complaint) || complaint.contains(title)
}


// --- Ollama API ---

const OLLAMA_URL: &str = "http://localhost:11434/v1/chat/completions";
const OLLAMA_MODEL: &str = "llama3.1:8b";

// Step 1: translate the complaint into English 311-style search terms
async fn extract_search_terms(client: &Client, prompt: &str) -> anyhow::Result<Vec<String>> {
    let resp = client
        .post(OLLAMA_URL)
        .json(&json!({
            "model": OLLAMA_MODEL,
            "stream": false,
            "temperature": 0,
            "messages": [
                {
                    "role": "system",
                    "content": "You are a NYC 311 classifier assistant. \
                                Given a resident complaint in any language, reply with 3-5 English \
                                words or short phrases that would appear in the title of the correct \
                                NYC 311 service article. Space-separated, lowercase, no other text."
                },
                { "role": "user", "content": prompt }
            ]
        }))
        .send()
        .await?
        .json::<Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        anyhow::bail!("{}", err);
    }

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(content
        .split_whitespace()
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() > 2)
        .collect())
}

async fn translate_to_english(client: &Client, prompt: &str) -> String {
    let Ok(resp) = client
        .post(OLLAMA_URL)
        .json(&json!({
            "model": OLLAMA_MODEL,
            "stream": false,
            "temperature": 0,
            "messages": [
                {
                    "role": "system",
                    "content": "Translate the following text to English. \
                                If it is already in English, return it unchanged. \
                                Return only the translation, no other text."
                },
                { "role": "user", "content": prompt }
            ]
        }))
        .send()
        .await else { return prompt.to_string() };
    let Ok(json) = resp.json::<Value>().await else { return prompt.to_string() };
    json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or(prompt)
        .trim()
        .to_string()
}

// Pick and rank the top 5 articles from title + body content
async fn select_best(
    client: &Client,
    prompt: &str,
    enriched: &[(Article, String)],
) -> anyhow::Result<Vec<String>> {
    let article_text = enriched
        .iter()
        .map(|(a, body)| format!("{} — {}\n{}", a.id, a.title, body))
        .collect::<Vec<_>>()
        .join("\n\n");

    let user_message = format!(
        "Resident complaint: \"{prompt}\"\n\n\
         {article_text}\n\n\
         Reply with ONLY the top 5 KA identifiers that best match the complaint, best match first, \
         one per line. No other text."
    );

    let resp = client
        .post(OLLAMA_URL)
        .json(&json!({
            "model": OLLAMA_MODEL,
            "stream": false,
            "temperature": 0,
            "messages": [
                {
                    "role": "system",
                    "content": "You are a precise NYC 311 classifier. \
                                When given knowledge articles and a resident's complaint, \
                                reply with the top 5 KA identifiers ranked best match first, \
                                one per line, nothing else."
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

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let ka_re = Regex::new(r"KA-\d+").unwrap();
    Ok(ka_re.find_iter(&content).map(|m| m.as_str().to_string()).take(5).collect())
}

async fn run_session(client: &Client, prompt: &str, debug: bool) -> anyhow::Result<Vec<(String, String)>> {
    let (articles_result, terms_result, translation) = tokio::join!(
        fetch_all_articles(client),
        extract_search_terms(client, prompt),
        translate_to_english(client, prompt)
    );
    let mut articles = articles_result?;
    let mut terms = terms_result?;

    // Add raw keywords from the (possibly translated) complaint
    for word in translation.split_whitespace() {
        let w = word.to_lowercase().trim_matches(|c: char| !c.is_alphanumeric()).to_string();
        if w.len() > 2 && looks_english(&w) && !terms.contains(&w) {
            terms.push(w);
        }
    }
    if debug { eprintln!("[debug] search terms: {:?}", terms); }

    let complaint_lower = prompt.trim().to_lowercase();

    // Near-exact title match sorts first; then by length-weighted keyword score
    // (longer terms score higher naturally, so short function words weigh less)
    articles.sort_by_key(|a| {
        let title = a.title.to_lowercase();
        let near_exact = is_near_exact(&title, &complaint_lower);
        let title_words: Vec<&str> = title
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let score: usize = terms.iter()
            .filter(|w| title_words.contains(&w.as_str()))
            .map(|w| w.len())
            .sum();
        Reverse((near_exact as usize, score))
    });

    let top: Vec<Article> = articles.into_iter().take(15).collect();

    // Near-exact matches bypass the LLM — the title speaks for itself
    let (exact, rest): (Vec<Article>, Vec<Article>) = top.into_iter().partition(|a| {
        is_near_exact(&a.title.to_lowercase(), &complaint_lower)
    });

    let mut results: Vec<(String, String)> =
        exact.iter().map(|a| (a.id.clone(), a.title.clone())).collect();

    let still_needed = 5usize.saturating_sub(results.len());
    if still_needed > 0 && !rest.is_empty() {
        let candidates: Vec<Article> = rest.into_iter().take(15 - exact.len()).collect();

        let handles: Vec<tokio::task::JoinHandle<String>> = candidates
            .iter()
            .map(|a| {
                let c = client.clone();
                let id = a.id.clone();
                tokio::spawn(async move { fetch_article_body(&c, &id).await })
            })
            .collect();

        let mut bodies = Vec::with_capacity(handles.len());
        for h in handles { bodies.push(h.await.unwrap_or_default()); }

        let enriched: Vec<(Article, String)> = candidates.into_iter().zip(bodies).collect();
        let title_map: HashMap<String, String> =
            enriched.iter().map(|(a, _)| (a.id.clone(), a.title.clone())).collect();

        let ids = select_best(client, prompt, &enriched).await?;
        results.extend(ids.into_iter().take(still_needed).map(|id| {
            (id.clone(), title_map.get(&id).cloned().unwrap_or_default())
        }));
    }

    Ok(results)
}

// --- Open article URL ---

fn open_article(ka: &str) {
    let url = format!("https://portal.311.nyc.gov/article/?kanumber={ka}");
    std::process::Command::new("open").arg(&url).spawn().ok();
}

// --- CLI ---

fn usage(name: &str) -> ! {
    eprintln!(
        "Usage:\n  {name} [-list] [-no-open] [-debug] '<prompt>'\n  {name} [-list] [-no-open] [-debug] --stdin\n\n\
         -list     Print all 5 results (default: print only the top result)\n\
         -no-open  Do not open the result in the browser\n\
         -debug    Print search terms used for keyword ranking\n\n\
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

    let list_flag = cli.contains(&"-list");
    let no_open_flag = cli.contains(&"-no-open");
    let debug_flag = cli.contains(&"-debug");
    let rest: Vec<&str> = cli.iter().copied().filter(|&s| !matches!(s, "-list" | "-no-open" | "-debug")).collect();

    let prompt: String = if rest == ["--stdin"] || rest == ["-"] {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).expect("failed to read stdin");
        buf.trim().to_string()
    } else if rest.len() == 1 {
        rest[0].to_string()
    } else {
        usage(&name)
    };

    if prompt.is_empty() {
        usage(&name);
    }

    let client = Client::new();
    let mut spinner = Spinner::new();
    spinner.start("Thinking");

    match run_session(&client, &prompt, debug_flag).await {
        Ok(results) => {
            spinner.stop();
            if results.is_empty() {
                eprintln!("No results found.");
                std::process::exit(1);
            }
            if list_flag {
                for (ka, title) in &results {
                    println!("{ka} — {title}");
                }
            } else {
                let (ka, title) = &results[0];
                println!("{ka} — {title}");
            }
            if !no_open_flag { open_article(&results[0].0); }
        }
        Err(e) => {
            spinner.stop();
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
