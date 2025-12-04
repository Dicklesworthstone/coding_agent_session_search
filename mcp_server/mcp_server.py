#!/usr/bin/env python3
"""
MCP Server for Coding Agent Session Search (cass)

Exposes cass tool capabilities via Model Context Protocol with SSE transport.
Allows AI agents to search across coding agent histories (Copilot, Claude, Codex, etc.)
"""

import asyncio
import json
import logging
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path
from typing import Any, Optional

from mcp.server import Server
from mcp.server.sse import SseServerTransport
from mcp.types import (
    Tool,
    TextContent,
    CallToolResult,
)
from starlette.applications import Starlette
from starlette.routing import Route
from starlette.responses import JSONResponse

# Configure logging
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s"
)
logger = logging.getLogger("cass-mcp-server")


def find_cass_binary() -> str:
    """Find the cass binary in various locations."""
    # Check environment variable first
    if env_binary := os.environ.get("CASS_BINARY"):
        if os.path.isfile(env_binary) and os.access(env_binary, os.X_OK):
            return env_binary
    
    # Check common locations
    candidates = [
        "/app/cass",
        "./cass",
        "../target/release/cass",
        str(Path.home() / ".cargo/bin/cass"),
        shutil.which("cass"),  # Check PATH
    ]
    
    for candidate in candidates:
        if candidate and os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    
    raise RuntimeError(
        "cass binary not found! Set CASS_BINARY environment variable or ensure cass is in PATH."
    )


# Path to cass binary
try:
    CASS_BINARY = find_cass_binary()
    logger.info(f"Found cass binary at: {CASS_BINARY}")
except RuntimeError as e:
    logger.warning(f"cass binary not found at startup: {e}")
    CASS_BINARY = "/app/cass"  # Fallback, will fail at runtime if not available

DATA_DIR = os.environ.get("CASS_DATA_DIR", str(Path.home() / ".cass"))

# Background indexer configuration
ENABLE_WATCH_MODE = os.environ.get("CASS_ENABLE_WATCH", "true").lower() in ("true", "1", "yes")
INITIAL_INDEX_ON_START = os.environ.get("CASS_INDEX_ON_START", "true").lower() in ("true", "1", "yes")
INDEX_INTERVAL_SECONDS = int(os.environ.get("CASS_INDEX_INTERVAL", "300"))  # 5 minutes default


class BackgroundIndexer:
    """Background indexer that keeps the cass index up-to-date."""
    
    def __init__(self, cass_binary: str, data_dir: str):
        self.cass_binary = cass_binary
        self.data_dir = data_dir
        self._watch_process: Optional[subprocess.Popen] = None
        self._periodic_thread: Optional[threading.Thread] = None
        self._stop_event = threading.Event()
        self._last_index_time: Optional[datetime] = None
        self._index_count = 0
        self._is_running = False
    
    def start(self, use_watch_mode: bool = True, initial_index: bool = True):
        """Start the background indexer."""
        if self._is_running:
            logger.warning("Background indexer already running")
            return
        
        self._is_running = True
        self._stop_event.clear()
        
        # Run initial index if requested
        if initial_index:
            logger.info("Running initial index...")
            self._run_index(full=False)
        
        if use_watch_mode:
            # Try to start watch mode (native file watching)
            self._start_watch_mode()
        else:
            # Fall back to periodic indexing
            self._start_periodic_indexing()
    
    def _start_watch_mode(self):
        """Start cass in watch mode for real-time file monitoring."""
        try:
            cmd = [self.cass_binary, "index", "--watch"]
            logger.info(f"Starting watch mode: {' '.join(cmd)}")
            
            self._watch_process = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={**os.environ, "CASS_DATA_DIR": self.data_dir},
                text=True
            )
            
            # Start a thread to monitor watch process output
            watch_monitor = threading.Thread(
                target=self._monitor_watch_process,
                daemon=True,
                name="cass-watch-monitor"
            )
            watch_monitor.start()
            
            logger.info(f"Watch mode started (PID: {self._watch_process.pid})")
            
        except Exception as e:
            logger.error(f"Failed to start watch mode: {e}, falling back to periodic indexing")
            self._start_periodic_indexing()
    
    def _monitor_watch_process(self):
        """Monitor the watch process and restart if needed."""
        if not self._watch_process:
            return
        
        while not self._stop_event.is_set():
            # Check if process is still running
            poll = self._watch_process.poll()
            if poll is not None:
                # Process exited
                stderr = self._watch_process.stderr.read() if self._watch_process.stderr else ""
                logger.warning(f"Watch process exited with code {poll}: {stderr}")
                
                if not self._stop_event.is_set():
                    # Restart watch mode
                    logger.info("Restarting watch mode...")
                    time.sleep(5)  # Wait before restart
                    self._start_watch_mode()
                return
            
            time.sleep(10)  # Check every 10 seconds
    
    def _start_periodic_indexing(self):
        """Start periodic indexing as fallback."""
        logger.info(f"Starting periodic indexing (interval: {INDEX_INTERVAL_SECONDS}s)")
        
        self._periodic_thread = threading.Thread(
            target=self._periodic_index_loop,
            daemon=True,
            name="cass-periodic-indexer"
        )
        self._periodic_thread.start()
    
    def _periodic_index_loop(self):
        """Periodically run incremental indexing."""
        while not self._stop_event.is_set():
            # Wait for the interval (interruptible)
            if self._stop_event.wait(timeout=INDEX_INTERVAL_SECONDS):
                break  # Stop event was set
            
            if not self._stop_event.is_set():
                logger.info("Running periodic incremental index...")
                self._run_index(full=False)
    
    def _run_index(self, full: bool = False):
        """Run the cass index command."""
        try:
            cmd = [self.cass_binary, "index"]
            if full:
                cmd.append("--full")
            
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=300,  # 5 minute timeout
                env={**os.environ, "CASS_DATA_DIR": self.data_dir}
            )
            
            self._last_index_time = datetime.now()
            self._index_count += 1
            
            if result.returncode == 0:
                logger.info(f"Index completed successfully (count: {self._index_count})")
            else:
                logger.warning(f"Index completed with errors: {result.stderr}")
                
        except subprocess.TimeoutExpired:
            logger.error("Index operation timed out")
        except Exception as e:
            logger.error(f"Index operation failed: {e}")
    
    def trigger_index(self, full: bool = False):
        """Manually trigger an index operation."""
        threading.Thread(
            target=self._run_index,
            args=(full,),
            daemon=True,
            name="cass-manual-index"
        ).start()
    
    def stop(self):
        """Stop the background indexer."""
        logger.info("Stopping background indexer...")
        self._stop_event.set()
        self._is_running = False
        
        # Stop watch process if running
        if self._watch_process and self._watch_process.poll() is None:
            logger.info(f"Terminating watch process (PID: {self._watch_process.pid})")
            self._watch_process.terminate()
            try:
                self._watch_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._watch_process.kill()
        
        logger.info("Background indexer stopped")
    
    def get_status(self) -> dict:
        """Get the current status of the background indexer."""
        watch_running = (
            self._watch_process is not None 
            and self._watch_process.poll() is None
        )
        periodic_running = (
            self._periodic_thread is not None 
            and self._periodic_thread.is_alive()
        )
        
        return {
            "is_running": self._is_running,
            "mode": "watch" if watch_running else ("periodic" if periodic_running else "stopped"),
            "watch_pid": self._watch_process.pid if watch_running else None,
            "last_index_time": self._last_index_time.isoformat() if self._last_index_time else None,
            "index_count": self._index_count,
            "index_interval_seconds": INDEX_INTERVAL_SECONDS if periodic_running else None
        }


# Global background indexer instance
background_indexer: Optional[BackgroundIndexer] = None

# Create MCP server
server = Server("cass-mcp-server")


def run_cass(args: list[str], timeout: int = 30) -> dict[str, Any]:
    """Execute cass command and return parsed output."""
    cmd = [CASS_BINARY] + args
    logger.info(f"Running: {' '.join(cmd)}")
    
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env={**os.environ, "CASS_DATA_DIR": DATA_DIR}
        )
        
        if result.returncode != 0:
            return {
                "error": True,
                "exit_code": result.returncode,
                "stderr": result.stderr.strip(),
                "stdout": result.stdout.strip()
            }
        
        # Try to parse as JSON
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError:
            return {"output": result.stdout.strip()}
            
    except subprocess.TimeoutExpired:
        return {"error": True, "message": f"Command timed out after {timeout}s"}
    except Exception as e:
        return {"error": True, "message": str(e)}


@server.list_tools()
async def list_tools() -> list[Tool]:
    """List available cass tools."""
    return [
        Tool(
            name="cass_search",
            description="""Search across all indexed coding agent histories (Copilot, Claude Code, Codex, Gemini, Cursor, Aider, etc.).
            
Use this to find past conversations, debugging sessions, solutions, and coding knowledge across all AI coding agents.

Examples:
- Search for error handling: query="authentication error"
- Find React solutions: query="React useState hook"
- Debug specific issues: query="memory leak fix", agent="claude_code"
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query. Supports wildcards: foo* (prefix), *foo (suffix), *foo* (contains)"
                    },
                    "agent": {
                        "type": "string",
                        "description": "Filter by agent: copilot, claude_code, codex, gemini, cursor, aider, chatgpt, cline, amp, opencode, pi_agent",
                        "enum": ["copilot", "claude_code", "codex", "gemini", "cursor", "aider", "chatgpt", "cline", "amp", "opencode", "pi_agent"]
                    },
                    "workspace": {
                        "type": "string",
                        "description": "Filter by workspace/project path"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 10, max: 100)",
                        "default": 10,
                        "minimum": 1,
                        "maximum": 100
                    },
                    "days": {
                        "type": "integer",
                        "description": "Filter to last N days"
                    },
                    "today": {
                        "type": "boolean",
                        "description": "Filter to today only"
                    },
                    "highlight": {
                        "type": "boolean",
                        "description": "Highlight matching terms in results"
                    }
                },
                "required": ["query"]
            }
        ),
        Tool(
            name="cass_stats",
            description="""Get statistics about indexed coding agent data.
            
Returns:
- Total conversations and messages indexed
- Breakdown by agent (Copilot, Claude, etc.)
- Top workspaces
- Date range of indexed data
""",
            inputSchema={
                "type": "object",
                "properties": {},
                "required": []
            }
        ),
        Tool(
            name="cass_capabilities",
            description="""Discover available features, supported agents, and limits.
            
Use this to check what connectors are available and what features the search supports.
""",
            inputSchema={
                "type": "object",
                "properties": {},
                "required": []
            }
        ),
        Tool(
            name="cass_timeline",
            description="""Show activity timeline - when were coding agents active?

Useful for understanding work patterns and finding conversations by time period.
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "days": {
                        "type": "integer",
                        "description": "Show last N days of activity",
                        "default": 7
                    },
                    "today": {
                        "type": "boolean",
                        "description": "Show only today's activity"
                    },
                    "group_by": {
                        "type": "string",
                        "description": "Group results by: hour, day, week",
                        "enum": ["hour", "day", "week"],
                        "default": "day"
                    },
                    "agent": {
                        "type": "string",
                        "description": "Filter to specific agent"
                    }
                },
                "required": []
            }
        ),
        Tool(
            name="cass_context",
            description="""Find related sessions for a given file or project path.

Use this to discover what AI agents have worked on in a specific codebase or file.
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File or directory path to find related sessions for"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum related sessions to return",
                        "default": 10
                    }
                },
                "required": ["path"]
            }
        ),
        Tool(
            name="cass_view",
            description="""View a specific conversation or message from search results.

Use the source_path and line_number from search results to view full context.
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the session file (from search results)"
                    },
                    "line": {
                        "type": "integer",
                        "description": "Line number to view (from search results)"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of context lines before/after",
                        "default": 5
                    }
                },
                "required": ["path"]
            }
        ),
        Tool(
            name="cass_expand",
            description="""Expand context around a specific line in a session file.

Shows messages before and after a specific point in a conversation.
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the session file"
                    },
                    "line": {
                        "type": "integer",
                        "description": "Line number to expand around"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of messages before/after to show",
                        "default": 5
                    }
                },
                "required": ["path", "line"]
            }
        ),
        Tool(
            name="cass_export",
            description="""Export a conversation to markdown format.

Useful for sharing or archiving important conversations.
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the session file to export"
                    },
                    "format": {
                        "type": "string",
                        "description": "Export format",
                        "enum": ["markdown", "json", "html"],
                        "default": "markdown"
                    }
                },
                "required": ["path"]
            }
        ),
        Tool(
            name="cass_health",
            description="""Quick health check of the cass index.

Returns whether the index is healthy and ready for queries.
""",
            inputSchema={
                "type": "object",
                "properties": {},
                "required": []
            }
        ),
        Tool(
            name="cass_index",
            description="""Trigger re-indexing of agent histories.

Use this to refresh the index after new conversations, or to do a full rebuild.
Note: This may take some time depending on the amount of data.
""",
            inputSchema={
                "type": "object",
                "properties": {
                    "full": {
                        "type": "boolean",
                        "description": "Do a full rebuild (slower but thorough)",
                        "default": False
                    }
                },
                "required": []
            }
        ),
    ]


@server.call_tool()
async def call_tool(name: str, arguments: dict[str, Any]) -> list[TextContent]:
    """Execute a cass tool."""
    logger.info(f"Tool call: {name} with args: {arguments}")
    
    try:
        if name == "cass_search":
            args = ["search", arguments["query"], "--json"]
            
            if arguments.get("agent"):
                args.extend(["--agent", arguments["agent"]])
            if arguments.get("workspace"):
                args.extend(["--workspace", arguments["workspace"]])
            if arguments.get("limit"):
                args.extend(["--limit", str(min(arguments["limit"], 100))])
            if arguments.get("days"):
                args.extend(["--days", str(arguments["days"])])
            if arguments.get("today"):
                args.append("--today")
            if arguments.get("highlight"):
                args.append("--highlight")
                
            result = run_cass(args)
            
        elif name == "cass_stats":
            result = run_cass(["stats", "--json"])
            
        elif name == "cass_capabilities":
            result = run_cass(["capabilities", "--json"])
            
        elif name == "cass_timeline":
            args = ["timeline", "--json"]
            
            if arguments.get("today"):
                args.append("--today")
            elif arguments.get("days"):
                args.extend(["--days", str(arguments["days"])])
            else:
                args.extend(["--days", "7"])
                
            if arguments.get("group_by"):
                args.extend(["--group-by", arguments["group_by"]])
            if arguments.get("agent"):
                args.extend(["--agent", arguments["agent"]])
                
            result = run_cass(args)
            
        elif name == "cass_context":
            args = ["context", arguments["path"], "--json"]
            if arguments.get("limit"):
                args.extend(["--limit", str(arguments["limit"])])
            result = run_cass(args)
            
        elif name == "cass_view":
            args = ["view", arguments["path"], "--json"]
            if arguments.get("line"):
                args.extend(["-n", str(arguments["line"])])
            if arguments.get("context"):
                args.extend(["-C", str(arguments["context"])])
            result = run_cass(args)
            
        elif name == "cass_expand":
            args = ["expand", arguments["path"], "-n", str(arguments["line"]), "--json"]
            if arguments.get("context"):
                args.extend(["-C", str(arguments["context"])])
            result = run_cass(args)
            
        elif name == "cass_export":
            args = ["export", arguments["path"]]
            fmt = arguments.get("format", "markdown")
            args.extend(["--format", fmt])
            result = run_cass(args, timeout=60)
            
        elif name == "cass_health":
            result = run_cass(["health", "--json"])
            
        elif name == "cass_index":
            args = ["index"]
            if arguments.get("full"):
                args.append("--full")
            result = run_cass(args, timeout=300)  # 5 min timeout for indexing
            
        else:
            result = {"error": True, "message": f"Unknown tool: {name}"}
            
        return [TextContent(
            type="text",
            text=json.dumps(result, indent=2, default=str)
        )]
        
    except Exception as e:
        logger.exception(f"Error executing tool {name}")
        return [TextContent(
            type="text",
            text=json.dumps({"error": True, "message": str(e)})
        )]


# SSE Transport setup
sse_transport = SseServerTransport("/messages/")


async def handle_sse(request):
    """Handle SSE connection."""
    from starlette.responses import Response
    logger.info(f"SSE connection from {request.client.host}")
    try:
        async with sse_transport.connect_sse(
            request.scope, request.receive, request._send
        ) as streams:
            await server.run(
                streams[0], streams[1], server.create_initialization_options()
            )
    except Exception as e:
        logger.warning(f"SSE connection closed: {e}")
    return Response(status_code=200)


async def handle_messages(request):
    """Handle incoming messages."""
    await sse_transport.handle_post_message(request.scope, request.receive, request._send)


async def health_check(request):
    """Health check endpoint."""
    indexer_status = background_indexer.get_status() if background_indexer else {"is_running": False}
    return JSONResponse({
        "status": "healthy",
        "server": "cass-mcp-server",
        "background_indexer": indexer_status
    })


async def indexer_status(request):
    """Get background indexer status."""
    if background_indexer:
        return JSONResponse(background_indexer.get_status())
    return JSONResponse({"error": "Background indexer not initialized"}, status_code=503)


async def trigger_index(request):
    """Manually trigger a re-index."""
    if not background_indexer:
        return JSONResponse({"error": "Background indexer not initialized"}, status_code=503)
    
    # Check for full rebuild flag
    full = False
    if request.method == "POST":
        try:
            body = await request.json()
            full = body.get("full", False)
        except:
            pass
    
    background_indexer.trigger_index(full=full)
    return JSONResponse({
        "status": "indexing",
        "full": full,
        "message": "Index operation triggered"
    })


async def info(request):
    """Server info endpoint."""
    indexer_status = background_indexer.get_status() if background_indexer else {"is_running": False}
    return JSONResponse({
        "name": "cass-mcp-server",
        "description": "MCP server for Coding Agent Session Search",
        "version": "1.0.0",
        "tools": [
            "cass_search", "cass_stats", "cass_capabilities", "cass_timeline",
            "cass_context", "cass_view", "cass_expand", "cass_export",
            "cass_health", "cass_index"
        ],
        "supported_agents": [
            "copilot", "claude_code", "codex", "gemini", "cursor",
            "aider", "chatgpt", "cline", "amp", "opencode", "pi_agent"
        ],
        "background_indexer": indexer_status,
        "config": {
            "watch_mode_enabled": ENABLE_WATCH_MODE,
            "index_on_start": INITIAL_INDEX_ON_START,
            "index_interval_seconds": INDEX_INTERVAL_SECONDS
        }
    })


def start_background_indexer():
    """Initialize and start the background indexer."""
    global background_indexer
    
    try:
        background_indexer = BackgroundIndexer(CASS_BINARY, DATA_DIR)
        background_indexer.start(
            use_watch_mode=ENABLE_WATCH_MODE,
            initial_index=INITIAL_INDEX_ON_START
        )
        logger.info("Background indexer started successfully")
    except Exception as e:
        logger.error(f"Failed to start background indexer: {e}")


def stop_background_indexer():
    """Stop the background indexer."""
    global background_indexer
    if background_indexer:
        background_indexer.stop()
        background_indexer = None


def signal_handler(signum, frame):
    """Handle shutdown signals."""
    logger.info(f"Received signal {signum}, shutting down...")
    stop_background_indexer()
    sys.exit(0)


# Register signal handlers
signal.signal(signal.SIGTERM, signal_handler)
signal.signal(signal.SIGINT, signal_handler)


# Starlette lifespan for startup/shutdown
from contextlib import asynccontextmanager

@asynccontextmanager
async def lifespan(app):
    """Manage app lifecycle - start indexer on startup, stop on shutdown."""
    logger.info("Application starting up...")
    start_background_indexer()
    yield
    logger.info("Application shutting down...")
    stop_background_indexer()


# Create Starlette app with routes
app = Starlette(
    debug=True,
    lifespan=lifespan,
    routes=[
        Route("/", info),
        Route("/health", health_check),
        Route("/indexer/status", indexer_status),
        Route("/indexer/trigger", trigger_index, methods=["GET", "POST"]),
        Route("/sse", handle_sse),
        Route("/messages/", handle_messages, methods=["POST"]),
    ],
)


if __name__ == "__main__":
    import uvicorn
    
    port = int(os.environ.get("PORT", 8080))
    host = os.environ.get("HOST", "0.0.0.0")
    
    logger.info(f"Starting cass MCP server on {host}:{port}")
    logger.info(f"CASS binary: {CASS_BINARY}")
    logger.info(f"Data directory: {DATA_DIR}")
    logger.info(f"Watch mode enabled: {ENABLE_WATCH_MODE}")
    logger.info(f"Index on start: {INITIAL_INDEX_ON_START}")
    logger.info(f"Index interval: {INDEX_INTERVAL_SECONDS}s")
    
    uvicorn.run(app, host=host, port=port, log_level="info")
