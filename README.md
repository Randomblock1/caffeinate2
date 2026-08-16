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

## Usage

```plaintext
Usage: caffeinate2 [OPTIONS] [COMMAND]...

Arguments:
  [COMMAND]...  Trailing command to run (takes priority over --timeout and --waitfor).
                Use `--` before the command when needed (see examples below).

Options:
  -v, --verbose             Verbose mode
      --dry-run             Dry run. Don't actually prevent sleep. Useful for testing
      --drop-root           Drop root privileges in command. You need root to disable sleep entirely, but some programs don't want to run as root
      --shell               Run COMMAND through /bin/sh -c instead of executing it directly
  -d, --display             Disable display sleep
  -m, --disk                Disable disk idle sleep
  -i, --system              Disable idle system sleep. [DEFAULT]
  -s, --system-on-ac        Disable system sleep while not on battery
  -e, --entirely            Disable system sleep entirely (ignores lid closing)
  -u, --user-active         Declare the user is active. If the display is off, this option turns it on and prevents it from going into idle sleep
  -t, --timeout <DURATION>  Duration to wait before timing out. Bare numbers are seconds; quote
                            human-readable durations (e.g. "1 hour and 30 minutes"). Use `--`
                            before a trailing command when it could be confused with flags
  -w, --waitfor <PID>       Wait for program with PID X to complete and pass its exit code
      --install-helper      Install the privileged helper for entirely mode (admin prompt or sudo)
      --uninstall-helper    Remove the privileged helper (requires root)
      --status              Show entirely-mode helper status (holders and sleep state)
  -h, --help                Print help
  -V, --version             Print version
```

## Sleep Timers (in order of priority)

### Command

Sleep is disabled until the command completes. Timeout and PID are ignored when a trailing command is present.

By default the command is executed directly, so arguments keep their boundaries (a filename with spaces stays one argument). Use `--shell` when you want shell features such as pipes, globbing, or `&&`.

Put `--` before the command when it could be parsed as another option or argument:

`caffeinate2 sleep 5`

`caffeinate2 --shell 'sleep 5 && echo done'`

`caffeinate2 -t 3600 -- ./script.sh --verbose`

`caffeinate2 -t 3600 -- hour`

### Timeout and PID

Sleep is disabled for a certain amount of time, or until the program with the specified PID completes. If both are specified, it waits until one of them completes. The `-t` countdown measures awake time: if the Mac sleeps before it elapses (for example the lid is closed, which the default modes don't prevent), the countdown pauses and resumes on wake.

**`-t` / `--timeout` takes one argument:**

- **Bare number** → seconds (e.g. `-t 3600`)
- **Single humantime token** → parsed duration (e.g. `-t 10m`, `-t 1.5h`)
- **Quoted multi-word string** → parsed duration (e.g. `-t "1 hour and 30 minutes"`)

The value must be a positive duration — `-t 0` (or anything that doesn't parse) is rejected in every mode, including alongside `-w` or a trailing command.

Supported units include seconds, minutes, hours, days, weeks, months, and years, plus short forms like `s`, `m`, `h`, and `d`. Anything after the timeout value is treated as a trailing command, not part of the duration — there is no multi-word duration inference, so quote the duration instead:

```bash
# Correct: 90 minutes
caffeinate2 -t "1 hour and 30 minutes"

# Wrong: rejected — the trailing words look like a misquoted duration
caffeinate2 -t 1 hour and 30 minutes
```

For PIDs, caffeinate2 waits until the specified program exits, then exits with the same exit code. If the program doesn't exist, it exits immediately with an error.

`caffeinate2 -t 600`

`caffeinate2 -t 10m`

`caffeinate2 -w 1234`

`caffeinate2 -t 600 -w 1234`

### None of the above

Sleep will be disabled indefinitely until you press `Ctrl+C`.

`caffeinate2`

## Menu bar

Run `caffeinate2-tray` after installing with `--features full`. To run it in the background instead of occupying the terminal, use `-d` / `--detach`:

`caffeinate2-tray -d`

This returns control to the shell immediately and keeps the menu bar icon running after the terminal closes. Its stderr is captured, best-effort, to `~/Library/Logs/caffeinate2-tray.log` (appended, never rotated), and is discarded only if that file cannot be opened; run it in the foreground to watch the logs live. Stop it with the tray's **Quit** item or `pkill -x caffeinate2-tray`.

- **Left click:** toggle the selected sleep mode on/off — or, while caffeinate2 is upgrading another app's sleep assertion, open the upgrade dialog (see below).
- **Right-click:** open the menu:
  - **Mode** — Display, Disk, System, System (on AC), User active, or Entirely.
  - **Time limit** — Off, 15 minutes, 30 minutes, 1 hour, and so on.
  - **Wait for apps…** — pick one or more running apps, or use the **Choose application** picker (button **Choose**) for any `.app`.
  - **Upgrade other apps' sleep prevention** — see below.
  - **Ignored apps** — the programs that are never upgraded; click one to stop ignoring it (shown only when the list is non-empty).
  - **Start at login** — toggle the LaunchAgent.
  - **Quit**.

When a **time limit** is set, left-clicking to start sleep prevention turns it off again after that duration (like `caffeinate2 -t`). **Wait for apps** keeps prevention on until every instance of *all* the selected apps has exited (it stops once at least one selection has been seen running and then none remain); any selected app that isn't running yet is waited on to launch. With both set, whichever comes first wins. While a time limit is counting down, the minutes remaining (rounded up, e.g. `29m` or `1h 29m`) are shown next to the menu bar icon; the tooltip shows remaining time and/or app status while active. Time limits count awake time: if the Mac sleeps mid-session (for example the lid is closed, which no mode prevents), the countdown pauses and resumes on wake.

**Upgrade other apps' sleep prevention** keeps the Mac awake on behalf of tools that can't. Tools like Claude Code, Codex, and `caffeinate -i` use a low-level assertion that *still allows sleep when the lid closes* — so a long-running agent dies the moment you shut the lid. With this on, caffeinate2 watches for those assertions and temporarily upgrades to **Entirely** mode while one is active.

- Requires the privileged helper. Enabling the toggle installs it if necessary (prompting once).
- Only *your* programs are upgraded. macOS's own processes hold sleep assertions during ordinary use all the time (the power daemon, `runningboardd`, audio and media daemons, Apple's apps); they release them on their own, so caffeinate2 ignores every one of them and never mentions them in the menu.
- The menu shows which app triggered the upgrade, and lists assertions it deliberately ignores.
- While an upgrade is active, left-clicking the icon opens the upgrade dialog (below) rather than simply switching sleep prevention off — "off" would be ambiguous when the hold is being taken on another program's behalf. It still toggles off directly in the one case the dialog has nothing to ask about: an upgrade whose holder exposes no process name.
- A manual left-click-off — when the watcher is idle, or from that nameless-holder case — overrides the watcher until the next new assertion.

**The upgrade dialog.** Left-clicking the icon while an upgrade is active asks what to do with it instead of just switching it off (with several apps upgraded at once, a pop-up in the dialog picks which one the ignore choices apply to):

- **Hold For _30m_** (or **Hold Until I Stop It** with no time limit set) — take over with caffeinate2's own session, using the configured mode and time limit. Useful when you don't know how long the other app will keep asserting but you *do* know how long you need the Mac awake. The watcher stays out of the way until that app's assertion goes away.
- **Ignore This Time** — stop upgrading the assertion it is holding right now. The ignore expires once that app has stopped asserting for about 20 seconds, so the next thing it starts is upgraded again.
- **Never Upgrade This App** — add it to the ignore list under **Ignored apps**, where clicking it removes it again.
- **Cancel** — leave the upgrade running.

Settings are stored in `~/Library/Application Support/caffeinate2/tray.toml`.

**Start at login** writes `~/Library/LaunchAgents/com.randomblock1.caffeinate2-tray.plist` and takes effect at the next login (the tray does not start a second instance immediately). Use `-d` for a one-off background launch right now; use the LaunchAgent to start the tray at every login. The menu toggle and these CLI flags use the same LaunchAgent:

`caffeinate2-tray --install-launch-agent`

`caffeinate2-tray --uninstall-launch-agent`

`caffeinate2-tray --launch-agent-status`

Unsigned binaries may require running from Terminal once (right-click → Open) or allowing in Privacy & Security.

### Details

- **Time limit countdown:** changing the time limit while sleep prevention is active restarts the countdown from that moment — it is not measured from when the session started.
- **Wait for apps matching:** the target is remembered by bundle ID (for example `Codex.app` stays matched across restarts).
- **Upgrade release delay:** caffeinate2 releases its Entirely hold about 20 seconds after the external assertion goes away.
- **Upgrade menu display:** a single named app appears as one line; several collapse into an expandable **Upgrading N apps** submenu (the tooltip lists them too). When an app's name is unavailable, a generic **Upgrading external app** line is shown instead.
- **Ignored assertions:** caffeinate2 only reacts to idle-system-sleep assertions (what agents use). Others are listed under **Ignoring _name_ (_reason_)** lines — collapsing into an **Ignoring N assertions** submenu when there are several — so a quiet menu is never mistaken for "nothing is keeping the Mac awake." The reason is _display only_ for the display-only assertions video players hold, _you ignore this app_ for the persistent ignore list, and _ignored this time_ for a one-off ignore. caffeinate2 never reacts to its own holds.
- **What counts as a system program:** anything running as another user (root daemons and service accounts like `coreaudiod`), any Apple `.app`, anything launched from `/System`, `/usr/libexec`, `/usr/sbin`, `/sbin` or `/Library/Apple` — macOS's daemons and per-user agents — and any process whose executable can't be read. These are silently dropped: never upgraded, never listed. Everything else you run is upgradeable, including Homebrew binaries and Apple's own command-line tools: `caffeinate -i` from `/usr/bin` is you asking to stay awake, not the OS doing housekeeping.
- **Ignored apps list:** stored as `ignored_apps` in `tray.toml` (a list of process names as IOKit reports them, matched case-insensitively) and editable by hand as well as from the menu. Ignoring an app takes effect immediately: if nothing else is worth upgrading, the hold is released right away rather than after the usual 20-second delay.
- **Any matching assertion counts:** a desktop app that holds an idle-system-sleep assertion while open (some Electron apps do) will keep the upgrade active until it quits.

## Entirely mode

Entirely mode (`-e` / tray **Entirely**) disables system sleep even when the lid is closed. It uses a small privileged helper daemon, so once installed it works without repeated password prompts.

**One-time setup** (either method):

1. Tray: select **Entirely** and approve the administrator dialog when prompted, or
2. CLI: `sudo caffeinate2 --install-helper`

Helper install requires `caffeinate2-helper` next to `caffeinate2`: use `cargo install caffeinate2 --features full`, build with `--features full`, or extract the GitHub release bundle (all three binaries together).

After that, `caffeinate2 -e` and tray Entirely use the helper without further passwords.

**Who can use it:** root and administrator accounts, plus members of the `caffeinate2` group (created by `--install-helper`). The helper denies everyone else, since disabling sleep entirely is otherwise a root-only setting. To allow a standard account:

`sudo dseditgroup -o edit -a USERNAME -t user caffeinate2`

The grant takes effect on the user's next attempt (no logout needed). Denied requests show the exact grant command; `caffeinate2 --status` works for every account.

Other helper commands:

- Remove the helper: `sudo caffeinate2 --uninstall-helper` (refuses while entirely-mode sessions are still active)
- Check helper state (running, how many holds, sleep disabled, version): `caffeinate2 --status`

**After upgrading caffeinate2**, reinstall the helper: the installed daemon is a copied snapshot, so it keeps running the old version until you run `sudo caffeinate2 --install-helper` again. `caffeinate2 --status` shows when the installed helper's version differs from the binary (the tray also logs a warning at startup).

Helper install also adds `/etc/newsyslog.d/com.randomblock1.caffeinate2.helper.conf` so `/var/log/caffeinate2-helper.log` is rotated.

## License

This project is licensed under the [MIT License](LICENSE.txt).
