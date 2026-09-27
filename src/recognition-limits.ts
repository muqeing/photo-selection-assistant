/**
 * Recognition batch limits are an IPC safety invariant. Keep these values in
 * sync with `src-tauri/src/providers.rs`; a contract test guards the mirror.
 */
export const MAX_RECOGNITION_IMAGES = 12;
export const MAX_RECOGNITION_BATCH_BYTES = 48 * 1024 * 1024;

export const RECOGNITION_BATCH_LIMIT_MESSAGE =
  "一次最多识别 12 张图片，图片合计不能超过 48 MiB";

export function validateRecognitionImageBatch(
  images: readonly Pick<Blob, "size">[],
): { ok: true } | { ok: false; message: string } {
  if (images.length > MAX_RECOGNITION_IMAGES) {
    return { ok: false, message: RECOGNITION_BATCH_LIMIT_MESSAGE };
  }

  let total = 0;
  for (const image of images) {
    if (
      !Number.isSafeInteger(image.size)
      || image.size < 0
      || image.size > MAX_RECOGNITION_BATCH_BYTES - total
    ) {
      return { ok: false, message: RECOGNITION_BATCH_LIMIT_MESSAGE };
    }
    total += image.size;
  }
  return { ok: true };
}
