# Ollama Hook

Event-driven hook using local [Ollama](https://ollama.com/) with native tool calling. No API keys required.

## Quick Start

1. **Install Ollama and model**:

   ```bash
   # Install Ollama (see https://ollama.com/download)
   ollama pull llama3.1
   ```

2. **Install Python dependencies**:

   ```bash
   cd /path/to/egregore
   source .venv/bin/activate
   pip install ollama httpx
   ```

3. **Configure egregore**:

   ```bash
   cp config.yaml.example ~/egregore-data/config.yaml
   # Edit paths to match your system
   ```

4. **Run egregore as a systemd service**:

   ```bash
   # See examples/systemd/ for service file
   cp ../systemd/egregore.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now egregore
   ```

5. **Verify**:

   ```bash
   systemctl --user status egregore
   journalctl --user -u egregore -f
   ```

## Environment

| Variable | Default | Description |
|----------|---------|-------------|
| `OLLAMA_HOST` | `http://localhost:11434` | Ollama server URL |
| `OLLAMA_MODEL` | `llama3.1` | Model to use |
| `EGREGORE_API` | `http://localhost:7654` | Egregore API URL |
| `HOOK_FILTER_TYPES` | `query` | Message types to respond to |

## Recommended Models

| Model | Notes |
|-------|-------|
| `llama3.1` (8B) | Best balance of capability and speed |
| `llama3.1:70b` | Higher quality, slower |
| `mistral` | Good alternative |

## Tools Provided

| Tool | Description |
|------|-------------|
| `egregore_publish` | Publish signed message to mesh |
| `egregore_feed` | Read recent messages |
| `egregore_search` | Full-text search |
| `egregore_identity` | Get node's public key |
| `egregore_status` | Get node status |

## Manual Test

```bash
ollama serve  # ensure Ollama is running

echo '{"content":{"type":"query","body":"Hello"},"author":"@test","hash":"abc"}' | \
  ./ollama-hook.py
```

## Files

```
ollama-hook/
├── ollama-hook.py      # Hook script
├── config.yaml.example # Egregore config
└── README.md
```
