# Third-Party Notices

## rv64.js

The Lish emulator started from
[`ibuildthecloud/rv64.js`](https://github.com/ibuildthecloud/rv64.js) at commit
`96aa93896e7bb6fa561d1f977c9bf23cd909a100`.

Copyright (c) 2026 rv64.js contributors

The source is available under the MIT License. See [LICENSE](LICENSE) and
[UPSTREAM.md](UPSTREAM.md).

## TinyEMU Software Floating Point

`crates/rv64-core/src/softfp.rs` is a Rust port of the TinyEMU software
floating-point implementation. Lish does not vendor the TinyEMU source tree.

Copyright (c) 2016-2017 Fabrice Bellard

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

## libslirp

The native network host links to libslirp. Lish does not vendor libslirp.

- Project: <https://gitlab.freedesktop.org/slirp/libslirp>
- License: BSD-3-Clause

Binary distributions must include the notices required by the exact libslirp
version that the application ships.
