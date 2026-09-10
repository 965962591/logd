# logd.exe CLI

`logd.exe` exposes the machine-readable command-line interface for the log engine.
It is intended for agents and scripts: successful commands write JSON to stdout;
`read`, `query`, `filter`, and `suggest` write one JSON object per line (JSONL).
Errors go to stderr and return a non-zero exit code. All reported line numbers
are 1-based.

Build and inspect the interface:

```powershell
logd.exe --help
```

## Commands

```powershell
# File size, detected encoding, and complete line count.
logd.exe info D:/logs/device.log

# Read a bounded range. Use --json for one JSON array instead of JSONL.
logd.exe read D:/logs/device.log --line 120 --count 20

# Boolean query language used by the desktop application.
logd.exe query D:/logs/device.log 'level>=W and tag:Render' --max-lines 50

# Apply a native .logd or compatible .tat filter configuration.
logd.exe filter D:/logs/device.log tat/ae_log.tat --max-lines 50

# Get query-input completions from a bounded log sample.
logd.exe suggest D:/logs/device.log 'tag:ae' --limit 20

# Convert a .tat or inspect a .logd configuration as JSON.
logd.exe config tat/ae_log.tat
```

`--max-lines` defaults to 100 and `--max-bytes` defaults to 4096, so an agent
cannot accidentally emit an unbounded log. `--encoding utf-8` or
`--encoding gb18030` overrides automatic encoding detection when needed.
