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

HEC-RAS geometry and plan HDF files (the `hecras` feature, part of the default build) work like other vector data: `info` lists their layers, and `info`
and `convert` take the HECRAS open options (`--oo "CELL_SUMMARIES=Maximum Water Surface"`, see
[HEC-RAS](hecras.md)), so an agent can "export the maximum water surface of each 2D cell of this
plan to GeoPackage".

The tools read and write whatever the commands can: local files, and inputs from URLs and cloud
storage (see [Remote Data](remote-data.md)). `info`, `crs` and `formats` only read; the other
commands write their output files, and can overwrite existing ones. The `vsi` tool also copies
local files to S3 or Azure destinations and recursively deletes local directories or object
prefixes with the same options and path guards as the CLI. It uses credentials available to the
server process. Its write/destructive annotations are hints, not authorization enforcement;
Terrio does not require a separate confirmation argument or restrict upload destinations.
Configure the MCP client's tool permissions, the server's filesystem access and cloud credentials
to the scope you intend to allow, and review source and destination arguments before permitting
a mutation.
The CLI's protected-path and object-prefix guards are the complete built-in path boundary:
other local paths writable by the server are allowed, including recursive mutations outside
its current directory. The tool does not add a filesystem sandbox.


Relative paths are relative to the directory the client started the server in; agents do better
with absolute paths. A command's output (standard output and error together) is cut after 100,000
bytes; for a large dataset, ask for `info --json` of one layer (`-l`) rather than all of them.

Commands run in the background, so the server keeps answering while a long `warp` or `filter`
works. When the client cancels a call (or times it out, if it sends a cancellation), or quits, the
command is stopped.
