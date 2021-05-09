# Dbgee - the Zero-Configuration Debuggee for Debuggers

With Dbgee, you can start a debug session without writing any `launch.json` by 
just launching your program in the integrated terminal.
Also, Dbgee's ability to pre-set a debug session to start allows you to
start debugging no matter how your program is launched.

Dbgee frees you from the hassle of writing `launch.json`.
It's also very useful especially when your program requires command line arguments or redirection,
or when your program is launched by some script.

## Features

### Launch your program in the integrated terminal, and start a debug session with zero configuration

<img alt="demo image" src="vscode-ext/images/DbgeeRunInVsCode.gif" width="850px">

### Configure your program to wait for a debug session, no matter by what means it is started

<img alt="demo image" src="vscode-ext/images/DbgeeSetInVsCode.gif" width="850px">

### Launch CUI debuggers in tmux

<img alt="demo image" src="vscode-ext/images/DbgeeRunSetInTmux.gif" width="850px">

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
