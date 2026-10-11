
### T1 working tree changes
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git status` | 11 | WORKS |  |

### T2 short status
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git status -s` | 7 | USELESS-ERROR |  |
| `git status --json` | 3 | WORKS |  |
| `git status` | 1 | WORKS |  |

### T3 staged vs not
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git status` | 10 | WORKS |  |
| `git diff --staged` | 3 | WORKS |  |
| `git diff` | 3 | WORKS |  |

### T4 diff unstaged
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git diff` | 11 | WORKS |  |

### T5 diff staged
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git diff --staged` | 10 | WORKS |  |
| `git diff --cached` | 1 | USELESS-ERROR |  |

### T6 last 10 one line
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git log --oneline -10` | 3 | USELESS-ERROR |  |
| `git log --limit 10` | 3 | WORKS |  |
| `git log -n 10 --oneline` | 3 | USELESS-ERROR |  |
| `git log --oneline -n 10` | 1 | USELESS-ERROR |  |
| `git log -n 10` | 1 | WORKS |  |

### T7 file history
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git log -- README.md` | 5 | USELESS-ERROR | K-b |
| `git log --follow -- README.md` | 3 | USELESS-ERROR | K-b |
| `git log --oneline --follow -- README.md` | 1 | USELESS-ERROR | K-b |
| `git log README.md` | 1 | USELESS-ERROR |  |
| `git log --path README.md` | 1 | SILENT-WRONG | K-a |

### T8 who/when file
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git log -1 --format='%an %ad' -- Cargo.toml` | 3 | USELESS-ERROR | K-b |
| `git log --limit 1 -- Cargo.toml` | 2 | USELESS-ERROR | K-b |
| `git log --limit 1 -- Cargo.toml --json` | 1 | USELESS-ERROR | K-b |
| `git log -1 Cargo.toml` | 1 | USELESS-ERROR |  |
| `git log -n 1 Cargo.toml` | 1 | USELESS-ERROR |  |
| `git log -1 --format=%an,%ad -- Cargo.toml` | 1 | USELESS-ERROR | K-b |
| `git log --limit 1 --path Cargo.toml` | 1 | SILENT-WRONG | K-a |
| `git log -n 1 -- Cargo.toml` | 1 | USELESS-ERROR | K-b |
| `git log -1 --format='%an <%ae> %ad' --date=iso -- Cargo.toml` | 1 | USELESS-ERROR | K-b |

### T9 show commit
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git show abc1234` | 11 | SILENT-WRONG |  |
| `git diff --from abc1234~1 --to abc1234` | 1 | WORKS | quote |
| `git diff --from abc1234^ --to abc1234` | 1 | USEFUL-REFUSAL | quote |

### T10 list branches
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git branch` | 8 | WORKS |  |
| `git branch --list` | 2 | USELESS-ERROR |  |
| `git branch -a` | 1 | USELESS-ERROR |  |

### T11 current branch
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git branch --show-current` | 6 | USELESS-ERROR |  |
| `git branch` | 5 | WORKS |  |
| `git info` | 2 | WORKS |  |

### T12 feature/x not on main
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git log --oneline main..feature/x` | 4 | USELESS-ERROR |  |
| `git log feature/x` | 3 | SILENT-WRONG |  |
| `git log main` | 2 | SILENT-WRONG |  |
| `git log main..feature/x` | 2 | USELESS-ERROR |  |
| `git log main..feature/x --oneline` | 1 | USELESS-ERROR |  |
| `git log feature/x --json` | 1 | WORKS |  |
| `git log main --json` | 1 | WORKS |  |

### T13 diff main..feature/x
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git diff --from main --to feature/x` | 5 | WORKS |  |
| `git diff main..feature/x` | 3 | USEFUL-REFUSAL |  |
| `git diff main feature/x` | 3 | USEFUL-REFUSAL |  |

### T14 tags
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git tag` | 9 | WORKS |  |
| `git tag --list` | 2 | USELESS-ERROR |  |

### T15 repo root
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git rev-parse --show-toplevel` | 6 | USELESS-ERROR |  |
| `git info` | 5 | WORKS |  |
| `git rev-parse --is-inside-work-tree` | 1 | USELESS-ERROR |  |

### T16 tracked files
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git ls-files` | 6 | USELESS-ERROR |  |
| `git ls --recursive HEAD` | 3 | WORKS |  |
| `git ls HEAD --recursive` | 2 | WORKS |  |

### T17 file at HEAD~2
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git show HEAD~2:README.md` | 10 | WORKS | quote |
| `git show 'HEAD~2:README.md'` | 1 | WORKS | quote |

### T18 stage+commit file
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git add README.md` | 9 | WRITE-REFUSED |  |
| `git commit -m "update readme"` | 7 | WRITE-REFUSED |  |
| `git commit -m 'update readme'` | 2 | WRITE-REFUSED |  |
| `git status` | 1 | WORKS |  |

### T19 stage all+commit
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git add -A` | 7 | WRITE-REFUSED |  |
| `git commit -m "wip"` | 7 | WRITE-REFUSED |  |
| `git add .` | 2 | WRITE-REFUSED |  |
| `git commit -m 'wip'` | 2 | WRITE-REFUSED |  |
| `git status` | 1 | WORKS |  |

### T20 amend
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git commit --amend -m "better message"` | 7 | WRITE-REFUSED |  |
| `git commit --amend -m 'better message'` | 2 | WRITE-REFUSED |  |
| `git log --limit 1` | 1 | WORKS |  |

### T21 new branch
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git checkout -b topic` | 5 | WRITE-REFUSED |  |
| `git switch -c topic` | 4 | WRITE-REFUSED |  |
| `git branch` | 1 | WORKS |  |

### T22 unstage
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git restore --staged README.md` | 9 | WRITE-REFUSED |  |
| `git diff --staged -- README.md` | 1 | USELESS-ERROR | K-b |

### T23 discard
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git restore Cargo.toml` | 9 | WRITE-REFUSED |  |
| `git diff -- Cargo.toml` | 1 | USELESS-ERROR | K-b |

### T24 stash
| spelling | runs (of 11) | bin | tags |
|---|---|---|---|
| `git stash pop` | 9 | WRITE-REFUSED |  |
| `git stash` | 8 | WRITE-REFUSED |  |
| `git status` | 1 | WORKS |  |
| `git stash push` | 1 | WRITE-REFUSED |  |