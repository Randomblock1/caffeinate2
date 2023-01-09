# caffeinate2

Caffeinate but it's written in Rust and it actually works. Keeps your Mac wide awake. Even when its lid is closed.

## UNDER CONSTRUCTION: NOT COMPLETELY COMPATIBLE WITH CAFFEINATE

Right now, you can only COMPLETELY prevent system sleep for a certain amount of time, indefinitely, or while a command is running.
It completely disables sleep, meaning that closing the laptop doesn't do anything.
I still have to add support for preventing other types of sleep, like display, disk, or idle sleep.
See `man caffeinate` for the flags I need to be compatible with.

## Installation

### GitHub Releases

Download the latest release from [here](https://github.com/randomblock1/caffeinate2/releases/latest).

### Homebrew

_This is not yet available._

### Cargo

_This is not yet available._

## Usage

### No arguments

Sleep will be disabled until you press `Ctrl+C`.

### `-t` followed by a number

Sleep will be prevented for the specified number of seconds.

### Anything else

Your computer will attempt to execute the input as a command. It is necessary to wrap the command in single quotes if you're going to use shell commands, like `&&`. This is just how the shell works.

## License

This project is licensed under the MIT License - see [the license file](LICENSE.txt) for details.

## TODO

Use MacOS assertions to prevent various types of sleep instead of completely disabling it.
