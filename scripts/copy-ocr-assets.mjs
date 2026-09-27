import { cp, mkdir, rm, writeFile } from "node:fs/promises";

const runtimeAssets = [
  "worker.min.js",
  "core/tesseract-core.wasm.js",
  "core/tesseract-core.wasm",
  "core/tesseract-core-lstm.wasm.js",
  "core/tesseract-core-lstm.wasm",
  "core/tesseract-core-simd.wasm.js",
  "core/tesseract-core-simd.wasm",
  "core/tesseract-core-simd-lstm.wasm.js",
  "core/tesseract-core-simd-lstm.wasm",
  "core/tesseract-core-relaxedsimd.wasm.js",
  "core/tesseract-core-relaxedsimd.wasm",
  "core/tesseract-core-relaxedsimd-lstm.wasm.js",
  "core/tesseract-core-relaxedsimd-lstm.wasm",
  "lang/chi_sim.traineddata.gz",
  "lang/eng.traineddata.gz",
];

await rm("public/ocr", { recursive: true, force: true });
await mkdir("public/ocr/core", { recursive: true });
await mkdir("public/ocr/lang", { recursive: true });
await cp("node_modules/tesseract.js/dist/worker.min.js", "public/ocr/worker.min.js");
// Keep the worker's bundled third-party notices beside the distributed asset.
await cp(
  "node_modules/tesseract.js/dist/worker.min.js.LICENSE.txt",
  "public/ocr/worker.min.js.LICENSE.txt",
);
await cp("licenses", "public/ocr/licenses", { recursive: true });
await cp("THIRD_PARTY_NOTICES.md", "public/ocr/THIRD_PARTY_NOTICES.md");
await cp("node_modules/tesseract.js-core", "public/ocr/core", {
  recursive: true,
  dereference: true,
  filter: (source) =>
    !source.endsWith(".map") &&
    !source.endsWith("README.md") &&
    !source.endsWith("package.json"),
});
await cp(
  "node_modules/@tesseract.js-data/chi_sim/4.0.0_best_int/chi_sim.traineddata.gz",
  "public/ocr/lang/chi_sim.traineddata.gz",
);
await cp(
  "node_modules/@tesseract.js-data/eng/4.0.0_best_int/eng.traineddata.gz",
  "public/ocr/lang/eng.traineddata.gz",
);
await writeFile("public/ocr/asset-manifest.json", `${JSON.stringify({ assets: runtimeAssets }, null, 2)}\n`);
