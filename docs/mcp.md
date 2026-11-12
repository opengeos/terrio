# AI Agents (MCP)

`terrio mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server: an AI agent
such as Claude Code, Claude Desktop, Cursor or VS Code's Copilot can run terrio's commands on your
data. Ask it to "summarize this GeoTIFF", "convert these shapefiles to GeoPackage in EPSG:3857" or
"make a hillshade of dem.tif" and it calls the matching command.

Each [command](cli.md) except `mcp`, `ui` and `view` is a tool, from `info`, `convert` and `warp` to `vsi`,
`pipeline` and `mdread`. A tool
takes the command line after the command name as a list of arguments, and its description is the
command's `--help`, so the agent sees every option. The server speaks over standard input and
output, and runs each call as a separate `terrio` process: its output goes back to the agent, and
a command that fails says why.

For native datum/geoid grid lookups, tool processes use the host's search directories and
reject grids named by absolute or dot-relative paths inside CRS definitions. Configure
trusted directories through `TERRIO_GRID_DIR`, `PROJ_DATA` or `PROJ_LIB` in the server's
environment, then use the grid's name. See [grid lookup policy](crs.md#datum-shift-grids).
CRS definition-file arguments accept regular files, including symlinks to them; pipes
and devices are refused. These rules govern CRS lookups, not other tool input paths.

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
storage (see [Remote Data](remote-data.md)).

HEC-RAS geometry and plan HDF files (the `hecras` feature, part of the default build) work like
other vector data: `info` lists their layers, and `info` and `convert` take the HECRAS open options
(`--oo "CELL_SUMMARIES=Maximum Water Surface"`, see [HEC-RAS](hecras.md)), so an agent can "export
the maximum water surface of each 2D cell of this plan to GeoPackage".

`preview` returns the PNG it draws as image content, which the agent can look at: "show me this
DEM with the terrain colormap" works without a file. See [preview](cli.md#preview).

## Permissions and safety

terrio does not sandbox its tools: an agent can do what the server process can. Configure the MCP
client's tool permissions, the server's filesystem access and its cloud credentials to the scope
you intend to allow, and review source and destination arguments before permitting a change.

- `info`, `formats`, `crs`, `mdinfo`, `mdread`, `compare`, `validate-cog`, `gpkg-validate`,
  `identify`, `dataset-files` and `check` are annotated read-only. The other commands can write
  output files, and can overwrite existing ones.
- These annotations are hints for the client, not enforcement. terrio requires no separate
  confirmation argument.
- The `vsi` tool also copies local files to S3 or Azure destinations, without restricting them,
  and recursively deletes local directories or object prefixes, using the credentials available
  to the server process.
- The CLI's protected-path and object-prefix guards are the only built-in path boundary. Other
  local paths the server can write to are allowed, including recursive changes outside its current
  directory.

## Limits

Over MCP, `@FILE` input lists are refused: pass each input as an item of `args`. Wildcards in an
input's file name (`tiles/*.tif`) are expanded, as on the [command line](cli.md#commands).

Relative paths are relative to the directory the client started the server in; agents do better
with absolute paths. A command's output (standard output and error together) is cut after 100,000
bytes; for a large dataset, ask for `info --json` of one layer (`-l`) rather than all of them.

Commands run in the background, so the server keeps answering while a long `warp` or `filter`
works. When the client cancels a call (or times it out, if it sends a cancellation), or quits, the
command is stopped. The tool calls of a JSON-RPC batch run the same way, at most 8 of a batch's
calls at a time (single calls are not limited), and the batch's replies, those of its other
requests included, are sent together once its calls are done. As with single calls, closing the
server's input stops the calls still running, so a client waits for a batch's reply before it
closes the stream.
