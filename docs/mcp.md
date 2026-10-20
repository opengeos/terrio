# AI Agents (MCP)

`terrio mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server: an AI agent
such as Claude Code, Claude Desktop, Cursor or VS Code's Copilot can run terrio's commands on your
data. Ask it to "summarize this GeoTIFF", "convert these shapefiles to GeoPackage in EPSG:3857" or
"make a hillshade of dem.tif" and it calls the matching command.

Each [command](cli.md) is a tool: `info`, `convert`, `translate`, `warp`, `mosaic`, `rasterize`,
`polygonize`, `dem`, `calc`, `proximity`, `sieve`, `crs`, `formats`, `grid` and `filter`. A tool
takes the command line after the command name as a list of arguments, and its description is the
command's `--help`, so the agent sees every option. The server speaks over standard input and
output, and runs each call as a separate `terrio` process: its output goes back to the agent, and
a command that fails says why.

## Set it up

Install the `terrio` command (see [Installation](installation.md)), then register it with your
client.

Claude Code:

```sh
claude mcp add terrio -- terrio mcp
```

Claude Desktop (`claude_desktop_config.json`), Cursor (`.cursor/mcp.json`) and most other
clients:

```json
{
  "mcpServers": {
    "terrio": { "command": "terrio", "args": ["mcp"] }
  }
}
```

VS Code (`.vscode/mcp.json`):

```json
{
  "servers": {
    "terrio": { "command": "terrio", "args": ["mcp"] }
  }
}
```

Without installing anything but [uv](https://docs.astral.sh/uv/), use `uvx` as the command:
`"command": "uvx", "args": ["terrio", "mcp"]`, or `claude mcp add terrio -- uvx terrio mcp`.

## What the agent can do

The tools read and write whatever the commands can: local files, and inputs from URLs and cloud
storage (see [Remote Data](remote-data.md)). `info`, `crs` and `formats` only read; the other
commands write their output files, and can overwrite existing ones. Clients ask before running a
tool unless you allow it, so review the arguments before you approve a command that writes.

Relative paths are relative to the directory the client started the server in; agents do better
with absolute paths. A command's output is cut after 100,000 bytes; for a large dataset, ask for
`info --json` of one layer (`-l`) rather than all of them.
