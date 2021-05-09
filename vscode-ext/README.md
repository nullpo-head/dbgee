# Dbgee - the Zero-Configuration Debuggee for Debuggers

Dbgee is a handy utility that allows the debugger to be actively started from the debuggee side.
Just start your program by a simple command in the integrated terminal, and you can start a debug session with zero configuration.
You don't have to bother with setting arguments, redirects, etc. in `launch.json` in order to start the debugger.

## Features

### Launch your program in the integrated terminal, and start a debug session with zero configuration

![Launch your program in the integrated terminal, and start a debug session with zero configuration](vscode-ext/images/DbgeeRunInVsCode.gif)

### Configure your program to wait for a debug session, no matter by what means it is started

![Configure your program to wait for a debug session, no matter by what means it is started](vscode-ext/images/DbgeeSetInVsCode.gif)

### Launch CUI debuggers in tmux

![Launch CUI debuggers in tmux](vscode-ext/images/DbgeeRunSetInTmux.gif)

## Requirements

**`dbgee` command**

This extension is a companion VSCode extension of `dbgee` command.
Get `dbgee` command first at [the GitHub repository](https://github.com/nullpo-head/dbgee).
You can also check the usage of `dbgee` command there.

**Debugger extensions for languages**

You need actual debugger extensions for each language to start debug sessions.

- [CodeLLDB](https://marketplace.visualstudio.com/items?itemName=vadimcn.vscode-lldb)

  To debug LLVM-based languages such as Rust

- [Go](https://marketplace.visualstudio.com/items?itemName=golang.go)
- [Python](https://marketplace.visualstudio.com/items?itemName=ms-python.python)
- [C/C++](https://marketplace.visualstudio.com/items?itemName=ms-vscode.cpptools)

## Supported platforms

### Platforms

Currently only Linux (including WSL2 on Windows) is supported. However, adding macOS support is pretty easy and will be added soon if there are any macOS users. Please say hi to me in a GitHub issue.

### Languages

The current supported languages are C, C++, Rust, Go, Python and any languages which Gdb, LLDB, or CodeLLDB support.

## Extension Settings

Dbgee VSCode extension has no setting for now.
