# caffeinate2

![logo](https://randomblock1.com/assets/images/caffeinate2.svg)

`caffeinate` but it's written in Rust and has more options. Keeps your Mac wide awake.

## Current Status

Almost ready for 1.0.0.

## Installation

### GitHub Releases

Download the latest release binary from [the releases page](https://github.com/randomblock1/caffeinate2/releases/latest).

### Homebrew

_This won't be available until version 1.0.0._

### Cargo

CLI only (default):

`cargo install caffeinate2`

Menu bar + privileged helper:

`cargo install caffeinate2 --features full`

From a clone:

`cargo build --release --features full`

## Menu bar

Run `caffeinate2-tray` after installing with `--features full`.

- **Left click:** toggle the selected sleep mode on/off.
- **Right click:** choose mode (Display, Disk, System, System on AC, User active, Entirely), set an optional **Time limit** (Off, 15 minutes, 30 minutes, 1 hour, and so on), toggle **Start at login**, or Quit.

When a time limit is set, left-clicking to start sleep prevention automatically turns it off again after that duration (similar to `caffeinate2 -t`). The menu bar tooltip shows the remaining time while active.

Settings are stored in `~/Library/Application Support/caffeinate2/tray.toml`.

Unsigned binaries may require running from Terminal once (right-click → Open) or allowing in Privacy & Security.

## Entirely mode (no repeated sudo)

Entirely mode (`-e` / tray **Entirely**) disables system sleep even when the lid is closed. It uses a small privileged helper daemon.

**One-time setup** (either method):

1. Tray: select **Entirely** and approve the administrator dialog when prompted, or
2. CLI: `sudo caffeinate2 install-helper`

After that, `caffeinate2 -e` and tray Entirely use the helper without further passwords.

Remove the helper: `sudo caffeinate2 uninstall-helper`

## Usage

```plaintext
Usage: caffeinate2 [OPTIONS] [COMMAND]...

Arguments:
  [COMMAND]...  Wait for given command to complete (takes priority above timeout and pid)

Options:
  -v, --verbose             Verbose mode
      --dry-run             Dry run. Don't actually sleep. Useful for testing
      --drop-root           Drop root privileges in command. You need root to disable sleep entirely, but some programs don't want to run as root
  -d, --display             Disable display sleep
  -m, --disk                Disable disk idle sleep
  -i, --system              Disable idle system sleep. [DEFAULT]
  -s, --system-on-ac        Disable system sleep while not on battery
  -e, --entirely            Disable system sleep entirely (ignores lid closing)
  -u, --user-active         Declare the user is active. If the display is off, this option turns it on and prevents it from going into idle sleep
  -t, --timeout <DURATION>  Wait for X seconds. Also supports time units (like "1 day 2 hours 3mins 4s")
  -w, --waitfor <PID>       Wait for program with PID X to complete and pass its exit code
  -h, --help                Print help
  -V, --version             Print version

Subcommands:
  install-helper            Install privileged helper for entirely mode (root)
  uninstall-helper          Remove privileged helper (root)
```

## Sleep Timers (in order of priority)

### Command

Sleep disabled until the command completes. You should enclose the command in quotes to prevent your shell from prematurely executing or piping it, although it isn't strictly required. Timeout and PID will be ignored if a command is specified.

`caffeinate2 'sleep 5'`

### Timeout and PID

Sleep is disabled for a certain amount of time or until the program with the specified PID completes. If both are
specified, it waits until one of them completes.

Timeout can either be a number of seconds or a duration string. For example, you can pass `-t 600` or `-t 10m` to wait
for 10 minutes. You can create more descriptive durations, like `-t "1 hour and 30 minutes"`. Supported units include
seconds, minutes, hours, days, weeks, months, and years, plus common short forms like `s`, `m`, `h`, and `d`.
**YOU MUST USE QUOTATION MARKS FOR MULTI-WORD DURATIONS TO WORK.** Otherwise, it will try to parse anything that's past
the space as a command, and ignore the timeout.

For PIDs, it will wait until the specified program exits. If the program doesn't exist, it will immediately exit with an
error. Once the program completes, caffeinate2 will exit with the same exit code as the program.

`caffeinate2 -t 600`

`caffeinate2 -t "1 hour and 30 minutes"`

`caffeinate2 -w 1234`

`caffeinate2 -t 600 -w 1234`

### None of the above

Sleep will be disabled indefinitely until you press `Ctrl+C`.

`caffeinate2`

## License

This project is licensed under the [MIT License](LICENSE.txt).
