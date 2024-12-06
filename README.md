# caffeinate2

![logo](https://randomblock1.com/assets/images/caffeinate2.svg)

`caffeinate` but it's written in Rust and has more options. Keeps your Mac wide awake.

## Current Status

Functionally complete. Only a few things left on the TODO before it's time for 1.0.0.

## Installation

### GitHub Releases

Download the latest release binary from [here](https://github.com/randomblock1/caffeinate2/releases/latest).

### Homebrew

_This won't be available until version 1.0.0._

### Cargo

`cargo install caffeinate2`

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
```

## Sleep Timers (in order of priority)

### Command

Sleep disabled until the command completes. You should enclose the command in quotes, although it isn't strictly
required. Timeout and PID will be ignored if a command is specified.

`caffeinate2 "sleep 5"`

### Timeout and PID

Sleep is disabled for a certain amount of time or until the program with the specified PID completes. If both are
specified, it waits until one of them completes.

Timeout can either be a number of seconds or a duration string. For example, you can pass `-t 600` or `-t 10m` to wait
for 10 minutes. You can create more descriptive durations, like `-t "1 hour and 30 minutes"`, but it only looks at the
first letter (so "3 movies" is just 3 minutes). Anything that's not a number followed by a letter will be ignored (the "
and" in the previous example). **YOU MUST USE QUOTATION MARKS FOR THIS TO WORK.** Otherwise, it will try to parse
anything that's past the space as a command, and ignore the timeout.

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

## TODO

- [x] Make timeout and PID work together
- [x] Figure out how to fix command output (for example, `caffeinate2 brew list` is uncolored)
- [ ] Document & experiemtn on all the sleep types (they are somewhat vague)
- [x] Get system sleep status without reading a plist
- [x] Get PID info & wait by using syscalls instead of a weird `lsof` hack
