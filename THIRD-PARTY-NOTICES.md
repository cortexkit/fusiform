# Third-party notices

## models.dev catalog

The following files contain data from the models.dev catalog
(<https://github.com/anomalyco/models.dev>, published at
<https://models.dev/api.json>):

- `crates/fusiform-module/data/models-dev-seed.json`: a verbatim copy of
  `https://models.dev/api.json`, written by `scripts/refresh-seed.sh` and
  compiled into the `ck-fusiform` binary as its bootstrap snapshot. Its fetch
  time, URL and SHA-256 are recorded in
  `crates/fusiform-module/data/models-dev-seed.meta.json`.
- `crates/fusiform-core/fixtures/models-dev-excerpt.json`: a test fixture in
  the models.dev document format, containing excerpts of catalog data.
- `crates/fusiform-module/fixtures/served-payloads.json`: a test fixture of
  fusiform's served responses, which carry facts from the catalog.

The models.dev catalog is distributed under the following license:

```text
MIT License

Copyright (c) 2025 models.dev

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
