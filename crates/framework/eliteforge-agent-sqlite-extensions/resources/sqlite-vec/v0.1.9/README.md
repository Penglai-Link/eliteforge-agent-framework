# sqlite-vec native resources

This directory vendors the sqlite-vec v0.1.9 loadable SQLite extension for optional SQLite vector operations.

- Upstream: https://github.com/asg017/sqlite-vec/releases/tag/v0.1.9
- Scope: all official `loadable-*` release assets for v0.1.9.
- Licenses: see `LICENSE-APACHE` and `LICENSE-MIT`

| Platform directory | Asset sha256 | Extracted file | Extracted sha256 |
| --- | --- | --- | --- |
| `android-aarch64` | `76f60d4d2d89d2e5070ef8f1868c52b140a10200dbe98b0c2ca7a4d02d483eaa` | `vec0.so` | `3083712f52da27a172469b34d1b82fe8388f2c76bc822b85ceaaed9f8e460c74` |
| `android-armv7a` | `637a4d38cbff2c46e296451381c25c062920455d03ddc1df955cfa0f7b5df3f0` | `vec0.so` | `35a19b53870192306f2f0b1dfc148101fa1f7e0409a65ea998078b390f514177` |
| `android-i686` | `d1a75768502e1ab050828e1e993833c622d81808233bba988c7f266607f63581` | `vec0.so` | `e218b3c4b1f7dd237b5e7becfe04a2522cb5c4b64773bd9ff2d7b386ca7f200b` |
| `android-x86_64` | `11e0b3db8b1386644966788c29c90d4f2f17689985b924e0ee57936d48e55cf7` | `vec0.so` | `783080adf3a90a45fd6c8dc110a7e8d050acea03767caa076152163a13a77366` |
| `ios-aarch64` | `3cb77b829cc42fe0544608790e19d87efd61076639bd8b78d68f4fefb8fb8561` | `vec0.dylib` | `d3552a43a98def3e343850183a7f88ba170002f67aa430c81e7a17e63c30e549` |
| `iossimulator-aarch64` | `7db1a8077ac496b79bb0a386ab6bfa5bd507cb45c9431ab644c69bf17f597070` | `vec0.dylib` | `86c8a7526e98f2aa26e1a95ab2e7b81f5a858b2bb07fef0cb22ba9e8844af751` |
| `iossimulator-x86_64` | `eb49248e616b0cedfd59d60d79bfa579c877b14118265d699a53dd0716b8ac48` | `vec0.dylib` | `e4597753c4e2ec07fc14488ce588811a4b5cc16e09a98a7d422d54f3223c0903` |
| `linux-aarch64` | `ea03d39541e478fab5974253c461e1cb5d77742f69e40cf96e3fad5bc309a37c` | `vec0.so` | `0b84cbd06418ca3040827deddd650539be05be0f657952426b926c8606217437` |
| `linux-x86_64` | `b959baa1d8dc88861b1edb337b8587178cdcb12d60b4998f9d10b6a82052d5d7` | `vec0.so` | `5923730861b86c707cca5602b5f91092f9e52a46706dbc6e269fd4bb9c4498e8` |
| `macos-aarch64` | `8282126333399ddfe98bbbcc7a1936e7252625aac49df056a98be602e46bfd29` | `vec0.dylib` | `193e480c50b59a55977d166f4aaf0e1bc8832d6963516e5950f39e4d2ce0b793` |
| `macos-x86_64` | `53ad76e400786515e2edcaed2f01271dda846316390b761fadbd2dcf56aa4713` | `vec0.dylib` | `d39d33d16d302d57440d9a9cdbe3cc095e8324aa96b979b5171f545a6346996d` |
| `windows-x86_64` | `51581189d52066b4dfc6631f6d7a3eab7dedc2260656ab09ca97ab3fb8165983` | `vec0.dll` | `fcf98662a7ad9dce394b96a88f91032047823831b951c76636787c312a6476e6` |

Resources are selected and loaded by the eliteforge-agent-sqlite-extensions Rust crate.
