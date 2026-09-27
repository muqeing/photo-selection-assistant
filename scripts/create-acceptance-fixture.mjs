import { cp, mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";

// This uses intentionally tiny synthetic bytes. Never point this script at a
// customer directory: its only purpose is to prepare a repeatable acceptance
// fixture for the desktop application's native-directory workflow.
const root = await mkdtemp(join(tmpdir(), "photo-selector-acceptance-"));
const order = join(root, "验收订单");
const source = join(order, "原始照片");
const target = join(order, "照片成片", "待精修的原片");

await mkdir(join(source, "RAW"), { recursive: true });
await mkdir(join(source, "JPG"), { recursive: true });
await mkdir(join(source, "重号目录"), { recursive: true });
await mkdir(target, { recursive: true });

const files = {
  "RAW/IMG_01234.CR3": "raw-1234",
  "RAW/IMG_0781.CR3": "raw-781",
  "JPG/IMG_01234.JPG": "jpg-1234",
  "JPG/IMG_0781.JPG": "jpg-781",
  "重号目录/IMG_001234.JPG": "duplicate-1234",
};
for (const [relative, contents] of Object.entries(files)) {
  await writeFile(join(source, relative), contents);
}

// The duplicate target represents a prior successful run. The app must report
// it as "identical target skipped", never overwrite it.
await cp(join(source, "JPG/IMG_0781.JPG"), join(target, "IMG_0781.JPG"));

const hashes = Object.fromEntries(
  await Promise.all(
    Object.keys(files).map(async (relative) => [
      relative,
      createHash("sha256").update(await readFile(join(source, relative))).digest("hex"),
    ]),
  ),
);
const manifest = {
  root,
  source,
  defaultTarget: target,
  inputs: ["1234", "781"],
  expected: {
    "1234": "ambiguous: select RAW + JPG group; ignore leading zero",
    "781": "complete: RAW + JPG",
    identicalTarget: "IMG_0781.JPG",
  },
  sourceSha256: hashes,
};
await writeFile(join(root, "acceptance-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(JSON.stringify(manifest, null, 2));
