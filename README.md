# Wez's Terminal

<img height="128" alt="WezTerm Icon" src="https://raw.githubusercontent.com/wezterm/wezterm/main/assets/icon/wezterm-icon.svg" align="left"> *A GPU-accelerated cross-platform terminal emulator and multiplexer written by <a href="https://github.com/wez">@wez</a> and implemented in <a href="https://www.rust-lang.org/">Rust</a>*

User facing docs and guide at: https://wezterm.org/

![Screenshot](docs/screenshots/two.png)

*Screenshot of wezterm on macOS, running vim*

## Installation

https://wezterm.org/installation

## Running this fork on macOS

If you want to try the code in this repository, the simplest way on macOS is to
build and run it from source.

### Prerequisites

- Install Apple's command line tools:

  ```console
  xcode-select --install
  ```

- Install Rust with `rustup`: https://rustup.rs/

### Build

From the repository root:

```console
cargo build -p wezterm-gui --release
```

The GUI binary will be built at:

```text
target/release/wezterm-gui
```

### Run

To launch it directly from the repository checkout:

```console
cargo run -p wezterm-gui --release
```

Or run the built binary directly:

```console
./target/release/wezterm-gui
```

If you already have another WezTerm installed on your Mac, a temporary test run
can accidentally try to connect to that existing mux server and fail with a
version mismatch. For a clean one-off test launch, prefer:

```console
cargo run -p wezterm-gui --release -- --always-new-process --no-auto-connect
```

If needed, quit the existing WezTerm first:

```console
osascript -e 'tell application "WezTerm" to quit'
```

### Install on your own Mac

If you want to keep using the version built from this repository on your Mac,
one practical approach is:

1. Build the release binary:

   ```console
   cargo build -p wezterm-gui --release
   ```

2. Copy it somewhere on your `PATH`, for example:

   ```console
   mkdir -p ~/.local/bin
   cp ./target/release/wezterm-gui ~/.local/bin/wezpilot-wezterm
   ```

3. Add that directory to your shell startup file if needed:

   ```console
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
   ```

4. Restart your shell, then launch it with:

   ```console
   wezpilot-wezterm
   ```

If you only want the official WezTerm release on macOS rather than this source
tree, the standard Homebrew install is:

```console
brew install --cask wezterm
```

## AI chat overlay

This fork adds a small AI chat overlay for the active pane.

- Open it with `Ctrl+Shift+A`
- Use a normal prompt for one-shot help and command generation
- Use `/watch <instruction>` to keep monitoring the active pane and auto-respond
- Use `/watch off` to stop automation

The overlay uses OpenRouter model `moonshotai/kimi-k2.5` and expects the API key
in the `OPENROUTER_API_KEY` environment variable:

```console
export OPENROUTER_API_KEY=...
```

## Getting help

This is a spare time project, so please bear with me.  There are a couple of channels for support:

* You can use the [GitHub issue tracker](https://github.com/wezterm/wezterm/issues) to see if someone else has a similar issue, or to file a new one.
* Start or join a thread in our [GitHub Discussions](https://github.com/wezterm/wezterm/discussions); if you have general
  questions or want to chat with other wezterm users, you're welcome here!
* There is a [Matrix room via Element.io](https://app.element.io/#/room/#wezterm:matrix.org)
  for (potentially!) real time discussions.

The GitHub Discussions and Element/Gitter rooms are better suited for questions
than bug reports, but don't be afraid to use whichever you are most comfortable
using and we'll work it out.

## Supporting the Project

If you use and like WezTerm, please consider sponsoring it: your support helps
to cover the fees required to maintain the project and to validate the time
spent working on it!

[Read more about sponsoring](https://wezterm.org/sponsor.html).

* [![Sponsor WezTerm](https://img.shields.io/github/sponsors/wez?label=Sponsor%20WezTerm&logo=github&style=for-the-badge)](https://github.com/sponsors/wez)
* [Patreon](https://patreon.com/WezFurlong)
* [Ko-Fi](https://ko-fi.com/wezfurlong)
* [Liberapay](https://liberapay.com/wez)
