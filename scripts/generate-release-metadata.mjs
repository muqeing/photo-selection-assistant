import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { mkdir, readdir, readFile, stat, writeFile } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const bundleDirectory = resolve(
  process.env.RELEASE_BUNDLE_DIRECTORY ??
    resolve(root, "src-tauri/target/release/bundle"),
);
const dmgDirectory = resolve(bundleDirectory, "dmg");
const output = process.argv[2]
  ? resolve(root, process.argv[2])
  : resolve(bundleDirectory, "release-artifacts.json");
const tauriConfigPath = resolve(
  process.env.RELEASE_TAURI_CONFIG ??
    resolve(root, "src-tauri/tauri.conf.json"),
);

function git(...args) {
  return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
}

const packageJson = JSON.parse(await readFile(resolve(root, "package.json"), "utf8"));
const version = packageJson.version;
if (typeof version !== "string" || !/^\d+\.\d+\.\d+$/.test(version)) {
  throw new Error("package.json must declare a semantic version.");
}
const tauriConfig = JSON.parse(await readFile(tauriConfigPath, "utf8"));
const wixLanguage = tauriConfig?.bundle?.windows?.wix?.language;
if (typeof wixLanguage !== "string") {
  throw new Error(
    "src-tauri/tauri.conf.json must declare bundle.windows.wix.language.",
  );
}
if (!/^[a-z]{2,3}(?:-[A-Z]{2})?$/.test(wixLanguage)) {
  throw new Error(
    "bundle.windows.wix.language must be a valid WiX locale such as zh-CN.",
  );
}

const dmgNames = (
  await readdir(dmgDirectory).catch((error) => {
    if (error?.code === "ENOENT") {
      return [];
    }
    throw error;
  })
).filter((name) => name.endsWith(".dmg"));
if (dmgNames.length > 1) {
  throw new Error(`Expected at most one DMG in ${dmgDirectory}; found ${dmgNames.length}.`);
}
const expectedDmgFilename = `照片筛选助手_${version}_aarch64.dmg`;
if (dmgNames.length === 1 && dmgNames[0] !== expectedDmgFilename) {
  throw new Error(
    `Expected the ${version} DMG named ${expectedDmgFilename}; found ${dmgNames[0]}.`,
  );
}

const sourceCommit = process.env.SOURCE_COMMIT ?? git("rev-parse", "HEAD");
if (!/^[0-9a-f]{40}$/i.test(sourceCommit)) {
  throw new Error("SOURCE_COMMIT must be a full 40-character Git commit id.");
}

async function verifiedProvenance(artifactPath, expectedFilename, bytes, details) {
  const sidecarPath = `${artifactPath}.provenance.json`;
  let provenance;
  try {
    provenance = JSON.parse(await readFile(sidecarPath, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT") {
      throw new Error(`Missing provenance sidecar for ${expectedFilename}.`);
    }
    throw new Error(`Invalid provenance sidecar for ${expectedFilename}.`);
  }
  const expectedKeys = [
    "filename",
    "schemaVersion",
    "sha256",
    "sizeBytes",
    "sourceCommit",
  ];
  if (
    !provenance
    || typeof provenance !== "object"
    || Array.isArray(provenance)
    || JSON.stringify(Object.keys(provenance).sort()) !== JSON.stringify(expectedKeys)
    || provenance.schemaVersion !== 1
  ) {
    throw new Error(`Invalid provenance sidecar schema for ${expectedFilename}.`);
  }
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  if (provenance.filename !== expectedFilename) {
    throw new Error(`Provenance filename mismatch for ${expectedFilename}.`);
  }
  if (provenance.sizeBytes !== details.size) {
    throw new Error(`Provenance sizeBytes mismatch for ${expectedFilename}.`);
  }
  if (provenance.sha256 !== sha256) {
    throw new Error(`Provenance sha256 mismatch for ${expectedFilename}.`);
  }
  if (provenance.sourceCommit !== sourceCommit) {
    throw new Error(`Provenance sourceCommit mismatch for ${expectedFilename}.`);
  }
  return provenance;
}

let macos;
if (dmgNames.length === 1) {
  const dmg = resolve(dmgDirectory, dmgNames[0]);
  const [bytes, details] = await Promise.all([readFile(dmg), stat(dmg)]);
  const provenance = await verifiedProvenance(
    dmg,
    expectedDmgFilename,
    bytes,
    details,
  );
  macos = {
    status: "built",
    architecture: "aarch64",
    artifact: {
      filename: basename(dmg),
      relativePath: `src-tauri/target/release/bundle/dmg/${basename(dmg)}`,
      sizeBytes: details.size,
      sha256: provenance.sha256,
      sourceCommit: provenance.sourceCommit,
    },
  };
} else {
  macos = {
    status: "not-built-on-this-host",
    architecture: "aarch64",
    expectedArtifact: {
      filename: expectedDmgFilename,
      relativePath: `src-tauri/target/release/bundle/dmg/${expectedDmgFilename}`,
      requiredRecordedFields: ["filename", "sizeBytes", "sha256", "sourceCommit"],
    },
  };
}

const metadata = {
  schemaVersion: 1,
  generatedBy: "node scripts/generate-release-metadata.mjs",
  version,
  sourceCommit,
  macos,
  windows: {
    status: "not-built-on-this-host",
    architecture: "x64",
    expectedArtifacts: [
      {
        kind: "nsis",
        filename: `照片筛选助手_${version}_x64-setup.exe`,
        relativePath: `src-tauri/target/release/bundle/nsis/照片筛选助手_${version}_x64-setup.exe`,
        requiredRecordedFields: ["filename", "sizeBytes", "sha256", "sourceCommit"],
      },
      {
        kind: "msi",
        filename: `照片筛选助手_${version}_x64_${wixLanguage}.msi`,
        relativePath: `src-tauri/target/release/bundle/msi/照片筛选助手_${version}_x64_${wixLanguage}.msi`,
        requiredRecordedFields: ["filename", "sizeBytes", "sha256", "sourceCommit"],
      },
    ],
  },
};

await mkdir(dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(metadata, null, 2)}\n`);
console.log(`Wrote ${output}`);
