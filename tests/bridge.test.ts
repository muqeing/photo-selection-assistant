import { describe, expect, it, vi } from "vitest";
import {
  encodeSessionInputMetadataHeader,
  decodeCandidatePreviewEnvelope,
  normalizeRawInputBytes,
  readCandidatePreviewRaw,
  readSessionInputRaw,
  recognizePersistedCloudInputs,
  saveSessionInputRaw,
} from "../src/bridge";

describe("persisted cloud recognition bridge", () => {
  it("passes the exact session and persisted IDs without JS image byte arrays", async () => {
    const draft = {
      detectedOrderId: null,
      numbers: [],
      rawText: "",
      method: "cloud" as const,
    };
    const cloud = vi.fn().mockResolvedValue(draft);

    await expect(
      recognizePersistedCloudInputs(
        "provider-1",
        "session-current",
        ["input-1", "input-2"],
        cloud,
      ),
    ).resolves.toEqual(draft);
    expect(cloud).toHaveBeenCalledWith(
      "provider-1",
      "session-current",
      ["input-1", "input-2"],
    );
  });
});

describe("raw candidate preview IPC", () => {
  it.each([
    [1, "image/jpeg"],
    [2, "image/png"],
    [3, "image/webp"],
  ] as const)("decodes tag %i as %s without a number array", (tag, mime) => {
    const bytes = new Uint8Array([tag, 10, 20, 30]);
    const preview = decodeCandidatePreviewEnvelope(bytes.buffer);
    expect(preview?.mime).toBe(mime);
    expect(preview?.bytes).toBeInstanceOf(Uint8Array);
    expect(Array.from(preview?.bytes ?? [])).toEqual([10, 20, 30]);
  });

  it("decodes the exact none envelope and rejects malformed raw responses", () => {
    expect(decodeCandidatePreviewEnvelope(new Uint8Array([0]))).toBeNull();
    for (const malformed of [
      new Uint8Array(),
      new Uint8Array([0, 1]),
      new Uint8Array([4, 1]),
      new Uint8Array([1]),
      [1, 2, 3],
    ]) {
      expect(() => decodeCandidatePreviewEnvelope(malformed)).toThrow(
        "候选预览",
      );
    }
  });

  it("reads the candidate preview as a raw ArrayBuffer response", async () => {
    const invokeRaw = vi.fn().mockResolvedValue(
      new Uint8Array([2, 7, 8]).buffer,
    );
    await expect(
      readCandidatePreviewRaw(
        "session-a",
        "12",
        "group-a",
        invokeRaw,
      ),
    ).resolves.toMatchObject({
      mime: "image/png",
      bytes: new Uint8Array([7, 8]),
    });
    expect(invokeRaw).toHaveBeenCalledWith("read_candidate_preview", {
      sessionId: "session-a",
      number: "12",
      groupId: "group-a",
    });
  });
});

describe("raw session input IPC", () => {
  it("sends the typed array as the whole invoke body and round-trips Chinese metadata", async () => {
    const invokeRaw = vi.fn().mockResolvedValue({
      id: "input-1",
      name: "客户手写编号.png",
      kind: "image",
      mime: "image/png",
      size: 3,
    });
    const bytes = new Uint8Array([1, 2, 3]);

    await saveSessionInputRaw(
      "550e8400-e29b-41d4-a716-446655440000",
      "客户手写编号.png",
      "image",
      bytes,
      invokeRaw,
    );

    expect(invokeRaw).toHaveBeenCalledOnce();
    const [command, body, options] = invokeRaw.mock.calls[0]!;
    expect(command).toBe("save_session_input");
    expect(body).toBe(bytes);
    expect(body).toBeInstanceOf(Uint8Array);
    expect(options).toEqual({
      headers: {
        "x-photo-input-metadata": expect.any(String),
      },
    });
    const encoded = options.headers["x-photo-input-metadata"]
      .replaceAll("-", "+")
      .replaceAll("_", "/");
    const metadata = JSON.parse(
      Buffer.from(encoded, "base64").toString("utf8"),
    );
    expect(metadata).toEqual({
      sessionId: "550e8400-e29b-41d4-a716-446655440000",
      name: "客户手写编号.png",
      kind: "image",
    });
  });

  it("normalizes raw ArrayBuffer and Uint8Array responses without number arrays", async () => {
    const direct = new Uint8Array([4, 5, 6]);
    expect(normalizeRawInputBytes(direct)).toBe(direct);
    expect(
      Array.from(normalizeRawInputBytes(new Uint8Array([7, 8]).buffer)),
    ).toEqual([7, 8]);
    expect(() => normalizeRawInputBytes([1, 2, 3])).toThrow(
      "原始二进制",
    );

    const invokeRaw = vi.fn().mockResolvedValue(
      new Uint8Array([9, 10]).buffer,
    );
    await expect(
      readSessionInputRaw("session-a", "input-a", invokeRaw),
    ).resolves.toEqual(new Uint8Array([9, 10]));
    expect(invokeRaw).toHaveBeenCalledWith("read_session_input", {
      sessionId: "session-a",
      inputId: "input-a",
    });
  });

  it("does not serialize input bytes through Array.from or JSON args", async () => {
    const source = encodeSessionInputMetadataHeader.toString()
      + saveSessionInputRaw.toString();
    expect(source).not.toContain("Array.from");
    const invokeRaw = vi.fn().mockResolvedValue({});
    const large = new Uint8Array(24 * 1024 * 1024);

    await saveSessionInputRaw(
      "550e8400-e29b-41d4-a716-446655440000",
      "large.png",
      "image",
      large,
      invokeRaw,
    );

    expect(invokeRaw.mock.calls[0]?.[1]).toBe(large);
  });
});
