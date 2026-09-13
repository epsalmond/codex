# Artifact recovery census and auto-shake savings estimate (2026-09-12)

## Does anyone ever read an artifact back?

`scripts/omp-artifact-census.py` over every local oh-my-pi session (an oh-my-pi session store, default `~/.omp/agent/sessions`, path configurable; an oh-my-pi wrapper writes to the same store):

| metric | count |
|---|---|
| sessions scanned | 346 |
| sessions that shook | 9 |
| artifacts created by shake | 17 |
| artifacts read back (URI or file path) | 0 |
| same tool call repeated after a shake | 121, all status polling |

Together with zero reads in the nine shake-bench replays (`warm-2026-09-12.md`), recovery has never been used. omp #11365 (unbounded recovery) is wontfix upstream; the maintainer replay found zero artifact reads and an unclamped output-token budget as the real cause. The fork sends no output-token budget, so that failure is unreachable here.

Decision (fork commit ee79c53c42): `read_artifact` removed. Placeholder is `[shaken ~N tokens from <label>. original: <abs_path>]`; the model greps the file with its shell. Basis: no reachable way to disable the shell tool (the `Disabled` variant exists only for the /models wire schema, upstream 903b7774bc); landlock and seatbelt both permit reads of `$CODEX_HOME/artifacts`.

## What would auto-shake have saved?

`scripts/shake-savings-estimate.py` simulates the fork policy (60% global, 40% Astra, protect 16k, floor 400, min share 30%, min savings 4k, escalated pass) over unshaken Codex rollouts, tokens at bytes/4, credits via `pricing_lib.py`.

Last 24 h (window 872k): primary account 31 threads, 325M input tokens, 0 saved, none crossed a threshold. Second account 77 threads, 1,020M input tokens, 148M saved, 80 credits, one luna thread of 599 requests.

Busiest week, 2026-07-13 to 2026-07-20, primary account, run with the 272k window in effect then:

| metric | value |
|---|---|
| threads | 182 (152 would have fired at 60%) |
| requests | 94,395 |
| input tokens | 12.37B (sol 11.56B, Astra 0.76B, terra 0.05B) |
| tokens saved under policy | 2.04B |
| naive ceiling | 3.13B |
| credits | 179,654 actual, 24,574 saved |

Not modeled: compactions avoided by shaking first. Known error: bytes/4 tokenizer.

Follow-ups: gpt-5.6 pays a long-context penalty above 272k, so the family default became an absolute 160k threshold (fork 2856022ecc). The 1 pp per 10M input-token quota rate from the warm matrix is inconsistent with a 12.4B-token week, so quota metering is model-weighted or the plan differed; unresolved.
