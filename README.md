# wl-clip-persist
Normally, when you copy something on Wayland and then close the application you copied from, the copied data (e.g. text) disappears and you cannot paste it anymore. If you run wl-clip-persist in the background, however, the copied data persists.

## Usage
Regular clipboard
```
wl-clip-persist --clipboard regular
```

Primary clipboard
```
wl-clip-persist --clipboard primary
```

Regular and Primary clipboard
```
wl-clip-persist --clipboard both
```

You can also modify the log level to see more of what is going on, e.g.
```
RUST_LOG=trace wl-clip-persist --clipboard regular
```

## Build from source
* Install `rustup` to get the `rust` compiler installed on your system. [Install rustup](https://www.rust-lang.org/en-US/install.html)
* Rust version 1.61.0 or later is required
* Install dependencies:
  - wayland
  - wayland-protocols (compile-time dependency)
* Build in release mode: `cargo build --release`
* The resulting executable can be found at `target/release/wl-clip-persist`

## Thanks
* [wl-clipboard-rs](https://github.com/YaLTeR/wl-clipboard-rs) for showing how to interact with the Wayland clipboard
* [wezterm](https://github.com/wez/wezterm) for the initial read and write functions with timeouts
