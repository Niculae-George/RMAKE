# RMake

**Build and run C/C++ projects with automatic source discovery.** Add source files to your project; RMake compiles and links them without a per-file build list.

## Install

Requirements:

- Rust 1.99.0 to build and install RMake.
- Linux with GCC or Clang, or macOS with Apple Clang.
- A compiler supporting your chosen language standard. Defaults are C17 and C++20.

From this repository's root:

```sh
cargo install --path crates/rmake-cli --locked
```

## Create a project

```sh
rmake new hello
cd hello
rmake run
```

For a C project:

```sh
rmake new hello-c --lang c
```

Use `--name` to choose a package name separately from its folder:

```sh
rmake new "My Project" --name my-project
```

## Add your code

```text
hello/
├── Rmake.toml
├── src/
│   ├── main.cpp
│   ├── parser.cpp
│   └── internal/
│       └── cache.cpp
└── include/
    └── parser.hpp
```

`src/main.cpp` or `src/main.c` identifies the executable. Other supported files under `src/` are discovered recursively. An optional `include/` directory is added to the compiler's header search paths.

Supported source extensions: `.c`, `.cpp`, `.cc`, `.cxx`, and `.C`. Mixed C/C++ projects are supported. Headers are included normally from your source code.

Hidden directories, build outputs, vendored folders, `src/bin/`, and package-level `tests/` and `examples/` stay outside the default application scan. Directory symlinks are skipped; source-file symlinks are rejected.

The generated manifest is small:

```toml
schema = 1

[package]
name = "hello"
version = "0.1.0"
```

Adding, removing, or renaming a source file requires no manifest edit.

## Build and run

```sh
rmake build
rmake run
rmake build --release
rmake run --release
```

Development builds include debug information. Release builds enable optimization. The build command prints the executable's location; development and release outputs are kept separately.

Pass arguments to your program after `--`:

```sh
rmake run -- --input scene.json
```

RMake returns your program's exit status. A failed compilation or link stops the command; `run` never launches an older executable after a failed build.

Run commands from the project root or a subdirectory. RMake finds the nearest parent `Rmake.toml`.

## Use existing code

For a project already containing `src/main.cpp` or `src/main.c`:

```sh
rmake init
rmake run
```

`init` preserves existing source files and `.gitignore`, and refuses to overwrite `Rmake.toml`.

For another layout, write the minimal manifest above and add a binary target. This example uses sources in `app/` and headers in `headers/`:

```toml
[[targets]]
name = "hello"
kind = "bin"
root = "app"
private-includes = ["headers"]
```

Files under the selected root are still discovered automatically.

## Configure a project

Add these tables to `Rmake.toml` when needed.

Choose language standards:

```toml
[build]
c-standard = "c17"
cpp-standard = "c++23"
```

Exclude optional code using paths relative to the project:

```toml
[discovery]
exclude = ["src/experimental/**"]
```

Choose a compiler for a build:

```sh
rmake build --cc gcc --cxx g++
rmake build --cxx clang++
```

Compiler selection uses command-line options first, then `.rmake/config.toml`, then `CC`/`CXX`, then platform defaults. To keep a local compiler preference, create `.rmake/config.toml`:

```toml
schema = 1

[toolchain]
c-compiler = "clang"
cpp-compiler = "clang++"
```

Compiler settings identify one executable, rather than a shell command containing extra flags. A C-only project needs only a C compiler.

## Useful commands

| Command | Purpose |
|---|---|
| `rmake build --verbose` | Show compiler and linker arguments |
| `rmake inspect files` | Show discovered sources and ignored paths |
| `rmake doctor` | Show selected compiler information |
| `rmake clean` | Remove RMake build outputs while preserving your code |
| `rmake --help` | Show available commands |

## Current limits

RMake currently builds one native executable per project and performs a full rebuild each time. Library targets, third-party dependency management, workspaces, incremental/parallel builds, generated sources, cross-compilation, and Windows are not supported yet.
