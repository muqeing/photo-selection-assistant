import { bridge } from "./bridge";
import {
  parseRecognizedText,
  recognizeWithFallback,
  type RecognitionStage,
} from "./recognition";
export {
  MAX_RECOGNITION_BATCH_BYTES,
  MAX_RECOGNITION_IMAGES,
  validateRecognitionImageBatch,
} from "./recognition-limits";
import {
  MAX_RECOGNITION_BATCH_BYTES,
  MAX_RECOGNITION_IMAGES,
  validateRecognitionImageBatch,
} from "./recognition-limits";
import type {
  AppSettings, BlockingIssue, CopyCompletionReport, CopyLaunch, CopyProgress, InputImportStatus,
  InputItem, MatchItem,
  MatchReport, ModalState, PersistedInput, PhotoNumber, ProviderProfile, ProviderTemplate,
  RecognitionDraft, ScanProgress, Session, SessionWorkflow,
} from "./types";

export type AppState = {
  page: "sessions" | "workbench" | "history" | "model-settings" | "file-settings";
  activeSession?: Session;
  sessions: Session[];
  inputs: InputItem[];
  detectedOrderId: string | null;
  draftNumbers: PhotoNumber[];
  numbersConfirmed: boolean;
  sourceDir?: string;
  targetDir?: string;
  matchReport?: MatchReport;
  blockingIssues: BlockingIssue[];
  copyProgress?: CopyProgress;
  scanProgress?: ScanProgress;
  modal?: ModalState;
  settings: AppSettings;
  providers: ProviderProfile[];
  providerTemplates: ProviderTemplate[];
  recognitionNote?: string;
  recognitionProgress?: string;
  completionReport?: CopyCompletionReport;
  editingProviderId?: string;
  candidatePreviewUrls: Record<string, string>;
  copyActive: boolean;
  activeCopyJobId?: string;
  terminalCopyJobId?: string;
  pausedCopyJobId?: string;
  retiredCopyJobIds?: string[];
  copyRuntimeSessionId?: string;
  pendingWorkflowSessionId?: string;
  workflowGeneration?: number;
  /** Bumped whenever a user action replaces the current scan/copy attempt. */
  copyAttemptGeneration?: number;
  pendingRecheckRequest?: CopyAttemptRequest;
  numberWorkflowCommandPending?: "scan" | "copyLaunch";
  copyRuntimeVersion?: number;
  providerDraft?: ProviderProfile;
  inputImportBusy?: boolean;
  inputImportStatuses?: InputImportStatus[];
  inputImportRequest?: InputImportRequest;
  inputImportSequence?: number;
  recognitionRetrySequence?: number;
  pendingRecognitionRetry?: RecognitionRetryRequest;
};

export type WorkflowRequest = {
  sessionId: string;
  generation: number;
  copyRuntimeVersion: number;
};

export type InputImportRequest = WorkflowRequest & {
  importId: number;
  /** OCR authority captured when this import starts; file persistence does not use it. */
  recognitionGeneration: number;
};

export type RecognitionRetryRequest = {
  sessionId: string;
  workflowGeneration: number;
  copyAttemptGeneration: number;
  retryId: number;
};

export type CopyAttemptRequest = {
  sessionId: string;
  generation: number;
};

export const MAX_SCREENSHOT_BYTES = 24 * 1024 * 1024;
export const SCREENSHOT_ACCEPT = "image/jpeg,image/png,image/webp,.jpg,.jpeg,.png,.webp";

const screenshotExtensions = new Set(["jpg", "jpeg", "png", "webp"]);
const screenshotMimeTypes = new Set(["image/jpeg", "image/png", "image/webp"]);

export function validateScreenshotFile(
  file: Pick<File, "name" | "type" | "size">,
): { ok: true } | { ok: false; message: string } {
  const extension = file.name.match(/\.([^.]+)$/)?.[1]?.toLowerCase() ?? "";
  if (!screenshotExtensions.has(extension)) {
    return { ok: false, message: `${file.name || "未命名文件"}：仅支持 JPG、PNG 或 WebP 图片` };
  }

  const mime = file.type.trim().toLowerCase();
  if (mime && !screenshotMimeTypes.has(mime)) {
    return { ok: false, message: `${file.name}：文件类型不是受支持的图片` };
  }

  if (file.size <= 0) {
    return { ok: false, message: `${file.name}：图片为空` };
  }
  if (file.size > MAX_SCREENSHOT_BYTES) {
    return { ok: false, message: `${file.name}：图片不能超过 24 MiB` };
  }

  return { ok: true };
}

export async function persistScreenshotBatch(
  files: File[],
  save: (file: File) => Promise<InputItem>,
): Promise<{ saved: InputItem[]; failed: InputImportStatus[] }> {
  const saved: InputItem[] = [];
  const failed: InputImportStatus[] = [];
  for (const file of files) {
    try {
      saved.push(await save(file));
    } catch (error) {
      failed.push({
        name: file.name,
        state: "failed",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }
  return { saved, failed };
}

export function preflightScreenshotImportBatch(files: File[]) {
  const entries = files.map(file => ({
    file,
    validation: validateScreenshotFile(file),
  }));
  const validFiles = entries.flatMap(entry =>
    entry.validation.ok ? [entry.file] : []);
  const statuses: InputImportStatus[] = entries.map(entry =>
    entry.validation.ok
      ? { name: entry.file.name, state: "pending" }
      : {
          name: entry.file.name,
          state: "failed",
          message: entry.validation.message,
        });

  if (validFiles.length === 0) {
    return {
      ok: false as const,
      message: "没有可导入的有效图片",
      validFiles,
      statuses,
    };
  }

  const batchValidation = validateRecognitionImageBatch(validFiles);
  if (!batchValidation.ok) {
    return {
      ok: false as const,
      message: batchValidation.message,
      validFiles: [] as File[],
      statuses: entries.map((entry, index) =>
        entry.validation.ok
          ? {
              name: entry.file.name,
              state: "failed" as const,
              message: batchValidation.message,
            }
          : statuses[index]!),
    };
  }

  return {
    ok: true as const,
    validFiles,
    statuses,
  };
}

export async function withValidatedRecognitionImageBatch<T>(
  files: File[],
  action: () => Promise<T>,
): Promise<
  | { ok: true; value: T }
  | { ok: false; message: string; statuses: InputImportStatus[] }
> {
  const preflight = preflightScreenshotImportBatch(files);
  if (!preflight.ok) {
    return {
      ok: false,
      message: preflight.message,
      statuses: preflight.statuses,
    };
  }
  return { ok: true, value: await action() };
}

function appRoot() {
  const root = document.querySelector<HTMLElement>("#app");
  if (!root) throw new Error("缺少 #app");
  return root;
}

const issueNames: Record<BlockingIssue, string> = {
  ambiguous: "候选重号", "partial-format": "格式不完整", missing: "未找到照片",
  "target-conflict": "目标目录冲突", "permission-denied": "目录权限不足",
  "insufficient-space": "可用空间不足", "source-changed": "源目录已变化",
  "source-disconnected": "源目录已断开",
  "target-disconnected": "目标目录已断开",
};

export const unresolvedIssues = (issues: BlockingIssue[]) => [...new Set(issues)];

export function modalFromPausePayload(payload: unknown): ModalState {
  if (!payload || typeof payload !== "object") throw new Error("复制暂停事件格式无效");
  const value = payload as Record<string, unknown>;
  const allowed: BlockingIssue[] = ["ambiguous", "partial-format", "missing", "target-conflict", "permission-denied", "insufficient-space", "source-changed", "source-disconnected", "target-disconnected"];
  if (!allowed.includes(value.issue as BlockingIssue)) throw new Error("未知的复制暂停原因");
  const actions = Array.isArray(value.actions)
    ? value.actions.filter((action): action is ModalState["actions"][number] =>
      ["recheck", "cancel", "manual", "retryRecognition", "close"].includes(String(action)))
    : ["close"] as ModalState["actions"];
  return { issue: value.issue as BlockingIssue, title: String(value.title ?? "复制已暂停"), message: String(value.message ?? ""), affected: Array.isArray(value.affected) ? value.affected.map(String) : [], actions };
}

export function canStartCopy(state: Pick<AppState, "numbersConfirmed" | "blockingIssues"> & { secondConfirmationEnabled: boolean }) {
  return state.numbersConfirmed && state.blockingIssues.length === 0;
}

/**
 * A scan is deliberately a separate action from confirming the customer IDs.
 * The source/target pair is bound by the native directory picker, so do not
 * offer the scan control until that step has completed.
 */
export function canScanDirectory(
  state: Pick<AppState, "numbersConfirmed" | "sourceDir" | "targetDir" | "copyActive">
    & Partial<Pick<
      AppState,
      | "activeSession"
      | "activeCopyJobId"
      | "terminalCopyJobId"
      | "pausedCopyJobId"
      | "pendingRecheckRequest"
      | "numberWorkflowCommandPending"
      | "matchReport"
      | "modal"
    >>,
) {
  return state.numbersConfirmed
    && Boolean(state.sourceDir)
    && Boolean(state.targetDir)
    && !isNumberWorkflowLocked(state);
}

export function isUnavailableDirectoryError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return /照片目录无法读取|SMB 网络目录不可用|目录无效|路径.*无效|os error 3|系统找不到指定的路径/i.test(message);
}

type NumberWorkflowLockState = Pick<AppState, "copyActive"> & Partial<Pick<
  AppState,
  | "activeSession"
  | "activeCopyJobId"
  | "terminalCopyJobId"
  | "pausedCopyJobId"
  | "pendingRecheckRequest"
  | "numberWorkflowCommandPending"
  | "matchReport"
  | "modal"
>>;

export function isNumberWorkflowLocked(state: NumberWorkflowLockState) {
  return Boolean(
    state.numberWorkflowCommandPending
    || state.copyActive
    || state.activeCopyJobId
    || state.terminalCopyJobId
    || state.pausedCopyJobId
    || state.pendingRecheckRequest
    || state.matchReport?.autoCopyStarted
    || state.modal?.actions.includes("recheck")
    || state.activeSession?.status === "scanning"
    || state.activeSession?.status === "copying",
  );
}

export function invalidateNumberWorkflow(state: AppState) {
  if (isNumberWorkflowLocked(state)) return false;
  state.numbersConfirmed = false;
  state.draftNumbers = (state.draftNumbers ?? []).map(number => ({
    ...number,
    confirmed: false,
  }));
  state.matchReport = undefined;
  state.blockingIssues = [];
  state.pendingRecognitionRetry = undefined;
  state.recognitionProgress = undefined;
  syncRecognitionProgressPanel(state);
  beginCopyAttempt(state);
  return true;
}

export function mutateNumberDraft(
  state: AppState,
  mutation: (numbers: PhotoNumber[]) => void,
  persist: (state: AppState) => Promise<unknown> = persistNumberDraft,
) {
  if (isNumberWorkflowLocked(state)) return undefined;
  mutation(state.draftNumbers);
  if (!invalidateNumberWorkflow(state)) return undefined;
  return { persistence: persist(state) };
}

export function beginNumberConfirmation(
  state: AppState,
  checked: boolean,
  persist: (
    state: AppState,
    snapshot: PhotoNumber[],
    request: CopyAttemptRequest,
  ) => Promise<boolean> = queueNumberSnapshot,
) {
  if (!state.activeSession || isNumberWorkflowLocked(state)) return undefined;
  const request = beginCopyAttempt(state, state.activeSession.id);
  state.pendingRecognitionRetry = undefined;
  state.recognitionProgress = undefined;
  syncRecognitionProgressPanel(state);
  state.matchReport = undefined;
  state.blockingIssues = [];
  state.numbersConfirmed = checked;
  state.draftNumbers = state.draftNumbers.map(number => ({
    ...number,
    confirmed: checked,
  }));
  const snapshot = state.draftNumbers.map(number => ({ ...number }));
  return {
    request,
    persistence: persist(state, snapshot, request),
  };
}

export function applyCopyLaunch(state: AppState, launch: CopyLaunch) {
  if (isRetiredCopyJob(state, launch.jobId)) return false;
  if (state.terminalCopyJobId) return false;
  if (state.pausedCopyJobId || (
    state.activeCopyJobId && state.activeCopyJobId !== launch.jobId
  )) return false;
  state.pausedCopyJobId = undefined;
  state.activeCopyJobId = launch.jobId;
  state.copyActive = true;
  state.completionReport = undefined;
  touchCopyRuntime(state);
  return true;
}

export function applyCopyCompletion(state: AppState, report: CopyCompletionReport) {
  if (isRetiredCopyJob(state, report.jobId)) return false;
  if (state.activeCopyJobId && state.activeCopyJobId !== report.jobId) return false;
  if (state.terminalCopyJobId) return false;
  state.terminalCopyJobId = report.jobId;
  state.activeCopyJobId = undefined;
  state.pausedCopyJobId = undefined;
  state.copyProgress = undefined;
  state.copyActive = false;
  state.completionReport = report;
  touchCopyRuntime(state);
  return true;
}

export function applyCopyProgress(state: AppState, progress: CopyProgress) {
  if (isRetiredCopyJob(state, progress.jobId)) return false;
  if (state.terminalCopyJobId || state.pausedCopyJobId) return false;
  if (state.activeCopyJobId && state.activeCopyJobId !== progress.jobId) return false;
  state.activeCopyJobId = progress.jobId;
  state.copyProgress = progress;
  state.copyActive = true;
  touchCopyRuntime(state);
  return true;
}

export function applyCopyPaused(state: AppState, jobId: string | undefined, modal: ModalState) {
  if (jobId && isRetiredCopyJob(state, jobId)) return false;
  if (state.terminalCopyJobId && (
    !jobId
    || state.terminalCopyJobId === jobId
    || !state.activeCopyJobId
  )) return false;
  if (jobId && state.activeCopyJobId && state.activeCopyJobId !== jobId) return false;
  state.modal = modal;
  state.pausedCopyJobId = jobId;
  state.activeCopyJobId = undefined;
  state.copyActive = false;
  touchCopyRuntime(state);
  return true;
}

export function shouldHandleSessionEvent(state: AppState, sessionId: string) {
  return (state.copyRuntimeSessionId ?? state.activeSession?.id) === sessionId;
}

export function beginSessionSwitch(state: AppState, sessionId?: string): WorkflowRequest {
  state.workflowGeneration = (state.workflowGeneration ?? 0) + 1;
  // A command still resolving for the previous workbench must not update this one.
  state.copyAttemptGeneration = (state.copyAttemptGeneration ?? 0) + 1;
  state.activeSession = undefined;
  resetCopyRuntime(state, true);
  state.copyRuntimeSessionId = sessionId ?? "";
  state.pendingWorkflowSessionId = sessionId;
  state.inputImportBusy = false;
  state.inputImportStatuses = [];
  state.inputImportRequest = undefined;
  state.pendingRecognitionRetry = undefined;
  state.recognitionProgress = undefined;
  return {
    sessionId: sessionId ?? "",
    generation: state.workflowGeneration,
    copyRuntimeVersion: state.copyRuntimeVersion ?? 0,
  };
}

export function captureWorkflowRequest(state: AppState, sessionId: string): WorkflowRequest {
  return {
    sessionId,
    generation: state.workflowGeneration ?? 0,
    copyRuntimeVersion: state.copyRuntimeVersion ?? 0,
  };
}

function isCurrentWorkflowRequest(state: AppState, request: WorkflowRequest) {
  return state.workflowGeneration === request.generation
    && state.copyRuntimeSessionId === request.sessionId;
}

function isSameInputImport(left: InputImportRequest | undefined, right: InputImportRequest) {
  return Boolean(left)
    && left!.sessionId === right.sessionId
    && left!.generation === right.generation
    && left!.importId === right.importId;
}

function isCurrentInputImport(state: AppState, request: InputImportRequest) {
  return isCurrentWorkflowRequest(state, request)
    && isSameInputImport(state.inputImportRequest, request);
}

export function isCurrentInputRecognition(
  state: AppState,
  request: InputImportRequest,
) {
  return isCurrentInputImport(state, request)
    && (state.copyAttemptGeneration ?? 0) === request.recognitionGeneration;
}

export function beginInputImport(
  state: AppState,
  sessionId: string,
  statuses: InputImportStatus[],
) {
  const workflow = captureWorkflowRequest(state, sessionId);
  if (
    state.inputImportBusy
    && state.inputImportRequest
    && isCurrentWorkflowRequest(state, state.inputImportRequest)
  ) return undefined;
  state.inputImportSequence = (state.inputImportSequence ?? 0) + 1;
  const request: InputImportRequest = {
    ...workflow,
    importId: state.inputImportSequence,
    recognitionGeneration: state.copyAttemptGeneration ?? 0,
  };
  state.inputImportBusy = true;
  state.inputImportStatuses = statuses.map(status => ({ ...status }));
  state.inputImportRequest = request;
  return request;
}

export function updateInputImportStatus(
  state: AppState,
  request: InputImportRequest,
  index: number,
  status: InputImportStatus,
  root?: ProgressRoot,
) {
  if (
    !isCurrentInputImport(state, request)
    || !state.inputImportStatuses?.[index]
  ) return false;
  state.inputImportStatuses[index] = { ...status };
  syncInputImportProgressPanel(state, root);
  return true;
}

export function finishInputImport(state: AppState, request: InputImportRequest) {
  if (!isCurrentInputImport(state, request)) return false;
  state.inputImportBusy = false;
  state.inputImportRequest = undefined;
  return true;
}

function touchCopyRuntime(state: AppState) {
  state.copyRuntimeVersion = (state.copyRuntimeVersion ?? 0) + 1;
}

function isRetiredCopyJob(state: AppState, jobId: string) {
  return state.retiredCopyJobIds?.includes(jobId) ?? false;
}

function retireCurrentCopyJobs(state: AppState) {
  const retired = new Set(state.retiredCopyJobIds ?? []);
  for (const jobId of [
    state.activeCopyJobId,
    state.terminalCopyJobId,
    state.pausedCopyJobId,
    state.copyProgress?.jobId,
    state.completionReport?.jobId,
  ]) {
    if (jobId) retired.add(jobId);
  }
  state.retiredCopyJobIds = [...retired];
}

export function beginCopyAttempt(state: AppState, sessionId = state.activeSession?.id ?? state.copyRuntimeSessionId ?? ""): CopyAttemptRequest {
  state.copyAttemptGeneration = (state.copyAttemptGeneration ?? 0) + 1;
  retireCurrentCopyJobs(state);
  clearCopyRuntime(state);
  return { sessionId, generation: state.copyAttemptGeneration };
}

export function captureCopyAttempt(state: AppState, sessionId: string): CopyAttemptRequest {
  return { sessionId, generation: state.copyAttemptGeneration ?? 0 };
}

export function isCurrentCopyAttempt(state: AppState, request: CopyAttemptRequest) {
  return state.activeSession?.id === request.sessionId
    && (state.copyAttemptGeneration ?? 0) === request.generation;
}

/**
 * Returns false when the recheck settled after its session or attempt was
 * superseded. Those outcomes are intentionally silent: the newer workbench
 * owns any error UI.
 */
export async function recheckCopyAttempt(
  state: AppState,
  sessionId: string,
  recheck = bridge.recheckCopy,
) {
  const pending = state.pendingRecheckRequest;
  if (
    pending
    && pending.sessionId === sessionId
    && isCurrentCopyAttempt(state, pending)
  ) return false;
  const request = {
    sessionId,
    generation: (state.copyAttemptGeneration ?? 0) + 1,
  };
  state.copyAttemptGeneration = request.generation;
  state.pendingRecheckRequest = request;
  try {
    const launch = await recheck(sessionId);
    if (!isCurrentCopyAttempt(state, request)) return false;
    retireCurrentCopyJobs(state);
    clearCopyRuntime(state);
    applyCopyLaunch(state, launch);
    return true;
  } catch (error) {
    if (!isCurrentCopyAttempt(state, request)) return false;
    if (state.modal) {
      state.modal = {
        ...state.modal,
        message: error instanceof Error ? error.message : String(error),
      };
    }
    return false;
  } finally {
    if (
      state.pendingRecheckRequest?.sessionId === request.sessionId
      && state.pendingRecheckRequest.generation === request.generation
    ) {
      state.pendingRecheckRequest = undefined;
    }
  }
}

export function syncPausedModalMessage(dialog: HTMLDialogElement, state: AppState) {
  const message = dialog.querySelector<HTMLElement>("[data-modal-message]");
  if (message && state.modal) message.textContent = state.modal.message;
}

function resetCopyRuntime(state: AppState, forgetRetiredJobs = false) {
  if (forgetRetiredJobs) state.retiredCopyJobIds = [];
  clearCopyRuntime(state);
}

function clearCopyRuntime(state: AppState) {
  state.copyProgress = undefined;
  state.completionReport = undefined;
  state.copyActive = false;
  state.numberWorkflowCommandPending = undefined;
  state.activeCopyJobId = undefined;
  state.terminalCopyJobId = undefined;
  state.pausedCopyJobId = undefined;
  state.modal = undefined;
  touchCopyRuntime(state);
}

export const shouldActivateDropzone = (key: string) => key === "Enter" || key === " ";

type ScreenshotImportControlsState = Pick<
  AppState,
  "inputImportBusy" | "inputImportStatuses"
> & { inputImportLocked?: boolean };

type ProgressRoot = {
  querySelector(selector: string): {
    innerHTML?: string;
    textContent?: string | null;
    hidden?: boolean;
  } | null;
};

function availableProgressRoot(root?: ProgressRoot): ProgressRoot | undefined {
  if (root) return root;
  return typeof document === "undefined" ? undefined : document as ProgressRoot;
}

function renderInputImportProgress(state: ScreenshotImportControlsState) {
  const statuses = state.inputImportStatuses ?? [];
  const pendingCount = statuses.filter(status => status.state === "pending").length;
  const summary = state.inputImportBusy
    ? pendingCount
      ? `正在导入 ${pendingCount} 张…`
      : "正在处理导入内容…"
    : statuses.length
      ? `本次导入：${statuses.filter(status => status.state === "saved").length} 张成功，${statuses.filter(status => status.state === "failed").length} 张失败`
      : "";
  const statusRows = statuses.map(status => {
    const label = status.state === "pending"
      ? status.message ?? "等待导入"
      : status.state === "saved"
        ? "已导入"
        : "导入失败";
    return `<li class="input-import-status ${status.state}"><span>${escapeHtml(status.name)}</span><strong>${label}</strong>${status.state === "failed" && status.message ? `<small>${escapeHtml(status.message)}</small>` : ""}</li>`;
  }).join("");
  return `${summary ? `<p class="input-import-summary">${summary}</p>` : ""}${statusRows ? `<ul class="input-import-statuses" aria-label="图片导入状态">${statusRows}</ul>` : ""}`;
}

function syncInputImportProgressPanel(
  state: ScreenshotImportControlsState,
  root?: ProgressRoot,
) {
  const panel = availableProgressRoot(root)?.querySelector("#input-import-progress");
  if (!panel) return false;
  panel.innerHTML = renderInputImportProgress(state);
  return true;
}

export function renderScreenshotImportControls(state: ScreenshotImportControlsState) {
  const busy = Boolean(state.inputImportBusy);
  const locked = busy || Boolean(state.inputImportLocked);
  return `<div id="input-dropzone" role="button" aria-label="导入客户选片截图" aria-disabled="${locked}" tabindex="${locked ? -1 : 0}" class="dropzone${busy ? " is-busy" : ""}">拖入多张截图<br><small>或点击选择图片</small></div><input id="screenshot-picker" type="file" accept="${SCREENSHOT_ACCEPT}" multiple hidden ${locked ? "disabled" : ""}><button id="choose-screenshots" type="button" ${locked ? "disabled" : ""}>选择图片</button><div id="input-import-progress" aria-live="polite">${renderInputImportProgress(state)}</div>`;
}

export async function persistScreenshotFileWithProgress<T>(
  state: AppState,
  request: InputImportRequest,
  index: number,
  file: Pick<File, "name" | "arrayBuffer">,
  save: (bytes: Uint8Array) => Promise<T>,
  root?: ProgressRoot,
  commit?: (saved: T) => void,
) {
  try {
    updateInputImportStatus(
      state,
      request,
      index,
      { name: file.name, state: "pending", message: "正在读取" },
      root,
    );
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (!isCurrentInputImport(state, request)) throw new Error("导入任务已失效");
    updateInputImportStatus(
      state,
      request,
      index,
      { name: file.name, state: "pending", message: "正在保存" },
      root,
    );
    const result = await save(bytes);
    if (!isCurrentInputImport(state, request)) throw new Error("导入任务已失效");
    commit?.(result);
    updateInputImportStatus(
      state,
      request,
      index,
      { name: file.name, state: "saved" },
      root,
    );
    return result;
  } catch (error) {
    updateInputImportStatus(
      state,
      request,
      index,
      {
        name: file.name,
        state: "failed",
        message: error instanceof Error ? error.message : String(error),
      },
      root,
    );
    throw error;
  }
}

function syncRecognitionProgressPanel(state: AppState, root?: ProgressRoot) {
  const panel = availableProgressRoot(root)?.querySelector("#recognition-progress");
  if (!panel) return false;
  panel.textContent = state.recognitionProgress ?? "";
  panel.hidden = !state.recognitionProgress;
  return true;
}

function recognitionProgressMessage(
  stage: RecognitionStage,
  done: number,
  total: number,
) {
  if (stage === "cloud") return `正在使用云端模型识别 ${done}/${total}`;
  if (stage === "fallback") {
    return `云端失败，正在改用离线识别 ${done}/${total}`;
  }
  return `正在离线识别 ${done}/${total}`;
}

export function updateInputRecognitionProgress(
  state: AppState,
  request: InputImportRequest,
  message: string | undefined,
  root?: ProgressRoot,
) {
  if (!isCurrentInputRecognition(state, request)) return false;
  state.recognitionProgress = message;
  syncRecognitionProgressPanel(state, root);
  return true;
}

function isCurrentRecognitionRetry(
  state: AppState,
  request: RecognitionRetryRequest,
) {
  const pending = state.pendingRecognitionRetry;
  return state.activeSession?.id === request.sessionId
    && (state.copyRuntimeSessionId ?? state.activeSession?.id) === request.sessionId
    && (state.workflowGeneration ?? 0) === request.workflowGeneration
    && (state.copyAttemptGeneration ?? 0) === request.copyAttemptGeneration
    && pending?.sessionId === request.sessionId
    && pending.workflowGeneration === request.workflowGeneration
    && pending.copyAttemptGeneration === request.copyAttemptGeneration
    && pending.retryId === request.retryId;
}

export function updateRetryRecognitionProgress(
  state: AppState,
  request: RecognitionRetryRequest,
  message: string | undefined,
  root?: ProgressRoot,
) {
  if (!isCurrentRecognitionRetry(state, request)) return false;
  state.recognitionProgress = message;
  syncRecognitionProgressPanel(state, root);
  return true;
}

type ScreenshotImportElements = {
  dropzone: HTMLElement;
  picker: HTMLInputElement;
  chooseButton: HTMLButtonElement;
  safetyTarget?: EventTarget;
};

const dropSafetyTargets = new WeakSet<EventTarget>();

function preventFileDropNavigation(target: EventTarget) {
  if (dropSafetyTargets.has(target)) return;
  const prevent = (event: Event) => event.preventDefault();
  target.addEventListener("dragover", prevent);
  target.addEventListener("drop", prevent);
  dropSafetyTargets.add(target);
}

export function bindScreenshotImportControls(
  elements: ScreenshotImportElements,
  importImageFiles: (files: File[]) => void | Promise<void>,
  isBusy: () => boolean,
) {
  const { dropzone, picker, chooseButton } = elements;
  let dragDepth = 0;
  if (elements.safetyTarget) preventFileDropNavigation(elements.safetyTarget);

  const openPicker = () => {
    if (!isBusy()) picker.click();
  };
  const submit = (files: File[]) => {
    if (!files.length || isBusy()) return;
    void Promise.resolve(importImageFiles(files)).catch(() => undefined);
  };
  const clearDragging = () => {
    dragDepth = 0;
    dropzone.classList.remove("is-dragging");
  };

  dropzone.addEventListener("click", openPicker);
  chooseButton.addEventListener("click", openPicker);
  dropzone.addEventListener("keydown", event => {
    const keyboardEvent = event as KeyboardEvent;
    if (!shouldActivateDropzone(keyboardEvent.key) || isBusy()) return;
    event.preventDefault();
    openPicker();
  });
  picker.addEventListener("change", () => {
    const files = Array.from(picker.files ?? []);
    picker.value = "";
    submit(files);
  });
  dropzone.addEventListener("dragenter", event => {
    event.preventDefault();
    if (isBusy()) return;
    dragDepth += 1;
    dropzone.classList.add("is-dragging");
  });
  dropzone.addEventListener("dragover", event => {
    event.preventDefault();
    if (!isBusy()) dropzone.classList.add("is-dragging");
  });
  dropzone.addEventListener("dragleave", event => {
    event.preventDefault();
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) dropzone.classList.remove("is-dragging");
  });
  dropzone.addEventListener("drop", event => {
    event.preventDefault();
    const files = Array.from((event as DragEvent).dataTransfer?.files ?? []);
    clearDragging();
    submit(files);
  });
}

export function providerDraftFromFormData(
  form: FormData,
  generatedId = crypto.randomUUID(),
) {
  const id = String(form.get("id") || generatedId);
  return {
    profile: {
      id,
      name: String(form.get("name") ?? "").trim(),
      template: form.get("template") as ProviderProfile["template"],
      address: String(form.get("address") ?? "").trim(),
      addressMode: form.get("addressMode") as ProviderProfile["addressMode"],
      apiFormat: form.get("apiFormat") as ProviderProfile["apiFormat"],
      model: String(form.get("model") ?? "").trim(),
      fallbackModel: String(form.get("fallbackModel") || "").trim() || null,
      timeoutSeconds: Number(form.get("timeoutSeconds")),
      enabled: form.has("enabled"),
      isDefault: form.has("isDefault"),
      secretRef: id,
    } satisfies ProviderProfile,
    apiKey: String(form.get("apiKey") || ""),
  };
}

function escapeHtml(value: string) {
  return value.replace(/[&<>"']/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char]!);
}
function redactPath(value: string) {
  const parts = value.split(/[\\/]+/).filter(Boolean);
  return parts.length > 1 ? `…/${parts.slice(-2).join("/")}` : (parts[0] ?? "文件");
}

export function releaseObjectUrls(state: AppState) {
  for (const item of state.inputs) {
    if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
  }
  for (const url of Object.values(state.candidatePreviewUrls)) {
    URL.revokeObjectURL(url);
  }
  state.inputs = [];
  state.candidatePreviewUrls = {};
  state.inputImportBusy = false;
  state.inputImportStatuses = [];
  state.inputImportRequest = undefined;
}
const navButton = (page: AppState["page"], label: string, active: AppState["page"]) =>
  `<button class="nav-item ${page === active ? "active" : ""}" data-page="${page}">${label}</button>`;

export function render(state: AppState) {
  const root = appRoot();
  root.innerHTML = `<div class="shell"><aside class="sidebar"><div class="brand" aria-label="FRAME SELECT 照片筛选助手"><i aria-hidden="true"></i><span>FRAME<br>SELECT</span></div><p class="machine-id">PHOTO OPERATOR / 01</p><nav aria-label="主导航">${navButton("sessions", "筛片会话", state.page)}${navButton("history", "历史记录", state.page)}</nav><div class="nav-spacer"></div><nav aria-label="设置导航">${navButton("model-settings", "模型设置", state.page)}${navButton("file-settings", "文件设置", state.page)}</nav><small>LOCAL GATEWAY · READY</small></aside><main class="page">${page(state)}</main></div><dialog id="blocking-dialog" class="blocking-dialog" aria-labelledby="blocking-title"></dialog>`;
  bindPageEvents(state);
  if (state.modal) showBlockingIssue(state);
}

function page(state: AppState) {
  switch (state.page) {
    case "sessions": return sessionsPage(state);
    case "workbench": return workbenchPage(state) + scanProgressMarkup(state);
    case "history": return historyPage(state);
    case "model-settings": return modelPage(state);
    case "file-settings": return filePage(state);
  }
}
function scanProgressMarkup(state: AppState) {
  if (state.numberWorkflowCommandPending !== "scan") return "";
  return `<section id="scan-progress" class="scan-progress" aria-live="polite"><progress></progress><p>正在快速检查文件名…</p><small>已检查 ${state.scanProgress?.checkedFiles ?? 0} 个文件 · 找到 ${state.scanProgress?.matchedFiles ?? 0} 个候选 · ${((state.scanProgress?.elapsedMs ?? 0) / 1000).toFixed(1)} 秒</small></section>`;
}
function sessionsPage(state: AppState) { return `<header class="page-head"><p>SESSION QUEUE</p><h1>筛片会话</h1><span>建立一次可追溯的选片任务。</span></header><section class="new-session"><form id="new-session"><label>任务标识 <b>REQUIRED</b><input name="taskLabel" required maxlength="120" placeholder="订单号 / 客户姓名 / 自定义名称"></label><label>操作备注<textarea name="note" maxlength="500" placeholder="可选，写给下一位操作员"></textarea></label><button class="amber" type="submit">创建并开始筛片 <em>↗</em></button></form></section><section class="session-list"><div class="section-label">RECENT SESSIONS <span>${state.sessions.length}</span></div>${state.sessions.length ? state.sessions.map(s => `<button data-open-session="${escapeHtml(s.id)}"><strong>${escapeHtml(s.taskLabel)}</strong><span>${escapeHtml(s.status)}</span><time>${escapeHtml(s.createdAt)}</time></button>`).join("") : "<p class=empty>尚无会话。先建立一个任务。</p>"}</section>`; }
function historyPage(state: AppState) { return `<header class="page-head"><p>ARCHIVE</p><h1>历史记录</h1></header><section class="session-list">${state.sessions.map(s => `<button data-open-session="${escapeHtml(s.id)}"><strong>${escapeHtml(s.taskLabel)}</strong><span>${escapeHtml(s.status)}</span><time>${escapeHtml(s.createdAt)}</time></button>`).join("") || "<p class=empty>没有可显示的记录。</p>"}</section>`; }
function workbenchPage(state: AppState) {
  const numberWorkflowLocked = isNumberWorkflowLocked(state);
  const n = state.draftNumbers.map((x, i) => {
    const confidence = x.confidence == null ? null : Number.isFinite(x.confidence) ? Math.max(0, Math.min(1, x.confidence)) : 0;
    return `<li class="${confidence != null && confidence < .75 ? "low-confidence" : ""}"><span class="row-no">${String(i + 1).padStart(2,"0")}</span><input aria-label="原始编号 ${i + 1}" data-number-index="${i}" value="${escapeHtml(x.original)}" ${numberWorkflowLocked ? "disabled" : ""}><output aria-label="规范编号">${escapeHtml(x.canonical)}</output><small>${confidence == null ? "人工" : `${Math.round(confidence * 100)}%`}</small><button type="button" aria-label="删除编号 ${i + 1}" data-delete-number="${i}" ${numberWorkflowLocked ? "disabled" : ""}>×</button></li>`;
  }).join("");
  const inputLog = state.inputs.map(x=>`<article class="input-log">${x.previewUrl ? `<img src="${escapeHtml(x.previewUrl)}" alt="${escapeHtml(x.name)} 的识别原图预览">` : ""}<span>${escapeHtml(x.name)}</span><small>${x.kind}</small></article>`).join("");
  const completion = state.completionReport ? `<section class="completion" aria-live="polite"><h2>${state.completionReport.status === "completed" ? "复制完成" : state.completionReport.status === "cancelled" ? "复制已取消" : "复制失败"}</h2><p>${escapeHtml(state.completionReport.message)}</p><dl><div><dt>已复制</dt><dd>${state.completionReport.copiedCount}</dd></div><div><dt>相同跳过</dt><dd>${state.completionReport.skippedIdenticalCount}</dd></div><div><dt>人工跳过</dt><dd>${state.completionReport.skippedUserCount}</dd></div><div><dt>失败</dt><dd>${state.completionReport.failedCount}</dd></div><div><dt>源目录</dt><dd>${escapeHtml(state.completionReport.source)}</dd></div><div><dt>目标目录</dt><dd>${escapeHtml(state.completionReport.target)}</dd></div><div><dt>开始</dt><dd>${state.completionReport.startedAt ? escapeHtml(new Date(state.completionReport.startedAt).toLocaleString()) : "未记录"}</dd></div><div><dt>结束</dt><dd>${escapeHtml(new Date(state.completionReport.finishedAt).toLocaleString())}</dd></div></dl>${state.completionReport.status === "failed" ? `<button id="retry-failed-copy" type="button">返回重新扫描</button>` : ""}</section>` : "";
  const directoryReady = Boolean(state.sourceDir && state.targetDir);
  const canScan = canScanDirectory(state);
  return `<header class="work-head"><div><p>ACTIVE SESSION</p><h1>${escapeHtml(state.activeSession?.taskLabel ?? "")}</h1></div><div class="state-chip ${state.numbersConfirmed ? "ok" : ""}">${state.copyActive ? "COPYING" : state.numbersConfirmed ? "NUMBERS LOCKED" : "AWAITING CHECK"}</div></header>${state.detectedOrderId ? `<p class="order-check">检测订单号 / ${escapeHtml(state.detectedOrderId)} · 仅核对用</p>` : ""}<div class="work-grid"><section class="station"><div class="station-title"><b>01</b><h2>客户选片信息</h2></div>${renderScreenshotImportControls({...state, inputImportLocked: numberWorkflowLocked})}<p id="recognition-progress" class="notice" aria-live="polite" ${state.recognitionProgress ? "" : "hidden"}>${escapeHtml(state.recognitionProgress ?? "")}</p><button id="paste-input" type="button" ${state.inputImportBusy || numberWorkflowLocked ? "disabled" : ""}>读取剪贴板</button>${state.inputs.length ? `<button id="retry-input-recognition" type="button" ${state.inputImportBusy || numberWorkflowLocked ? "disabled" : ""}>重新识别已导入内容</button>` : ""}<div class="input-previews">${inputLog}</div></section><section class="station"><div class="station-title"><b>02</b><h2>人工核对编号</h2></div>${state.recognitionNote ? `<p class="notice" aria-live="polite">${escapeHtml(state.recognitionNote)}</p>` : ""}<ul id="number-list">${n || "<li class=empty>等待导入或人工录入。</li>"}</ul><button id="add-number" type="button" ${numberWorkflowLocked ? "disabled" : ""}>+ 添加编号</button><label class="confirm"><input id="confirm-numbers" type="checkbox" ${state.numbersConfirmed ? "checked" : ""} ${numberWorkflowLocked ? "disabled" : ""}> 我已逐项确认编号</label>${state.numbersConfirmed ? `<p class="notice">编号已确认，请继续第 03 步选择照片目录。</p>` : ""}</section><section class="station paths"><div class="station-title"><b>03</b><h2>照片目录</h2></div><p class="notice">${state.numbersConfirmed ? "先选择源照片目录；系统会自动生成目标目录，也可改选目标位置。" : "请先在第 02 步确认编号。"}</p><button id="choose-source" type="button" ${state.numbersConfirmed && !numberWorkflowLocked ? "" : "disabled"}>选择源照片目录</button><form id="manual-source-form" class="manual-path"><label>手动填写源目录<input id="manual-source-path" name="path" type="text" autocomplete="off" placeholder="Z:\\照片 或 \\\\NAS\\共享\\照片" ${state.numbersConfirmed && !numberWorkflowLocked ? "" : "disabled"}></label><button id="bind-manual-source" type="submit" ${state.numbersConfirmed && !numberWorkflowLocked ? "" : "disabled"}>确认此源目录</button><small>也可直接粘贴本机、映射盘或 UNC 路径</small></form><output>${escapeHtml(state.sourceDir ? redactPath(state.sourceDir) : "未选择")}</output><button id="choose-target-base" type="button" ${state.numbersConfirmed && state.sourceDir && !numberWorkflowLocked ? "" : "disabled"}>选择目标目录基准</button><form id="manual-target-base-form" class="manual-path"><label>手动填写目标基准目录<input id="manual-target-base" name="path" type="text" autocomplete="off" placeholder="D:\\项目 或 \\\\NAS\\共享\\项目" ${state.numbersConfirmed && state.sourceDir && !numberWorkflowLocked ? "" : "disabled"}></label><button id="bind-manual-target-base" type="submit" ${state.numbersConfirmed && state.sourceDir && !numberWorkflowLocked ? "" : "disabled"}>确认此目标基准</button></form><output>${escapeHtml(state.targetDir ? redactPath(state.targetDir) : "将自动创建 /照片成片/待精修的原片")}</output>${state.numbersConfirmed ? `<button id="scan" class="amber" type="button" ${canScan ? "" : "disabled"}>开始扫描照片目录</button>${directoryReady ? "" : "<p class=empty>选择完成源照片目录后，才可开始扫描。</p>"}` : ""}</section></div><section class="results"><div class="section-label">04 / MATCH & COPY</div>${matchReport(state)}${state.copyActive ? `<div class="copy-progress" aria-live="polite"><progress max="${state.copyProgress?.totalBytes ?? 1}" value="${state.copyProgress?.copiedBytes ?? 0}"></progress><p>${escapeHtml(state.copyProgress?.currentFile ?? "正在执行复制前检查…")} · ${state.copyProgress?.completedFiles ?? 0}/${state.copyProgress?.totalFiles ?? "?"}</p><button id="cancel-copy" type="button">取消复制</button></div>` : ""}${completion}</section>`;
}
export function shouldShowStartCopy(state: {
  sessionStatus?: Session["status"];
  numbersConfirmed: boolean;
  blockingIssues: BlockingIssue[];
  copyActive: boolean;
  autoCopyStarted: boolean;
  requiresSecondConfirmation: boolean;
} & Partial<NumberWorkflowLockState>) {
  return state.sessionStatus === "readyToCopy"
    && !isNumberWorkflowLocked(state)
    && !state.autoCopyStarted
    && canStartCopy({
      numbersConfirmed: state.numbersConfirmed,
      blockingIssues: state.blockingIssues,
      secondConfirmationEnabled: state.requiresSecondConfirmation,
    });
}
function matchReport(state: AppState) { const report = state.matchReport; if (!report) return "<p class=empty>通过编号确认与目录选择后开始扫描。</p>"; return `${report.items.map(item => matchItem(state, item)).join("")}<div class="copy-action">${state.blockingIssues.length ? `<p class="blocked">必须先处理：${state.blockingIssues.map(x=>issueNames[x]).join("、")}</p>` : ""}${shouldShowStartCopy({...state, sessionStatus: state.activeSession?.status, autoCopyStarted: report.autoCopyStarted, requiresSecondConfirmation: report.requiresSecondConfirmation}) ? `<button id="start-copy" class="amber">${report.requiresSecondConfirmation ? "确认清单并开始复制" : "开始复制"}</button>` : ""}</div>`; }
function matchItem(state: AppState, item: MatchItem) {
  const skipped = state.matchReport?.skippedNumbers.includes(item.canonicalNumber);
  const groups = item.groups.map((group, index) => {
    const key = `${item.canonicalNumber}:${group.id}`;
    const preview = state.candidatePreviewUrls[key];
    const files = group.files.map(file => `<li><b>${escapeHtml(file.extension)}</b> ${escapeHtml(redactPath(file.path))} <time>${new Date(file.modifiedMs).toLocaleString()}</time></li>`).join("");
    return `<section class="candidate-group"><h4>候选组 ${index + 1}</h4>${preview ? `<img src="${escapeHtml(preview)}" alt="编号 ${escapeHtml(item.canonicalNumber)} 候选组 ${index + 1} 缩略图">` : `<button type="button" data-preview-number="${escapeHtml(item.canonicalNumber)}" data-preview-group="${escapeHtml(group.id)}">加载缩略图</button>`}<ul>${files}</ul>${item.status === "ambiguous" ? `<button type="button" data-resolve-group="${escapeHtml(item.canonicalNumber)}" data-group-id="${escapeHtml(group.id)}">采用此候选组</button>` : ""}</section>`;
  }).join("");
  const resolution = !skipped && (item.status === "partial" || item.status === "missing") ? `<div class="resolution-actions">${item.status === "partial" ? `<button type="button" data-resolve="${escapeHtml(item.canonicalNumber)}" data-resolution="acceptPartial">接受不完整组</button>` : ""}<button type="button" data-resolve="${escapeHtml(item.canonicalNumber)}" data-resolution="skip">跳过此编号</button></div>` : "";
  return `<article class="match ${item.status}"><header><strong>${escapeHtml(item.canonicalNumber)}</strong><span>${skipped ? "已跳过" : escapeHtml(item.status)}</span><small>${item.groups.length} 个候选组</small></header>${groups}${resolution}</article>`;
}
function modelPage(state: AppState) {
  const editing = state.providerDraft ?? state.providers.find(provider => provider.id === state.editingProviderId);
  const value = (field: keyof ProviderProfile, fallback = "") => escapeHtml(String(editing?.[field] ?? fallback));
  const templateOptions = state.providerTemplates.map(template => `<option value="${template.id}" ${editing?.template === template.id ? "selected" : ""}>${escapeHtml(template.label)}</option>`).join("");
  return `<header class="page-head"><p>RECOGNITION ROUTE</p><h1>模型设置</h1><span>密钥交给系统钥匙串；保存后立即清空，不回显。</span></header>${state.recognitionNote ? `<p class="notice" aria-live="polite">${escapeHtml(state.recognitionNote)}</p>` : ""}<form id="provider-form" class="settings-grid"><input name=id type=hidden value="${value("id")}"><label>名称<input name=name required value="${value("name")}"></label><label>平台模板<select name=template>${templateOptions}</select></label><label>地址模式<select name=addressMode><option value=baseUrl ${editing?.addressMode !== "fullEndpoint" ? "selected" : ""}>Base URL</option><option value=fullEndpoint ${editing?.addressMode === "fullEndpoint" ? "selected" : ""}>完整请求地址</option></select></label><label>API 地址<input name=address type=url required value="${value("address")}" placeholder="https://api.example.com/v1"></label><label>API 格式<select name=apiFormat><option value=responses ${editing?.apiFormat !== "chatCompletions" ? "selected" : ""}>Responses API</option><option value=chatCompletions ${editing?.apiFormat === "chatCompletions" ? "selected" : ""}>Chat Completions</option></select></label><label>模型<input name=model required value="${value("model")}"></label><label>备用模型<input name=fallbackModel value="${value("fallbackModel")}"></label><label>超时（秒）<input name=timeoutSeconds type=number min=5 max=300 value="${value("timeoutSeconds", "60")}"></label><label>API Key<input name=apiKey type=password autocomplete=new-password placeholder="${editing ? "留空则保持现有密钥" : "新配置必须填写"}"></label><label class=check><input name=enabled type=checkbox ${editing?.enabled === false ? "" : "checked"}>启用</label><label class=check><input name=isDefault type=checkbox ${editing?.isDefault ? "checked" : ""}>设为默认并切换云端</label><div class=form-actions><button id="test-provider-form" type=button>测试当前表单</button><button class=amber type=submit>${editing ? "保存修改" : "保存配置"}</button>${editing ? `<button id="cancel-provider-edit" type=button>取消编辑</button>` : ""}</div></form><section class="provider-list" aria-label="已保存云模型">${state.providers.map(x=>`<article><strong>${escapeHtml(x.name)}</strong><span>${x.isDefault ? "DEFAULT / CLOUD" : x.enabled ? "ENABLED" : "DISABLED"}</span><small>${escapeHtml(x.model)} · ${escapeHtml(redactPath(x.address))}</small><div><button type=button data-test-provider="${escapeHtml(x.id)}">测试</button><button type=button data-edit-provider="${escapeHtml(x.id)}">编辑</button><button type=button data-delete-provider="${escapeHtml(x.id)}">删除</button></div></article>`).join("") || "<p class=empty>尚未保存云端配置。当前新会话使用离线识别。</p>"}</section>`;
}
function filePage(state: AppState) { const s=state.settings; return `<header class="page-head"><p>OPERATION POLICY</p><h1>文件设置</h1></header><form id="file-settings" class="file-settings"><label>默认识别策略<select name=recognitionMode><option value=offline ${s.recognitionMode === "offline" ? "selected" : ""}>离线识别</option><option value=cloud ${s.recognitionMode === "cloud" ? "selected" : ""}>默认云模型</option></select></label><label class=check><input name=secondConfirmation type=checkbox ${s.secondConfirmationEnabled ? "checked" : ""}>复制前二次确认</label><label class=check><input name=cloudFallbackOffline type=checkbox ${s.cloudFallbackOffline ? "checked" : ""}>云端失败后自动使用离线识别</label><label>启用扩展名<input name=extensions value="${escapeHtml(s.extensions.join(","))}"></label><button class=amber type=submit>保存文件策略</button></form>`; }

export function applyRecognitionDraft(
  state: AppState,
  draft: RecognitionDraft,
  note?: string,
) {
  if (isNumberWorkflowLocked(state)) return false;
  state.detectedOrderId = draft.detectedOrderId;
  state.draftNumbers = draft.numbers.map(number => ({
    ...number,
    confirmed: false,
  }));
  if (!invalidateNumberWorkflow(state)) return false;
  state.recognitionNote = note;
  return true;
}

export function mergeRecognitionNumbers(
  existing: readonly PhotoNumber[],
  recognized: readonly PhotoNumber[],
) {
  const merged = existing.map(number => ({ ...number, confirmed: false }));
  const seen = new Set(merged.map(number => number.canonical));
  for (const number of recognized) {
    if (!number.canonical || seen.has(number.canonical)) continue;
    seen.add(number.canonical);
    merged.push({ ...number, confirmed: false });
  }
  return merged;
}

function numberFromInput(value: string): PhotoNumber | null { const parsed = parseRecognizedText(value); return parsed.numbers[0] ?? null; }
export function blockingIssuesFrom(report: MatchReport): BlockingIssue[] { return unresolvedIssues(report.items.flatMap(x => report.skippedNumbers.includes(x.canonicalNumber) ? [] : x.status === "ambiguous" ? ["ambiguous"] : x.status === "partial" ? ["partial-format"] : x.status === "missing" ? ["missing"] : [])); }
function fail(state: AppState, error: unknown, issue: BlockingIssue = "partial-format") {
  const directoryUnavailable = isUnavailableDirectoryError(error);
  const resolvedIssue = directoryUnavailable ? "source-disconnected" : issue;
  const message = directoryUnavailable
    ? "照片目录无法读取或已断开。请关闭此提示后，重新选择源照片目录，再开始扫描。"
    : error instanceof Error ? error.message : String(error);
  state.modal = { issue: resolvedIssue, title: issueNames[resolvedIssue], message, affected: [], actions: ["close"] };
  render(state);
}

export function recoveredCompletionReport(workflow: SessionWorkflow): CopyCompletionReport | undefined {
  if (!["completed", "failed", "cancelled"].includes(workflow.session.status)) return undefined;
  const latestPlanRevision = workflow.copyItems
    .filter(item => item.status !== "planSuperseded")
    .reduce<number | undefined>(
      (latest, item) => latest == null ? item.planRevision : Math.max(latest, item.planRevision),
      undefined,
    );
  const copyItems = latestPlanRevision == null
    ? []
    : workflow.copyItems.filter(item => item.planRevision === latestPlanRevision);
  const copied = copyItems.filter(item => item.status === "copied");
  const skippedIdentical = copyItems.filter(item =>
    item.status === "skipped" && item.skippedReason === "identical-target");
  const skippedUser = copyItems.filter(item =>
    item.status === "skipped" && item.skippedReason === "user-skipped");
  const failed = copyItems.filter(item => item.status === "failed");
  const indexedSizes = new Map(
    workflow.snapshot?.items.flatMap(item => item.groups.flatMap(group =>
      group.files.map(file => [file.path, file.size] as const),
    )) ?? [],
  );
  const finishedAt = copyItems.at(-1)?.updatedAt ?? workflow.session.createdAt;
  return {
    jobId: `recovered:${workflow.session.id}:${finishedAt}`,
    status: workflow.session.status as "completed" | "failed" | "cancelled",
    copiedCount: copied.length,
    skippedIdenticalCount: skippedIdentical.length,
    skippedUserCount: skippedUser.length,
    failedCount: failed.length,
    copiedBytes: copied.reduce((total, item) => total + (indexedSizes.get(item.source) ?? 0), 0),
    source: workflow.bindings.source ? redactPath(workflow.bindings.source) : "未记录",
    target: workflow.bindings.target ? redactPath(workflow.bindings.target) : "未记录",
    startedAt: copyItems[0]?.createdAt ?? null,
    finishedAt,
    message: workflow.session.status === "completed" ? "复制已完成。" : workflow.session.status === "cancelled" ? "任务已取消。" : "复制失败，请查看失败项。",
  };
}

export function applyWorkflow(
  state: AppState,
  workflow: SessionWorkflow,
  request?: WorkflowRequest,
) {
  if (request && (
    request.sessionId !== workflow.session.id
    || !isCurrentWorkflowRequest(state, request)
  )) return false;
  const copyRuntimeChanged = Boolean(
    request && request.copyRuntimeVersion !== (state.copyRuntimeVersion ?? 0)
  );
  const preserveMatchingEvent = copyRuntimeChanged || (
    state.pendingWorkflowSessionId === workflow.session.id && Boolean(
    state.activeCopyJobId
    || state.terminalCopyJobId
    || state.pausedCopyJobId
    || state.copyProgress
    || state.completionReport
    || state.modal,
    )
  );
  if (!preserveMatchingEvent) {
    resetCopyRuntime(state, state.copyRuntimeSessionId !== workflow.session.id);
  }
  state.pendingWorkflowSessionId = undefined;
  state.copyRuntimeSessionId = workflow.session.id;
  state.activeSession = workflow.session;
  state.draftNumbers = workflow.numbers;
  state.numbersConfirmed = workflow.session.numbersConfirmed;
  state.sourceDir = workflow.bindings.source ?? undefined;
  state.targetDir = workflow.bindings.target ?? undefined;
  state.matchReport = workflow.snapshot ? {
    items: workflow.snapshot.items,
    skippedNumbers: workflow.snapshot.skippedNumbers,
    autoCopyStarted: workflow.session.status === "copying",
    requiresSecondConfirmation: workflow.requiresSecondConfirmation,
    confirmationToken: workflow.confirmationToken,
    copyJob: null,
  } : undefined;
  state.blockingIssues = state.matchReport ? blockingIssuesFrom(state.matchReport) : [];
  if (!preserveMatchingEvent) {
    state.copyActive = workflow.session.status === "copying" || workflow.session.status === "scanning";
    state.completionReport = recoveredCompletionReport(workflow);
    if (state.completionReport) state.terminalCopyJobId = state.completionReport.jobId;
  }
  return true;
}

async function refreshWorkflow(state: AppState) {
  if (!state.activeSession) return false;
  const request = captureWorkflowRequest(state, state.activeSession.id);
  try {
    return applyWorkflow(state, await bridge.openSession(request.sessionId), request);
  } catch (error) {
    if (isCurrentWorkflowRequest(state, request)) throw error;
    return false;
  }
}

export async function hydrateSessionInputs(
  state: AppState,
  request: WorkflowRequest,
  persisted: PersistedInput[],
) {
  const hydrated: InputItem[] = [];
  const imageBatchValidation = validateRecognitionImageBatch(
    persisted.filter(input => input.kind === "image"),
  );
  for (const input of persisted) {
    if (input.kind === "image" && !imageBatchValidation.ok) {
      hydrated.push({
        id: input.id,
        persistedId: input.id,
        kind: input.kind,
        name: input.name,
        mime: input.mime,
        size: input.size,
      });
      continue;
    }
    try {
      const bytes = await bridge.readSessionInput(request.sessionId, input.id);
      const blob = new Blob([bytes], { type: input.mime });
      hydrated.push({
        id: input.id,
        persistedId: input.id,
        kind: input.kind,
        name: input.name,
        mime: input.mime,
        size: input.size,
        text: input.kind === "text" ? new TextDecoder().decode(bytes) : undefined,
        blob: input.kind === "image" ? blob : undefined,
        previewUrl: input.kind === "image" ? URL.createObjectURL(blob) : undefined,
      });
    } catch (error) {
      hydrated.push({
        id: input.id,
        persistedId: input.id,
        kind: input.kind,
        name: input.name,
        mime: input.mime,
        size: input.size,
      });
      if (isCurrentWorkflowRequest(state, request)) {
        state.recognitionNote = `读取“${input.name}”失败：${error instanceof Error ? error.message : String(error)}`;
      }
    }
  }
  if (!imageBatchValidation.ok && isCurrentWorkflowRequest(state, request)) {
    state.recognitionNote =
      `${imageBatchValidation.message}；本会话图片未载入内存，可点击重新识别，软件会自动安全分批处理。`;
  }
  if (!isCurrentWorkflowRequest(state, request)) {
    for (const item of hydrated) {
      if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
    }
    return false;
  }
  state.inputs = hydrated;
  return true;
}

export function applyMatchReport(state: AppState, report: MatchReport) {
  state.scanProgress = undefined;
  state.matchReport = report;
  state.blockingIssues = blockingIssuesFrom(report);
  if (report.copyJob) applyCopyLaunch(state, report.copyJob);
}

export function applyScanProgress(state: AppState, progress: ScanProgress) {
  if (state.numberWorkflowCommandPending !== "scan") return false;
  state.scanProgress = progress;
  return true;
}

const numberSaveQueues = new WeakMap<AppState, Promise<void>>();
export function queueNumberSnapshot(
  state: AppState,
  snapshot: PhotoNumber[],
  request = state.activeSession ? captureCopyAttempt(state, state.activeSession.id) : undefined,
) {
  if (!request) return Promise.resolve(false);
  const frozenSnapshot = snapshot.map(number => ({ ...number }));
  const queued = (numberSaveQueues.get(state) ?? Promise.resolve())
    .catch(() => undefined)
    .then(async () => {
      if (!isCurrentCopyAttempt(state, request)) return false;
      await bridge.saveConfirmedNumbers(request.sessionId, frozenSnapshot);
      return isCurrentCopyAttempt(state, request);
    });
  numberSaveQueues.set(state, queued.then(() => undefined));
  return queued;
}

export function persistNumberDraft(state: AppState) {
  const request = state.activeSession
    ? captureCopyAttempt(state, state.activeSession.id)
    : undefined;
  return queueNumberSnapshot(
    state,
    state.draftNumbers
      .filter(number => number.original.trim() && number.canonical)
      .map(number => ({ ...number, confirmed: false })),
    request,
  );
}

type PersistedRecognitionImage = {
  inputId: string;
  blob?: Blob;
  size: number;
  mime?: string;
};

export function chunkPersistedRecognitionImages(
  images: readonly PersistedRecognitionImage[],
) {
  const chunks: PersistedRecognitionImage[][] = [];
  let chunk: PersistedRecognitionImage[] = [];
  let chunkBytes = 0;
  for (const image of images) {
    if (
      !Number.isSafeInteger(image.size)
      || image.size <= 0
      || image.size > MAX_SCREENSHOT_BYTES
    ) {
      throw new Error("持久化图片大小无效");
    }
    if (
      chunk.length === MAX_RECOGNITION_IMAGES
      || chunkBytes + image.size > MAX_RECOGNITION_BATCH_BYTES
    ) {
      chunks.push(chunk);
      chunk = [];
      chunkBytes = 0;
    }
    chunk.push(image);
    chunkBytes += image.size;
  }
  if (chunk.length) chunks.push(chunk);
  return chunks;
}

async function loadRecognitionChunkBlobs(
  state: AppState,
  request: RecognitionRetryRequest,
  inputs: readonly InputItem[],
  chunk: readonly PersistedRecognitionImage[],
) {
  const byPersistedId = new Map(
    inputs.flatMap(input => input.persistedId ? [[input.persistedId, input] as const] : []),
  );
  const loaded: PersistedRecognitionImage[] = [];
  for (const image of chunk) {
    if (!isCurrentRecognitionRetry(state, request)) return undefined;
    if (image.blob) {
      loaded.push(image);
      continue;
    }
    const input = byPersistedId.get(image.inputId);
    if (!input) throw new Error("持久化图片索引无效");
    const bytes = await bridge.readSessionInput(request.sessionId, image.inputId);
    if (!isCurrentRecognitionRetry(state, request)) return undefined;
    if (bytes.byteLength !== image.size) {
      throw new Error("持久化图片大小已变化");
    }
    loaded.push({
      ...image,
      blob: new Blob([bytes], { type: input.mime ?? "application/octet-stream" }),
    });
  }
  return loaded;
}

export async function retryPersistedRecognition(state: AppState) {
  const sessionId = state.activeSession?.id;
  const existingNumbers = state.draftNumbers.map(number => ({ ...number }));
  const existingOrderId = state.detectedOrderId;
  if (!sessionId || !invalidateNumberWorkflow(state)) return false;
  state.recognitionRetrySequence = (state.recognitionRetrySequence ?? 0) + 1;
  const request: RecognitionRetryRequest = {
    sessionId,
    workflowGeneration: state.workflowGeneration ?? 0,
    copyAttemptGeneration: state.copyAttemptGeneration ?? 0,
    retryId: state.recognitionRetrySequence,
  };
  state.pendingRecognitionRetry = request;
  const inputs = state.inputs.map(input => ({ ...input }));
  const retrySettings = { ...state.settings, extensions: [...state.settings.extensions] };

  const isCurrent = () => isCurrentRecognitionRetry(state, request);
  const clearIfOwned = () => {
    if (state.pendingRecognitionRetry?.retryId === request.retryId) {
      state.pendingRecognitionRetry = undefined;
    }
  };
  const isStale = () => {
    if (isCurrent()) return false;
    clearIfOwned();
    return true;
  };

  try {
    const drafts: RecognitionDraft[] = [];
    const text = inputs.flatMap(input => input.text ? [input.text] : []).join("\n");
    const images = inputs.flatMap(input =>
      input.kind === "image" && input.persistedId
        ? [{
            inputId: input.persistedId,
            blob: input.blob,
            size: input.size ?? input.blob?.size ?? 0,
            mime: input.mime,
          }]
        : []);
    if (text) drafts.push(parseRecognizedText(text));
    if (images.length) {
      const chunks = chunkPersistedRecognitionImages(images);
      let completed = 0;
      for (const chunk of chunks) {
        if (isStale()) return false;
        const needsBlobs =
          retrySettings.recognitionMode === "offline"
          || retrySettings.cloudFallbackOffline;
        const recognitionChunk = needsBlobs
          ? await loadRecognitionChunkBlobs(state, request, inputs, chunk)
          : chunk;
        if (!recognitionChunk || isStale()) return false;
        let recognitionStage: RecognitionStage =
          retrySettings.recognitionMode === "cloud" ? "cloud" : "offline";
        const draft = await recognizeWithFallback(
          retrySettings,
          request.sessionId,
          recognitionChunk,
          (done) => {
            updateRetryRecognitionProgress(
              state,
              request,
              recognitionProgressMessage(
                recognitionStage,
                completed + done,
                images.length,
              ),
            );
          },
          stage => {
            recognitionStage = stage;
            updateRetryRecognitionProgress(
              state,
              request,
              recognitionProgressMessage(stage, completed, images.length),
            );
          },
          isCurrent,
        );
        if (isStale()) return false;
        drafts.push(draft);
        completed += chunk.length;
      }
    }
    if (isStale()) return false;
    if (!drafts.length) throw new Error("没有可重新识别的持久输入");
    const seen = new Set<string>();
    const recognizedNumbers = drafts
      .flatMap(draft => draft.numbers)
      .filter(number => !seen.has(number.canonical) && seen.add(number.canonical))
      .map(number => ({ ...number, confirmed: false }));
    const numbers = mergeRecognitionNumbers(existingNumbers, recognizedNumbers);
    if (isStale()) return false;
    state.detectedOrderId =
      existingOrderId
      ?? drafts.find(draft => draft.detectedOrderId)?.detectedOrderId
      ?? null;
    state.draftNumbers = numbers;
    state.recognitionNote = numbers.length
      ? undefined
      : "未识别到编号，请在下方人工录入。";
    if (isStale()) return false;
    const saved = await queueNumberSnapshot(
      state,
      numbers,
      {
        sessionId: request.sessionId,
        generation: request.copyAttemptGeneration,
      },
    );
    if (isStale()) return false;
    if (!saved) {
      updateRetryRecognitionProgress(state, request, undefined);
      clearIfOwned();
      return false;
    }
    updateRetryRecognitionProgress(state, request, undefined);
    clearIfOwned();
    return true;
  } catch (error) {
    if (isStale()) return false;
    updateRetryRecognitionProgress(state, request, undefined);
    clearIfOwned();
    throw error;
  }
}

export async function handleToolbarRecognitionRetry(
  state: AppState,
  retry: (state: AppState) => Promise<boolean> = retryPersistedRecognition,
  rerender: (state: AppState) => void = render,
) {
  if (isNumberWorkflowLocked(state)) return false;
  try {
    if (!await retry(state)) return false;
    rerender(state);
    return true;
  } catch (error) {
    state.modal = {
      issue: "partial-format",
      title: "识别失败",
      message: error instanceof Error ? error.message : String(error),
      affected: state.inputs.map(input => input.name),
      actions: ["retryRecognition", "manual", "cancel"],
    };
    rerender(state);
    return false;
  }
}

export async function handleModalRecognitionRetry(
  state: AppState,
  dialog: Pick<HTMLDialogElement, "close">,
  retry: (state: AppState) => Promise<boolean> = retryPersistedRecognition,
  rerender: (state: AppState) => void = render,
) {
  if (isNumberWorkflowLocked(state)) return false;
  if (!await retry(state)) return false;
  state.modal = undefined;
  dialog.close();
  rerender(state);
  return true;
}

export function updateInvalidatedNumberUi(root: Pick<Document, "querySelector"> = document) {
  const confirmation = root.querySelector<HTMLInputElement>("#confirm-numbers");
  if (confirmation) confirmation.checked = false;
  const scan = root.querySelector<HTMLButtonElement>("#scan");
  if (scan) scan.disabled = true;
  const results = root.querySelector<HTMLElement>(".results");
  if (results) results.innerHTML = `<div class="section-label">04 / MATCH & COPY</div><p class="empty">编号已变化，请重新确认并扫描。</p>`;
}

export function applyNumberInputEdit(
  state: AppState,
  index: number,
  value: string,
  persist: (state: AppState) => Promise<unknown> = persistNumberDraft,
) {
  const number = numberFromInput(value);
  return mutateNumberDraft(
    state,
    numbers => {
      numbers[index] = number ?? {
        original: value,
        canonical: "",
        confidence: null,
        confirmed: false,
      };
    },
    persist,
  );
}

type WorkbenchIngestDependencies = {
  renderState?: (state: AppState) => void;
  persistRecognizedNumbers?: (state: AppState) => Promise<unknown>;
};

export async function ingestWorkbenchItems(
  state: AppState,
  items: InputItem[],
  dependencies: WorkbenchIngestDependencies = {},
) {
  const renderState = dependencies.renderState ?? render;
  const persistRecognizedNumbers =
    dependencies.persistRecognizedNumbers ?? persistNumberDraft;
  if (!state.activeSession || isNumberWorkflowLocked(state)) return;
  const existingNumbers = state.draftNumbers.map(number => ({ ...number }));
  const existingOrderId = state.detectedOrderId;
  if (!invalidateNumberWorkflow(state)) return;
  const sessionId = state.activeSession.id;
  const imageItems = items.filter(
    (item): item is InputItem & { blob: Blob } => Boolean(item.blob),
  );
  const imageEntries = imageItems.map((item, index) => {
    const file = item.blob instanceof File
      ? item.blob
      : new File([item.blob], item.name, { type: item.blob.type });
    const validation = validateScreenshotFile(file);
    return { file, item, index, validation };
  });
  const validEntries = imageEntries.filter(
    (entry): entry is typeof entry & { validation: { ok: true } } =>
      entry.validation.ok,
  );
  const files = validEntries.map(entry => entry.file);
  const itemByFile = new Map(validEntries.map(entry => [
    entry.file,
    { item: entry.item, index: entry.index },
  ]));
  const request = beginInputImport(state, sessionId, imageEntries.map(entry =>
    entry.validation.ok
      ? { name: entry.file.name, state: "pending" }
      : {
          name: entry.file.name,
          state: "failed",
          message: entry.validation.message,
        },
  ));
  if (!request) return;
  let ownedRecognitionGeneration = request.recognitionGeneration;
  const ownsRecognition = () =>
    isCurrentInputImport(state, request)
    && (state.copyAttemptGeneration ?? 0) === ownedRecognitionGeneration;
  renderState(state);
  try {
    const savedTextInputs: InputItem[] = [];
    for (const item of items.filter(item => !item.blob)) {
      if (!isCurrentInputImport(state, request)) return;
      const bytes = new TextEncoder().encode(item.text ?? "");
      const saved = await bridge.saveSessionInput(
        sessionId,
        item.name,
        item.kind,
        bytes,
      );
      if (!isCurrentInputImport(state, request)) return;
      item.persistedId = saved.id;
      item.mime = saved.mime;
      item.size = saved.size;
      state.inputs.push(item);
      savedTextInputs.push(item);
    }

    const imageResult = await persistScreenshotBatch(files, async file => {
      const { item, index } = itemByFile.get(file)!;
      if (!isCurrentInputImport(state, request)) {
        throw new Error("导入任务已失效");
      }
      return persistScreenshotFileWithProgress(
        state,
        request,
        index,
        file,
        bytes => bridge.saveSessionInput(
          sessionId,
          item.name,
          item.kind,
          bytes,
        ),
        undefined,
        saved => {
          item.persistedId = saved.id;
          item.mime = saved.mime;
          item.size = saved.size;
          item.previewUrl = URL.createObjectURL(item.blob);
          state.inputs.push(item);
        },
      ).then(() => item);
    });
    if (!isCurrentInputImport(state, request)) return;
    if (!ownsRecognition()) return;

    const drafts: RecognitionDraft[] = [];
    const text = savedTextInputs.flatMap(input =>
      input.text ? [input.text] : []).join("\n");
    if (text) drafts.push(parseRecognizedText(text));
    const images = imageResult.saved.flatMap(input =>
      input.blob && input.persistedId
        ? [{
            inputId: input.persistedId,
            blob: input.blob,
            size: input.size ?? input.blob.size,
          }]
        : []);
    if (images.length) {
      let recognitionStage: RecognitionStage =
        state.settings.recognitionMode === "cloud" ? "cloud" : "offline";
      const recognized = await recognizeWithFallback(
        state.settings,
        sessionId,
        images,
        (done, total) => {
          updateInputRecognitionProgress(
            state,
            request,
            recognitionProgressMessage(recognitionStage, done, total),
          );
        },
        stage => {
          recognitionStage = stage;
          updateInputRecognitionProgress(
            state,
            request,
            recognitionProgressMessage(stage, 0, images.length),
          );
        },
        ownsRecognition,
      );
      if (!ownsRecognition()) return;
      drafts.push(recognized);
    }
    if (!ownsRecognition()) return;
    if (drafts.length) {
      const seen = new Set<string>();
      const recognizedNumbers = drafts
        .flatMap(draft => draft.numbers)
        .filter(number => !seen.has(number.canonical) && seen.add(number.canonical));
      const numbers = mergeRecognitionNumbers(existingNumbers, recognizedNumbers);
      const lastDraft = drafts.at(-1)!;
      if (!ownsRecognition()) return;
      if (!applyRecognitionDraft(state, {
        detectedOrderId:
          existingOrderId
          ?? drafts.find(draft => draft.detectedOrderId)?.detectedOrderId
          ?? null,
        numbers,
        rawText: drafts.map(draft => draft.rawText).join("\n"),
        method: lastDraft.method,
      }, lastDraft.method === "offline" && state.settings.recognitionMode === "cloud"
        ? "云端失败，已自动改用离线识别。"
        : undefined)) return;
      ownedRecognitionGeneration = state.copyAttemptGeneration ?? 0;
      if (!ownsRecognition()) return;
      if (!numbers.length) {
        state.recognitionNote = "未识别到编号，请在下方人工录入。";
      }
      await persistRecognizedNumbers(state);
      if (!ownsRecognition()) return;
    }
  } catch(error) {
    if (!ownsRecognition()) return;
    state.recognitionNote = "云端识别失败；可重新识别或改为人工录入。";
    state.modal = {
      issue: "partial-format",
      title: "识别失败",
      message: error instanceof Error ? error.message : String(error),
      affected: items.map(item => item.name),
      actions: ["retryRecognition", "manual", "cancel"],
    };
  } finally {
    if (isCurrentInputRecognition(state, request)) {
      updateInputRecognitionProgress(state, request, undefined);
    }
    if (finishInputImport(state, request)) {
      renderState(state);
    }
  }
}

function bindPageEvents(state: AppState) {
  document.querySelectorAll<HTMLButtonElement>("[data-page]").forEach(b => b.onclick = async () => {
    try {
      state.page = b.dataset.page as AppState["page"];
      if (state.page === "history" || state.page === "sessions") state.sessions = await bridge.listSessions();
      if (state.page === "model-settings") state.providers = await bridge.listProviders();
      if (state.page === "model-settings" || state.page === "file-settings") state.settings = await bridge.loadSettings();
      render(state);
    } catch (error) { fail(state, error); }
  });
  document.querySelectorAll<HTMLButtonElement>("[data-open-session]").forEach(b => b.onclick = async () => {
    const sessionId = b.dataset.openSession!;
    const request = beginSessionSwitch(state, sessionId);
    try {
      releaseObjectUrls(state);
      const workflow = await bridge.openSession(sessionId);
      if (!applyWorkflow(state, workflow, request)) return;
      if (!await hydrateSessionInputs(state, request, workflow.inputs)) return;
      state.page = "workbench";
      render(state);
    } catch(e) {
      if (isCurrentWorkflowRequest(state, request)) fail(state,e);
    }
  });
  document.querySelector<HTMLFormElement>("#new-session")?.addEventListener("submit", async e => {
    e.preventDefault();
    const f=new FormData(e.currentTarget as HTMLFormElement);
    const label=String(f.get("taskLabel") ?? "").trim();
    if (!label) return;
    const request = beginSessionSwitch(state);
    try {
      releaseObjectUrls(state);
      const created = await bridge.createSession(label,String(f.get("note") ?? "").trim() || undefined);
      if (state.workflowGeneration !== request.generation || state.copyRuntimeSessionId !== "") return;
      state.activeSession=created;
      state.copyRuntimeSessionId=state.activeSession.id;
      state.copyActive = false;
      applyRecognitionDraft(state,{detectedOrderId:null,numbers:[],rawText:"",method:"manual"});
      state.page="workbench";
      render(state);
    } catch(e) {
      if (state.workflowGeneration === request.generation) fail(state,e);
    }
  });
  const ingest = (items: InputItem[]) => ingestWorkbenchItems(state, items);
  const importImageFiles = async (files: File[]) => {
    const result = await withValidatedRecognitionImageBatch(files, () =>
      ingest(files.map((file, index) => ({
        id: `img-${Date.now()}-${index}`,
        kind: "image",
        name: file.name,
        blob: file,
        size: file.size,
      }))));
    if (!result.ok) {
      state.recognitionNote = result.message;
      state.inputImportStatuses = result.statuses;
      render(state);
    }
  };
  const dropzone = document.querySelector<HTMLElement>("#input-dropzone");
  const picker = document.querySelector<HTMLInputElement>("#screenshot-picker");
  const chooseButton = document.querySelector<HTMLButtonElement>("#choose-screenshots");
  if (dropzone && picker && chooseButton) {
    bindScreenshotImportControls(
      {
        dropzone,
        picker,
        chooseButton,
        safetyTarget: window,
      },
      importImageFiles,
      () => Boolean(state.inputImportBusy) || isNumberWorkflowLocked(state),
    );
  }
  document.querySelector<HTMLButtonElement>("#paste-input")?.addEventListener("click", async ()=> { if (state.inputImportBusy || isNumberWorkflowLocked(state)) return; try { const text=await bridge.readClipboardText(); if (text) return void ingest([{id:`text-${Date.now()}`,kind:"text",name:"剪贴板文字",text}]); const image=await bridge.readClipboardImage(); if (!image) return; const canvas=document.createElement("canvas"); const size=await image.size(); canvas.width=size.width; canvas.height=size.height; const ctx=canvas.getContext("2d"); if (!ctx) throw new Error("无法读取剪贴板图片"); const data=ctx.createImageData(size.width,size.height); data.data.set(await image.rgba()); ctx.putImageData(data,0,0); const blob=await new Promise<Blob | null>(r=>canvas.toBlob(r,"image/png")); if (!blob) throw new Error("无法转换剪贴板图片"); await importImageFiles([new File([blob],"剪贴板图片.png",{type:"image/png"})]); } catch(e) { fail(state,e); }});
  document.querySelector<HTMLButtonElement>("#retry-input-recognition")?.addEventListener("click",async()=>{await handleToolbarRecognitionRetry(state);});
  document.querySelectorAll<HTMLInputElement>("[data-number-index]").forEach(i=>i.oninput=()=>{ const index=Number(i.dataset.numberIndex); const result=applyNumberInputEdit(state,index,i.value);if(!result){i.value=state.draftNumbers[index]?.original??"";return;}const row=i.closest("li");const output=row?.querySelector("output");if(output)output.textContent=state.draftNumbers[index].canonical;updateInvalidatedNumberUi();void result.persistence.catch(error=>fail(state,error)); });
  document.querySelectorAll<HTMLButtonElement>("[data-delete-number]").forEach(b=>b.onclick=async()=>{const result=mutateNumberDraft(state,numbers=>numbers.splice(Number(b.dataset.deleteNumber),1));if(!result)return;try{await result.persistence;render(state);}catch(error){fail(state,error);}});
  document.querySelector<HTMLButtonElement>("#add-number")?.addEventListener("click",async()=>{const result=mutateNumberDraft(state,numbers=>numbers.push({original:"",canonical:"",confidence:null,confirmed:false}));if(!result)return;try{await result.persistence;render(state);}catch(error){fail(state,error);}});
  document.querySelector<HTMLInputElement>("#confirm-numbers")?.addEventListener("change", async e=>{const input=e.currentTarget as HTMLInputElement;const operation=beginNumberConfirmation(state,input.checked);if(!operation){input.checked=state.numbersConfirmed;return;}try{const saved=await operation.persistence;if(!saved||!isCurrentCopyAttempt(state,operation.request))return;render(state);}catch(error){if(!isCurrentCopyAttempt(state,operation.request))return;state.numbersConfirmed=false;state.draftNumbers=state.draftNumbers.map(x=>({...x,confirmed:false}));fail(state,error);}});
  document.querySelector<HTMLButtonElement>("#choose-source")?.addEventListener("click",async()=>{if(!state.activeSession||isNumberWorkflowLocked(state))return;try{const p=await bridge.chooseDirectory(state.activeSession.id,"source");state.sourceDir=p.source ?? undefined;state.targetDir=p.target ?? undefined;render(state);}catch(e){fail(state,e);}}); document.querySelector<HTMLButtonElement>("#choose-target-base")?.addEventListener("click",async()=>{if(!state.activeSession||isNumberWorkflowLocked(state))return;try{state.targetDir=await bridge.targetUnderSelectedBase(state.activeSession.id);render(state);}catch(e){fail(state,e);}});
  document.querySelector<HTMLFormElement>("#manual-source-form")?.addEventListener("submit",async e=>{e.preventDefault();if(!state.activeSession||isNumberWorkflowLocked(state))return;const form=e.currentTarget as HTMLFormElement;const path=String(new FormData(form).get("path")??"").trim();if(!path)return;try{const p=await bridge.bindManualDirectory(state.activeSession.id,"source",path);state.sourceDir=p.source ?? undefined;state.targetDir=p.target ?? undefined;render(state);}catch(error){fail(state,error);}});
  document.querySelector<HTMLFormElement>("#manual-target-base-form")?.addEventListener("submit",async e=>{e.preventDefault();if(!state.activeSession||!state.sourceDir||isNumberWorkflowLocked(state))return;const form=e.currentTarget as HTMLFormElement;const path=String(new FormData(form).get("path")??"").trim();if(!path)return;try{const p=await bridge.bindManualDirectory(state.activeSession.id,"targetBase",path);state.targetDir=p.target ?? undefined;render(state);}catch(error){fail(state,error);}});
  document.querySelector<HTMLButtonElement>("#scan")?.addEventListener("click",async()=>{if(!state.activeSession||!canScanDirectory(state))return;const sessionId=state.activeSession.id;const source=state.sourceDir!;const target=state.targetDir!;const request=beginCopyAttempt(state,sessionId);state.numberWorkflowCommandPending="scan";render(state);try{const report=await bridge.scanAndMatch(sessionId,source,target);if(!isCurrentCopyAttempt(state,request))return;state.numberWorkflowCommandPending=undefined;applyMatchReport(state,report);render(state);}catch(e){if(isCurrentCopyAttempt(state,request)){state.numberWorkflowCommandPending=undefined;fail(state,e);}}});
  document.querySelectorAll<HTMLButtonElement>("[data-resolve-group]").forEach(b=>b.onclick=async()=>{if(!state.activeSession)return;try{await bridge.resolveAmbiguousMatch(state.activeSession.id,b.dataset.resolveGroup!,b.dataset.groupId!);if(!await refreshWorkflow(state))return;state.modal=undefined;render(state);}catch(e){fail(state,e,"ambiguous");}});
  document.querySelectorAll<HTMLButtonElement>("[data-resolve]").forEach(b=>b.onclick=async()=>{if(!state.activeSession)return;try{await bridge.resolveMatch(state.activeSession.id,b.dataset.resolve!,{kind:b.dataset.resolution as "acceptPartial"|"skip"});if(!await refreshWorkflow(state))return;state.modal=undefined;render(state);}catch(e){fail(state,e);}});
  document.querySelectorAll<HTMLButtonElement>("[data-preview-number]").forEach(b=>b.onclick=async()=>{if(!state.activeSession)return;try{const number=b.dataset.previewNumber!;const group=b.dataset.previewGroup!;const preview=await bridge.readCandidatePreview(state.activeSession.id,number,group);if(preview){const url=URL.createObjectURL(new Blob([preview.bytes],{type:preview.mime}));state.candidatePreviewUrls[`${number}:${group}`]=url;}render(state);}catch(e){fail(state,e,"ambiguous");}});
  document.querySelector<HTMLButtonElement>("#start-copy")?.addEventListener("click",async()=>{if(!state.activeSession||!state.matchReport||isNumberWorkflowLocked(state)||!canStartCopy({...state,secondConfirmationEnabled:state.settings.secondConfirmationEnabled}))return;const sessionId=state.activeSession.id;const confirmationToken=state.matchReport.confirmationToken ?? undefined;const request=beginCopyAttempt(state,sessionId);state.numberWorkflowCommandPending="copyLaunch";render(state);try{const launch=await bridge.startCopy(sessionId,confirmationToken);if(!isCurrentCopyAttempt(state,request))return;state.numberWorkflowCommandPending=undefined;applyCopyLaunch(state,launch);render(state);}catch(e){if(isCurrentCopyAttempt(state,request)){state.numberWorkflowCommandPending=undefined;fail(state,e);}}});
  document.querySelector<HTMLButtonElement>("#cancel-copy")?.addEventListener("click",async()=>{if(!state.activeSession)return;try{await bridge.cancelCopy(state.activeSession.id);state.recognitionNote="正在取消复制…";render(state);}catch(e){fail(state,e);}});
  document.querySelector<HTMLButtonElement>("#retry-failed-copy")?.addEventListener("click",async()=>{if(!state.activeSession)return;const sessionId=state.activeSession.id;const confirmed=state.draftNumbers.map(number=>({...number,confirmed:true}));const request=beginCopyAttempt(state,sessionId);try{await bridge.saveConfirmedNumbers(sessionId,confirmed);if(!isCurrentCopyAttempt(state,request))return;state.draftNumbers=confirmed;state.numbersConfirmed=true;state.matchReport=undefined;state.blockingIssues=[];state.completionReport=undefined;state.copyProgress=undefined;state.copyActive=false;render(state);}catch(e){if(isCurrentCopyAttempt(state,request))fail(state,e);}});
  document.querySelector<HTMLFormElement>("#file-settings")?.addEventListener("submit",async e=>{e.preventDefault();const f=new FormData(e.currentTarget as HTMLFormElement);const next:AppSettings={recognitionMode:f.get("recognitionMode") as "offline"|"cloud",defaultProviderId:state.settings.defaultProviderId,secondConfirmationEnabled:f.has("secondConfirmation"),cloudFallbackOffline:f.has("cloudFallbackOffline"),extensions:String(f.get("extensions")??"").split(",").map(x=>x.trim()).filter(Boolean)};try{await bridge.saveSettings(next);state.settings=next;render(state);}catch(e){fail(state,e);}});
  const providerFromForm = (form: HTMLFormElement) => providerDraftFromFormData(new FormData(form));
  document.querySelector<HTMLFormElement>("#provider-form")?.addEventListener("submit",async e=>{e.preventDefault();const form=e.currentTarget as HTMLFormElement;const draft=providerFromForm(form);state.providerDraft=draft.profile;try{const result=await bridge.saveProvider(draft.profile,draft.apiKey);state.providers=result.providers;state.settings=result.settings;state.editingProviderId=undefined;state.providerDraft=undefined;state.recognitionNote="云模型配置已保存，API Key 输入已清空。";render(state);}catch(e){fail(state,e);}});
  document.querySelector<HTMLButtonElement>("#test-provider-form")?.addEventListener("click",async()=>{const form=document.querySelector<HTMLFormElement>("#provider-form");if(!form)return;const draft=providerFromForm(form);state.providerDraft=draft.profile;try{const result=await bridge.testProviderDraft(draft.profile,draft.apiKey);state.recognitionNote=result.message;const notice=document.querySelector<HTMLElement>(".notice");if(notice)notice.textContent=result.message;else form.insertAdjacentHTML("beforebegin",`<p class="notice" aria-live="polite">${escapeHtml(result.message)}</p>`);}catch(e){fail(state,e);}});
  document.querySelector<HTMLSelectElement>("#provider-form [name=template]")?.addEventListener("change",e=>{const form=(e.currentTarget as HTMLElement).closest("form")!;const template=state.providerTemplates.find(value=>value.id===(e.currentTarget as HTMLSelectElement).value);if(!template)return;const address=form.querySelector<HTMLInputElement>("[name=address]")!;const model=form.querySelector<HTMLInputElement>("[name=model]")!;const addressMode=form.querySelector<HTMLSelectElement>("[name=addressMode]")!;const apiFormat=form.querySelector<HTMLSelectElement>("[name=apiFormat]")!;address.value=template.address;model.value=template.model;addressMode.value=template.addressMode;apiFormat.value=template.apiFormat;});
  document.querySelector<HTMLButtonElement>("#cancel-provider-edit")?.addEventListener("click",()=>{state.editingProviderId=undefined;state.providerDraft=undefined;render(state);});
  document.querySelectorAll<HTMLButtonElement>("[data-edit-provider]").forEach(b=>b.onclick=()=>{state.editingProviderId=b.dataset.editProvider;state.providerDraft=undefined;render(state);});
  document.querySelectorAll<HTMLButtonElement>("[data-delete-provider]").forEach(b=>b.onclick=async()=>{if(!confirm("删除这套云模型配置？系统钥匙串中的对应密钥也会删除。"))return;try{const result=await bridge.deleteProvider(b.dataset.deleteProvider!);state.providers=result.providers;state.settings=result.settings;state.editingProviderId=undefined;render(state);}catch(e){fail(state,e);}});
  document.querySelectorAll<HTMLButtonElement>("[data-test-provider]").forEach(b=>b.onclick=async()=>{try{const result=await bridge.testProvider(b.dataset.testProvider!);state.recognitionNote=result.message;render(state);}catch(e){fail(state,e);}});
}
function showBlockingIssue(state: AppState) {
  const dialog=document.querySelector<HTMLDialogElement>("#blocking-dialog");
  if(!dialog||!state.modal)return;
  const m=state.modal;
  const labels: Record<ModalState["actions"][number], string>={recheck:"处理后重新检查",cancel:"取消任务",manual:"改为人工录入",retryRecognition:"重新识别",close:"返回处理"};
  dialog.innerHTML=`<p>OPERATION PAUSED</p><h2 id="blocking-title">${escapeHtml(m.title)}</h2><div data-modal-message>${escapeHtml(m.message)}</div>${m.affected.length?`<ul>${m.affected.map(x=>`<li>${escapeHtml(x)}</li>`).join("")}</ul>`:""}<div class="modal-actions">${m.actions.map(action=>`<button type=button data-modal-action="${action}">${labels[action]}</button>`).join("")}</div>`;
  dialog.querySelectorAll<HTMLButtonElement>("[data-modal-action]").forEach(button=>button.onclick=async()=>{
    const action=button.dataset.modalAction as ModalState["actions"][number];
    try {
      if(action==="recheck"&&state.activeSession&&!await recheckCopyAttempt(state,state.activeSession.id)){syncPausedModalMessage(dialog,state);return;}
      if(action==="cancel"&&state.activeSession){await bridge.cancelSession(state.activeSession.id);await refreshWorkflow(state);}
      if(action==="manual"){const result=mutateNumberDraft(state,numbers=>numbers.push({original:"",canonical:"",confidence:null,confirmed:false}));if(!result)return;await result.persistence;state.recognitionNote="请人工填写编号并逐项确认。";}
      if(action==="retryRecognition"){await handleModalRecognitionRetry(state,dialog);return;}
      state.modal=undefined;dialog.close();render(state);
    } catch(error){state.modal=undefined;dialog.close();fail(state,error);}
  });
  dialog.addEventListener("cancel",event=>event.preventDefault());
  dialog.showModal();
}
