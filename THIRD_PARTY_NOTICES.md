# Third-party notices

The project license covers this project's own code and original icon. Dependencies retain their own licenses. This source publication does not vendor dependency source archives, OCR model weights, or compiled installers; the build downloads dependencies recorded by the lockfiles.

## JavaScript dependencies

Versions and license expressions below were read from the installed, lockfile-pinned package manifests. Full relevant upstream license texts and worker notices are retained in `licenses/`.

| Package | Version | Declared license | Scope |
| --- | --- | --- | --- |
| `@tauri-apps/api` | 2.11.1 | Apache-2.0 OR MIT | dependencies |
| `@tauri-apps/plugin-clipboard-manager` | 2.3.2 | MIT OR Apache-2.0 | dependencies |
| `@tauri-apps/plugin-dialog` | 2.7.2 | MIT OR Apache-2.0 | dependencies |
| `@tesseract.js-data/chi_sim` | 1.0.0 | MIT | dependencies |
| `@tesseract.js-data/eng` | 1.0.0 | MIT | dependencies |
| `tesseract.js` | 7.0.0 | Apache-2.0 | dependencies |
| `tesseract.js-core` | 7.0.0 | Apache-2.0 | dependencies |
| `@tauri-apps/cli` | 2.11.4 | Apache-2.0 OR MIT | devDependencies |
| `@types/node` | 22.19.7 | MIT | devDependencies |
| `typescript` | 7.0.2 | Apache-2.0 | devDependencies |
| `vite` | 8.1.5 | MIT | devDependencies |
| `vitest` | 4.1.10 | MIT | devDependencies |

## OCR worker and trained data

- Tesseract.js and Tesseract.js-core: Apache-2.0. Their license texts are included in `licenses/`.
- The worker also bundles buffer (MIT), ieee754 (BSD-3-Clause), regenerator-runtime (MIT), and zlib.js (MIT). Preserve the complete upstream `licenses/Tesseract-worker-third-party.txt`; the asset preparation script also copies the original sidecar beside `worker.min.js`.
- The `@tesseract.js-data/chi_sim` and `eng` npm manifests declare MIT. The actual `4.0.0_best_int` trained data are described by [naptha/tessdata](https://github.com/naptha/tessdata) as integerized Tessdata Best. The upstream [tessdata LICENSE](https://github.com/naptha/tessdata/blob/gh-pages/LICENSE) is Apache-2.0. This distinction is retained rather than treating the package metadata as a replacement for model-data licensing.
- The upstream Tessdata Apache license is included in `licenses/Tessdata-Apache-2.0.txt`. `scripts/copy-ocr-assets.mjs` copies notices and licenses into the generated OCR resource directory. Model weights are downloaded from the locked npm packages and are not committed to this repository.

## Rust direct dependencies

The following reflects the resolved direct dependencies, including platform-specific and development/build dependencies. The complete dependency graph remains defined by `src-tauri/Cargo.lock`; this table is not a full transitive license audit or a binary SBOM.

| Crate | Version | Declared license |
| --- | --- | --- |
| `base64` | 0.22.1 | MIT OR Apache-2.0 |
| `blake3` | 1.8.2 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception |
| `cap-fs-ext` | 4.0.2 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `cap-primitives` | 4.0.2 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `cap-std` | 4.0.2 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `chrono` | 0.4.45 | MIT OR Apache-2.0 |
| `crc32fast` | 1.5.0 | MIT OR Apache-2.0 |
| `fs2` | 0.4.3 | MIT/Apache-2.0 |
| `httpmock` | 0.7.0 | MIT |
| `image` | 0.25.5 | MIT OR Apache-2.0 |
| `image-webp` | 0.2.0 | MIT OR Apache-2.0 |
| `keyring` | 3.6.3 | MIT OR Apache-2.0 |
| `libc` | 0.2.189 | MIT OR Apache-2.0 |
| `log` | 0.4.33 | MIT OR Apache-2.0 |
| `reqwest` | 0.12.24 | MIT OR Apache-2.0 |
| `rusqlite` | 0.32.1 | MIT |
| `security-framework` | 3.6.0 | MIT OR Apache-2.0 |
| `serde` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_json` | 1.0.151 | MIT OR Apache-2.0 |
| `tauri` | 2.11.5 | Apache-2.0 OR MIT |
| `tauri-build` | 2.6.3 | Apache-2.0 OR MIT |
| `tauri-plugin-clipboard-manager` | 2.3.2 | Apache-2.0 OR MIT |
| `tauri-plugin-dialog` | 2.7.2 | Apache-2.0 OR MIT |
| `tauri-plugin-log` | 2.9.0 | Apache-2.0 OR MIT |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 |
| `thiserror` | 2.0.19 | MIT OR Apache-2.0 |
| `tokio` | 1.53.1 | MIT |
| `url` | 2.5.8 | MIT OR Apache-2.0 |
| `uuid` | 1.20.0 | Apache-2.0 OR MIT |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 |

The machine-readable direct-dependency inventory is in `docs/direct-dependency-licenses.json`. When distributing binaries, also collect notices for the full resolved transitive graph and any bundled native components. Keep upstream copyright notices and license texts with redistributed components.

## Project images

The application icon is generated from this project's original `assets/app-icon.svg`; see `assets/README.md`. Unused historical image fixtures with undocumented provenance are not included in the public source. Tests generate anonymous fixture bytes locally.
