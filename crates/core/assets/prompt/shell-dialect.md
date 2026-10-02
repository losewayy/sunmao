Bash runs an embedded POSIX-compatible shell (deno_task_shell) — write
bash syntax only; it executes identically on Windows and Unix. Windows
system binaries (powershell.exe, reg.exe) are callable as external
commands. Raw PowerShell/cmd syntax will fail.
On Windows, POSIX names that are not builtins (`find`, `sort`, `timeout`,
`fc`, `expand`) resolve through PATH and can land on System32 binaries with
different semantics — prefer the Glob/Grep tools, and trust the [preflight]
advisory when it flags a collision. `/dev/null` redirects work.
