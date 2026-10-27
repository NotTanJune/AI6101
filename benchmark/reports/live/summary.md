# Live run summary

| Strategy | Stage | TF | Grounding | PE |
|---|---|---:|---:|---:|
| no_update | early | 6.660 | 8.270 | 8.340 |
| no_update | middle | 6.850 | 8.510 | 7.900 |
| no_update | late | 7.130 | 8.320 | 7.670 |
| selective_patch | early | 6.540 | 8.450 | 8.670 |
| selective_patch | middle | 6.520 | 8.430 | 8.160 |
| selective_patch | late | 7.020 | 8.570 | 8.390 |
| full_rewrite | early | 6.210 | 8.550 | 8.050 |
| full_rewrite | middle | 7.250 | 8.810 | 8.890 |
| full_rewrite | late | 7.380 | 9.030 | 7.790 |

| Strategy | ECS | tasks |
|---|---:|---:|
| no_update | 6.0926 | 20 |
| selective_patch | 6.1205 | 20 |
| full_rewrite | 6.2082 | 20 |

| Comparison | mean diff | W/L/T | 95% CI |
|---|---:|---|---|
| selective_patch - no_update | +0.028 | 11/7/2 | [-0.362, 0.425] |
| selective_patch - full_rewrite | -0.088 | 9/9/2 | [-0.798, 0.632] |
| no_update - full_rewrite | -0.116 | 7/12/1 | [-0.895, 0.611] |

| Strategy | Stage | Adaptation | Canary | Adapt calls | In tok | Out tok | Adapt USD | Task+judge USD |
|---|---|---|---|---:|---:|---:|---:|---:|
| no_update | early | unchanged | {'not_applicable': 1} | 0 | 0 | 0 | 0.0000 | 0.1373 |
| no_update | middle | unchanged | {'not_applicable': 1} | 0 | 0 | 0 | 0.0000 | 0.1350 |
| no_update | late | unchanged | {'not_applicable': 1} | 0 | 0 | 0 | 0.0000 | 0.1371 |
| selective_patch | early | blocked | {'inconclusive': 1} | 0 | 0 | 0 | 0.0000 | 0.1284 |
| selective_patch | middle | promoted | {'pass': 4} | 5 | 52102 | 15150 | 0.0128 | 0.1402 |
| selective_patch | late | promoted | {'pass': 4} | 7 | 80619 | 27276 | 0.0217 | 0.1311 |
| full_rewrite | early | blocked | {'inconclusive': 1} | 40 | 110679 | 42646 | 0.0324 | 0.2012 |
| full_rewrite | middle | promoted | {'pass': 4} | 39 | 108563 | 40640 | 0.0312 | 0.1320 |
| full_rewrite | late | promoted | {'pass': 4} | 39 | 113947 | 43902 | 0.0333 | 0.1342 |
