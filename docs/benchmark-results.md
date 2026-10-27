# Teammate benchmark results

These results evaluate an agent completing tasks through evolving MCP tools. The experiment
changes the tool information available to the agent. It does not train or update the model's
weights. The project's question is whether selectively updating stale tool knowledge preserves
task performance with less unnecessary adaptation.

The Rust TUI/harness provides the experiment infrastructure. The teammate's Python work
implements and runs the comparison, collects trajectories, and asks an LLM judge to score them.
Its implementation differs from the Rust strategies and live canary; see
[integration boundaries](../benchmark/INTEGRATION.md#integration-boundaries).

## What the numbers measure

All three reported metrics are LLM judge ratings from 1 to 10, with higher scores preferred.
The judge receives the task, tool descriptions, recorded tool-use trajectory and final answer.

| Metric | What the judge compares | Meaning of a higher score |
|---|---|---|
| Task fulfillment | The user's explicit requirements versus the recorded actions and answer | More requirements appear to have been completed correctly |
| Grounding | Factual claims in the final answer versus tool outputs | More claims are supported by the available evidence |
| Planning and efficiency | The tool-call sequence versus calls needed to complete the task | Fewer redundant, unnecessary or unchanged retry calls |

The rubric maps roughly 90 to 100% completion/support to scores 9 to 10, 70 to 80% to
7 to 8, 40 to 60% to 4 to 6, and 10 to 30% to 1 to 3. Its efficiency rubric gives 9 to 10
when fewer than 10% of calls are redundant and 1 to 3 when more than 60% are redundant.
These are rubric-guided judgments, not directly measured binary success rates. A task
fulfillment score of 6.120 does not mean that 61.2% of tasks passed an independent verifier.

Each of the 20 tasks has one saved agent trajectory per strategy and version. Each trajectory
has five valid judge assessments, with the order of the rubric dimensions shuffled. The
five ratings are averaged per task, then the 20 task means are averaged. Thus each headline
score summarizes 100 ratings, but only 20 saved task trajectories. Across the nine
conditions there are 180 trajectories and 900 judge assessments.

The scoring reference is the rubric in
[`eval_error_prompt.py`](../benchmark/src/custom/prompt/eval_error_prompt.py).
The experimental comparator is the **no-update baseline on the same server version**.
The original version also shows each strategy's starting performance before evolution.
There is no demonstrated pass threshold, human-labelled gold score, or matched leaderboard
comparison for this 20-task pilot. The upstream README leaderboard uses a different experiment
and must not be treated as a direct comparison.

## Verified results

Original, iteration 3 and iteration 5 are MCP server evolution versions, not model training
epochs. The agent model is recorded under `gpt-6-luna` artifact directories. The original
judge files do not record enough model/version metadata to independently establish the exact
judge configuration for every call.

| Strategy | Version | Task fulfillment | Grounding | Planning and efficiency |
|---|---|---:|---:|---:|
| No update | Original | 6.120 | 8.990 | 8.420 |
| No update | Iteration 3 | 7.090 | 8.540 | 8.320 |
| No update | Iteration 5 | 6.620 | 9.110 | 8.350 |
| Selective patch | Original | 6.400 | 8.530 | 9.000 |
| Selective patch | Iteration 3 | 6.480 | 8.370 | 8.380 |
| Selective patch | Iteration 5 | 6.710 | 8.770 | 8.390 |
| Full rewrite | Original | 6.490 | 8.910 | 8.390 |
| Full rewrite | Iteration 3 | 5.650 | 8.470 | 9.020 |
| Full rewrite | Iteration 5 | 6.230 | 8.660 | 8.390 |

All 27 reported means reproduce from the selected raw judgments. The exact mix of first-run
and rerun folders follows [`TEAM_HANDOFF.md`](../benchmark/TEAM_HANDOFF.md). The audit checks
task IDs, answer/trajectory equality between ANSWER and EVAL, each raw score, stored task
means and the reported means. [JSON evidence](../benchmark/reports/verified-results.json)
records the selected paths and their SHA-256 hashes.

## ECS: performance and consistency across versions

Evolutionary Competency Score combines task fulfillment with stability. For each task:

```text
task ECS = mean(TF_original, TF_iter3, TF_iter5)
           - population_standard_deviation(TF_original, TF_iter3, TF_iter5)

overall ECS = mean(task ECS across the 20 aligned tasks)
```

Grounding and planning efficiency do not enter this formula. The standard-deviation penalty
reduces scores for variation in either direction, including improvements. It is not a pure
measure of regression or recovery.

| Strategy | Recomputed ECS |
|---|---:|
| Selective patch | 5.739725 |
| No update | 5.643489 |
| Full rewrite | 5.305757 |

Selective patch leads ECS by 0.096 over no update and 0.434 over full rewrite. It has the
highest iteration-5 task fulfillment, 6.710, compared with 6.620 and 6.230. No update has
the highest iteration-3 task fulfillment, 7.090. Full rewrite's iteration-3 efficiency is
highest at 9.020 while its task fulfillment is lowest at 5.650. Efficient tool use can still
leave a task incomplete.

These are mixed, weakly positive results for the selective-patch hypothesis. Selective patch
has the best ECS, but its lead over no update is only 0.096. It trails no update on iteration-3
task fulfillment by 0.610 and leads by only 0.090 on iteration 5. The data support a cautious
statement that selective patch was competitive and slightly more stable under this ECS formula.
They do not establish that it reliably improves task completion or that the difference is
statistically meaningful. These scores are not pass/fail grades; task-fulfillment values around
5.6 to 7.1 are midrange judge ratings on this rubric, while grounding and efficiency are higher.
No validated pass threshold exists.

A supported report statement is: "On this 20-task pilot, selective patch achieved the highest
ECS and iteration-5 task fulfillment, while no update performed best on iteration-3 task
fulfillment." The small advantage does not establish statistical significance or consistent
superiority across tasks and repeated agent runs.

## Adaptation cost

The selective patch audits record adaptation work only, excluding task execution and judging.

| Version | Tasks with changes | Model calls | Input tokens | Output tokens | Adaptation time |
|---|---:|---:|---:|---:|---:|
| Original | 0 | 0 | 0 | 0 | 0.199 s |
| Iteration 3 | 8 | 11 | 4,444 | 671 | 18.818 s |
| Iteration 5 | 11 | 19 | 7,616 | 1,433 | 49.529 s |

The evolved stages total 30 adaptation calls, 12,060 input tokens and 2,104 output tokens.
Dollar cost is unknown because no pricing was recorded for those calls. These records do not
support a complete dollar-cost comparison with no update or full rewrite, or a claim about
total experiment cost. Per-task audits supply the totals; resumed-run summary files can omit
previously completed cases.

## Limits that matter for the report

- The judge prompt conflicts with itself. Its independence instructions say redundant calls
  should not lower task fulfillment, but its later strictness example treats a correctly
  completed task with a redundant call as zero-percent perfect. This can affect scores.
  The historical rubric is preserved. A corrected rubric needs a separately labelled evaluation.
- Five judgments of the same trajectory do not measure variability across five agent runs.
  Original-stage scores also differ across strategies even though selective adaptation made
  no changes there. Run variation, judge variation and run conditions can contribute.
- The agent stores tool responses truncated to 500 characters. The judge also applies its
  own truncation and removes intermediate assistant text. It has limited execution evidence.
- Historical judging fetched descriptions from whichever localhost MCP instance was active.
  The saved artifacts do not prove which live tool listing each judgment used. New local
  judging uses saved or explicitly supplied tool context and records provenance.
- No separate state-based completion verifier is present. A normal stop or a forced final
  answer is not proof that an external action succeeded. Two selected trajectories record
  forced finalization: selective original and selective iteration 5, one task each.
- All 60 selective audits pass the unchanged-card hash check. That supports preservation
  of recorded tool cards, not guaranteed behavioral retention. Tasks 5 and 8 also contain
  three unresolved tool references absent from both snapshots.
- Python's schema argument probes do not execute tools or test server state. These results
  cannot establish that the Rust live canary or completion verification ran.

The next combined experiment needs aligned stage/task definitions, the server bundle and
reset procedure, the Rust runner/event/budget adapter, and a frozen judge configuration.
Repeated agent executions and uncertainty estimates are needed before stronger comparative
claims. Those requirements do not prevent analysis of the historical pilot above.

To improve the evidence, keep the historical scores fixed and run a new, clearly labelled
experiment: use the same tasks and live server versions for every strategy, freeze model and
decoding settings, resolve the judge rubric contradiction, add an independent state-based
completion check, and repeat agent runs per task. Report paired task-level differences with
uncertainty intervals. Use a development subset to diagnose stale-schema, missing-tool,
parameter, and retention failures; freeze any adaptation changes before evaluating a held-out
subset. Changing the rubric or method requires new scores and cannot be presented as improving
the historical results.

## Live Rust-to-Python experiment

A separate experiment ran the Rust strategies end to end against the real `original`, `iter3`
and `iter5` servers (Early, Middle, Late), with gpt-6-luna as agent, judge and adaptation model.
KBs are the live tool listings (original is byte-identical to the teammate's frozen snapshot).
All 180 task runs completed with status ok. It must not be pooled with the historical table.
Full tables: [`benchmark/reports/live/summary.md`](../benchmark/reports/live/summary.md).

| Strategy | Early TF | Middle TF | Late TF | ECS |
|---|---:|---:|---:|---:|
| No update | 6.660 | 6.850 | 7.130 | 6.093 |
| Selective patch | 6.540 | 6.520 | 7.020 | 6.121 |
| Full rewrite | 6.210 | 7.250 | 7.380 | 6.208 |

Paired task-level ECS differences (10,000-sample bootstrap, seed 0): selective minus no update
+0.028 (95% CI [-0.362, 0.425]); selective minus full rewrite -0.088 ([-0.798, 0.632]); no update
minus full rewrite -0.116 ([-0.895, 0.611]). All intervals include zero. At Early, all three
strategies ran with the identical original KB, yet mean TF spans 6.210 to 6.660, so run-to-run
variation exceeds the strategy differences.

Adaptation cost (rate-card estimates): selective patch 5 calls / USD 0.013 at Middle and 7 / 0.022
at Late; full rewrite 39 / 0.031 and 39 / 0.033, plus 40 / 0.032 at Early, where its candidate was
discarded. Every promoted candidate passed 3 targeted and 1 control canary probe. Total live
spend was about USD 1.41.

Server state was not reset between runs. Task 40's agent appended 12 notes to
`anotation_path/markdown/work.md` across its nine runs, and other tasks created task-list and log
files under `anotation_path/`. These are archived in `benchmark/reports/live/tool-side-effects/`,
and the imported dataset was restored (`verify_import.py` passes). Stateful tasks are confounded
by this accumulation.
