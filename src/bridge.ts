import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { readImage, readText } from "@tauri-apps/plugin-clipboard-manager";
import type {
  AppSettings,
  BoundPaths,
  CandidatePreview,
  CopyCompletionReport,
  CopyLaunch,
  CopyPausedPayload,
  CopyProgress,
  DirectoryPurpose,
  MatchReport,
  MatchResolution,
  PhotoNumber,
  ProviderProfile,
  ProviderTemplate,
  ProviderTestResult,
  RecognitionDraft,
  ResolutionResult,
  Session,
  SessionEvent,
  PersistedInput,
  SessionWorkflow,
  ScanProgress,
} from "./types";

type RawInvoke = (
  command: string,
  body?: Record<string, unknown> | ArrayBuffer | Uint8Array,
  options?: { headers: Record<string, string> },
) => Promise<unknown>;

const INPUT_METADATA_HEADER = "x-photo-input-metadata";
const MAX_IMAGE_INPUT_BYTES = 24 * 1024 * 1024;
const MAX_TEXT_INPUT_BYTES = 4 * 1024 * 1024;
const MAX_INPUT_METADATA_BYTES = 8 * 1024;

export function encodeSessionInputMetadataHeader(
  sessionId: string,
  name: string,
  kind: PersistedInput["kind"],
) {
  const encoded = new TextEncoder().encode(JSON.stringify({
    sessionId,
    name,
    kind,
  }));
  if (encoded.byteLength > MAX_INPUT_METADATA_BYTES) {
    throw new Error("输入元数据过长");
  }
  let binary = "";
  for (const byte of encoded) binary += String.fromCharCode(byte);
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");
}

export function normalizeRawInputBytes(value: unknown): Uint8Array<ArrayBuffer> {
  if (value instanceof Uint8Array && value.buffer instanceof ArrayBuffer) {
    return value as Uint8Array<ArrayBuffer>;
  }
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  throw new Error("无法读取原始二进制输入");
}

export async function saveSessionInputRaw(
  sessionId: string,
  name: string,
  kind: PersistedInput["kind"],
  bytes: Uint8Array,
  invokeRaw: RawInvoke = invoke,
): Promise<PersistedInput> {
  const limit = kind === "image" ? MAX_IMAGE_INPUT_BYTES : MAX_TEXT_INPUT_BYTES;
  if (bytes.byteLength > limit) throw new Error("输入文件过大");
  return await invokeRaw("save_session_input", bytes, {
    headers: {
      [INPUT_METADATA_HEADER]: encodeSessionInputMetadataHeader(
        sessionId,
        name,
        kind,
      ),
    },
  }) as PersistedInput;
}

export async function readSessionInputRaw(
  sessionId: string,
  inputId: string,
  invokeRaw: RawInvoke = invoke,
): Promise<Uint8Array<ArrayBuffer>> {
  return normalizeRawInputBytes(await invokeRaw("read_session_input", {
    sessionId,
    inputId,
  }));
}

export function decodeCandidatePreviewEnvelope(
  value: unknown,
): CandidatePreview | null {
  let envelope: Uint8Array<ArrayBuffer>;
  try {
    envelope = normalizeRawInputBytes(value);
  } catch {
    throw new Error("候选预览响应不是原始二进制");
  }
  if (envelope.byteLength === 1 && envelope[0] === 0) return null;
  if (
    envelope.byteLength < 2
    || envelope.byteLength > MAX_IMAGE_INPUT_BYTES + 1
  ) {
    throw new Error("候选预览响应长度无效");
  }
  const mime = envelope[0] === 1
    ? "image/jpeg"
    : envelope[0] === 2
      ? "image/png"
      : envelope[0] === 3
        ? "image/webp"
        : undefined;
  if (!mime) throw new Error("候选预览响应类型无效");
  return { bytes: envelope.subarray(1), mime };
}

export async function readCandidatePreviewRaw(
  sessionId: string,
  number: string,
  groupId: string,
  invokeRaw: RawInvoke = invoke,
): Promise<CandidatePreview | null> {
  return decodeCandidatePreviewEnvelope(
    await invokeRaw("read_candidate_preview", {
      sessionId,
      number,
      groupId,
    }),
  );
}

export async function recognizePersistedCloudInputs(
  profileId: string,
  sessionId: string,
  inputIds: string[],
  cloud: (
    profileId: string,
    sessionId: string,
    inputIds: string[],
  ) => Promise<RecognitionDraft> = (id, activeSessionId, payload) =>
    invoke<RecognitionDraft>("recognize_cloud", {
      profileId: id,
      sessionId: activeSessionId,
      inputIds: payload,
    }),
): Promise<RecognitionDraft> {
  return cloud(profileId, sessionId, inputIds);
}

const onSessionEvent = <T>(
  event: string,
  handler: (payload: SessionEvent<T>) => void,
) => listen<SessionEvent<T>>(event, ({ payload }) => handler(payload));

export const bridge = {
  createSession: (taskLabel: string, note?: string) =>
    invoke<Session>("create_session", { taskLabel, note }),
  listSessions: () => invoke<Session[]>("list_sessions"),
  loadSettings: () => invoke<AppSettings>("load_settings"),
  listProviders: () => invoke<ProviderProfile[]>("list_providers"),
  providerTemplates: () => invoke<ProviderTemplate[]>("provider_templates"),
  openSession: (sessionId: string) =>
    invoke<SessionWorkflow>("open_session", { sessionId }),
  saveSessionInput: (
    sessionId: string,
    name: string,
    kind: PersistedInput["kind"],
    bytes: Uint8Array,
  ) => saveSessionInputRaw(sessionId, name, kind, bytes),
  listSessionInputs: (sessionId: string) =>
    invoke<PersistedInput[]>("list_session_inputs", { sessionId }),
  readSessionInput: readSessionInputRaw,
  saveConfirmedNumbers: (sessionId: string, numbers: PhotoNumber[]) =>
    invoke<void>("save_confirmed_numbers", { sessionId, numbers }),
  chooseDirectory: (sessionId: string, purpose: DirectoryPurpose) =>
    invoke<BoundPaths>("choose_directory", { sessionId, purpose }),
  bindManualDirectory: (
    sessionId: string,
    purpose: DirectoryPurpose,
    path: string,
  ) => invoke<BoundPaths>("bind_manual_directory", { sessionId, purpose, path }),
  defaultTargetForSource: (sessionId: string, source: string) =>
    invoke<string>("default_target_for_source", { sessionId, source }),
  targetUnderSelectedBase: (sessionId: string) =>
    invoke<string>("target_under_selected_base", { sessionId }),
  scanAndMatch: (sessionId: string, source: string, target: string) =>
    invoke<MatchReport>("scan_and_match", { sessionId, source, target }),
  resolveMatch: (
    sessionId: string,
    number: string,
    resolution: MatchResolution,
  ) =>
    invoke<ResolutionResult>("resolve_match", {
      sessionId,
      number,
      resolution,
    }),
  resolveAmbiguousMatch: (
    sessionId: string,
    number: string,
    groupId: string,
  ) =>
    invoke<ResolutionResult>("resolve_ambiguous_match", {
      sessionId,
      number,
      groupId,
    }),
  readCandidatePreview: readCandidatePreviewRaw,
  startCopy: (sessionId: string, confirmationToken?: string) =>
    invoke<CopyLaunch>("start_copy", { sessionId, confirmationToken }),
  cancelCopy: (sessionId: string) =>
    invoke<void>("cancel_copy", { sessionId }),
  recheckCopy: (sessionId: string) =>
    invoke<CopyLaunch>("recheck_copy", { sessionId }),
  cancelSession: (sessionId: string) =>
    invoke<void>("cancel_session", { sessionId }),
  saveSettings: (settings: AppSettings) =>
    invoke<void>("save_settings", { settings }),
  saveProvider: (profile: ProviderProfile, apiKey: string) =>
    invoke<{ settings: AppSettings; providers: ProviderProfile[] }>(
      "save_provider",
      { profile, apiKey },
    ),
  deleteProvider: (profileId: string) =>
    invoke<{ settings: AppSettings; providers: ProviderProfile[] }>(
      "delete_provider",
      { profileId },
    ),
  testProviderDraft: (profile: ProviderProfile, apiKey: string) =>
    invoke<ProviderTestResult>("test_provider_draft", { profile, apiKey }),
  testProvider: (profileId: string) =>
    invoke<ProviderTestResult>("test_provider", { profileId }),
  listProviderModels: (profileId: string) =>
    invoke<string[]>("list_provider_models", { profileId }),
  recognizeCloud: recognizePersistedCloudInputs,
  readClipboardText: () => readText(),
  readClipboardImage: () => readImage(),
  onScanProgress: (handler: (payload: SessionEvent<ScanProgress>) => void) =>
    onSessionEvent("scan-progress", handler),
  onCopyPaused: (handler: (payload: SessionEvent<CopyPausedPayload>) => void) =>
    onSessionEvent("copy-paused", handler),
  onCopyProgress: (
    handler: (payload: SessionEvent<CopyProgress>) => void,
  ) => onSessionEvent("copy-progress", handler),
  onCopyComplete: (
    handler: (payload: SessionEvent<CopyCompletionReport>) => void,
  ) =>
    onSessionEvent("copy-complete", handler),
};
