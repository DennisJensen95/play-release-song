# Play release song

This app will play a release song on all Sonos devices on the local network when a new release is detected.

## Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled

## Getting Started

### Development Environment

To enter the development environment:

```bash
nix develop
```

This will provide all the necessary tools and dependencies for development.

### Building the Project

From within the development environment:

```bash
cargo build
```

### Running the Project

From within the development environment:

```bash
cargo run
```

## Project Structure

- `src/main.rs` - Main application entry point
- `Cargo.toml` - Rust package configuration
- `flake.nix` - Nix flake definition for the project
