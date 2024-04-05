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

> [!NOTE]
> The general recommendation is to operate on the regular clipboard only,
since the primary clipboard seems to have unintended side effects for some applications,
[see here](#primary-selection-mode-breaks-the-selection-system-3).

## Optional arguments
### Write timeout
*Default write timeout: 3000 ms*

It is possible to change the write timeout.
In this example, the write timeout is reduced to 1000 ms.
```
wl-clip-persist --clipboard regular --write-timeout 1000
```

If the data size exceeds the pipe buffer capacity, we will have to
wait for the receiving client to read some of the content to write the rest to it.
To avoid keeping old clipboard data around for too long, there is a timeout
which is also useful to limit the memory usage.

### Ignore event on error
*Default: disabled*

With `--ignore-event-on-error` only selection events where no error occurred are handled. If an error occurred and the selection event is ignored, you will still be able to paste the clipboard, but only for as long as the program you copied from is open.

When this option is disabled, it will try to read the entire data for as many MIME types as possible. For example, when a clipboard event offers `image/png` and `text/plain` data and we are only able to read the `text/plain` data entirely because a read error occurred for `image/png`, then the clipboard will be overwritten with our data that only offers `text/plain`.

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

### Selection size limit
*Default: no limit*

With `--selection-size-limit <BYTES>` only selection events whose total data size does not exceed the size limit are handled. If the size limit has been exceeded, you will still be able to paste the clipboard, but only for as long as the program you copied from is open.

Ignore events that exceed the size of 1 MiB
```
wl-clip-persist --clipboard regular --selection-size-limit 1048576
```

This option can be used to limit the memory usage.

### Logging
You can modify the log level to see more of what is going on, e.g.
```
RUST_LOG=trace wl-clip-persist --clipboard regular
```

## Troubleshooting
### Primary selection mode breaks the selection system ([#3](https://github.com/Linus789/wl-clip-persist/issues/3))
> especially those based on GTK, e.g. Thunar and Inkscape

> [...] once you start using the primary mode or both, it becomes impossible to select text, because once you release the cursor to finalize the selection, it disappears.

**Solution:**
Use the regular clipboard only, e.g.
```
wl-clip-persist --clipboard regular
```

### Inkscape crashes when copy-pasting anything ([#7](https://github.com/Linus789/wl-clip-persist/issues/7))
**Solution:**
```
wl-clip-persist --clipboard regular --all-mime-type-regex '(?i)^(?!image/x-inkscape-svg).+'
```

## FAQ
### Is it possible to have a clipboard history?
It is perfectly possible to use a clipboard history application alongside wl-clip-persist.
For example [cliphist](https://github.com/sentriz/cliphist) will work.

## Build from source
* Install `rustup` to get the `rust` compiler installed on your system. [Install rustup](https://www.rust-lang.org/en-US/install.html)
* Rust version 1.76.0 or later is required
* Build in release mode: `cargo build --release`
* The resulting executable can be found at `target/release/wl-clip-persist`

## Thanks
* [wl-clipboard-rs](https://github.com/YaLTeR/wl-clipboard-rs) for showing how to interact with the Wayland clipboard
* [wl-gammarelay-rs](https://github.com/MaxVerevkin/wl-gammarelay-rs) for showing how to use [wayrs](https://github.com/MaxVerevkin/wayrs)
