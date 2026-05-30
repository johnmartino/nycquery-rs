**Step 1 — Install Ollama**
```sh
brew install ollama
```

**Step 2 — Start the Ollama server** (keep this running in a terminal tab)
```sh
ollama serve
```

**Step 3 — Pull the model** (in a *new* terminal tab, ~2 GB download)
```sh
ollama pull llama3.2
```

**Step 4 — Run the tool**
```sh
cargo run -- 'my heat is out'
```

> **Note the `--`** before the prompt — that separates Cargo's arguments from the program's arguments. Without it, Cargo tries to interpret `'my heat is out'` as a Cargo flag.

Once Ollama is installed, you can also run it as a background service so you don't need to keep a terminal tab open:
```sh
brew services start ollama
```
