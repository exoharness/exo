---
title: Installation
parent: Getting Started
nav_order: 1
---

# Installation

## Prerequisites

- Rust (see `mise.toml` for the pinned toolchain)
- Node.js + pnpm (for TypeScript harnesses)
- Docker (for sandboxed conversations)

## Build the CLI

```bash
cargo build -p exo
./target/debug/exo --help
```
