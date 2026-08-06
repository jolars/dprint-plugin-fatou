# dprint-plugin-fatou

A [dprint](https://dprint.dev) Wasm plugin that wraps the
[fatou](https://fatou.dev) formatter for the Julia language (`.jl`).

It is released independently of the main fatou CLI. The plugin lives in its own
repository so that its `plugin.wasm` release asset does not interfere with
fatou's own GitHub release stream, which the VS Code extension and the install
scripts resolve platform binaries from.

## Usage

Add the plugin with the dprint CLI:

```bash
dprint config add jolars/fatou
```

This adds a versioned, checksummed entry under `plugins` in your `dprint.json`:

```jsonc
{
  "fatou": {},
  "plugins": [
    "https://plugins.dprint.dev/jolars/fatou-x.x.x.wasm@<checksum>"
  ]
}
```

Then format:

```bash
dprint fmt
```

## Configuration

Configure under the `fatou` key in `dprint.json`. Supported keys:

| Key           | Values                         | Default                   |
| ------------- | ------------------------------ | ------------------------- |
| `lineWidth`   | integer                        | dprint global, else `92`  |
| `indentWidth` | integer                        | dprint global, else `4`   |
| `lineEnding`  | `auto`, `lf`, `crlf`, `native` | from global `newLineKind` |

Fatou always indents with spaces, so dprint's global `useTabs` has no effect.
The defaults follow Julia conventions (width 92, indent 4) and match the
`fatou` CLI's.

Formatting is deterministic and rule-based: the input's existing line breaks
never influence the result. See [the fatou docs](https://fatou.dev) for the
formatting rules themselves.

## Building

The plugin is only usable when built for `wasm32-unknown-unknown`:

```bash
cargo build --release --target wasm32-unknown-unknown
```

The resulting `target/wasm32-unknown-unknown/release/dprint_plugin_fatou.wasm`
is published as `plugin.wasm` on each GitHub release.

It also builds for the host target — `generate_plugin_code!` is gated to
`target_arch = "wasm32"` — so `cargo test` can run the config, formatting, and
schema tests natively. That native build is not a usable plugin.

## License

MIT
