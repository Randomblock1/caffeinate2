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
- **Right click:** choose mode (Display, Disk, System, System on AC, User active, Entirely), set an optional **Time limit** (Off, 15 minutes, 30 minutes, 1 hour, and so on), optionally set **Until app quits** (pick a running app or **Choose application…** for any `.app`), toggle **Upgrade other apps' sleep prevention**, toggle **Start at login**, or Quit.

When a time limit is set, left-clicking to start sleep prevention automatically turns it off again after that duration (similar to `caffeinate2 -t`). Changing the time limit while sleep prevention is active restarts the countdown from that moment (it is not measured from when the session started). **Until app quits** keeps prevention on until every instance of the chosen app has exited; if the app is not running when you turn on, caffeinate2 waits for it to launch first. The target is remembered by bundle ID (for example `Codex.app` stays matched across restarts). Time limit and until-app quit whichever comes first. The menu bar tooltip shows remaining time and/or app status while active.

**Upgrade other apps' sleep prevention** is a background watcher. Tools like Claude Code, Codex, and `caffeinate -i` keep the Mac awake with a low-level assertion that still lets it sleep when you close the lid — so a long-running agent dies the moment the lid shuts. When this is on, caffeinate2 watches for any process holding that idle-system-sleep assertion and, while one is present, takes a stronger **Entirely** hold (the only mode that ignores lid close on both AC and battery), then releases it about 20 seconds after the external assertion goes away. While it is upgrading, the menu shows which process holds the assertion (when its name is available; otherwise a generic **Upgrading external app** line) — right-click to see it; a single named app appears as one line, and several collapse into an expandable **Upgrading N apps** submenu (the tooltip lists them too). It only reacts to idle-system-sleep assertions (what agents use); assertions it sees but does not upgrade are listed in the menu under **Ignoring _name_ (_reason_)** lines (collapsing into an **Ignoring N assertions** submenu when there are several) so a quiet menu is never mistaken for "nothing is keeping the Mac awake." The reasons cover the macOS power daemon (`powerd`, shown as _system process_) and display-only assertions from video players (_display only_); caffeinate2 never reacts to its own holds. Because it upgrades to Entirely mode, it needs the privileged helper (see below); enabling the toggle installs it if necessary, prompting once. Note this reacts to *any* such assertion, so a desktop app that holds one while open (some Electron apps do) will also keep the upgrade active until it quits. A manual left-click overrides the watcher: if you click the icon off while it is upgrading, it stays off until the external assertion goes away (the next one upgrades again as usual).

Settings are stored in `~/Library/Application Support/caffeinate2/tray.toml`.

**Start at login** writes `~/Library/LaunchAgents/com.randomblock1.caffeinate2-tray.plist` and takes effect at the next login (the tray does not start a second instance immediately). The menu toggle and the CLI flags below use the same LaunchAgent:

`caffeinate2-tray --install-launch-agent`

`caffeinate2-tray --uninstall-launch-agent`

`caffeinate2-tray --launch-agent-status`

Unsigned binaries may require running from Terminal once (right-click → Open) or allowing in Privacy & Security.

## Entirely mode (no repeated sudo)

Entirely mode (`-e` / tray **Entirely**) disables system sleep even when the lid is closed. It uses a small privileged helper daemon.

**One-time setup** (either method):

1. Tray: select **Entirely** and approve the administrator dialog when prompted, or
2. CLI: `sudo caffeinate2 --install-helper`

Helper install requires `caffeinate2-helper` next to `caffeinate2`: use `cargo install caffeinate2 --features full`, build with `--features full`, or extract the GitHub release bundle (all three binaries together).

After that, `caffeinate2 -e` and tray Entirely use the helper without further passwords.

**Who can use it:** root and administrator accounts, plus members of the `caffeinate2` group (created by `--install-helper`). The helper denies everyone else, since disabling sleep entirely is otherwise a root-only setting. To allow a standard account:

`sudo dseditgroup -o edit -a USERNAME -t user caffeinate2`

The grant takes effect on the user's next attempt (no logout needed). Denied requests show the exact grant command; `caffeinate2 --status` works for every account.

Remove the helper: `sudo caffeinate2 --uninstall-helper`

Check helper state (is it running, how many holds, is sleep disabled): `caffeinate2 --status`

Helper install also adds `/etc/newsyslog.d/com.randomblock1.caffeinate2.helper.conf` so `/var/log/caffeinate2-helper.log` is rotated.

## Usage

```plaintext
Usage: caffeinate2 [OPTIONS] [COMMAND]...

Arguments:
  [COMMAND]...  Wait for given command to complete (takes priority above timeout and pid)

Options:
  -v, --verbose             Verbose mode
      --dry-run             Dry run. Don't actually sleep. Useful for testing
      --drop-root           Drop root privileges in command. You need root to disable sleep entirely, but some programs don't want to run as root
      --shell               Run COMMAND through /bin/sh -c instead of executing it directly
  -d, --display             Disable display sleep
  -m, --disk                Disable disk idle sleep
  -i, --system              Disable idle system sleep. [DEFAULT]
  -s, --system-on-ac        Disable system sleep while not on battery
  -e, --entirely            Disable system sleep entirely (ignores lid closing)
  -u, --user-active         Declare the user is active. If the display is off, this option turns it on and prevents it from going into idle sleep
  -t, --timeout <DURATION>  Wait for X seconds. Also supports time units (like "1 day 2 hours 3mins 4s")
  -w, --waitfor <PID>       Wait for program with PID X to complete and pass its exit code
      --install-helper      Install the privileged helper for entirely mode (admin prompt or sudo)
      --uninstall-helper    Remove the privileged helper (requires root)
      --status              Show entirely-mode helper status (holders and sleep state)
  -h, --help                Print help
  -V, --version             Print version
```

## Sleep Timers (in order of priority)

### Command

Sleep disabled until the command completes. By default the command is executed directly, so arguments keep their boundaries (for example, a filename containing spaces stays one argument). Use `--shell` when you intentionally want shell features such as pipes, globbing, or `&&`. Timeout and PID will be ignored if a command is specified.

`caffeinate2 sleep 5`

`caffeinate2 --shell 'sleep 5 && echo done'`

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
