# rustyfi benchmark results

**Clean rate (achievable):** 3/9 (33%) · **median errors:** 12.5 · **prompt-cache:** 15 errors

| repo | lang | expectation | verdict | errors | todos | files | secs |
|---|---|---|---|---|---|---|---|
| calculator | go | clean | 🟢 clean | 0 | 0 | 3/3 | 132 |
| prompt-cache | go | clean | 🟠 partial | 15 | 3 | 23/23 | 1382 |
| cobra | go | clean | 🟠 partial | 118 | 8 | 36/36 | 2155 |
| itsdangerous | python | clean | 🟢 clean | 0 | 1 | 15/15 | 361 |
| axios | javascript | partial | 🟠 partial | 36 | 12 | 222/222 | 2701 |
| paint | ruby | clean | 🟠 partial | 10 | 8 | 14/14 | 814 |
| emoji-java | java | partial | 🟢 clean | 0 | 0 | 13/13 | 523 |
| thc-hydra | c | impossible | 🟠 partial | 1 | 125 | 85/85 | 5976 |
| ky | typescript | clean | 🟠 partial | 162 | 1 | 52/52 | 1365 |
| clifx | csharp | partial | 🟠 partial | 138 | 77 | 115/115 | 7290 |

_impossible repos are shown but excluded from the clean-rate denominator._

