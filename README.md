# wl-clip-persist
Normally, when you copy something on Wayland and then close the application you copied from, the copied data (e.g. text) disappears and you cannot paste it anymore. If you run wl-clip-persist in the background, however, the copied data persists.

## How it works
Whenever you copy something, it reads all the clipboard data into memory and then overwrites the clipboard with the data from our memory.
By doing so, the data is available even after the program you copied from exits.

## Usage
### Clipboard Type
When you specify the clipboard to operate on, the clipboard data there will persist.
The clipboards we don’t operate on will continue to behave like before.

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

## Optional arguments
### Wayland Display
The wayland display is usually derived from the environment variables (`WAYLAND_DISPLAY`), but it can also be set explicitly.
```
wl-clip-persist --clipboard regular --display wayland-1
```

### Timeout
*Default read timeout: 500ms*<br>
*Default write timeout: 3000ms*

It is possible to change the read and write timeouts.
In this example, the read timeout is reduced to 50ms and the write timeout to 100ms.
```
wl-clip-persist --clipboard regular --read-timeout 50 --write-timeout 1000
```

Currently, whenever we read the clipboard data it blocks our main thread. That means that until we haven’t read the entire data or the read timeout occurred, we won’t be able to see new clipboard events. It is even possible that while we are reading the clipboard data a new clipboard event occurs and we overwrite their clipboard data with our old data. When a read timeout occurs, the current clipboard is overwritten with the data that we were able to read until then. For example, when a clipboard event offers `image/png` and `text/plain` data and we are only able to read the `text/plain` data entirely until the timeout occurs, then the clipboard will be overwritten with our data that only offers `text/plain`.
When we write clipboard data (i.e. send the clipboard data to another program), though, we do that in a background thread.

### Filter
*Default: no filter*

With `--all-mime-type-regex <REGEX>` only selection events where all offered MIME types have a match for the regex are handled.
You might want to use this option to ignore selection events that offer for example images. If the event is ignored, you will still be able to paste the images, but only for as long as the program you copied them from is open.

Ignore events that offer images
```
wl-clip-persist --clipboard regular --all-mime-type-regex '(?i)^(?!image/).+'
```

Ignore most events that offer something else than text
```
wl-clip-persist --clipboard regular --all-mime-type-regex '(?i)^(?!(?:image|audio|video|font|model)/).+'
```

Since the clipboard data might be huge (e.g. for images), this option might be helpful to avoid [blocking the main thread by reading the data](#timeout "also see here") for too long. Also, it might happen that the program we are copying the data from might freeze during the time we are reading the data, so it is possible to use this option to avoid these scenarios for specific data types like images.

### Logging
You can modify the log level to see more of what is going on, e.g.
```
RUST_LOG=trace wl-clip-persist --clipboard regular
```

## Build from source
* Install `rustup` to get the `rust` compiler installed on your system. [Install rustup](https://www.rust-lang.org/en-US/install.html)
* Rust version 1.61.0 or later is required
* Install dependencies:
  - wayland
  - wayland-protocols (compile-time dependency)
  - pkg-config (compile-time dependency)
* Build in release mode: `cargo build --release`
* The resulting executable can be found at `target/release/wl-clip-persist`

## Thanks
* [wl-clipboard-rs](https://github.com/YaLTeR/wl-clipboard-rs) for showing how to interact with the Wayland clipboard
* [wezterm](https://github.com/wez/wezterm) for the initial read and write functions with timeouts
