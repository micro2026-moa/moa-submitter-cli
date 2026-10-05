# moa-submitter

Submit your sources to the MOA 2026 Gemma-4 optimization competition.

## Install

```sh
cargo binstall moa-submitter-cli
```

## Use

```sh
moa-submitter login              # once; the token lasts 30 days
cd furiosa-opt-gemma4-12B        # your clone of the baseline
moa-submitter submit
moa-submitter submit --concurrency 4   # requests kept in flight during grading, default 1
moa-submitter status
```

`submit` finds the repository by walking up from the current directory, so it works from
anywhere inside your clone.

| Command | |
| --- | --- |
| `moa-submitter status` | your 20 most recent submissions, and your team's submissions in the last 24 hours |
| `moa-submitter status -n 50` | the 50 most recent instead |
| `moa-submitter status --all` | every submission you have made |
| `moa-submitter status <ID>` | status, score, and why it was not scored |
| `moa-submitter log <ID>` | that submission's log, split by stage |

## What gets uploaded

Everything under `src/` except `src/api/`.
