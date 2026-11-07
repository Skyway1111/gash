<img src="assets/gash-icon.svg" width="112" height="112" alt="gash icon">

# gash

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/speed-dark.svg">
  <img src="assets/speed-light.svg" width="100%" alt="gash vs Git Bash in real time: each ball crosses its track once per run. gash's balls fly back and forth while Git Bash's barely move.">
</picture>

A fast, native, from-scratch **bash-compatible shell for Windows**, written in Rust with
zero dependencies. It is a drop-in replacement for Git Bash's `bash.exe`: same syntax,
same `/c/Users/...` paths, same MSYS tools (`ls`, `grep`, `sed`, `awk`, ... from Git for
Windows), but without Cygwin's `fork()` emulation, which is what makes Git Bash slow.

## Why it's fast

Git Bash runs on the MSYS2/Cygwin runtime, which emulates `fork()` by copying the whole
process. On Windows (especially with Defender scanning every new process) each fork
costs ~100–250 ms. Every `$(...)`, `( ... )`, pipeline stage and external command pays it.

gash never forks:

| Construct | Git Bash | gash |
|---|---|---|
| `$(...)`, `( ... )`, `<( ... )` | fork + copy | in-process clone of shell state (copy-on-write variables) |
| pipeline of builtins/functions | fork per stage | one thread per stage, OS pipes between them |
| `./script.sh`, `#!/bin/bash` scripts | fork + exec bash | runs in a fresh in-process child shell |
| external program | fork + exec | single `CreateProcess` |
| startup | `/etc/profile` runs `cygpath`, `locale`, `uname`… | environment set up natively (~9 ms) |
| git prompt | `__git_ps1` spawns `git` several times per prompt | native `__git_ps1` reads `.git/HEAD` |

Measured on the development machine (Git Bash 5.3, Windows 11, Defender on):

| Benchmark | Git Bash | gash | Speedup |
|---|---:|---:|---:|
| startup `-c true` | 251 ms | 9 ms | 28× |
| 100× `$(echo)` | 14,704 ms | 37 ms | ~400× |
| 10× `( )` subshell | 1,787 ms | 14 ms | ~130× |
| 10× `echo hi \| cat` | 3,255 ms | 325 ms | 10× |
| 20× external `/usr/bin/true` | 3,362 ms | 495 ms | 7× |
| realistic script (read loop over 5k lines, recursive `fib`, assoc arrays) | 290,087 ms | 260 ms | ~1100× |

## Compatibility

The `tests/scripts` corpus (61 scripts, ~2,000 labelled cases, expected output generated
by real Git Bash) passes 100%. Supported:

- Full grammar: pipelines, `&&`/`||`, `&`, subshells, groups, `if`/`case` (incl. `;&` `;;&`),
  `while`/`until`/`for`/C-style `for`/`select`, functions, `[[ ]]`, `(( ))`, `time`, `!`,
  here-docs (`<<`, `<<-`, quoted delimiters), here-strings, process substitution, aliases.
- All parameter expansions: defaults/alternates, `#`/`%` trimming, `/` `//` `/#` `/%`
  substitution (with `patsub_replacement` `&`), substrings, case modification, `${!ref}`,
  `${!prefix*}`, `${!arr[@]}`, `@Q @E @P @A @a @U @u @L @K`, namerefs.
- Indexed and associative arrays, `declare`/`local`/`readonly`/`export` with all attributes,
  integer/upper/lower variables.
- Arithmetic with all bash operators, bases (`0x`, `0`, `N#`), assignment operators.
- Globbing with `nullglob`, `failglob`, `dotglob`, `nocaseglob`, `globstar`, `extglob`,
  `GLOBIGNORE`; brace expansion including sequences.
- `set -euxo pipefail`, `set -C`, `shopt` (all bash option names), traps (`EXIT`, `ERR`,
  `RETURN`, `INT`, signals to self), `getopts`, `read` (all flags), `mapfile`, `printf`
  (incl. `%q %b %(fmt)T`, `-v`), `echo`, `test`, `type`, `command`, `hash`, `eval`,
  `source`, `exec` (incl. fd redirection), `pushd`/`popd`/`dirs`, `jobs`/`wait`/`kill`/`fg`,
  `alias`, `history`, `compgen`, ...
- Bash's error-recovery rules (arithmetic and substitution errors discard the current
  command line; `${x:?}` and `set -u` exit the script).

### Windows/MSYS integration

- Paths: `/c/foo` ↔ `C:\foo`, `/tmp` → `%TMP%` (see below), `/usr/bin`, `/bin`, `/ucrt64` →
  Git's install dir, `/dev/null`, `/dev/stdin|stdout|stderr|fd/N`, `/dev/tty`. Windows paths
  (`C:\x`, `C:/x`) work everywhere too. `$PWD` and `pwd` show the MSYS form, `pwd -W` the
  Windows one (`C:/...`).
- `/tmp` is the folder the MSYS runtime mounts there, which Windows' GetTempPathW gives at
  startup: `TMP`, else `TEMP`, else your profile folder (`%USERPROFILE%`), else the Windows
  folder. A value in MSYS form such as `TMP=/tmp` therefore means `\tmp` on the current drive,
  as it does for Git Bash. gash creates that folder if it doesn't exist, and only if it can't
  uses `%LOCALAPPDATA%\Temp` instead. Programs gash starts get `TMP` and `TEMP` set to the same
  folder. MSYS programs still see another `/tmp` if an MSYS process from the same Git install
  was already running with different `TMP`/`TEMP` values: they share one mount table, and the
  first process to start sets `/tmp` for all of them.
- PATH is kept in MSYS form inside the shell and converted when launching programs.
- Arguments to native programs get MSYS2-style path conversion (`/c/x` → `C:/x`),
  honoring `MSYS_NO_PATHCONV` and `MSYS2_ARG_CONV_EXCL` when they are exported or given as
  a command prefix (`MSYS_NO_PATHCONV=1 cmd ...`), as in Git Bash.
- Arguments to MSYS programs are quoted so the MSYS runtime doesn't glob/brace/tilde-expand
  or `@file`-expand them. Its globbing would cut an argument past 8 KB short, so such a
  command goes through Git's `env` started with `MSYS=noglob`, which passes it on intact.
- Executables are found with or without `.exe`/`.com`/`.bat`/`.cmd`; extensionless files with a
  `#!` line are run via their interpreter (`#!/usr/bin/env node`, `#!/usr/bin/python3`, ...).
- `$(...)` strips trailing `\r\n` like MSYS2 bash, so output of Windows programs is clean.
  Scripts with CRLF line endings run (Git Bash would fail with `$'\r': command not found`).
- Interactive mode: Git Bash's default prompt (with a native `__git_ps1`), the `ls`/`ll`
  aliases from Git for Windows, emacs-style line editing, history (`~/.bash_history`,
  `HISTCONTROL`, `!!`, `!$`, `^old^new`, Ctrl+R), Tab completion of commands, files,
  `$variables`, `~` and `/c/...` paths, job notifications.

## Microsoft Coreutils (optional)

If [Microsoft's Coreutils for Windows](https://github.com/microsoft/coreutils) (native uutils
builds of coreutils, findutils and grep) is available, gash uses it in place of Git's MSYS
versions of the same tools. Native tools start faster and avoid the MSYS runtime; on a script
that calls `grep`/`head`/`tail`/`wc`/`cut`/`uniq` in a loop this measured about 15–25% faster
end to end.

gash looks for `coreutils.exe` in this order:

1. `GASH_COREUTILS`: a path to `coreutils.exe` or its folder (`GASH_COREUTILS=0` turns this off)
2. `coreutils\coreutils.exe` next to `gash.exe` (a portable copy: unzip a release there)
3. `%ProgramFiles%\coreutils\coreutils.exe` (installed with `winget install Microsoft.Coreutils`)

How it behaves:

- Only commands that PATH lookup would resolve to Git's `/usr/bin` are swapped, so anything you
  put earlier on PATH, or call by full path (`/usr/bin/cat`), still wins.
- gash creates per-command hard links next to `coreutils.exe` (`bin\cat.exe`, ...) on first
  use, so `command -v cat` prints a path that really runs `cat`.
- MSYS paths in arguments are translated for the native tools even when `MSYS_NO_PATHCONV` is
  set (the caller asked for an MSYS tool), with a per-command policy: file operands are
  converted, while `tr` sets, `basename`/`dirname`/`expr` strings and `grep` patterns are
  never touched (`echo "$PATH" | grep /usr/bin` still works).
- Arguments with wildcards are quoted so the native tools don't glob them a second time.
- Kept on Git's GNU versions: `sort` and `find` (they can switch to the DOS commands' behaviour),
  `mktemp`, `realpath`, `readlink` (they print Windows paths), `ln` (native symlinks need
  Developer Mode), `env` and `xargs` (they can't start shell scripts). gash builtins such as
  `echo`, `printf`, `test` and `pwd` are unaffected.

Limitations: native tools can't create or open file names containing `* ? : " < > |`, which the
MSYS runtime supports by remapping them (native `touch` also stops at the first such error), and
tools that echo absolute paths they were given print them as `C:/...` rather than `/c/...`.

## Installing

```bash
winget install hex2f.gash
```

This also installs Git for Windows if winget doesn't find it. Or download
`gash-<version>-x64.zip` (or `-arm64.zip`) from
[Releases](https://github.com/hex2f/gash/releases) and unzip it anywhere. `gash.exe` is a single
file with no runtime dependencies besides Git for Windows' tools.

### macOS and Linux

You already have it installed. It's called `bash`.

## Building

```bash
cargo build --release
```

The binary is `target\release\gash.exe` (no runtime dependencies besides Git for Windows'
tools, which it finds automatically; override with `GASH_ROOT=C:\path\to\Git`). The C runtime
is linked statically (`.cargo/config.toml`), so it doesn't need the Visual C++ Redistributable.
Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed; see
[packaging/winget/README.md](packaging/winget/README.md).

## Using it

```bash
gash                      # interactive shell
gash script.sh args       # run a script
gash -c 'echo hi'         # run a command string
gash -lc '...'            # login shell (sources ~/.bash_profile)
```

To use it **as** `bash`, copy (don't symlink) `gash.exe` to `bash.exe` (and `sh.exe`) in a
directory of your choice; it reports itself as `bash` in error messages when invoked that way.

- **Windows Terminal**: add a profile whose command line is
  `C:\path\to\gash.exe -l` (or the copied `bash.exe -l`).
- **Claude Code** (or anything else that looks for Git Bash): set
  `CLAUDE_CODE_GIT_BASH_PATH=C:\path\to\bash.exe` pointing at the copy.
- **VS Code**: add a terminal profile with `"path": "C:\\path\\to\\gash.exe", "args": ["-l"]`.

Startup files: login shells read the first of `~/.bash_profile`, `~/.bash_login`,
`~/.profile`; interactive non-login shells read `~/.bashrc` (or `--rcfile`). Git's
`/etc/profile` is **not** sourced (its environment is set up natively, which is a large part
of the startup speedup); set `GASH_ETC_PROFILE=1` to source it anyway.

Flags: `-c`, `-s`, `-i`, `-l`/`--login`, `--norc`, `--noprofile`, `--rcfile FILE`,
`--posix`, `-o opt`, `-O shopt`, any `set` flag (`-e -u -x ...`), `--version`.

## Differences from Git Bash

- **No `fork()`**: subshells, `$(...)` and pipeline stages run in-process (threads with
  their own variables, cwd and fd table). Observable differences are rare: `$BASHPID` in a
  subshell is a synthetic number, and `kill` of a subshell's `$BASHPID` only works for
  signals delivered to itself.
- **Process substitution** `<(cmd)` / `>(cmd)` uses temporary files rather than pipes, so
  `<(cmd)` runs `cmd` to completion first (not streaming).
- **Job control**: no Ctrl+Z/stop (Windows has no SIGTSTP). Background jobs, `jobs`, `wait`,
  `fg` (waits), `kill` work.
- **Signals**: Windows only has Ctrl+C/Ctrl+Break. `kill -SIG pid` terminates external
  processes; signals to the shell itself (`kill -USR1 $$`) run the matching trap.
- **File descriptors > 2** work inside the shell (`exec 3>file`, `>&3`) but are not passed to
  external programs (Windows can only pass stdin/stdout/stderr).
- Strings are UTF-8 internally: `$'\xff'`-style bytes ≥ 0x80 in variables are approximated;
  `printf`/`echo -e` write raw bytes correctly.
- `/etc/profile` and `/etc/bash.bashrc` aren't sourced by default (see above).

## Testing

```bash
cargo test                          # unit tests + differential compat suite
GASH_COMPAT_FILTER=015 cargo test --test compat   # one script
GASH_COMPAT_JOBS=8 cargo test --test compat       # parallel
```

`tests/regen_expected.ps1` regenerates the expected outputs from Git Bash.

## Contributing

**Every commit must be attributed to the model that produced it, if any.** End the commit
message with a trailer for each model used, naming the model and its version:

```
Assisted-By: GPT-5.1
Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

`Assisted-By: <model and version>` works for any model. `Co-Authored-By: <model and version> <noreply@vendor>`
is the form some tools, such as Claude Code, write themselves. If no model was involved, end the
message with `Assisted-By: none`. A PR check fails on any commit without one of these and comments
on the PR with what to fix; reword the commits with `git rebase -i` and force-push.

## Layout

| File | Purpose |
|---|---|
| `src/parser.rs` | Recursive-descent parser (heredocs, aliases, `[[ ]]`, arithmetic, all word forms) |
| `src/expand.rs` | Brace/tilde/parameter/command/arithmetic expansion, IFS splitting, globbing |
| `src/exec.rs` | Pipelines, subshells, functions, redirections, process launching, script execution |
| `src/builtins.rs`, `src/builtins_io.rs` | Builtins |
| `src/shell.rs`, `src/vars.rs` | Shell state, copy-on-write variable scopes, options, jobs |
| `src/io.rs` | Per-shell fd table over files, pipes, memory buffers and the console |
| `src/path.rs` | MSYS mount table emulation and path/argument conversion |
| `src/pattern.rs`, `src/regex.rs` | Glob/extglob matcher, POSIX ERE engine for `=~` |
| `src/arith.rs` | Arithmetic evaluator |
| `src/lineedit.rs`, `src/interactive.rs`, `src/prompt.rs` | Line editor, REPL, completion, prompts |
| `src/sys.rs` | Win32 FFI (no `windows-sys` dependency) |
| `assets/gash-icon.svg` | The gash icon |
| `assets/make_icon.py`, `build.rs` | Rasterize the icon into `assets/gash.ico`; embed it in `gash.exe` (writes the `.res` directly, no resource compiler) |
| `assets/make_speed_svg.py` | Builds the speed animation at the top of this README from the measured timings |
| `.github/workflows/release.yml` | Builds x64/arm64 zips on a `v*` tag, publishes the GitHub release, submits to winget |
| `.github/workflows/attribution*.yml`, `.github/scripts/check-attribution.sh` | Checks that every PR commit names the models that produced it, and comments on the PR when one doesn't |
| `packaging/winget/` | winget manifests for the first submission, and how releases reach winget |
| `assets/video/` | Renders that animation frame by frame into an MP4 (`python assets/video/server.py`, then open `http://localhost:8765/video/render.html` and run `renderVideo('dark')`) |

The speed animation's style is inspired by Ben Dicken's "language balls". Git logo by
[Jason Long](https://git-scm.com/downloads/logos), licensed under CC BY 3.0.
