import { createHash, randomUUID } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import { createReadStream } from "node:fs";
import {
  lstat,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { basename, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const platform = process.argv[2];
if (platform !== "macos" && platform !== "windows") {
  throw new Error("Usage: package-release.mjs <macos|windows>");
}

function git(...args) {
  return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
}

function requireCleanSource(expectedHead) {
  const currentHead = git("rev-parse", "HEAD");
  const dirty = git("status", "--porcelain=v1", "--untracked-files=all");
  if (currentHead !== expectedHead || dirty) {
    throw new Error("Source HEAD or worktree changed during release build.");
  }
}

const packageJson = JSON.parse(
  await readFile(resolve(root, "package.json"), "utf8"),
);
const tauriConfig = JSON.parse(
  await readFile(resolve(root, "src-tauri/tauri.conf.json"), "utf8"),
);
const version = packageJson.version;
const wixLanguage = tauriConfig?.bundle?.windows?.wix?.language;
const macosConfig = tauriConfig?.bundle?.macOS;
const macosSigningIdentity =
  process.env.APPLE_SIGNING_IDENTITY?.trim()
  || macosConfig?.signingIdentity?.trim();
const macosMinimumVersion = macosConfig?.minimumSystemVersion;
if (typeof version !== "string" || !/^\d+\.\d+\.\d+$/u.test(version)) {
  throw new Error("package.json must declare a semantic version.");
}
if (
  platform === "macos"
  && (
    typeof macosSigningIdentity !== "string"
    || macosSigningIdentity.length === 0
  )
) {
  throw new Error(
    "A macOS signing identity is required so the whole application bundle is sealed.",
  );
}
if (
  platform === "macos"
  && (
    typeof macosMinimumVersion !== "string"
    || !/^\d+(?:\.\d+){0,2}$/u.test(macosMinimumVersion)
    || Number(macosMinimumVersion.split(".")[0]) < 11
  )
) {
  throw new Error(
    "Apple Silicon releases must require macOS 11.0 or newer.",
  );
}
if (
  platform === "windows"
  && (
    typeof wixLanguage !== "string"
    || !/^[a-z]{2,3}(?:-[A-Z]{2})?$/u.test(wixLanguage)
  )
) {
  throw new Error("Tauri config must declare a valid WiX locale.");
}

const bundleDirectory = resolve(root, "src-tauri/target/release/bundle");
const artifacts = platform === "macos"
  ? [
      resolve(
        bundleDirectory,
        "dmg",
        `照片筛选助手_${version}_aarch64.dmg`,
      ),
    ]
  : [
      resolve(
        bundleDirectory,
        "nsis",
        `照片筛选助手_${version}_x64-setup.exe`,
      ),
      resolve(
        bundleDirectory,
        "msi",
        `照片筛选助手_${version}_x64_${wixLanguage}.msi`,
      ),
    ];
const sidecars = artifacts.map(path => `${path}.provenance.json`);
const sourceCommit = git("rev-parse", "HEAD");
if (!/^[0-9a-f]{40}$/iu.test(sourceCommit)) {
  throw new Error("Git HEAD is not a full source commit.");
}
requireCleanSource(sourceCommit);

// Only these exact, expected bundle files are removed. This guarantees a
// same-version artifact from an earlier build cannot be stamped again.
for (const path of [...artifacts, ...sidecars]) {
  await rm(path, { force: true });
}
const buildStartedAt = Date.now();
const bundles = platform === "macos" ? "app,dmg" : "nsis,msi";
const build = spawnSync(
  "pnpm",
  ["tauri", "build", "--bundles", bundles],
  {
    cwd: root,
    env: process.env,
    encoding: "utf8",
    shell: process.platform === "win32",
  },
);
if (build.stdout) process.stdout.write(build.stdout);
if (build.stderr) process.stderr.write(build.stderr);
if (build.error) throw build.error;
if (build.status !== 0) {
  throw new Error(`Release build failed with exit code ${build.status}.`);
}
requireCleanSource(sourceCommit);

async function sha256File(path) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}

const records = [];
for (const artifact of artifacts) {
  const details = await lstat(artifact).catch((error) => {
    if (error?.code === "ENOENT") {
      throw new Error(`Expected release artifact is missing after build: ${basename(artifact)}`);
    }
    throw error;
  });
  if (!details.isFile()) {
    throw new Error(`Release artifact is not a regular file: ${basename(artifact)}`);
  }
  if (details.mtimeMs < buildStartedAt) {
    throw new Error(`Release artifact was not freshly created: ${basename(artifact)}`);
  }
  records.push({
    artifact,
    sidecar: `${artifact}.provenance.json`,
    provenance: {
      schemaVersion: 1,
      filename: basename(artifact),
      sizeBytes: details.size,
      sha256: await sha256File(artifact),
      sourceCommit,
    },
  });
}
requireCleanSource(sourceCommit);

const staged = records.map(record => ({
  ...record,
  temporary: `${record.sidecar}.tmp-${process.pid}-${randomUUID()}`,
}));
const committed = [];
try {
  for (const record of staged) {
    await writeFile(
      record.temporary,
      `${JSON.stringify(record.provenance, null, 2)}\n`,
      { encoding: "utf8", flag: "wx" },
    );
  }
  requireCleanSource(sourceCommit);
  for (const record of staged) {
    await rename(record.temporary, record.sidecar);
    committed.push(record.sidecar);
  }
  requireCleanSource(sourceCommit);
} catch (error) {
  for (const record of staged) {
    await rm(record.temporary, { force: true });
  }
  for (const sidecar of committed) {
    await rm(sidecar, { force: true });
  }
  throw error;
}

for (const record of records) {
  console.log(`Packaged ${basename(record.artifact)} with verified provenance.`);
}
