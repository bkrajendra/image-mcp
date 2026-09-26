<div align="center">

# image-mcp

**A sandboxed image processing MCP server for AI agents.**

Resize, compress, convert and optimize images — with every path checked against a workspace jail before it touches disk.

[![Release](https://img.shields.io/github/v/release/bkrajendra/image-mcp?style=flat-square&label=release)](https://github.com/bkrajendra/image-mcp/releases/latest)
[![Release workflow](https://img.shields.io/github/actions/workflow/status/bkrajendra/image-mcp/release.yml?branch=main&style=flat-square&label=build)](https://github.com/bkrajendra/image-mcp/actions/workflows/release.yml)
[![Rust](https://img.shields.io/badge/rust-2024-orange?style=flat-square)](Cargo.toml)

</div>

---

`image-mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io) over stdio, so any MCP-capable agent (Claude Code, Claude Desktop, etc.) can inspect and transform images through a fixed set of tools — without ever reading or writing outside the directory you point it at.

## Features

- **image_info** — dimensions, format, file size
- **resize_image** — with automatic aspect-ratio preservation
- **compress_image** — quality-based JPEG compression
- **convert_image** — between JPEG, PNG and WebP
- **optimize_image** — binary-search JPEG quality to hit a target file size
- **batch_optimize** — run optimization over a whole directory at once
- **Workspace jail** — every input/output path is resolved and verified against a single allowed directory before any file is created, read, or written

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bkrajendra/image-mcp/main/install.sh | sh
```

This detects your OS/architecture, downloads the matching binary from the [latest release](https://github.com/bkrajendra/image-mcp/releases/latest), and installs it to `~/.local/bin` (override with `INSTALL_DIR=...`). Supported platforms: Linux and macOS, `x86_64` and `arm64`. Windows users can grab the `.zip` asset directly from the [releases page](https://github.com/bkrajendra/image-mcp/releases).

To install a specific version instead of the latest:

```sh
curl -fsSL https://raw.githubusercontent.com/bkrajendra/image-mcp/main/install.sh | IMAGE_MCP_VERSION=v0.1.3 sh
```

To uninstall:

```sh
curl -fsSL https://raw.githubusercontent.com/bkrajendra/image-mcp/main/install.sh | sh -s -- uninstall
```

### Build from source

```sh
git clone https://github.com/bkrajendra/image-mcp.git
cd image-mcp
cargo build --release
# binary at target/release/image-mcp
```

## Usage

```sh
image-mcp --workspace /path/to/your/project
```

Every tool call is restricted to files inside `--workspace`; paths outside it (including via `..` traversal or symlinks) are rejected.

### Configure as an MCP server

```json
{
  "mcpServers": {
    "image-mcp": {
      "command": "image-mcp",
      "args": ["--workspace", "/path/to/your/project"]
    }
  }
}
```

## Tools

| Tool | Description |
| --- | --- |
| `image_info` | Inspect an image's dimensions, format and file size |
| `resize_image` | Resize an image; preserves aspect ratio by default |
| `compress_image` | Recompress an image without changing its dimensions |
| `convert_image` | Convert between JPEG, PNG and WebP |
| `optimize_image` | Resize and/or compress an image toward a target file size (JPEG) |
| `batch_optimize` | Run `optimize_image` over every supported image in a directory |

PNG and WebP output is always encoded losslessly; quality and target-size controls apply to JPEG output.

## Development

```sh
cargo test     # unit + integration tests
cargo clippy --all-targets
```

## Releases

Every commit pushed to `main` is automatically tagged (`vX.Y.Z`, patch bump) and built for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

See [`.github/workflows/release.yml`](.github/workflows/release.yml).
