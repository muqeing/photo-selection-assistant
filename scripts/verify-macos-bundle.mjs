import { execFileSync } from "node:child_process";
import { readFile, stat } from "node:fs/promises";
import { resolve } from "node:path";

if (process.platform !== "darwin") throw new Error("This smoke check must run on macOS.");

const root = resolve(import.meta.dirname, "..");
const app = resolve(root, "src-tauri/target/release/bundle/macos/照片筛选助手.app");
const executable = resolve(app, "Contents/MacOS/app");
const infoPlist = resolve(app, "Contents/Info.plist");
const packageManifest = JSON.parse(
  await readFile(resolve(root, "package.json"), "utf8"),
);
const expectedInfo = {
  CFBundleIdentifier: "top.muliai.photo-selection-assistant",
  CFBundleShortVersionString: packageManifest.version,
  CFBundleExecutable: "app",
};

for (const [key, expected] of Object.entries(expectedInfo)) {
  const actual = execFileSync("plutil", ["-extract", key, "raw", "-o", "-", infoPlist], { encoding: "utf8" }).trim();
  if (actual !== expected) throw new Error(`${key}: expected ${expected}, got ${actual}`);
}

const binary = await readFile(executable);
for (const asset of ["/ocr/worker.min.js", "/ocr/core", "/ocr/lang", "chi_sim.traineddata.gz", "eng.traineddata.gz"]) {
  if (!binary.includes(Buffer.from(asset))) throw new Error(`Packaged executable does not contain OCR asset reference: ${asset}`);
}
await stat(app);
console.log("Bundle metadata and embedded OCR asset references: PASS");
console.log("Launch is deliberately not performed: it can create per-user app state and requires a valid install/signature gate.");
