# caffeinate2

Caffeinate but it's written in Rust and it actually works. Keeps your Mac wide awake. Even when its lid is closed.

## CURRENTLY DOESN'T WORK BECAUSE IDK HOW TO MAKE MACOS RELOAD

So it writes to the preferences file just fine... but since MacOS's PowerManagement daemon doesn't reload the preferences file, it doesn't actually do anything. I'm not sure how to make it reload the preferences file, so if you know how, please let me know. So for now it just calls pmset to disable sleep, which is not ideal. Eventually I'll figure out how to make it work properly. (Probably by linking to IOKit.)

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

Your computer will attempt to execute the input as a command. It is probably necessary to wrap the command in single quotes.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE.txt) file for details

## TODO

Use MacOS assertions to prevent various types of sleep instead of completely disabling it.
