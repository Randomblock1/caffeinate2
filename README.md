# caffeinate2

![logo](https://randomblock1.com/assets/images/caffeinate2.svg)

`Caffeinate` but it's written in Rust and has more options. Keeps your Mac wide awake.

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
  -d, --display            Disable display sleep
  -m, --disk               Disable disk idle sleep
  -i, --system             Disable idle system sleep. Default if no other options are specified
  -s, --system-on-ac       Disable system sleep while not on battery
  -e, --entirely           Disable system sleep entirely (ignores lid closing)
  -u, --user-active        Declare the user is active. If the display is off, this option turns the display on and prevents the display from going into idle sleep. If a timeout is not specified with '-t' option, then this assertion is taken with a default of 5 second timeout
  -t, --timeout <SECONDS>  Wait for X seconds
  -w, --waitfor <PID>      Wait for program with PID X to complete
  -h, --help               Print help information
  -V, --version            Print version information
```

### No arguments

Sleep will be disabled until you press `Ctrl+C`.

## License

This project is licensed under the MIT License - see [the license file](LICENSE.txt) for details.

## TODO

Use MacOS assertions to prevent various types of sleep instead of completely disabling it.
