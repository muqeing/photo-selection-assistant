import { createWorker, type Worker } from "tesseract.js";
import { bridge } from "./bridge";
import { validateRecognitionImageBatch } from "./recognition-limits";
import type { AppSettings, PhotoNumber, RecognitionDraft } from "./types";

export type RecognitionStage = "cloud" | "offline" | "fallback";

class StaleRecognitionError extends Error {}

function requireCurrent(isCurrent: () => boolean) {
  if (!isCurrent()) throw new StaleRecognitionError("识别任务已失效");
}

function canonicalNumber(digits: string) {
  return digits.replace(/^0+/, "") || "0";
}

export function parseRecognizedText(
  text: string,
  confidence?: number,
  method: RecognitionDraft["method"] = "text",
): RecognitionDraft {
  const normalizedConfidence = confidence == null
    ? null
    : Number.isFinite(confidence)
      ? Math.max(0, Math.min(1, confidence > 1 ? confidence / 100 : confidence))
      : 0;
  const numbers: PhotoNumber[] = [];
  const seen = new Set<string>();
  let detectedOrderId: string | null = null;

  for (const line of text.split(/\r?\n/).map((value) => value.trim()).filter(Boolean)) {
    const order = line.match(
      /(?:订单(?:号)?|order\b\s*(?:(?:id|no\.?|number)\b)?)\s*[:：]?\s*([A-Za-z0-9_-]+)/i,
    );
    let candidateLine = line;
    if (order) {
      detectedOrderId ??= order[1];
      const start = order.index ?? 0;
      candidateLine = `${line.slice(0, start)} ${line.slice(start + order[0].length)}`;
    }

    const candidates = candidateLine
      .replace(/\b\d{4}[-/]\d{1,2}[-/]\d{1,2}\b|\b\d+(?:\.\d+)+\b|[A-Za-z0-9]+(?:_[A-Za-z0-9]+){2,}/g, " ")
      .match(/(?<![A-Za-z0-9_.-])(?:[A-Za-z]+[_-]?\d{2,}|\d{2,})(?:\.[A-Za-z][A-Za-z0-9]{0,7})?(?![A-Za-z0-9_.-])/g) ?? [];

    for (const original of candidates) {
      const digits = original.replace(/\.[A-Za-z][A-Za-z0-9]{0,7}$/, "").match(/(\d+)$/)?.[1];
      if (!digits) continue;
      const canonical = canonicalNumber(digits);
      if (seen.has(canonical)) continue;
      seen.add(canonical);
      numbers.push({ original, canonical, confidence: normalizedConfidence, confirmed: false });
    }
  }

  return { detectedOrderId, numbers, rawText: text, method };
}

let workerPromise: Promise<Worker> | undefined;

function getWorker() {
  workerPromise ??= createWorker(["chi_sim", "eng"], 1, {
    workerPath: "/ocr/worker.min.js",
    corePath: "/ocr/core",
    langPath: "/ocr/lang",
    gzip: true,
    workerBlobURL: false,
  });
  return workerPromise;
}

export async function recognizeOffline(
  images: Blob[],
  onProgress: (done: number, total: number) => void,
  isCurrent: () => boolean = () => true,
): Promise<RecognitionDraft> {
  const worker = await getWorker();
  requireCurrent(isCurrent);
  const drafts: RecognitionDraft[] = [];
  for (const [index, image] of images.entries()) {
    requireCurrent(isCurrent);
    const result = await worker.recognize(image);
    requireCurrent(isCurrent);
    drafts.push(parseRecognizedText(result.data.text, result.data.confidence, "offline"));
    onProgress(index + 1, images.length);
  }
  return {
    detectedOrderId: drafts.find((draft) => draft.detectedOrderId)?.detectedOrderId ?? null,
    numbers: drafts.flatMap((draft) => draft.numbers),
    rawText: drafts.map((draft) => draft.rawText).join("\n"),
    method: "offline",
  };
}

/** The only recognition routing boundary used by the workbench. */
export async function recognizeWithFallback(
  settings: AppSettings,
  sessionId: string,
  images: Array<{ inputId: string; blob?: Blob; size: number }>,
  onProgress: (done: number, total: number) => void,
  onStage: (stage: RecognitionStage) => void = () => undefined,
  isCurrent: () => boolean = () => true,
): Promise<RecognitionDraft> {
  requireCurrent(isCurrent);
  const validation = validateRecognitionImageBatch(images);
  if (!validation.ok) throw new Error(validation.message);
  const offlineImages = () => {
    const blobs = images.map(image => image.blob);
    if (blobs.some(blob => !blob)) {
      throw new Error("持久化图片暂时无法读取，不能进行离线识别");
    }
    return blobs as Blob[];
  };
  if (settings.recognitionMode === "offline") {
    onStage("offline");
    return recognizeOffline(offlineImages(), onProgress, isCurrent);
  }
  if (!settings.defaultProviderId) throw new Error("尚未设置默认云模型");
  try {
    requireCurrent(isCurrent);
    onStage("cloud");
    onProgress(0, images.length);
    const result = await bridge.recognizeCloud(
      settings.defaultProviderId,
      sessionId,
      images.map(image => image.inputId),
    );
    requireCurrent(isCurrent);
    onProgress(images.length, images.length);
    return result;
  } catch (error) {
    // The request can become stale while the cloud promise is rejecting.
    // Re-check before emitting a fallback stage or touching the OCR worker.
    requireCurrent(isCurrent);
    if (error instanceof StaleRecognitionError) throw error;
    if (settings.cloudFallbackOffline) {
      onStage("fallback");
      return recognizeOffline(offlineImages(), onProgress, isCurrent);
    }
    throw error;
  }
}
