import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const root = resolve(import.meta.dirname, "..");

describe("Tauri invoke bridge contract", () => {
  it("prepares ignored OCR runtime assets before every test run", async () => {
    const packageJson = JSON.parse(
      await readFile(resolve(root, "package.json"), "utf8"),
    );

    expect(packageJson.scripts.pretest).toBe("pnpm prepare:ocr");
  });

  it("keeps every declared application version synchronized at 0.1.24", async () => {
    const [packageJson, cargoToml, tauriConfig] = await Promise.all([
      readFile(resolve(root, "package.json"), "utf8"),
      readFile(resolve(root, "src-tauri/Cargo.toml"), "utf8"),
      readFile(resolve(root, "src-tauri/tauri.conf.json"), "utf8"),
    ]);
    const packageVersion = JSON.parse(packageJson).version;
    const cargoVersion = cargoToml.match(
      /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
    )?.[1];
    const tauriVersion = JSON.parse(tauriConfig).version;

    expect([packageVersion, cargoVersion, tauriVersion]).toEqual([
      "0.1.24",
      "0.1.24",
      "0.1.24",
    ]);
  });

  it("registers every frontend invoke command in the desktop handler", async () => {
    const [bridge, app] = await Promise.all([
      readFile(resolve(root, "src/bridge.ts"), "utf8"),
      readFile(resolve(root, "src-tauri/src/lib.rs"), "utf8"),
    ]);
    const invoked = [...bridge.matchAll(/invoke(?:<[^>]+>)?\(\s*"([a-z_]+)"/g)].map((match) => match[1]);
    const registered = [...app.matchAll(/commands::([a-z_]+),/g)].map((match) => match[1]);

    expect(invoked).not.toHaveLength(0);
    expect(registered).toEqual(expect.arrayContaining(invoked));
  });

  it("lets the webview receive HTML5 file drop events", async () => {
    const config = JSON.parse(
      await readFile(resolve(root, "src-tauri/tauri.conf.json"), "utf8"),
    );

    expect(config.app.windows[0].dragDropEnabled).toBe(false);
  });

  it("uses a least-privilege production CSP that permits local IPC, OCR WASM, and blob previews", async () => {
    const config = JSON.parse(
      await readFile(resolve(root, "src-tauri/tauri.conf.json"), "utf8"),
    );
    const csp = config.app.security.csp
      .split(";")
      .map((directive: string) => directive.trim())
      .filter(Boolean);

    expect(csp).toEqual([
      "default-src 'self'",
      "script-src 'self' 'wasm-unsafe-eval'",
      "worker-src 'self'",
      "img-src 'self' blob:",
      "connect-src 'self' ipc: http://ipc.localhost",
    ]);
    expect(config.app.security.csp).not.toContain("'unsafe-eval'");
    expect(config.app.security.csp).not.toContain("'unsafe-inline'");
    expect(
      config.app.security.csp.replace("http://ipc.localhost", ""),
    ).not.toMatch(/https?:/);
    expect(config.app.security.csp).not.toContain("*");
    expect(config.app.security.csp).not.toContain("worker-src 'self' blob:");
    expect(config.app.security.csp).not.toContain("img-src 'self' blob: data:");
    expect(config.app.security.csp).not.toContain("connect-src 'self' ipc: blob:");
    expect(config.app.security.csp).not.toContain("connect-src 'self' ipc: data:");
  });

  it("keeps frontend and Rust recognition batch limits synchronized", async () => {
    const [frontend, rust] = await Promise.all([
      readFile(resolve(root, "src/recognition-limits.ts"), "utf8"),
      readFile(resolve(root, "src-tauri/src/providers.rs"), "utf8"),
    ]);
    const value = (source: string, name: string) =>
      source.match(new RegExp(
        `^\\s*(?:pub\\(crate\\)\\s+)?(?:export\\s+)?const\\s+${name}[^=]*=\\s*([^;]+);`,
        "m",
      ))?.[1]
        ?.replaceAll("_", "")
        .replaceAll(" ", "");

    expect(value(frontend, "MAX_RECOGNITION_IMAGES")).toBe("12");
    expect(value(rust, "MAX_RECOGNITION_IMAGES")).toBe("12");
    expect(value(frontend, "MAX_RECOGNITION_BATCH_BYTES")).toBe("48*1024*1024");
    expect(value(rust, "MAX_RECOGNITION_BATCH_BYTES")).toBe("48*1024*1024");
  });

  it("uses persisted input IDs rather than JS image byte arrays for cloud IPC", async () => {
    const [bridge, recognition] = await Promise.all([
      readFile(resolve(root, "src/bridge.ts"), "utf8"),
      readFile(resolve(root, "src/recognition.ts"), "utf8"),
    ]);

    expect(bridge).toContain("sessionId: string");
    expect(bridge).toContain("inputIds: string[]");
    expect(bridge).not.toContain("images: number[][]");
    expect(recognition).not.toContain("Promise.all(images");
    expect(recognition).not.toContain("Array.from(new Uint8Array");
  });

  it("keeps session input bytes raw in both IPC directions", async () => {
    const [bridge, app, commands] = await Promise.all([
      readFile(resolve(root, "src/bridge.ts"), "utf8"),
      readFile(resolve(root, "src/app.ts"), "utf8"),
      readFile(resolve(root, "src-tauri/src/commands.rs"), "utf8"),
    ]);

    expect(bridge).toContain('invokeRaw("save_session_input", bytes');
    expect(bridge).toContain('"x-photo-input-metadata"');
    expect(bridge).not.toContain("bytes: number[]");
    expect(app).not.toContain("Array.from(new Uint8Array");
    expect(commands).toContain("request: tauri::ipc::Request<'_>");
    expect(commands).toContain("tauri::ipc::InvokeBody::Raw(bytes)");
    expect(commands).toContain("tauri::ipc::Response::new(read_session_input_inner");
    expect(commands).not.toContain("struct SessionInputContent");
  });

  it("keeps candidate preview payloads out of JSON serialization", async () => {
    const [bridge, app, types, commands] = await Promise.all([
      readFile(resolve(root, "src/bridge.ts"), "utf8"),
      readFile(resolve(root, "src/app.ts"), "utf8"),
      readFile(resolve(root, "src/types.ts"), "utf8"),
      readFile(resolve(root, "src-tauri/src/commands.rs"), "utf8"),
    ]);

    expect(bridge).toContain('invokeRaw("read_candidate_preview"');
    expect(bridge).toContain("decodeCandidatePreviewEnvelope");
    expect(types).toContain("bytes: Uint8Array<ArrayBuffer>");
    expect(app).toContain("new Blob([preview.bytes]");
    expect(app).not.toContain("new Uint8Array(preview.bytes)");
    expect(commands).toContain(
      "Result<tauri::ipc::Response, CommandError>",
    );
    expect(commands).toContain(
      "tauri::ipc::Response::new(candidate_preview_envelope",
    );
    expect(commands).not.toContain("struct CandidatePreview");
  });
});
