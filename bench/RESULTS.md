# rustyfi benchmark results

**Clean rate (achievable):** 4/9 (44%) · **median errors:** 0.0 · **prompt-cache:** 179 errors

| repo | lang | expectation | verdict | errors | todos | files | secs |
|---|---|---|---|---|---|---|---|
| calculator | go | clean | 🟢 clean | 0 | 0 | 3/3 | 149 |
| prompt-cache | go | clean | 🟠 partial | 179 | 12 | 23/23 | 1446 |
| cobra | go | clean | 🟠 partial | 1 | 17 | 35/36 | 986 |
| itsdangerous | python | clean | 🟢 clean | 0 | 0 | 15/15 | 336 |
| axios | javascript | partial | 🟠 partial | 0 | 64 | 222/222 | 901 |
| paint | ruby | clean | 🟢 clean | 0 | 2 | 14/14 | 820 |
| emoji-java | java | partial | 🟢 clean | 0 | 0 | 13/13 | 651 |
| thc-hydra | c | impossible | 🟠 partial | 0 | 297 | 85/85 | 1600 |
| ky | typescript | clean | 🟠 partial | 127 | 1 | 52/52 | 953 |
| clifx | csharp | partial | 🟠 partial | 0 | 70 | 115/115 | 732 |

_impossible repos are shown but excluded from the clean-rate denominator._

