# caffeinate2

Caffeinate but it's written in Rust and it actually works. Keeps your Mac wide awake. Even when its lid is closed.

## Installation

### GitHub Releases

Download the latest release from [here](https://github.com/randomblock1/caffeinate2/releases/latest).

### Homebrew

_This is not yet available._

### Cargo

`cargo install caffeinate2`

## Usage

### No arguments

Sleep will be disabled until you press `Ctrl+C`.

### `-t` followed by a number

Sleep will be prevented for the specified number of seconds.

### Anything else

Your computer will attempt to execute the input as a command. Depending on your shell, it might be necessary to wrap the command in quotes.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE.txt) file for details

## TODO

Use MacOS assertions to prevent various types of sleep instead of completely disabling it.
