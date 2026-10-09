# RMake

**Build, run, and test C/C++ projects without listing every source file.** RMake discovers your code, compiles changed inputs, and links local libraries automatically.

## Install

Requirements: Rust **1.99.0**, plus GCC or Clang on Linux, or Apple Clang on macOS. Defaults: **C17** and **C++20**. Static libraries use `ar`.

From this repository:

```sh
cargo install --path crates/rmake-cli --locked
```

## Start a project

```sh
rmake new hello
cd hello
rmake run
```

Choose C with `--lang c`. Use `--name hello` when your folder name contains spaces. Initialize existing conventional code with `rmake init`; existing sources and `.gitignore` are preserved, and an existing manifest is never overwritten.

Your manifest can stay this small:

```toml
schema = 1

[package]
name = "hello"
version = "0.1.0"
```

Put `main.cpp` or `main.c` under `src/`. Add more files anywhere beneath that root; no manifest edit needed. Supported extensions: `.c`, `.cpp`, `.cc`, `.cxx`, `.C`. Headers are included normally; `include/` is added to conventional targets' include paths.

## Build and run

```sh
rmake build
rmake run -- --input "scene file.json"
rmake build --release
rmake build --jobs 2
rmake build --rebuild
```

Unchanged supported builds execute zero compile, archive, or link actions. RMake checks contents of sources, consumed headers, outputs, and compiler dependencies. `--rebuild` forces native actions. Development and release outputs are separate under `build/rmake/artifacts/`.

`run` builds first and returns your program's exit status. Failed builds never launch an older executable. Ctrl-C stops managed builds and tests with status 130. Commands work from project subdirectories too.

## Local libraries, tests, and examples

```sh
rmake new math --lib
cd math
rmake build
```

`src/lib.cpp` or `src/lib.c` identifies a static library. Its `include/` directory is exported to consumers. Use this layout to add programs around a library:

```text
math/
├── Rmake.toml
├── include/math.h
├── src/
│   ├── lib.cpp
│   └── bin/calculator.cpp
├── tests/arithmetic.cpp
└── examples/demo/main.cpp
```

| Location | Automatic target |
|---|---|
| `src/main.cpp` or `src/main.c` | Package binary |
| `src/lib.cpp` or `src/lib.c` | Package static library |
| `src/bin/tool.cpp` or `src/bin/tool/main.cpp` | Binary `tool` |
| `tests/check.cpp` or `tests/check/main.cpp` | Test `check` |
| `examples/demo.cpp` or `examples/demo/main.cpp` | Example `demo` |

The auxiliary conventions also accept `.c`. A marked directory owns its sources recursively; a direct file owns only itself. Auxiliary binaries, tests, and examples automatically consume the package's sole library.

```sh
rmake run --bin calculator
rmake run --example demo
rmake test
rmake test -- --your-test-argument
```

Tests are ordinary C/C++ programs: exit 0 passes; any other status fails. They run sequentially from the package root, receive arguments after `--`, and continue after ordinary test failures. Any failure makes `rmake test` return 1; no tests returns 0.

`build` builds production targets. Tests and examples are built when selected. Multiple binaries require `run --bin NAME`, unless `[package] default-target = "NAME"` selects one.

## Declare another layout

Use named targets when you need several libraries or different roots:

```toml
[[targets]]
name = "math"
kind = "static"
root = "math/src"
public-includes = ["math/include"]

[[targets]]
name = "calculator"
kind = "bin"
root = "app"

[targets.dependencies]
"target:math" = "private"
```

```sh
rmake build --target calculator
rmake run --bin calculator
rmake inspect files
```

Target kinds: `bin`, `static`, `header-only`, `test`, `example`. Compiled targets require a directory or source-file `root`. Header-only targets require `public-includes` and need no dummy source. Nested roots belong to the more specific target.

Dependencies reference another library in the same package using `target:NAME`:

| Visibility | Used to compile this target | Exported to its consumers |
|---|---|---|
| `private` | Yes | No |
| `public` | Yes | Yes |
| `interface` | No | Yes |

Only libraries may declare public/interface dependencies. Private static dependencies still reach the final executable link. RMake chooses the C++ linker driver when any linked library contains C++ code.

Library consumer requirements can include definitions and minimum standards:

```toml
[targets.interface]
cpp-standard = "c++20"
defines = { MATH_API_LEVEL = "1" }
```

`private-includes` and `defines` apply locally. Private `compile-options` and `link-options` accept individual compiler arguments; output and dependency-file controls are reserved. Raw options conservatively disable reuse for the affected actions.

Explicit production declarations replace automatic production inference. Tests/examples remain automatic unless disabled:

```toml
[discovery]
exclude = ["src/experimental/**"]
tests = false
examples = false
```

Exclusions are package-relative. Hidden/reserved directories and nested packages are excluded from source scans; directory symlinks are skipped and source symlinks rejected. Roots must be unambiguous; target names must be unique, including differences in letter case. With several libraries, declare auxiliary targets and their dependencies explicitly.

## Configure tools

```sh
rmake build --cc gcc --cxx g++ --ar ar
rmake build --cc clang --cxx clang++ --verbose
rmake doctor
```

Selection order: CLI, `.rmake/config.toml`, `CC`/`CXX`/`AR`, platform defaults. Settings identify executable paths, including paths with spaces, rather than shell command strings.

```toml
# .rmake/config.toml
schema = 1

[toolchain]
c-compiler = "clang"
cpp-compiler = "clang++"
archiver = "ar"
```

Choose language defaults in `Rmake.toml`:

```toml
[build]
c-standard = "c17"
cpp-standard = "c++23"
```

## Useful commands

| Command | Purpose |
|---|---|
| `rmake build --verbose` | Show native command arguments |
| `rmake inspect files` | Show source ownership and ignored paths |
| `rmake doctor` | Show selected native tools |
| `rmake clean` | Remove verified RMake outputs; preserve your code |
| `rmake --help` | List commands and options |

## Current limits

M1 supports native, single-package applications and static/header-only libraries on Linux and macOS. External libraries—including manually downloaded folders—workspaces, shared libraries, generators, Windows, and cross-compilation are deferred.

Builds track consumed system headers and SDK identity; they do not scan every SDK tree or provide hermetic builds. Project include-directory changes can conservatively rebuild more than one source. Tests have no built-in timeout or test framework.
