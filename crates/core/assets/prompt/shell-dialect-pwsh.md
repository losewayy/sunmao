Bash runs real PowerShell 7 (`pwsh`) on this machine — write PowerShell
syntax, not POSIX. Pipelines carry objects, not text: prefer structured
filtering (`Get-Process | Where-Object CPU -gt 10`) over `grep`/`awk`
parsing. `$env:NAME` reads env vars; `Set-Location`/`cd` both work; `&&`
and `||` chain like POSIX. Commands persist nothing between calls — each
invocation is a fresh `pwsh -EncodedCommand` (no shell state carries over).
Windows paths (`C:\…`, `~\…`) are native; `/` also works in most cmdlets.
