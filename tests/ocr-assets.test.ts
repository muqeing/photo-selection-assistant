import { readFile, stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const ocrDirectory = resolve(dirname(fileURLToPath(import.meta.url)), "../public/ocr");

describe("offline OCR runtime assets", () => {
  it("publishes every worker-loadable core variant in its asset manifest", async () => {
    const manifest = JSON.parse(await readFile(resolve(ocrDirectory, "asset-manifest.json"), "utf8")) as {
      assets: string[];
    };

    expect(manifest.assets).toEqual(expect.arrayContaining([
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
    ]));

    const files = await Promise.all(manifest.assets.map(async (asset) => stat(resolve(ocrDirectory, asset))));
    expect(files.every((file) => file.isFile() && file.size > 0)).toBe(true);
  });
});
