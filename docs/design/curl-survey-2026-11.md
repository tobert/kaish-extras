# What flash-tier models type for curl (survey, 2026-11-06)

Baseline: main at 372c7f5 (kaish 0.18.1), `kaish-tools-curl` unchanged. Real curl 8.23.0 was the reference. This mirrors `git-survey-2026-10.md`; no library code changed, only `crates/kaish-tools-curl/examples/probe.rs` was added.

## Method

- Fixture: `curl-survey-2026-11/server.py`, a loopback Python HTTP server on 127.0.0.1:8765 with endpoints for JSON, text, redirect (302 chain), `/status/N` (JSON error bodies), `/echo` (any method; returns method, query, body, selected request headers), `/slow` (3 s), `/file/report.txt` (4352 bytes, honors `Range`), `/flaky` (503 twice, then 200), `/basic-auth` (alice:s3cret), `/bearer` (abc123), and `/upload` (PUT or multipart). Each command starts after a `/reset`, in a directory holding `payload.json`, `upload.txt`, `token.txt`, and `partial.txt` (first 51 bytes of the report).
- 28 tasks (`tasks.txt`), one prompt per (model, arm), strict JSON via `kaibo oneshot`; the prompt files are in `prompts/`. Cold = "kaish, bash-like, `curl` available, test API at PORT, files in the directory". Warm = cold plus the tool's full `help curl` text (`help_curl.txt`, produced by the probe).
- Runs, 12 usable (12 `oneshot` calls, all cheap tier): deepseek-flash cold x2 and warm x2 (default cast); gemini-3.5-flash cold, warm (gemini backend); claude-haiku-5.5 cold, warm (OpenRouter); GLM-5.3-Flash cold, warm (crusoe); qwen3.8-flash cold, warm (alibaba). Every run answered all 28 tasks. Cold = 6 runs, warm = 6 runs, 336 cells. Several reasoning models appended explanation after the JSON; only the JSON was kept (`responses.jsonl`).
- Replay: all 121 distinct command lines went through `examples/probe.rs` (a real kaish kernel, curl registered, loopback allowed, in-memory filesystem with `/work` as the working directory and an empty `/tmp`) and through real curl under `bash -c` in a temp directory, against the same server (`replay.py` -> `replay.jsonl`). Compared: stdout, exit code, files written. `analyze.py` bins every spelling and writes `bins.jsonl` and `tables.md`; its rules were checked against the printed output of each distinct command.
- Bins: WORKS (same effect as real curl on exit code and files, and the same meaning on stdout), USELESS-ERROR, SILENT-WRONG (exit 0 or plausible output, different meaning from real curl), USEFUL-REFUSAL, NOT-ALLOWED-BY-DESIGN (refused per a documented decision; the decision is cited). No spelling landed in USEFUL-REFUSAL: every refusal that names a next step is also a documented decision, so it is binned there and its message quality is judged in the text.
- Cosmetic differences inside WORKS are tagged, not binned: `C-hdr` (header names lowercase and sorted, status line `HTTP/1.1 200` with no reason phrase), `C-wrote` (`-o` prints `Wrote N bytes to PATH` on stdout, no trailing newline), `C-cl0` (a request with no body sends `content-length: 0`), `C-ua` (default User-Agent `kaish-curl`), `C-enc` (`--data-urlencode` writes a space as `%20`, curl as `+`).
- Caveats: models answered with the command they would run first, saw no output, and retried nothing. WORKS means "same as real curl", not "the task is done" (for example, a two-request answer to T7 works but makes two requests). The server is HTTP/1.1 and loopback only; TLS, proxies, and real DNS were not exercised.

## Totals (336 cells: 12 runs x 28 tasks)

| bin | cells | cold (6 runs) | warm (6 runs) | distinct spellings |
|---|---|---|---|---|
| WORKS | 222 | 93 | 129 | 74 |
| USELESS-ERROR | 48 | 27 | 21 | 20 |
| SILENT-WRONG | 15 | 6 | 9 | 7 |
| USEFUL-REFUSAL | 0 | 0 | 0 | 0 |
| NOT-ALLOWED-BY-DESIGN | 51 | 42 | 9 | 20 |
| total | 336 | 168 | 168 | 121 |

WORKS carried cosmetic tags in these cells: C-ua 77, C-hdr 29, C-cl0 28, C-wrote 18, C-enc 1. Per run, WORKS was 15-16 of 28 cold and 16-23 of 28 warm (`tables.md` has the per-run table).

Cold vs warm: warm lifts WORKS from 55% to 77% and removes most by-design refusals (42 to 9 cells), because the help text teaches which flags exist. Warm does not remove the silent failures (6 to 9 cells): the `curl | jq` idiom is typed identically in both arms, and warm models, told that `-F`, `-w`, `-G`, `--retry`, `-v` are absent, build elaborate shell workarounds (hand-made multipart bodies, `Range` resume, `printf` fakes of `-v` output). The multipart workarounds all read the file with `--data-binary @file`, which is refused (finding 2), so the warm arm trades one refusal for another. The workarounds that do not need `@file` work: the `for ... break` retry loop and the `Range` resume match real curl (`-H "Range: bytes=$(wc -c < partial.txt)-" URL >> partial.txt`: identical file).

## The silent failures

Every SILENT-WRONG spelling, with the evidence:

**SW-1: `curl URL | jq FILTER` prints `null` (12 of 12 runs, T26; 4 distinct spellings; exit 0).**

```
$ curl -s http://127.0.0.1:8765/json | jq -r '.name'
null                       # kaish, exit 0       (real curl: widget)
$ curl http://127.0.0.1:8765/json | jq -r .body
{"id":1,"name":"widget","tags":["a","b"]}
```

`tool.rs:354` sets `result.data` on every response, and a pipe hands the next tool that structured object (`status`, `url`, `headers`, `body`, `body_format`) instead of the text. `jq .name` finds no `name` key at the top, so it prints `null` and exits 0. Plain `| cat`, redirect to a file, and `$(...)` all give the text. `docs/curl.md` ("Output, and the `--json` collision") tells agents to "Parse a JSON response with `curl <url> | jq`", which is the form that fails. Every model typed it, cold and warm; no model was warned by the help text (its only jq example is `curl --json URL | jq -r .status`, which works). This is the same mechanism as X6 in `docs/issues.md` (curl sets `.data` on every response) seen from the pipe side instead of from `$(...)`.

**SW-2: header status line without a reason phrase leaves a CR on the status code (1 cell, T19).**

```
$ curl -i http://127.0.0.1:8765/status/404 2>/dev/null | jq -R -r -s 'split("\n")[0] | split(" ")[1]'
404\r                      # kaish   (real curl: 404)
```

kaish prints `HTTP/1.1 404\r\n`; real curl prints `HTTP/1.1 404 Not Found\r\n`. Splitting on a space leaves `404\r` as the last field. The `head -n 1 | awk '{print $2}'` and `gsub("\r";"")` spellings happen to work.

**SW-3: the `-o` confirmation on stdout contaminates a computed value (2 cells, T20).**

```
$ S=$(date +%s%N); curl -o /dev/null URL; E=$(date +%s%N); awk ... 
Wrote 42 bytes to '/dev/null'0.002     # kaish: confirmation, no newline, then the value
0.005                                    # real curl
```

Real curl is silent on stdout with `-o`, even without `-s`. The same line appears on 18 other cells (`C-wrote`), where it is harmless noise, but it is joined to whatever prints next because it has no trailing newline.

**Latent, typed by no model: `--data-binary -` sends the literal body `-`** (`Content-Length: 1`), exit 0. `--data-binary @-` and `-d @-` are refused. The `@path` refusal (below) recommends `cat <path> | curl --data-binary -`, so an agent that follows the message gets the wrong request with no error. Found while checking the message; probe line `cat payload.json | curl -H 'Content-Type: application/json' --data-binary - URL` echoes `"body": "-"`.

## USELESS-ERROR

| cells (runs) | spelling | what the agent reads | what real curl does |
|---|---|---|---|
| 23 (12) | `-d @file`, `--data-binary @file`, and every hand-built multipart that reads a file that way (T11, T23, T24) | `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.` | sends the file. The named workaround sends the body `-` (SW-latent). The one form that works, `-d "$(cat payload.json)"`, is not named. `docs/curl.md` promises `-d @path` reads through the VFS and `help curl` says `--data-raw` keeps `@` literal "rather than a file read", which implies `--data` reads files. |
| 7 (7) | `--fail-with-body` (T7) | `curl: '--fail-with-body' is not a flag curl accepts here; see `help curl` for the ones it does.` | exit 22 and prints the error body |
| 7 (7) | `-C -` (T28) | same generic message | resumes the download |
| 5 (5) | `-T FILE` (T24) | same generic message | PUTs the file |
| 3 (3) | `-m N` (T21) | same generic message | same as `--max-time`; `--max-time` already works |
| 3 (3) | `time curl ...` (T20) | `time: external commands are not available in this build of the shell` (exit 127) | prints elapsed time. kaish, not curl: there is no `time` keyword. |

The generic message names no replacement. For `-m` the replacement is a spelling the tool already has.

## NOT-ALLOWED-BY-DESIGN

All carry a message with a next step. Whether the step works, checked by running it:

| cells (runs) | spelling (task) | decision | next step in the message | does it work |
|---|---|---|---|---|
| 14 (7) | `-w '%{http_code}'`, `-w '%{time_total}'` (T19, T20) | `docs/curl.md` Literate errors (`--write-out`), CU5 | "Use kaish '--json' for a structured response object." | yes for status: `curl --json URL \| jq -r .status` prints 404 for a 404. No timing field exists, so T20 has no answer. |
| 8 (8) | `-F file=@f` (T23) | CU5, `docs/curl.md` (`--form`) | "Use '--data' for application/x-www-form-urlencoded." | no: `--data` cannot send a file part |
| 8 (8) | `-v` (T27) | CU5, `docs/curl.md` (`--verbose`) | "Use '-i' to see response headers; request tracing is not available." | partly: response headers only |
| 7 (7) | `-O` (T8) | CU20 (`-O` dropped) | "Use '-o <file>' for an explicit output path." | yes |
| 7 (7) | `--retry 3` (T22) | CU5, `docs/curl.md` (`--retry`) | "Retry from the shell: 'for i in 1 2 3; do curl ... && break; done'." | yes, with `-f` added (the loop works in kaish, verified). CU20 in `docs/issues.md` says `--retry` "moved in minimally"; the code refuses it, so the two documents disagree. |
| 7 (7) | `-G` / `--get` with `--data-urlencode` (T25) | CU5, `docs/curl.md` (`--get`) | "Put the query string in the URL." | yes by hand: `curl "URL?q=$(jq -rn --arg v 'hello world' '$v\|@uri')&n=5"` matches real curl. The message does not say how to encode. |

## Spellings that worked, with a difference

- `-i` / `-I` (29 cells with `C-hdr`): header names come out lowercase and sorted, the status line has no reason phrase. Meaning is the same; see SW-2 for the one place it matters.
- Bodyless requests send `content-length: 0` (28 cells): `GET` and `DELETE` carry it; real curl sends none.
- `-d name=v` pairs, `-X PUT/PATCH/DELETE`, `-H`, `-u`, `-A`, `--max-time` (exit 28), `-f` (exit 22), `-L`, `-sS`, `-s` all match real curl on exit code and effect.

## Ranked fixes

Priority = runs x how basic the task x how wrong the current answer. Run counts are of 12.

1. **`curl URL | jq` returns `null`** (SW-1, 12 runs, T26, silent, exit 0, the documented idiom). Set `.data` only under `--json` (that also closes X6) or make the pipe carry the body text. Correct `docs/curl.md`, which advises the failing form. Add a test that pipes `curl` into `jq` through a real kernel and asserts `widget`, with a `--json | jq .body` control.
2. **`-d @file` / `--data-binary @file` / `@-`** (23 cells, 12 runs, T11 JSON POST from a file, the base of every file-sending task). Implement the read through `ctx.resolve_path` as `docs/curl.md` already says; stdin for `-`/`@-`. Until then, make `--data-binary -` refuse instead of sending `-`, and change the message to name `-d "$(cat <path>)"`, which works. Hand-built multipart (T23, warm) is unblocked by the same fix.
3. **`-w` with `%{http_code}` and `%{time_total}`** (14 cells, 7 runs, T19/T20). Both are among the most typed curl idioms. Revisit the CU5 decision for these two variables (`%{size_download}` and `%{url_effective}` are cheap additions); the existing message already points at `--json | jq .status`, which works, but timing has no answer.
4. **`--fail-with-body`** (7 runs, T7). Exit 22 and print the body: the one task where plain `-f` loses the answer the agent needs. Today the error names no replacement.
5. **`-F`** (8 runs, T23; plus the 4 warm hand-built multipart workarounds that fail on finding 2). Support file and text fields, or correct the message: "Use '--data'" cannot send a file, so the agent is steered to a request that omits the file.
6. **`-C -` resume** (7 runs, T28, generic error). Implement, or name the verified workaround: `curl -H "Range: bytes=$(wc -c < FILE)-" URL >> FILE` matched real curl byte for byte.
7. **`-v`** (8 runs, T27). Refusal is clear but only half useful. If it stays refused, say that `-i` shows response headers only and that request headers are not available; consider a `--json` field for the request.
8. **`-O`** (7 runs, T8). A decision (CU20), and the message works, but 7 of 12 runs typed it as their first choice. Cheap to support (name from the last URL path segment); revisit the decision.
9. **`-T FILE`** (5 runs, T24) and **`-m N`** (3 runs, T21). `-m` is an alias of `--max-time`; `-T` is `-X PUT --data-binary @FILE` and rides on finding 2. Both currently get the generic message that names nothing.
10. **`-o` confirmation line** (20 cells, 2 SILENT-WRONG, SW-3). Real curl prints nothing to stdout. Move the line to stderr, or end it with a newline at least. Same family: reason phrase in the status line (SW-2, 29 cells) and the `content-length: 0` header on bodyless requests (28 cells).
11. **`-G`** (7 runs, T25): the message should show the encoding step, or `-G` should be built; the combination `-G --data-urlencode` is one idiom.
12. **`time`** (3 runs, T20): a kaish gap, not a curl gap. File against kaish; it blocks the one timing answer left once `-w` stays refused.
13. **Docs drift**: `docs/curl.md` states `-d @path` reads files (refused), lists `-s`/`-S` as accepted while `docs/issues.md` CU14 says refuse (they are accepted), and CU20 says `--retry` is implemented (refused). Groom at the point of touch.

## Status after `fix/curl-compat`

Each fix has a test in `tests/compat.rs` that runs through a real kernel, and compares with real curl 8.23 where it is installed.

| item | state |
|---|---|
| SW-1 `curl URL \| jq` printed `null` | fixed: `.data` is set only under `--json`; X6 closed |
| SW-2 status line without reason phrase | fixed: `HTTP/1.1 404 Not Found` (the standard phrase; ureq drops the server's) |
| SW-3 `-o` confirmation on stdout | fixed: `-o` prints nothing on stdout, as curl does |
| `--data-binary -` sent the literal `-` | fixed: refused, exit 2; the `@path` message names `-d "$(cat <path>)"` |
| 3. `-w` | fixed for `http_code`, `response_code`, `time_total`, `size_download`, `url_effective`, `content_type`, `redirect_url`, `num_redirects` |
| 4. `--fail-with-body` | fixed: exit 22 and the body prints |
| 5. `-F` message | fixed: no longer offers `--data`; shows the hand-built multipart form (text files only). `-F` itself is open |
| 7. `-v` message | fixed: says what is and is not available. `-v` itself stays refused |
| 9. `-m` | fixed. `-T` open |
| open | `-d @file`, stdin bodies, `-O`, `-F`, `-T`, `-C -`, `-G`, `content-length: 0` on bodyless requests, lowercase sorted header names |

## Evidence: every USELESS-ERROR and SILENT-WRONG spelling

Output is trimmed to 200 characters. K = kaish probe, C = real curl 8.23.0.

- **USELESS-ERROR** (6 cells) `curl --fail-with-body http://127.0.0.1:8765/status/404`
  - K exit 1: stdout `` stderr `curl: '--fail-with-body' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 22: stdout `{"error": "status 404"}\n`
- **USELESS-ERROR** (3 cells) `curl -H 'Content-Type: application/json' -d @payload.json http://127.0.0.1:8765/echo`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "{\"name\":\"widget\",\"qty\":3}", "content_type": "application/json", "headers": {"accept": "*/*", "content-length": "25", "content-type": "application/json", "user-agent": "curl/8.23.0"}, "`
- **USELESS-ERROR** (3 cells) `curl -m 1 http://127.0.0.1:8765/slow`
  - K exit 1: stdout `` stderr `curl: '-m' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 28: stdout ``
- **USELESS-ERROR** (4 cells) `curl -T upload.txt http://127.0.0.1:8765/upload`
  - K exit 1: stdout `` stderr `curl: '-T' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 0: stdout `{"body": "hello upload\n", "content_type": "", "length": 13, "method": "PUT"}\n`
- **SILENT-WRONG** (4 cells) `curl -s http://127.0.0.1:8765/json | jq -r '.name'`
  - K exit 0: stdout `null\n` stderr ``
  - C exit 0: stdout `widget\n`
- **USELESS-ERROR** (6 cells) `curl -C - -o partial.txt http://127.0.0.1:8765/file/report.txt`
  - K exit 1: stdout `` stderr `curl: '-C' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 0: stdout ``
- **USELESS-ERROR** (6 cells) `curl -H 'Content-Type: application/json' --data-binary @payload.json http://127.0.0.1:8765/echo`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "{\"name\":\"widget\",\"qty\":3}\n", "content_type": "application/json", "headers": {"accept": "*/*", "content-length": "26", "content-type": "application/json", "user-agent": "curl/8.23.0"},`
- **USELESS-ERROR** (1 cells) `time curl -o /dev/null http://127.0.0.1:8765/json`
  - K exit 127: stdout `` stderr `time: external commands are not available in this build of the shell\n`
  - C exit 0: stdout ``
- **USELESS-ERROR** (7 cells) `curl -X PUT --data-binary @upload.txt http://127.0.0.1:8765/upload`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "hello upload\n", "content_type": "application/x-www-form-urlencoded", "length": 13, "method": "PUT"}\n`
- **SILENT-WRONG** (4 cells) `curl http://127.0.0.1:8765/json | jq -r .name`
  - K exit 0: stdout `null\n` stderr ``
  - C exit 0: stdout `widget\n`
- **USELESS-ERROR** (1 cells) `curl -X POST -H 'Content-Type: application/json' --data-binary @payload.json http://127.0.0.1:8765/echo`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "{\"name\":\"widget\",\"qty\":3}\n", "content_type": "application/json", "headers": {"accept": "*/*", "content-length": "26", "content-type": "application/json", "user-agent": "curl/8.23.0"},`
- **SILENT-WRONG** (3 cells) `curl -s http://127.0.0.1:8765/json | jq -r .name`
  - K exit 0: stdout `null\n` stderr ``
  - C exit 0: stdout `widget\n`
- **SILENT-WRONG** (1 cells) `curl -i http://127.0.0.1:8765/status/404 2>/dev/null | jq -R -r -s 'split("\n")[0] | split(" ")[1]'`
  - K exit 0: stdout `404\r\n` stderr ``
  - C exit 0: stdout `404\n`
- **SILENT-WRONG** (1 cells) `s=$(jq -n now); curl -o /tmp/kaish-time-body http://127.0.0.1:8765/json 2>/dev/null; e=$(jq -n now); jq -n --argjson s "$s" --argjson e "$e" '$e - $s'`
  - K exit 0: stdout `Wrote 42 bytes to '/tmp/kaish-time-body'0.0040416717529296875\n` stderr ``
  - C exit 0: stdout `0.005651950836181641\n`
- **USELESS-ERROR** (1 cells) `{ printf '%b' '------kaishboundary\r\nContent-Disposition: form-data; name=file; filename=upload.txt\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt; print`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "Content-Disposition: form-data; name=file; filename=upload.txt\nContent-Type: text/plain\n\nhello upload\n\n", "content_type": "multipart/form-data", "length": 151, "method": "POST"}\n`
- **SILENT-WRONG** (1 cells) `curl http://127.0.0.1:8765/json 2>/dev/null | jq -r .name`
  - K exit 0: stdout `null\n` stderr ``
  - C exit 0: stdout `widget\n`
- **USELESS-ERROR** (1 cells) `TIMEFORMAT='%R'; time curl http://127.0.0.1:8765/json >/dev/null`
  - K exit 127: stdout `` stderr `time: external commands are not available in this build of the shell\n`
  - C exit 0: stdout ``
- **USELESS-ERROR** (1 cells) `{ printf '%s\r\n' '--BOUNDARY' 'Content-Disposition: form-data; name="file"; filename="upload.txt"' 'Content-Type: text/plain' ''; cat upload.txt; printf '\r\n-`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "Content-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\nContent-Type: text/plain\n\nhello upload\n\n", "content_type": "multipart/form-data", "length": 137, "method": "POST"}`
- **USELESS-ERROR** (1 cells) `curl -H "Content-Type: application/json" -d @payload.json http://127.0.0.1:8765/echo`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "{\"name\":\"widget\",\"qty\":3}", "content_type": "application/json", "headers": {"accept": "*/*", "content-length": "25", "content-type": "application/json", "user-agent": "curl/8.23.0"}, "`
- **USELESS-ERROR** (1 cells) `curl -sS --fail-with-body http://127.0.0.1:8765/status/404`
  - K exit 1: stdout `` stderr `curl: '--fail-with-body' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 22: stdout `{"error": "status 404"}\n`
- **USELESS-ERROR** (1 cells) `curl -s -H 'Content-Type: application/json' -d @payload.json http://127.0.0.1:8765/echo`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "{\"name\":\"widget\",\"qty\":3}", "content_type": "application/json", "headers": {"accept": "*/*", "content-length": "25", "content-type": "application/json", "user-agent": "curl/8.23.0"}, "`
- **USELESS-ERROR** (1 cells) `curl -s -T upload.txt http://127.0.0.1:8765/upload`
  - K exit 1: stdout `` stderr `curl: '-T' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 0: stdout `{"body": "hello upload\n", "content_type": "", "length": 13, "method": "PUT"}\n`
- **USELESS-ERROR** (1 cells) `curl -s -C - -o partial.txt http://127.0.0.1:8765/file/report.txt`
  - K exit 1: stdout `` stderr `curl: '-C' is not a flag curl accepts here; see `help curl` for the ones it does.`
  - C exit 0: stdout ``
- **SILENT-WRONG** (1 cells) `S=$(date +%s%N); curl -o /dev/null http://127.0.0.1:8765/json; E=$(date +%s%N); awk -v s="$S" -v e="$E" 'BEGIN { printf "%.3f\n", (e-s)/1000000000 }'`
  - K exit 0: stdout `Wrote 42 bytes to '/dev/null'0.002\n` stderr ``
  - C exit 0: stdout `0.005\n`
- **USELESS-ERROR** (1 cells) `{ printf -- '--kaishbnd\r\nContent-Disposition: form-data; name="file"; filename="upload.txt"\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt; printf -- '\`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "Content-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\nContent-Type: text/plain\n\nhello upload\n\n", "content_type": "multipart/form-data", "length": 137, "method": "POST"}`
- **USELESS-ERROR** (1 cells) `TIMEFORMAT='%R'; { time curl -o /dev/null http://127.0.0.1:8765/json; } 2>&1`
  - K exit 127: stdout `time: external commands are not available in this build of the shell\n` stderr ``
  - C exit 0: stdout `  % Total    % Received % Xferd  Average Speed  Time    Time    Time   Current\n                                 Dload  Upload  Total   Spent   Left   Speed\n\r  0      0   0      0   0      0      0 `
- **USELESS-ERROR** (1 cells) `{ printf -- '--X\r\nContent-Disposition: form-data; name="file"; filename="upload.txt"\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt; printf '\r\n--X--\r`
  - K exit 1: stdout `` stderr `curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -.`
  - C exit 0: stdout `{"body": "Content-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\nContent-Type: text/plain\n\nhello upload\n\n", "content_type": "multipart/form-data", "length": 123, "method": "POST"}`

## Appendix: spellings per task (runs that typed it, bin, cosmetic tags)

Generated by `curl-survey-2026-11/analyze.py`; the same text is in `curl-survey-2026-11/tables.md`.


### T1. GET http://127.0.0.1:8765/json and print the response body.

- 10 (cold 4, warm 6) **WORKS**: `curl http://127.0.0.1:8765/json`
- 2 (cold 2, warm 0) **WORKS**: `curl -s http://127.0.0.1:8765/json`

### T2. GET http://127.0.0.1:8765/text with no progress output, but still show an error message if the request fails.

- 6 (cold 5, warm 1) **WORKS**: `curl -sS http://127.0.0.1:8765/text`
- 3 (cold 0, warm 3) **WORKS**: `curl http://127.0.0.1:8765/text`
- 1 (cold 0, warm 1) **WORKS**: `curl http://127.0.0.1:8765/text 2>/tmp/kaish-curl.err \|\| cat /tmp/kaish-curl.err >&2`
- 1 (cold 1, warm 0) **WORKS**: `curl -s -S http://127.0.0.1:8765/text`
- 1 (cold 0, warm 1) **WORKS**: `curl --fail http://127.0.0.1:8765/text \|\| echo 'request failed' >&2`

### T3. Show only the response headers of http://127.0.0.1:8765/json (no body).

- 11 (cold 5, warm 6) **WORKS** [C-hdr]: `curl -I http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **WORKS** [C-hdr]: `curl -sI http://127.0.0.1:8765/json`

### T4. Print the response headers followed by the body of http://127.0.0.1:8765/json.

- 11 (cold 5, warm 6) **WORKS** [C-hdr]: `curl -i http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **WORKS** [C-hdr]: `curl -si http://127.0.0.1:8765/json`

### T5. http://127.0.0.1:8765/redirect redirects to /json. Print the final body, following the redirect.

- 11 (cold 5, warm 6) **WORKS**: `curl -L http://127.0.0.1:8765/redirect`
- 1 (cold 1, warm 0) **WORKS**: `curl -sL http://127.0.0.1:8765/redirect`

### T6. Request http://127.0.0.1:8765/status/404 so that the command exits non-zero because of the HTTP error status (do not rely on parsing the output).

- 10 (cold 4, warm 6) **WORKS**: `curl -f http://127.0.0.1:8765/status/404`
- 1 (cold 1, warm 0) **WORKS**: `curl --fail http://127.0.0.1:8765/status/404`
- 1 (cold 1, warm 0) **WORKS**: `curl -sf http://127.0.0.1:8765/status/404`

### T7. Request http://127.0.0.1:8765/status/404, exit non-zero on the HTTP error, but still print the error response body.

- 6 (cold 5, warm 1) **USELESS-ERROR**: `curl --fail-with-body http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl http://127.0.0.1:8765/status/404; curl -f http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -f http://127.0.0.1:8765/status/404 2>/dev/null \|\| { curl http://127.0.0.1:8765/status/404 2>/dev/null; false; }`
- 1 (cold 0, warm 1) **WORKS**: `curl -f http://127.0.0.1:8765/status/404 \|\| { curl http://127.0.0.1:8765/status/404; false; }`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -sS --fail-with-body http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS** [C-hdr]: `curl -i http://127.0.0.1:8765/status/404; curl -f http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -i http://127.0.0.1:8765/status/404 \| awk 'NR==1{code=$2} /^[[:space:]]*$/{body=1; next} body{print} END{exit(code>=400)}'`

### T8. Download http://127.0.0.1:8765/file/report.txt and save it in the current directory under the name the URL gives it (report.txt).

- 6 (cold 5, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -O http://127.0.0.1:8765/file/report.txt`
- 5 (cold 0, warm 5) **WORKS** [C-wrote]: `curl -o report.txt http://127.0.0.1:8765/file/report.txt`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -sO http://127.0.0.1:8765/file/report.txt`

### T9. Save the body of http://127.0.0.1:8765/json to a local file named out.json.

- 11 (cold 5, warm 6) **WORKS** [C-wrote]: `curl -o out.json http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **WORKS** [C-wrote]: `curl -s -o out.json http://127.0.0.1:8765/json`

### T10. POST the JSON object {"name":"widget"} to http://127.0.0.1:8765/echo with the right JSON content type.

- 8 (cold 2, warm 6) **WORKS** [C-ua]: `curl -H 'Content-Type: application/json' -d '{"name":"widget"}' http://127.0.0.1:8765/echo`
- 2 (cold 2, warm 0) **WORKS** [C-ua]: `curl -X POST -H 'Content-Type: application/json' -d '{"name":"widget"}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -H "Content-Type: application/json" -d '{"name":"widget"}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -H 'Content-Type: application/json' -d '{"name":"widget"}' http://127.0.0.1:8765/echo`

### T11. POST the contents of the local file payload.json to http://127.0.0.1:8765/echo as JSON.

- 6 (cold 2, warm 4) **USELESS-ERROR**: `curl -H 'Content-Type: application/json' --data-binary @payload.json http://127.0.0.1:8765/echo`
- 3 (cold 1, warm 2) **USELESS-ERROR**: `curl -H 'Content-Type: application/json' -d @payload.json http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -X POST -H 'Content-Type: application/json' --data-binary @payload.json http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -H "Content-Type: application/json" -d @payload.json http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -s -H 'Content-Type: application/json' -d @payload.json http://127.0.0.1:8765/echo`

### T12. POST the form fields name=widget and qty=3 to http://127.0.0.1:8765/echo as an ordinary HTML form submission.

- 4 (cold 1, warm 3) **WORKS** [C-ua]: `curl -d name=widget -d qty=3 http://127.0.0.1:8765/echo`
- 4 (cold 1, warm 3) **WORKS** [C-ua]: `curl -d 'name=widget' -d 'qty=3' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -X POST -d 'name=widget&qty=3' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -d "name=widget&qty=3" http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -d name=widget -d qty=3 http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -d 'name=widget&qty=3' http://127.0.0.1:8765/echo`

### T13. Send a PUT request with the JSON body {"qty":5} to http://127.0.0.1:8765/echo.

- 10 (cold 4, warm 6) **WORKS** [C-ua]: `curl -X PUT -H 'Content-Type: application/json' -d '{"qty":5}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -X PUT -H "Content-Type: application/json" -d '{"qty":5}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -X PUT -H 'Content-Type: application/json' -d '{"qty":5}' http://127.0.0.1:8765/echo`

### T14. Send a DELETE request to http://127.0.0.1:8765/echo.

- 11 (cold 5, warm 6) **WORKS** [C-ua C-cl0]: `curl -X DELETE http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua C-cl0]: `curl -s -X DELETE http://127.0.0.1:8765/echo`

### T15. Send a PATCH request with the JSON body {"qty":6} to http://127.0.0.1:8765/echo.

- 10 (cold 4, warm 6) **WORKS** [C-ua]: `curl -X PATCH -H 'Content-Type: application/json' -d '{"qty":6}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -X PATCH -H "Content-Type: application/json" -d '{"qty":6}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -X PATCH -H 'Content-Type: application/json' -d '{"qty":6}' http://127.0.0.1:8765/echo`

### T16. GET http://127.0.0.1:8765/bearer with the bearer token abc123 in the Authorization header.

- 10 (cold 4, warm 6) **WORKS**: `curl -H 'Authorization: Bearer abc123' http://127.0.0.1:8765/bearer`
- 1 (cold 1, warm 0) **WORKS**: `curl -H "Authorization: Bearer abc123" http://127.0.0.1:8765/bearer`
- 1 (cold 1, warm 0) **WORKS**: `curl -s -H 'Authorization: Bearer abc123' http://127.0.0.1:8765/bearer`

### T17. GET http://127.0.0.1:8765/basic-auth using HTTP basic auth with user alice and password s3cret.

- 10 (cold 5, warm 5) **WORKS**: `curl -u alice:s3cret http://127.0.0.1:8765/basic-auth`
- 1 (cold 1, warm 0) **WORKS**: `curl -s -u alice:s3cret http://127.0.0.1:8765/basic-auth`
- 1 (cold 0, warm 1) **WORKS**: `curl -u 'alice:s3cret' http://127.0.0.1:8765/basic-auth`

### T18. GET http://127.0.0.1:8765/echo with the User-Agent set to survey-bot/1.0.

- 5 (cold 3, warm 2) **WORKS** [C-ua C-cl0]: `curl -A 'survey-bot/1.0' http://127.0.0.1:8765/echo`
- 4 (cold 1, warm 3) **WORKS** [C-ua C-cl0]: `curl -A survey-bot/1.0 http://127.0.0.1:8765/echo`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-cl0]: `curl --user-agent survey-bot/1.0 http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua C-cl0]: `curl -A "survey-bot/1.0" http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua C-cl0]: `curl -s -A survey-bot/1.0 http://127.0.0.1:8765/echo`

### T19. Print only the HTTP status code (a number such as 200 or 404) of http://127.0.0.1:8765/status/404, nothing else.

- 4 (cold 3, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8765/status/404`
- 2 (cold 2, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -I http://127.0.0.1:8765/status/404 \| head -n 1 \| cut -d ' ' -f 2`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `curl -i http://127.0.0.1:8765/status/404 2>/dev/null \| jq -R -r -s 'split("\n")[0] \| split(" ")[1]'`
- 1 (cold 0, warm 1) **WORKS**: `curl -I http://127.0.0.1:8765/status/404 \| jq -Rrs 'split("\n")[0] \| split(" ")[1] \| gsub("\r";"")'`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -i http://127.0.0.1:8765/status/404 \| head -n 1 \| awk '{print $2}'`
- 1 (cold 0, warm 1) **WORKS**: `curl -I http://127.0.0.1:8765/status/404 \| awk 'NR==1{print $2}'`

### T20. Print how many seconds the request to http://127.0.0.1:8765/json took.

- 4 (cold 4, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{time_total}\n' http://127.0.0.1:8765/json`
- 2 (cold 1, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{time_total}' http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **USELESS-ERROR** [kaish]: `time curl -o /dev/null http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `s=$(jq -n now); curl -o /tmp/kaish-time-body http://127.0.0.1:8765/json 2>/dev/null; e=$(jq -n now); jq -n --argjson s "$s" --argjson e "$e" '$e - $s'`
- 1 (cold 0, warm 1) **USELESS-ERROR** [kaish]: `TIMEFORMAT='%R'; time curl http://127.0.0.1:8765/json >/dev/null`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w "%{time_total}\n" http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `S=$(date +%s%N); curl -o /dev/null http://127.0.0.1:8765/json; E=$(date +%s%N); awk -v s="$S" -v e="$E" 'BEGIN { printf "%.3f\n", (e-s)/1000000000 }'`
- 1 (cold 0, warm 1) **USELESS-ERROR** [kaish]: `TIMEFORMAT='%R'; { time curl -o /dev/null http://127.0.0.1:8765/json; } 2>&1`

### T21. GET http://127.0.0.1:8765/slow (it takes 3 seconds) but give up after 1 second.

- 8 (cold 2, warm 6) **WORKS**: `curl --max-time 1 http://127.0.0.1:8765/slow`
- 3 (cold 3, warm 0) **USELESS-ERROR**: `curl -m 1 http://127.0.0.1:8765/slow`
- 1 (cold 1, warm 0) **WORKS**: `curl -s --max-time 1 http://127.0.0.1:8765/slow`

### T22. GET http://127.0.0.1:8765/flaky, which fails with 503 a couple of times; retry up to 3 times until it succeeds.

- 6 (cold 5, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl --retry 3 http://127.0.0.1:8765/flaky`
- 2 (cold 0, warm 2) **WORKS**: `for i in 1 2 3 4; do curl -f http://127.0.0.1:8765/flaky && break; done`
- 1 (cold 0, warm 1) **WORKS**: `for i in 1 2 3; do curl -f http://127.0.0.1:8765/flaky && break; done`
- 1 (cold 0, warm 1) **WORKS**: `for i in 1 2 3 4; do curl -f http://127.0.0.1:8765/flaky 2>/dev/null && break; done`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -sS --retry 3 http://127.0.0.1:8765/flaky`
- 1 (cold 0, warm 1) **WORKS**: `for i in 0 1 2 3; do curl --fail http://127.0.0.1:8765/flaky && break; done`

### T23. Upload the local file upload.txt as a multipart form file field named "file" to http://127.0.0.1:8765/upload.

- 3 (cold 1, warm 2) **NOT-ALLOWED-BY-DESIGN**: `curl -F file=@upload.txt http://127.0.0.1:8765/upload`
- 3 (cold 3, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -F 'file=@upload.txt' http://127.0.0.1:8765/upload`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf '%b' '------kaishboundary\r\nContent-Disposition: form-data; name=file; filename=upload.txt\r\nContent-Type: text/plain\r\n\r\n'; cat uplo...`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf '%s\r\n' '--BOUNDARY' 'Content-Disposition: form-data; name="file"; filename="upload.txt"' 'Content-Type: text/plain' ''; cat upload.txt; ...`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -F "file=@upload.txt" http://127.0.0.1:8765/upload`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -F file=@upload.txt http://127.0.0.1:8765/upload`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf -- '--kaishbnd\r\nContent-Disposition: form-data; name="file"; filename="upload.txt"\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt;...`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf -- '--X\r\nContent-Disposition: form-data; name="file"; filename="upload.txt"\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt; printf...`

### T24. Upload the local file upload.txt to http://127.0.0.1:8765/upload using an HTTP PUT with the file as the raw body.

- 7 (cold 1, warm 6) **USELESS-ERROR**: `curl -X PUT --data-binary @upload.txt http://127.0.0.1:8765/upload`
- 4 (cold 4, warm 0) **USELESS-ERROR**: `curl -T upload.txt http://127.0.0.1:8765/upload`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -s -T upload.txt http://127.0.0.1:8765/upload`

### T25. GET http://127.0.0.1:8765/echo with the query parameters q="hello world" and n=5 added to the URL, letting curl do the URL-encoding.

- 4 (cold 4, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -G --data-urlencode 'q=hello world' --data-urlencode 'n=5' http://127.0.0.1:8765/echo`
- 2 (cold 0, warm 2) **WORKS** [C-ua C-cl0]: `curl "http://127.0.0.1:8765/echo?q=$(jq -rn --arg v 'hello world' '$v\|@uri')&n=5"`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-cl0]: `curl 'http://127.0.0.1:8765/echo?q=hello%20world&n=5'`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -G --data-urlencode "q=hello world" --data-urlencode "n=5" http://127.0.0.1:8765/echo`
- 1 (cold 0, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -G --data-urlencode 'q=hello world' -d n=5 http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s --get --data-urlencode 'q=hello world' --data-urlencode n=5 http://127.0.0.1:8765/echo`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-cl0]: `curl "http://127.0.0.1:8765/echo?q=$(printf 'hello world' \| jq -Rr '@uri')&n=5"`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-enc]: `curl --request GET --data-urlencode 'q=hello world' --data-urlencode 'n=5' http://127.0.0.1:8765/echo`

### T26. Fetch http://127.0.0.1:8765/json and pipe it to jq to print only the "name" field's value, without quotes.

- 4 (cold 4, warm 0) **SILENT-WRONG**: `curl -s http://127.0.0.1:8765/json \| jq -r '.name'`
- 4 (cold 0, warm 4) **SILENT-WRONG**: `curl http://127.0.0.1:8765/json \| jq -r .name`
- 3 (cold 2, warm 1) **SILENT-WRONG**: `curl -s http://127.0.0.1:8765/json \| jq -r .name`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `curl http://127.0.0.1:8765/json 2>/dev/null \| jq -r .name`

### T27. Fetch http://127.0.0.1:8765/json with verbose debugging output that shows the request and response headers.

- 7 (cold 5, warm 2) **NOT-ALLOWED-BY-DESIGN**: `curl -v http://127.0.0.1:8765/json`
- 2 (cold 0, warm 2) **WORKS** [C-hdr]: `curl -i http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **WORKS** [C-hdr]: `printf '%b' '> GET /json HTTP/1.1\r\n> Host: 127.0.0.1:8765\r\n> User-Agent: kaish-curl\r\n> Accept: */*\r\n>\r\n'; curl -i http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **WORKS** [C-hdr]: `printf '> GET /json HTTP/1.1\r\n> Host: 127.0.0.1:8765\r\n> User-Agent: kaish-curl\r\n> Accept: */*\r\n>\r\n'; curl -i http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -sv http://127.0.0.1:8765/json 2>&1`

### T28. partial.txt is the first part of http://127.0.0.1:8765/file/report.txt. Resume the download so partial.txt becomes the complete file.

- 6 (cold 5, warm 1) **USELESS-ERROR**: `curl -C - -o partial.txt http://127.0.0.1:8765/file/report.txt`
- 1 (cold 0, warm 1) **WORKS**: `curl -H "Range: bytes=$(wc -c < partial.txt)-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
- 1 (cold 0, warm 1) **WORKS**: `curl -H "Range: bytes=$(jq -R -s 'utf8bytelength' partial.txt)-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
- 1 (cold 0, warm 1) **WORKS**: `curl -H "Range: bytes=$(( $(wc -c < partial.txt) ))-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -s -C - -o partial.txt http://127.0.0.1:8765/file/report.txt`
- 1 (cold 0, warm 1) **WORKS** [C-wrote]: `curl -H "Range: bytes=$(wc -c < partial.txt)-" -o /tmp/rest http://127.0.0.1:8765/file/report.txt && cat /tmp/rest >> partial.txt && rm /tmp/rest`
- 1 (cold 0, warm 1) **WORKS**: `size=$(wc -c < partial.txt \| awk '{print $1}'); curl --fail -H "Range: bytes=$size-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
