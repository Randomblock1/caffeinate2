# caffeinate2

![logo](https://randomblock1.com/assets/images/caffeinate2.svg)

`caffeinate` but it's written in Rust and has more options. Keeps your Mac wide awake.

## UNDER CONSTRUCTION

There are probably some bugs hiding somewhere, but regardless, it works.

## Installation

### GitHub Releases

Download the latest release from [here](https://github.com/randomblock1/caffeinate2/releases/latest).

### Homebrew

_This is not yet available._

### Cargo

_This is not yet available._

## Usage

```plaintext
Usage: caffeinate2 [OPTIONS] [COMMAND]...

Arguments:
  [COMMAND]...  Wait for given command to complete (takes priority above timeout and pid)

Options:
  -v, --verbose             Verbose mode
  -d, --display             Disable display sleep
  -m, --disk                Disable disk idle sleep
  -i, --system              Disable idle system sleep. Default if no other options are specified
  -s, --system-on-ac        Disable system sleep while not on battery
  -e, --entirely            Disable system sleep entirely (ignores lid closing)
  -u, --user-active         Declare the user is active. If the display is off, this option turns it on and prevents it from going into idle sleep
  -t, --timeout <DURATION>  Wait for X seconds. Also supports time units (e.g. 1s, 1m, 1h, 1d)
  -w, --waitfor <PID>       Wait for program with PID X to complete
  -h, --help                Print help
  -V, --version             Print version
```

## Sleep Timers (in order of priority)

### Command

Sleep disabled until the command completes. Timeout and PID will be ignored if a command is specified.

### Timeout and PID

Sleep disabled for a certain amount of time or until program with the specified PID completes. If both timeout and PID are specified, whichever was specified first will be used.

Timeout can either be a number of seconds or a duration string. For example you can pass `-t 600` or `-t 10m` to wait for 10 minutes. You can create more descriptive durations, like `-t "1 hour and 30 minutes"`. Anything that's not a number followed by a unit will be ignored (the "and" in the previous example). **YOU MUST USE QUOTATION MARKS FOR THIS TO WORK.** Otherwise it will try to parse anything that's past the space as a command, and ignore the timeout.

### None of the above

Sleep will be disabled until you press `Ctrl+C`.

## License

This project is licensed under the MIT License - see [the license file](LICENSE.txt) for details.
