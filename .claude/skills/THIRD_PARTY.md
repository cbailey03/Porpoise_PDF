# Third-party skills

These Claude Code skills come from Matt Pocock's skills repository:
https://github.com/mattpocock/skills, commit `49dd158d1076134a641b33efb035946536778336`.

| Skill | Source path |
|---|---|
| `improve-codebase-architecture` | `skills/engineering/improve-codebase-architecture/` |
| `codebase-design` | `skills/engineering/codebase-design/` |
| `grilling` | `skills/productivity/grilling/` |
| `domain-modeling` | `skills/engineering/domain-modeling/` |

`improve-codebase-architecture` runs only when typed
(`/improve-codebase-architecture`); it calls the other three through the
Skill tool. Day-to-day development uses the superpowers skills.

## Changes from the source

- Converted to plain ASCII for this repo's rule: box-drawing diagrams and
  file trees redrawn with `+`, `-`, `|`, and `` ` ``; arrows written as
  `<-`, `->`, and `<->`; ellipses, dashes, `<=`, and middle dots replaced;
  the grilling round's emoji markers replaced by `**Q1**` and
  `Recommended:`. No other wording was changed.
- The Codex-only `agents/openai.yaml` files were left out.

To update, fetch the four folders at a newer commit, convert them the same
way, and change the commit above.

## License

```text
MIT License

Copyright (c) 2026 Matt Pocock

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
