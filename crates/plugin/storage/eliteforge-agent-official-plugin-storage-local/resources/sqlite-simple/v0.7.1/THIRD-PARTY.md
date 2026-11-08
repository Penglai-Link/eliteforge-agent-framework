# SQLite simple third-party resources

The bundled native libraries and dictionaries come from the sqlite-simple v0.7.1 release. `PROVENANCE.json` records each release asset, archive member, size and SHA-256. The parent `SHA256SUMS` covers the files included in this repository.

- [sqlite-simple v0.7.1](https://github.com/wangfenjin/simple/tree/v0.7.1): MIT or GPL-3.0-or-later; this project chooses the MIT option. See `LICENSE-MIT-OR-GPL`.
- Its [build definition](https://github.com/wangfenjin/simple/blob/v0.7.1/src/CMakeLists.txt) pins [cppjieba](https://github.com/yanyiwu/cppjieba/tree/194c144d8b5ed1baf3190d07c5226e804454ab47) for the Jieba implementation and dictionaries. Its MIT license is in `LICENSE-CPPJIEBA`. All nine bundled dictionary files match the SHA-256 of the corresponding files at that pinned revision.
- The upstream [pinyin data](https://github.com/wangfenjin/simple/blob/v0.7.1/contrib/pinyin.txt) identifies [pinyin-data](https://github.com/mozillazg/pinyin-data) as its source. Its MIT license is in `LICENSE-PINYIN-DATA`. The upstream build embeds that data in the library.

These notices document identified components; they are not a complete audit of every dependency statically linked into every upstream release binary. Before distributing binaries, verify the upstream build configuration, transitive components, and any additional notices against each platform release.
