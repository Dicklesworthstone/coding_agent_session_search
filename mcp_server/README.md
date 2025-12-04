# CASS MCP Server

MCP Server for **Coding Agent Session Search** - search across all your AI coding agent histories via the Model Context Protocol with SSE transport.

## Features

Search and explore conversation histories from:
- ✈️ **GitHub Copilot** (VS Code)
- 🤖 **Claude Code**
- 🔹 **Codex** (OpenAI)
- 💎 **Gemini CLI**
- 🎯 **Cursor**
- 🔧 **Aider**
- 💬 **ChatGPT**
- 🧭 **Cline**
- ⚡ **Amp**
- 📦 **OpenCode**

## Quick Start

### Option 1: Docker Compose (Recommended)

```bash
cd mcp_server
docker-compose up -d
```

The server will be available at `http://localhost:8888`

### Option 2: Docker Build

```bash
# From the repository root
docker build -f mcp_server/Dockerfile -t cass-mcp-server .
docker run -p 8888:8888 \
  -v "$HOME/Library/Application Support/Code/User:/vscode-data:ro" \
  -v cass-data:/data \
  cass-mcp-server
```

### Option 3: Local Development

```bash
cd mcp_server
pip install -e .
python mcp_server.py
```

## Available Tools

| Tool | Description |
|------|-------------|
| `cass_search` | Search across all indexed coding agent histories |
| `cass_stats` | Get statistics about indexed data |
| `cass_capabilities` | Discover available features and limits |
| `cass_timeline` | Show activity timeline |
| `cass_context` | Find related sessions for a file/project |
| `cass_view` | View a specific conversation |
| `cass_expand` | Expand context around a message |
| `cass_export` | Export conversation to markdown/JSON |
| `cass_health` | Health check of the index |
| `cass_index` | Trigger re-indexing |

## MCP Client Configuration

### VS Code (Copilot)

Add to your `settings.json`:

```json
{
  "mcp.servers": {
    "cass": {
      "transport": "sse",
      "url": "http://localhost:8888/sse"
    }
  }
}
```

### Claude Desktop

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "cass": {
      "transport": "sse",
      "url": "http://localhost:8888/sse"
    }
  }
}
```

### Cursor

Add to MCP settings:

```json
{
  "cass": {
    "transport": "sse", 
    "url": "http://localhost:8888/sse"
  }
}
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/` | GET | Server info |
| `/health` | GET | Health check |
| `/sse` | GET | SSE connection for MCP |
| `/messages/` | POST | Message handler |

## Example Usage

Once connected, you can use the tools:

```
Search for Python debugging sessions:
> cass_search(query="python debug", agent="copilot", limit=5)

Get index statistics:
> cass_stats()

Find sessions related to a project:
> cass_context(path="/path/to/my/project")

View timeline of recent activity:
> cass_timeline(days=7, group_by="day")
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CASS_BINARY` | `/app/cass` | Path to cass binary |
| `CASS_DATA_DIR` | `/data` | Data directory for index |
| `PORT` | `8888` | Server port |
| `HOST` | `0.0.0.0` | Server host |

## Indexing Your Data

On first run, you'll need to index your agent histories:

```bash
# Via the MCP tool
cass_index()

# Or directly
docker exec cass-mcp-server /app/cass index
```

For a full rebuild:
```bash
docker exec cass-mcp-server /app/cass index --full
```

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│   AI Client     │────▶│  MCP Server      │────▶│  cass binary    │
│ (Copilot/Claude)│ SSE │  (Python/SSE)    │     │  (Rust/Tantivy) │
└─────────────────┘     └──────────────────┘     └─────────────────┘
                                                         │
                                                         ▼
                                               ┌─────────────────┐
                                               │  Agent Sessions │
                                               │  (Copilot, etc) │
                                               └─────────────────┘
```

## License

MIT
