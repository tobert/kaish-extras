# What flash-tier models type for git (survey, 2026-10-11)

Baseline: main at 926edd4, before any of the fixes it motivated. Replay used `crates/kaish-tools-git/examples/probe.rs` with default features (no `textdiff`).


**Outcome (2026-10-11, PRs #30–#35).** Landed: items 1 (`--oneline`), 2
(`log -- FILE` and a bare tracked path), 3 (`show` lists files), 4
(`status -s`), 5 (`--show-current`), 6 (next-step refusals for `rev-parse`,
`ls-files` and the rest), 7 (`log A..B`), 8 (`--cached`, `--list`, `-a`), 10
(write verbs refused as read-only). Item 9 was fixed upstream: kaish 0.18.0
parses `HEAD~2:README.md` unquoted. Item 11 (`--format`) is Q3 in
docs/issues.md. The findings below describe main at 926edd4, before any of
it; re-running the survey is Q4.

## Method

- Fixture: clone of kaish-extras at 926edd4 with one staged edit (README.md), one unstaged edit (Cargo.toml), one untracked file, branch `feature/x` (+1 commit), tag `v0.1.0-survey`. Reproduce with `git-survey-2026-10/setup.sh <empty-dir>`.
- 24 tasks (17 read-only, 7 commit-path) in one prompt per (model, arm), strict JSON, via `kaibo oneshot`. Cold = "kaish, bash-like, `git` available, you are at repo root". Warm = the same plus the full `help git` text.
- Runs (11 usable): deepseek-flash cold x2 and warm x2; gemini-3.5-flash cold, warm; claude-haiku-latest cold, warm; glm-flash cold, warm; qwen3.8-flash cold. The qwen3.8-flash warm run is **unusable** (it renumbered the tasks and answered a different question set) and is excluded. Cold = 6 runs, warm = 5 runs.
- Every distinct command (71) was replayed with the probe against the fixture, and read-only ones against real git. Write verbs were not run against real git. `HEAD~N` was quoted for replay (the models never quoted it; kaish refuses it unquoted).
- Counts below are runs (of 11) that typed the spelling for the task. Bin is per spelling. Data: `git-survey-2026-10/` holds `tasks.txt`, `responses.jsonl`, `tables.md` (full per-task tables), and `help_git.txt` (the warm arm's context). Replay output is not kept; rerun the probe over `responses.jsonl`.
- Caveat: models answered with commands they would run first; no model saw a result and retried. Real agents get a second turn, so error quality matters more than first-try rate.

## Totals (first command per run x task, 257 cells)

| bin | cells | note |
|---|---|---|
| WORKS | 105 | 100 of 187 on read-only tasks 1-17 |
| USELESS-ERROR | 67 | 65 on read-only tasks |
| SILENT-WRONG | 16 | all on read-only tasks (11 `show <commit>`, 3 `log feature/x`, 2 `--path`) |
| USEFUL-REFUSAL | 6 | all `diff A..B` / `diff A B` |
| WRITE-REFUSED | 63 | refusal is not useful (see below) |

Distinct spellings: 18 WORKS, 29 USELESS-ERROR, 5 SILENT-WRONG, 3 USEFUL-REFUSAL, 16 WRITE-REFUSED.

Per-run first-command WORKS on tasks 1-17: cold 5-7 of 17; warm roughly 11-14 of 17. Warm roughly doubles the hit rate; it also moves the dominant failure from "unknown flag" to the silent/steered-wrong ones below.

Tags: K-a hits 2 (haiku warm `--path`, both SILENT-WRONG). K-b hits 19 cells (`log -- FILE`, `diff -- FILE`); every file-history task (T7, T8) hit it or the bare-operand variant. Quoting: 11 cells typed `HEAD~2` unquoted (T17), plus `abc1234~1` and `abc1234^`.

## Spellings ranked per task (runs of 11 -> bin)

- T1 working tree: `status` 11 W.
- T2 short status: `status -s` 7 U; `status --json` 3 W (warm); `status` 1 W.
- T3 staged vs not: `status` 10 W; `diff --staged`+`diff` 3 W.
- T4 diff unstaged: `diff` 11 W*. T5: `diff --staged` 10 W*; `diff --cached` 1 U.
- T6 last 10: `log --oneline -10` / `-n 10 --oneline` / `--oneline -n 10` 7 U; `log --limit 10` 3 W; `log -n 10` 1 W.
- T7 file history: `log -- README.md` 5 U(K-b); `log --follow -- README.md` 3 U; `log --oneline --follow -- ...` 1 U; `log README.md` 1 U; `log --path README.md` 1 SW(K-a).
- T8 who/when: `log -1 --format=... -- Cargo.toml` (3 + 2 variants) U; `log --limit 1 -- Cargo.toml` 2 U(K-b); `log -1/-n 1 Cargo.toml` 2 U; `log --limit 1 --path` 1 SW(K-a).
- T9 show commit: `show abc1234` 11 SW; `diff --from X~1 --to X` 1 W; `diff --from X^ ...` 1 (unquoted `^`).
- T10 branches: `branch` 8 W; `branch --list` 2 U; `branch -a` 1 U.
- T11 current branch: `branch --show-current` 6 U; `branch` 5 W; `info` 2 W.
- T12 feature/x not on main: `log [--oneline] main..feature/x` 7 U; `log feature/x` 3 SW; `log main` 2 SW; `log main --json`/`feature/x --json` 1 W (two lists, model must diff).
- T13: `diff --from main --to feature/x` 5 W*; `diff main..feature/x` 3 UR; `diff main feature/x` 3 UR.
- T14: `tag` 9 W; `tag --list` 2 U.
- T15 root: `rev-parse --show-toplevel` 7 U; `info` 5 W.
- T16 tracked files: `ls-files` 6 U; `ls --recursive HEAD` / `ls HEAD --recursive` 5 W.
- T17: `show HEAD~2:README.md` 11 W (needs quoting, see Short list item 9).
- T18-T24 (commit path): all 9 models-runs that answered typed real git (`add`, `commit -m`, `commit --amend -m`, `checkout -b` 5 / `switch -c` 4, `restore --staged`, `restore FILE`, `stash` + `stash pop`); all WRITE-REFUSED. Warm deepseek runs 1 and 2 declined or substituted a read verb (`status`), which is the correct behavior and was taught by the help text; glm warm used `command git ...` to try to bypass the tool (not honored by the probe; would reach a real binary if the shell had one).

\* W* = exit 0 but file-level summary only (status letter, +/- counts); `diff --patch` is refused with exit 4 ("textdiff feature"). That refusal is useful, but real git users expect hunks.

## Bin reasons

- USELESS-ERROR unknown flags: clap says `unexpected argument '-s' found ... tip: to pass '-s' as a value, use '-- -s'`. The tip is wrong for this situation; it never says the flag has no equivalent or what to type. Several get a near-miss tip (`--cached` -> "similar: --patch", `--list` -> "similar: --limit"), which steers wrongly.
- USELESS-ERROR `log README.md` / `log -- README.md` (K-b): "'README.md' does not name a commit" - calls a path a revision. `diff -- Cargo.toml` (K-b): "takes no bare operands ... a path after '--'" tells the model to do what it just did.
- USELESS-ERROR `log main..feature/x`: says `..` is not parsed and lists accepted rev forms, but not how to get the answer (two logs? `--rev`?). `diff A..B` by contrast says exactly `--from/--to` and is UR.
- USELESS-ERROR unknown verbs (`ls-files`, `rev-parse`): "a verb is required - 'rev-parse' is not one of them. This build offers: ..." lists verbs but names none for the intent.
- WRITE-REFUSED (`add`, `commit`, `restore`, `stash`, `switch`, `checkout`): same "a verb is required - 'add' is not one of them" message. It never says the tool is read-only, so a model cannot tell "not built yet" from "never".
- SILENT-WRONG `show <commit>`: exit 0, prints oid/parents/author/summary/body and **no changes** (no file list). A model asking "what did this commit change" gets a confident non-answer. `log <branch>` for "commits on X not on main" prints the whole history of X.

## The short list (priority order)

Priority = runs x how basic the task is x how wrong the current answer is.

1. **`git log --oneline` (and `-n N`, `-N`)** - 7+ runs on T6, 5 more on T12; most-typed unsupported flag. Now: unknown-flag error with wrong tip. Action: accept `--oneline` (default `log` output is already one line per commit; make it a no-op-or-explicit alias) and `-N`/`-n N` (the latter already works). `-10` form still fails.
2. **File history: `git log -- FILE`, `git log FILE`, `log --follow -- FILE`** - 10 runs across T7/T8, every model. Now: K-b plus "path is not a commit" misdiagnosis. Action: after the K-b fix accept `-- FILE` as git means it; independently, when a bare operand is not a revision but is a path in HEAD or the work tree, accept it as a path (git does) or say "put paths after `--`". `--follow`: refuse with "no rename following in this build; use `git log -- FILE`".
3. **`git show <commit>` shows no changes** - 11 runs (T9), the only SILENT-WRONG that every model hits. Action: accept as git means it: append the file-level change list to `show <commit>`; until then, state in output/stderr "no changes shown; use `git diff --from <c>~1 --to <c>`" (exit stays 0, notice on stderr).
4. **`git status -s` / `--short`** - 7 runs (T2). Now: wrong tip. Action: accept; default output is already near porcelain, so alias `-s/--short` to it. (`--json` is what warm models found; keep.)
5. **`git branch --show-current`** - 6 runs (T11). Action: accept (print the branch name or empty when detached, as git does). Plain `git branch` already works and marks `*`, as do `git info` (`head main (...)`).
6. **`git rev-parse --show-toplevel` / `--is-inside-work-tree`, `git ls-files`** - 7 + 6 runs (T15, T16). Action: refuse with a next step: "`rev-parse` is not built; the repo root is `repo_root_vfs` in `git info`" and "`ls-files` is not built; use `git ls --recursive HEAD`" (a content difference: `ls HEAD` lists the commit, not the index, and the message should say so). Accepting `ls-files` as an alias risks the HEAD-vs-index difference; refuse, do not alias.
7. **Ranges: `git log main..feature/x`, `git diff main..feature/x`, `git diff main feature/x`** - 12 runs (T12/T13). `diff` is already a good refusal. Action: make the `log` refusal say what to type (needs a supported spelling first: a `--not <rev>` / range feature is the real fix; until then say "range log is not built; `git branch --contains` / `--merged` can answer 'is it on main'"). Do not accept `A..B` silently for `log`; the models want it though, and it is git's most common spelling here.
8. **Short flag spellings on other verbs: `diff --cached`, `branch --list`, `branch -a`, `tag --list`** - 6 runs. Action: accept `--cached` (= `--staged`), `--list` (no-op on `branch`/`tag`), `-a` (= `--all`). They are exact git synonyms.
9. **Unquoted `HEAD~2:README.md`** - 10 runs typed it bare; kaish refuses before git sees it. Action: out of scope for this tool (a kaish lexer issue); either the error from the kernel should say "quote it: 'HEAD~2:README.md'", or help text should show the quoted form in every `~`/`^` example (warm models dropped the quotes too; the help text was not checked for a quoted example).
10. **Write verbs (`add`, `commit`, `checkout`, `switch`, `restore`, `stash`)** - 63 cells, every model, every run in the cold arm. Action: refuse with one sentence stating the profile and the next step: "git is read-only in this shell; `add`/`commit`/... are not available. Use `git status` / `git diff` to inspect". Today's message ("a verb is required") misdiagnoses. Keep them out of scope for implementation.
11. **`git log --format=...` / `--date` / `--follow`** - 8 runs (T8 mostly). The models want author+date in one line. Now: unknown flag / kaish lexer error on `%`. Action: refuse with "use `git log --limit 1 --json` for author and date" (default `log` output already prints oid, date, author, summary per row; say so in `help git` for the "who/when" question).
12. **K-a (`--path`) and K-b (`--`)** - already diagnosed; haiku warm used `--path` because the help text names it. Fixing K-a is a priority regardless: it is a silent-empty result on the flag our own help recommends.

## Cold vs warm: only where it changes a recommendation

- Warm teaches `--from/--to`, `ls --recursive HEAD`, `info`, `--limit`, `--json`, and the read-only boundary (2 of 5 warm runs declined writes outright). Keep that. It cut USELESS-ERROR cells from 8-10 per run (cold) to 0-5 (warm).
- Warm steers wrong in two places: (a) it says range `A..B` is unsupported but never gives the substitute for `log`, so models fall back to `log feature/x` (SILENT-WRONG, T12); (b) it recommends `--path`, which K-a breaks silently (haiku). Item 7 and item 12 fix these.
- Warm does not fix `git show <commit>` (item 3): models believe it shows changes because the help says "shows the commit".
- Warm run variance (deepseek x2) was small on spellings (same verbs; differ in `--limit` vs `-n` and `log feature/x` vs `log main`), but the cold-vs-warm gap dwarfs it. Single run per other model.
