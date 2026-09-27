import { describe, expect, it, vi } from "vitest";
import {
  parseRecognizedText,
  recognizeOffline,
  recognizeWithFallback,
} from "../src/recognition";
import {
  applyRecognitionDraft,
  applyNumberInputEdit,
  applyWorkflow,
  applyCopyCompletion,
  applyCopyLaunch,
  applyCopyProgress,
  applyMatchReport,
  bindScreenshotImportControls,
  beginCopyAttempt,
  beginInputImport,
  beginNumberConfirmation,
  beginSessionSwitch,
  blockingIssuesFrom,
  canStartCopy,
  canScanDirectory,
  captureWorkflowRequest,
  captureCopyAttempt,
  hydrateSessionInputs,
  ingestWorkbenchItems,
  handleModalRecognitionRetry,
  handleToolbarRecognitionRetry,
  invalidateNumberWorkflow,
  isNumberWorkflowLocked,
  isCurrentCopyAttempt,
  isUnavailableDirectoryError,
  MAX_RECOGNITION_BATCH_BYTES,
  MAX_RECOGNITION_IMAGES,
  MAX_SCREENSHOT_BYTES,
  modalFromPausePayload,
  mutateNumberDraft,
  finishInputImport,
  persistScreenshotBatch,
  persistScreenshotFileWithProgress,
  preflightScreenshotImportBatch,
  providerDraftFromFormData,
  persistNumberDraft,
  queueNumberSnapshot,
  recheckCopyAttempt,
  recoveredCompletionReport,
  render,
  renderScreenshotImportControls,
  retryPersistedRecognition,
  SCREENSHOT_ACCEPT,
  shouldHandleSessionEvent,
  shouldShowStartCopy,
  shouldActivateDropzone,
  syncPausedModalMessage,
  updateInputImportStatus,
  updateInputRecognitionProgress,
  updateRetryRecognitionProgress,
  updateInvalidatedNumberUi,
  unresolvedIssues,
  validateRecognitionImageBatch,
  validateScreenshotFile,
  withValidatedRecognitionImageBatch,
} from "../src/app";
import { bridge } from "../src/bridge";
import type { AppState } from "../src/app";

const { createWorker, recognize } = vi.hoisted(() => ({
  createWorker: vi.fn(),
  recognize: vi.fn(),
}));

vi.mock("tesseract.js", () => ({ createWorker }));

function renderWorkbenchForTest(state: AppState) {
  const root = { innerHTML: "" };
  const fakeDocument = {
    querySelector: (selector: string) => selector === "#app" ? root : null,
    querySelectorAll: () => [],
  };
  vi.stubGlobal("document", fakeDocument);
  try {
    render(state);
    return root.innerHTML;
  } finally {
    vi.unstubAllGlobals();
  }
}

describe("screenshot file validation", () => {
  const file = (name: string, type: string, size = 1) => ({ name, type, size });

  it("publishes the shared picker limits", () => {
    expect(MAX_SCREENSHOT_BYTES).toBe(24 * 1024 * 1024);
    expect(MAX_RECOGNITION_IMAGES).toBe(12);
    expect(MAX_RECOGNITION_BATCH_BYTES).toBe(48 * 1024 * 1024);
    expect(SCREENSHOT_ACCEPT).toBe("image/jpeg,image/png,image/webp,.jpg,.jpeg,.png,.webp");
  });

  it.each([
    ["customer.jpg", "image/jpeg"],
    ["customer.jpeg", "image/jpeg"],
    ["customer.png", "image/png"],
    ["customer.webp", "image/webp"],
    ["CUSTOMER.JpG", "image/jpeg"],
    ["CUSTOMER.JpEg", "image/jpeg"],
    ["CUSTOMER.PnG", "image/png"],
    ["CUSTOMER.WeBp", "image/webp"],
  ])("accepts supported screenshots: %s", (name, type) => {
    expect(validateScreenshotFile(file(name, type))).toEqual({ ok: true });
  });

  it("accepts an empty MIME only when the filename extension is supported", () => {
    expect(validateScreenshotFile(file("customer.PNG", ""))).toEqual({ ok: true });
    expect(validateScreenshotFile(file("customer.txt", "")).ok).toBe(false);
  });

  it("rejects empty files", () => {
    expect(validateScreenshotFile(file("customer.jpg", "image/jpeg", 0))).toMatchObject({ ok: false });
  });

  it("accepts exactly 24 MiB and rejects one byte more", () => {
    expect(validateScreenshotFile(file("customer.webp", "image/webp", MAX_SCREENSHOT_BYTES))).toEqual({ ok: true });
    expect(validateScreenshotFile(file("customer.webp", "image/webp", MAX_SCREENSHOT_BYTES + 1))).toMatchObject({ ok: false });
  });

  it("requires both a supported extension and a supported supplied MIME", () => {
    expect(validateScreenshotFile(file("renamed.jpg", "text/plain")).ok).toBe(false);
    expect(validateScreenshotFile(file("renamed.txt", "image/jpeg")).ok).toBe(false);
  });
});

describe("live import and recognition progress", () => {
  const activeState = () => {
    const state = {
      activeSession: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      copyAttemptGeneration: 2,
      copyActive: false,
      inputImportBusy: false,
      inputImportStatuses: [],
    } as unknown as AppState;
    const request = beginInputImport(state, "session-a", [
      { name: "one.png", state: "pending" },
    ])!;
    return { state, request };
  };
  const cloudWorkbenchState = () => ({
    page: "workbench" as const,
    activeSession: {
      id: "session-progress",
      taskLabel: "progress",
      note: null,
      status: "draft" as const,
      createdAt: "now",
      numbersConfirmed: false,
    },
    copyRuntimeSessionId: "session-progress",
    workflowGeneration: 1,
    copyAttemptGeneration: 1,
    sessions: [],
    inputs: [],
    detectedOrderId: null,
    draftNumbers: [],
    numbersConfirmed: false,
    blockingIssues: [],
    settings: {
      recognitionMode: "cloud" as const,
      defaultProviderId: "provider-a",
      cloudFallbackOffline: false,
      secondConfirmationEnabled: false,
      extensions: [],
    },
    providers: [],
    providerTemplates: [],
    candidatePreviewUrls: {},
    copyActive: false,
  }) as AppState;

  it("updates reading and saving status in the existing live panel before promises settle", async () => {
    const { state, request } = activeState();
    const panel = { innerHTML: "" };
    const root = {
      querySelector: (selector: string) =>
        selector === "#input-import-progress" ? panel : null,
    };
    let resolveRead!: (value: ArrayBuffer) => void;
    let resolveSave!: (value: string) => void;
    const file = {
      name: "one.png",
      arrayBuffer: () => new Promise<ArrayBuffer>(resolve => {
        resolveRead = resolve;
      }),
    } as File;

    const operation = persistScreenshotFileWithProgress(
      state,
      request,
      0,
      file,
      () => new Promise<string>(resolve => {
        resolveSave = resolve;
      }),
      root,
    );
    expect(panel.innerHTML).toContain("正在读取");

    resolveRead(new Uint8Array([1, 2, 3]).buffer);
    await Promise.resolve();
    expect(panel.innerHTML).toContain("正在保存");

    resolveSave("saved");
    await expect(operation).resolves.toBe("saved");
    expect(panel.innerHTML).toContain("已导入");
  });

  it("does not commit a saved image after its import request becomes stale", async () => {
    const { state, request } = activeState();
    let resolveSave!: (value: string) => void;
    const commit = vi.fn();
    const operation = persistScreenshotFileWithProgress(
      state,
      request,
      0,
      {
        name: "one.png",
        arrayBuffer: async () => new Uint8Array([1]).buffer,
      },
      () => new Promise<string>(resolve => {
        resolveSave = resolve;
      }),
      undefined,
      commit,
    );
    await Promise.resolve();
    beginSessionSwitch(state, "session-b");
    resolveSave("persisted-a");

    await expect(operation).rejects.toThrow("导入任务已失效");
    expect(commit).not.toHaveBeenCalled();
    expect(state.inputs ?? []).toHaveLength(0);
  });

  it("keeps partial failure rows while later files finish", async () => {
    const { state, request } = activeState();
    state.inputImportStatuses = [
      { name: "bad.png", state: "pending" },
      { name: "good.png", state: "pending" },
    ];
    const panel = { innerHTML: "" };
    const root = {
      querySelector: () => panel,
    };

    updateInputImportStatus(
      state,
      request,
      0,
      { name: "bad.png", state: "failed", message: "读取失败" },
      root,
    );
    updateInputImportStatus(
      state,
      request,
      1,
      { name: "good.png", state: "saved" },
      root,
    );

    expect(panel.innerHTML).toContain("bad.png");
    expect(panel.innerHTML).toContain("读取失败");
    expect(panel.innerHTML).toContain("good.png");
    expect(panel.innerHTML).toContain("已导入");
  });

  it("ignores stale import and retry progress callbacks", () => {
    const { state, request } = activeState();
    const panel = { textContent: "", hidden: true, innerHTML: "" };
    const root = {
      querySelector: (selector: string) =>
        selector === "#recognition-progress" ? panel : null,
    };
    beginSessionSwitch(state, "session-b");

    expect(
      updateInputRecognitionProgress(
        state,
        request,
        "正在离线识别 1/2",
        root,
      ),
    ).toBe(false);
    expect(panel.textContent).toBe("");

    state.pendingRecognitionRetry = {
      sessionId: "session-a",
      workflowGeneration: 1,
      copyAttemptGeneration: 2,
      retryId: 1,
    };
    expect(
      updateRetryRecognitionProgress(
        state,
        state.pendingRecognitionRetry,
        "正在使用云端模型识别",
        root,
      ),
    ).toBe(false);
    expect(panel.textContent).toBe("");
  });

  it("keeps file persistence current after a number edit while invalidating only OCR", () => {
    const { state, request } = activeState();
    invalidateNumberWorkflow(state);

    expect(updateInputImportStatus(state, request, 0, {
      name: "one.png",
      state: "saved",
    })).toBe(true);
    expect(
      updateInputRecognitionProgress(
        state,
        request,
        "正在使用云端模型识别 1/1",
      ),
    ).toBe(false);
  });

  it("shows active OCR progress without overwriting the final recognition note", () => {
    const { state, request } = activeState();
    state.recognitionNote = "识别完成后仍需人工确认";
    const panel = { textContent: "", hidden: true };
    const root = {
      querySelector: () => panel,
    };

    expect(
      updateInputRecognitionProgress(
        state,
        request,
        "正在离线识别 1/2",
        root,
      ),
    ).toBe(true);
    expect(panel.textContent).toBe("正在离线识别 1/2");
    expect(panel.hidden).toBe(false);
    expect(state.recognitionNote).toBe("识别完成后仍需人工确认");

    updateInputRecognitionProgress(state, request, undefined, root);
    expect(panel.textContent).toBe("");
    expect(panel.hidden).toBe(true);
    expect(state.recognitionNote).toBe("识别完成后仍需人工确认");
  });

  it("clears an obsolete retry progress indicator when manual editing supersedes it", () => {
    const { state } = activeState();
    state.recognitionProgress = "正在使用云端模型识别…";
    state.pendingRecognitionRetry = {
      sessionId: "session-a",
      workflowGeneration: 1,
      copyAttemptGeneration: 2,
      retryId: 9,
    };

    expect(invalidateNumberWorkflow(state)).toBe(true);
    expect(state.recognitionProgress).toBeUndefined();
    expect(state.pendingRecognitionRetry).toBeUndefined();
  });

  it("renders an aria-live region for recognition progress without replacing final notes", () => {
    const { state, request } = activeState();
    state.recognitionNote = "最终编号需要人工确认";
    const markup = renderWorkbenchForTest({
      ...state,
      page: "workbench",
      sessions: [],
      inputs: [],
      detectedOrderId: null,
      draftNumbers: [],
      numbersConfirmed: false,
      blockingIssues: [],
      settings: {
        recognitionMode: "offline",
        defaultProviderId: null,
        secondConfirmationEnabled: false,
        cloudFallbackOffline: true,
        extensions: [],
      },
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
    } as AppState);

    expect(markup).toContain('id="recognition-progress"');
    expect(markup).toContain('aria-live="polite"');
    expect(markup).toContain("最终编号需要人工确认");
    expect(request.importId).toBeGreaterThan(0);
  });

  it("persists an imported screenshot but drops deferred OCR after a real number edit", async () => {
    let resolveCloud!: (value: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
      () => new Promise(resolve => {
        resolveCloud = resolve;
      }),
    );
    const saveInput = vi.spyOn(bridge, "saveSessionInput").mockResolvedValue({
      id: "persisted-image",
      name: "customer.png",
      kind: "image",
      mime: "image/png",
      size: 3,
    });
    const renderState = vi.fn();
    const persistRecognizedNumbers = vi.fn().mockResolvedValue(undefined);
    const persistManualEdit = vi.fn().mockResolvedValue(undefined);
    const progressValues: string[] = [];
    const progressPanel = {
      innerHTML: "",
      hidden: true,
      set textContent(value: string | null) {
        progressValues.push(value ?? "");
      },
    };
    vi.stubGlobal("document", {
      querySelector: (selector: string) =>
        selector === "#recognition-progress" ? progressPanel : null,
    });
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:preview");
    const image = new File([new Uint8Array([1, 2, 3])], "customer.png", {
      type: "image/png",
    });
    const state = {
      page: "workbench",
      activeSession: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      copyAttemptGeneration: 1,
      sessions: [],
      inputs: [],
      detectedOrderId: null,
      draftNumbers: [{
        original: "0001",
        canonical: "1",
        confidence: null,
        confirmed: false,
      }],
      numbersConfirmed: false,
      blockingIssues: [],
      settings: {
        recognitionMode: "cloud",
        defaultProviderId: "provider-a",
        cloudFallbackOffline: false,
        secondConfirmationEnabled: false,
        extensions: [],
      },
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
      copyActive: false,
    } as AppState;

    try {
      const importTask = ingestWorkbenchItems(
        state,
        [{
          id: "new-image",
          kind: "image",
          name: image.name,
          blob: image,
          size: image.size,
        }],
        { renderState, persistRecognizedNumbers },
      );
      await vi.waitFor(() => expect(cloud).toHaveBeenCalledOnce());
      expect(state.inputs).toHaveLength(1);
      expect(state.inputs[0]?.persistedId).toBe("persisted-image");
      expect(progressValues).toContain("正在使用云端模型识别 0/1");

      const edit = applyNumberInputEdit(
        state,
        0,
        "0099",
        persistManualEdit,
      );
      expect(edit).toBeDefined();
      await edit!.persistence;
      state.recognitionNote = "人工编号已保留";

      resolveCloud({
        detectedOrderId: "ORDER-OLD",
        numbers: [{
          original: "0007",
          canonical: "7",
          confidence: 0.99,
          confirmed: false,
        }],
        rawText: "0007",
        method: "cloud",
      });
      await importTask;

      expect(state.inputs).toHaveLength(1);
      expect(state.draftNumbers.map(number => number.canonical)).toEqual(["99"]);
      expect(state.recognitionNote).toBe("人工编号已保留");
      expect(state.modal).toBeUndefined();
      expect(persistRecognizedNumbers).not.toHaveBeenCalled();
      expect(renderState).toHaveBeenCalledTimes(2);
      expect(progressValues).not.toContain("正在使用云端模型识别 1/1");
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
    expect(saveInput).toHaveBeenCalledOnce();
  });

  it("renders the saved preview and a bound retry button after stale OCR settles", async () => {
    let resolveOldCloud!: (
      value: Awaited<ReturnType<typeof bridge.recognizeCloud>>,
    ) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud")
      .mockImplementationOnce(
        () => new Promise(resolve => {
          resolveOldCloud = resolve;
        }),
      )
      .mockResolvedValueOnce({
        detectedOrderId: null,
        numbers: [{
          original: "0099",
          canonical: "99",
          confidence: 0.9,
          confirmed: false,
        }],
        rawText: "0099",
        method: "cloud",
      });
    vi.spyOn(bridge, "saveSessionInput").mockResolvedValue({
      id: "persisted-dom-preview",
      name: "dom-preview.png",
      kind: "image",
      mime: "image/png",
      size: 1,
    });
    vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:dom-preview");
    let currentMarkup = "";
    let retryButton: EventTarget | undefined;
    const appRoot = {
      get innerHTML() {
        return currentMarkup;
      },
      set innerHTML(value: string) {
        currentMarkup = value;
        retryButton = value.includes('id="retry-input-recognition"')
          ? new EventTarget()
          : undefined;
      },
    };
    vi.stubGlobal("document", {
      querySelector: (selector: string) => {
        if (selector === "#app") return appRoot;
        if (selector === "#retry-input-recognition") return retryButton ?? null;
        return null;
      },
      querySelectorAll: () => [],
    });
    const state = cloudWorkbenchState();

    try {
      const importTask = ingestWorkbenchItems(
        state,
        [{
          id: "dom-preview",
          kind: "image",
          name: "dom-preview.png",
          blob: new File([new Uint8Array([1])], "dom-preview.png", {
            type: "image/png",
          }),
          size: 1,
        }],
      );
      await vi.waitFor(() => expect(cloud).toHaveBeenCalledOnce());
      const edit = applyNumberInputEdit(
        state,
        0,
        "0099",
        vi.fn().mockResolvedValue(undefined),
      )!;
      await edit.persistence;
      state.recognitionNote = "人工编号已保留";
      resolveOldCloud({
        detectedOrderId: "ORDER-OLD",
        numbers: [{
          original: "0007",
          canonical: "7",
          confidence: 0.99,
          confirmed: false,
        }],
        rawText: "0007",
        method: "cloud",
      });
      await importTask;

      expect(state.draftNumbers.map(number => number.canonical)).toEqual(["99"]);
      expect(currentMarkup).toContain("blob:dom-preview");
      expect(currentMarkup).toContain('id="retry-input-recognition"');
      expect(currentMarkup).toContain('value="0099"');
      expect(currentMarkup).toContain('aria-disabled="false"');
      expect(retryButton).toBeDefined();

      retryButton!.dispatchEvent(new Event("click"));
      await vi.waitFor(() => expect(cloud).toHaveBeenCalledTimes(2));
      await vi.waitFor(() =>
        expect(state.pendingRecognitionRetry).toBeUndefined());
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
  });

  it("shows cloud 0/N then N/N through the workbench import binding", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockResolvedValue({
      detectedOrderId: null,
      numbers: [{
        original: "0007",
        canonical: "7",
        confidence: 0.9,
        confirmed: false,
      }],
      rawText: "0007",
      method: "cloud",
    });
    vi.spyOn(bridge, "saveSessionInput").mockResolvedValue({
      id: "persisted-progress",
      name: "progress.png",
      kind: "image",
      mime: "image/png",
      size: 1,
    });
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:progress");
    const progressValues: string[] = [];
    vi.stubGlobal("document", {
      querySelector: (selector: string) =>
        selector === "#recognition-progress"
          ? {
              hidden: false,
              set textContent(value: string | null) {
                progressValues.push(value ?? "");
              },
            }
          : null,
    });
    const persistRecognizedNumbers = vi.fn().mockResolvedValue(undefined);
    const state = cloudWorkbenchState();

    try {
      await ingestWorkbenchItems(
        state,
        [{
          id: "progress-image",
          kind: "image",
          name: "progress.png",
          blob: new File([new Uint8Array([1])], "progress.png", {
            type: "image/png",
          }),
          size: 1,
        }],
        {
          renderState: vi.fn(),
          persistRecognizedNumbers,
        },
      );

      expect(progressValues).toContain("正在使用云端模型识别 0/1");
      expect(progressValues).toContain("正在使用云端模型识别 1/1");
      expect(state.draftNumbers.map(number => number.canonical)).toEqual(["7"]);
      expect(persistRecognizedNumbers).toHaveBeenCalledOnce();
      expect(state.recognitionProgress).toBeUndefined();
      expect(progressValues.at(-1)).toBe("");
      expect(cloud).toHaveBeenCalledOnce();
    } finally {
      vi.restoreAllMocks();
      vi.unstubAllGlobals();
    }
  });

  it("merges later import recognition into the existing draft without replacing manual values", async () => {
    let saved = 0;
    vi.spyOn(bridge, "saveSessionInput").mockImplementation(async (
      _sessionId,
      name,
      kind,
      bytes,
    ) => ({
      id: `persisted-${++saved}`,
      name,
      kind,
      mime: "image/png",
      size: bytes.byteLength,
    }));
    vi.spyOn(bridge, "recognizeCloud")
      .mockResolvedValueOnce({
        detectedOrderId: "ORDER-FIRST",
        numbers: [{
          original: "0012",
          canonical: "12",
          confidence: 0.8,
          confirmed: false,
        }],
        rawText: "0012",
        method: "cloud",
      })
      .mockResolvedValueOnce({
        detectedOrderId: "ORDER-SECOND",
        numbers: [
          {
            original: "12",
            canonical: "12",
            confidence: 0.99,
            confirmed: false,
          },
          {
            original: "0013",
            canonical: "13",
            confidence: 0.9,
            confirmed: false,
          },
        ],
        rawText: "12 0013",
        method: "cloud",
      });
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:batch");
    const state = cloudWorkbenchState();
    const persistRecognizedNumbers = vi.fn().mockResolvedValue(undefined);
    const imported = (name: string) => [{
      id: name,
      kind: "image" as const,
      name,
      blob: new File([new Uint8Array([1])], name, { type: "image/png" }),
      size: 1,
    }];

    try {
      await ingestWorkbenchItems(state, imported("first.png"), {
        renderState: vi.fn(),
        persistRecognizedNumbers,
      });
      state.draftNumbers[0] = {
        original: "人工保留的0012",
        canonical: "12",
        confidence: null,
        confirmed: false,
      };
      await ingestWorkbenchItems(state, imported("second.png"), {
        renderState: vi.fn(),
        persistRecognizedNumbers,
      });

      expect(state.detectedOrderId).toBe("ORDER-FIRST");
      expect(state.draftNumbers).toEqual([
        {
          original: "人工保留的0012",
          canonical: "12",
          confidence: null,
          confirmed: false,
        },
        {
          original: "0013",
          canonical: "13",
          confidence: 0.9,
          confirmed: false,
        },
      ]);
      expect(persistRecognizedNumbers).toHaveBeenCalledTimes(2);
    } finally {
      vi.restoreAllMocks();
    }
  });

  it.each(["success", "failure"] as const)(
    "confirmation immediately retires an in-flight import OCR %s",
    async outcome => {
      let resolveCloud!: (value: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
      let rejectCloud!: (reason: Error) => void;
      const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
        () => new Promise((resolve, reject) => {
          resolveCloud = resolve;
          rejectCloud = reject;
        }),
      );
      vi.spyOn(bridge, "saveSessionInput").mockResolvedValue({
        id: "persisted-confirm",
        name: "confirm.png",
        kind: "image",
        mime: "image/png",
        size: 1,
      });
      vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:confirm");
      const progressValues: string[] = [];
      const progressPanel = {
        hidden: false,
        set textContent(value: string | null) {
          progressValues.push(value ?? "");
        },
      };
      vi.stubGlobal("document", {
        querySelector: (selector: string) =>
          selector === "#recognition-progress" ? progressPanel : null,
      });
      const renderState = vi.fn();
      const persistRecognizedNumbers = vi.fn().mockResolvedValue(undefined);
      const persistConfirmation = vi.fn().mockResolvedValue(true);
      const state = cloudWorkbenchState();

      try {
        const importTask = ingestWorkbenchItems(
          state,
          [{
            id: "confirm-image",
            kind: "image",
            name: "confirm.png",
            blob: new File([new Uint8Array([1])], "confirm.png", {
              type: "image/png",
            }),
            size: 1,
          }],
          { renderState, persistRecognizedNumbers },
        );
        await vi.waitFor(() => expect(cloud).toHaveBeenCalledOnce());
        expect(state.inputs[0]?.persistedId).toBe("persisted-confirm");
        expect(state.recognitionProgress).toBe("正在使用云端模型识别 0/1");

        const confirmation = beginNumberConfirmation(
          state,
          true,
          persistConfirmation,
        )!;
        await confirmation.persistence;
        expect(state.recognitionProgress).toBeUndefined();
        expect(state.pendingRecognitionRetry).toBeUndefined();
        expect(progressValues.at(-1)).toBe("");
        expect(progressPanel.hidden).toBe(true);

        if (outcome === "success") {
          resolveCloud({
            detectedOrderId: "ORDER-OLD",
            numbers: [{
              original: "0007",
              canonical: "7",
              confidence: 0.9,
              confirmed: false,
            }],
            rawText: "0007",
            method: "cloud",
          });
        } else {
          rejectCloud(new Error("old cloud failed"));
        }
        await importTask;

        expect(state.inputs[0]?.persistedId).toBe("persisted-confirm");
        expect(state.inputImportBusy).toBe(false);
        expect(state.inputImportRequest).toBeUndefined();
        expect(state.numbersConfirmed).toBe(true);
        expect(state.recognitionNote).toBeUndefined();
        expect(state.modal).toBeUndefined();
        expect(persistRecognizedNumbers).not.toHaveBeenCalled();
        expect(renderState).toHaveBeenCalledTimes(2);
        expect(progressValues).not.toContain("正在使用云端模型识别 1/1");
      } finally {
        vi.restoreAllMocks();
        vi.unstubAllGlobals();
      }
    },
  );
});

describe("recognition image batch limits", () => {
  const image = (size: number, index: number) => ({
    name: `customer-${index}.png`,
    type: "image/png",
    size,
  });

  it("accepts exactly 12 images and exactly 48 MiB", async () => {
    const files = Array.from({ length: MAX_RECOGNITION_IMAGES }, (_, index) =>
      image(MAX_RECOGNITION_BATCH_BYTES / MAX_RECOGNITION_IMAGES, index)) as File[];
    const action = vi.fn().mockResolvedValue("accepted");

    expect(validateRecognitionImageBatch(files)).toEqual({ ok: true });
    await expect(withValidatedRecognitionImageBatch(files, action)).resolves.toEqual({
      ok: true,
      value: "accepted",
    });
    expect(action).toHaveBeenCalledOnce();
  });

  it("rejects a thirteenth image before starting the import action", async () => {
    const files = Array.from({ length: MAX_RECOGNITION_IMAGES + 1 }, (_, index) =>
      image(1, index)) as File[];
    const action = vi.fn(async () => {
      await files[0]!.arrayBuffer();
      await bridge.saveSessionInput(
        "session-a",
        "customer.png",
        "image",
        new Uint8Array(),
      );
      await bridge.recognizeCloud("provider-a", "session-a", ["input-a"]);
    });

    const result = await withValidatedRecognitionImageBatch(files, action);

    expect(result).toMatchObject({ ok: false });
    expect(action).not.toHaveBeenCalled();
  });

  it("rejects one byte over 48 MiB before arrayBuffer, save, or cloud", async () => {
    const files = [
      image(16 * 1024 * 1024, 0),
      image(16 * 1024 * 1024, 1),
      image(16 * 1024 * 1024 + 1, 2),
    ] as File[];
    const arrayBuffer = vi.fn();
    Object.defineProperty(files[0], "arrayBuffer", { value: arrayBuffer });
    const save = vi.spyOn(bridge, "saveSessionInput");
    const cloud = vi.spyOn(bridge, "recognizeCloud");
    const action = vi.fn(async () => {
      await files[0]!.arrayBuffer();
      await bridge.saveSessionInput(
        "session-a",
        "customer.png",
        "image",
        new Uint8Array(),
      );
      await bridge.recognizeCloud("provider-a", "session-a", ["input-a"]);
    });

    const result = await withValidatedRecognitionImageBatch(files, action);

    expect(result).toMatchObject({ ok: false });
    expect(arrayBuffer).not.toHaveBeenCalled();
    expect(save).not.toHaveBeenCalled();
    expect(cloud).not.toHaveBeenCalled();
    save.mockRestore();
    cloud.mockRestore();
  });

  it("keeps per-file partial validation when the batch itself is within bounds", () => {
    const files = [
      image(1024, 0),
      { name: "not-an-image.txt", type: "text/plain", size: 12 },
      image(MAX_SCREENSHOT_BYTES + 1, 2),
    ];

    expect(validateRecognitionImageBatch(files)).toEqual({ ok: true });
    expect(files.map(validateScreenshotFile).map(result => result.ok)).toEqual([
      true,
      false,
      false,
    ]);
  });

  it("counts only valid candidates when 12 images are accompanied by a text file", () => {
    const files = [
      ...Array.from({ length: MAX_RECOGNITION_IMAGES }, (_, index) =>
        image(1024, index)),
      { name: "notes.txt", type: "text/plain", size: 999_999_999 },
    ] as File[];

    const result = preflightScreenshotImportBatch(files);

    expect(result.ok).toBe(true);
    expect(result.validFiles).toHaveLength(MAX_RECOGNITION_IMAGES);
    expect(result.statuses.at(-1)).toMatchObject({
      name: "notes.txt",
      state: "failed",
    });
  });

  it("keeps a valid image when its oversized sibling fails per-file validation", () => {
    const valid = image(1024, 0) as File;
    const oversized = image(MAX_SCREENSHOT_BYTES + 1, 1) as File;

    const result = preflightScreenshotImportBatch([valid, oversized]);

    expect(result.ok).toBe(true);
    expect(result.validFiles).toEqual([valid]);
    expect(result.statuses).toEqual([
      { name: valid.name, state: "pending" },
      expect.objectContaining({ name: oversized.name, state: "failed" }),
    ]);
  });

  it("does not start an import when every candidate is individually invalid", async () => {
    const files = [
      { name: "notes.txt", type: "text/plain", size: 20 },
      image(0, 1),
      image(MAX_SCREENSHOT_BYTES + 1, 2),
    ] as File[];
    const action = vi.fn();

    const result = await withValidatedRecognitionImageBatch(files, action);

    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("invalid-only batch unexpectedly started");
    expect(result.statuses).toHaveLength(3);
    expect(result.statuses.every(status => status.state === "failed")).toBe(true);
    expect(action).not.toHaveBeenCalled();
  });

  it("accepts valid candidates at 48 MiB even with an invalid sibling", () => {
    const first = image(MAX_SCREENSHOT_BYTES, 0) as File;
    const second = image(MAX_SCREENSHOT_BYTES, 1) as File;
    const invalid = {
      name: "renamed.txt",
      type: "text/plain",
      size: MAX_RECOGNITION_BATCH_BYTES,
    } as File;

    const result = preflightScreenshotImportBatch([first, invalid, second]);

    expect(result.ok).toBe(true);
    expect(result.validFiles).toEqual([first, second]);
    expect(result.statuses[1]).toMatchObject({
      name: "renamed.txt",
      state: "failed",
    });
  });
});

describe("screenshot batch persistence", () => {
  it("keeps valid siblings and continues after the middle save fails", async () => {
    const files = [
      { name: "first.jpg" },
      { name: "broken.jpg" },
      { name: "third.jpg" },
    ] as File[];
    const save = vi.fn(async (file: File) => {
      if (file.name === "broken.jpg") throw new Error("disk write failed");
      return {
        id: `saved-${file.name}`,
        kind: "image" as const,
        name: file.name,
        blob: file,
      };
    });

    const result = await persistScreenshotBatch(files, save);

    expect(save.mock.calls.map(([file]) => file.name)).toEqual([
      "first.jpg",
      "broken.jpg",
      "third.jpg",
    ]);
    expect(result.saved.map(input => input.name)).toEqual([
      "first.jpg",
      "third.jpg",
    ]);
    expect(result.failed).toEqual([
      {
        name: "broken.jpg",
        state: "failed",
        message: "disk write failed",
      },
    ]);
  });

  it("keeps same-named files distinct when the first fails and the second saves", async () => {
    const first = { name: "same.jpg" } as File;
    const second = { name: "same.jpg" } as File;
    const save = vi.fn(async (file: File) => {
      if (file === first) throw new Error("first failed");
      return {
        id: "saved-second",
        kind: "image" as const,
        name: file.name,
        blob: file,
      };
    });

    const result = await persistScreenshotBatch([first, second], save);

    expect(result.saved).toHaveLength(1);
    expect(result.saved[0]?.id).toBe("saved-second");
    expect(result.failed).toEqual([
      { name: "same.jpg", state: "failed", message: "first failed" },
    ]);
    expect(save).toHaveBeenNthCalledWith(2, second);
  });

  it("lets a new session import while a stale import settles without clearing the new status", () => {
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      inputImportBusy: false,
      inputImportStatuses: [],
    } as unknown as AppState;
    const oldRequest = beginInputImport(state, "session-a", [
      { name: "old.jpg", state: "pending" },
    ]);
    expect(oldRequest).toBeDefined();

    beginSessionSwitch(state, "session-b");
    state.activeSession = { id: "session-b" } as AppState["activeSession"];
    const newRequest = beginInputImport(state, "session-b", [
      { name: "same.jpg", state: "pending" },
      { name: "same.jpg", state: "pending" },
    ]);
    expect(newRequest).toBeDefined();
    expect(updateInputImportStatus(state, newRequest!, 0, {
      name: "same.jpg",
      state: "failed",
      message: "first failed",
    })).toBe(true);
    expect(updateInputImportStatus(state, newRequest!, 1, {
      name: "same.jpg",
      state: "saved",
    })).toBe(true);

    expect(updateInputImportStatus(state, oldRequest!, 0, {
      name: "old.jpg",
      state: "failed",
      message: "late failure",
    })).toBe(false);
    expect(finishInputImport(state, oldRequest!)).toBe(false);
    expect(state.inputImportBusy).toBe(true);
    expect(state.inputImportStatuses).toEqual([
      { name: "same.jpg", state: "failed", message: "first failed" },
      { name: "same.jpg", state: "saved" },
    ]);
  });
});

describe("persisted cloud recognition contract", () => {
  const settings = {
    recognitionMode: "cloud",
    defaultProviderId: "provider-a",
    cloudFallbackOffline: false,
    secondConfirmationEnabled: false,
    extensions: [] as string[],
  } as const;
  const persistedRetryState = (
    sessionId: string,
    sizes: number[],
    retrySettings: AppState["settings"] = settings,
  ) => ({
    activeSession: {
      id: sessionId,
      taskLabel: sessionId,
      note: null,
      status: "draft" as const,
      createdAt: "now",
      numbersConfirmed: false,
    },
    copyRuntimeSessionId: sessionId,
    workflowGeneration: 1,
    copyAttemptGeneration: 1,
    inputs: sizes.map((size, index) => ({
      id: `input-${index}`,
      persistedId: `input-${index}`,
      kind: "image" as const,
      name: `${index}.png`,
      mime: "image/png",
      size,
    })),
    settings: retrySettings,
    detectedOrderId: null,
    draftNumbers: [],
    numbersConfirmed: false,
    blockingIssues: [],
    copyActive: false,
  }) as unknown as AppState;

  it("sends only the active session and persisted image IDs to cloud recognition", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockResolvedValue({
      detectedOrderId: null,
      numbers: [],
      rawText: "",
      method: "cloud",
    });
    const first = new Blob(["first"], { type: "image/png" });
    const second = new Blob(["second"], { type: "image/png" });
    const onProgress = vi.fn();

    await recognizeWithFallback(
      settings,
      "session-current",
      [
        { inputId: "input-first", blob: first, size: first.size },
        { inputId: "input-second", blob: second, size: second.size },
      ],
      onProgress,
    );

    expect(cloud).toHaveBeenCalledWith(
      "provider-a",
      "session-current",
      ["input-first", "input-second"],
    );
    expect(onProgress.mock.calls).toEqual([[0, 2], [2, 2]]);
    cloud.mockRestore();
  });

  it("retries cloud recognition from persisted IDs in the active session", async () => {
    let resolveCloud!: (value: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
      () => new Promise(resolve => {
        resolveCloud = resolve;
      }),
    );
    const cloudResult = {
      detectedOrderId: null,
      numbers: [{
        original: "IMG_0007.JPG",
        canonical: "7",
        confidence: 0.9,
        confirmed: false,
      }],
      rawText: "",
      method: "cloud" as const,
    };
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const blob = new Blob(["persisted"], { type: "image/png" });
    const state = {
      activeSession: {
        id: "session-current",
        taskLabel: "current",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-current",
      workflowGeneration: 1,
      inputs: [{
        id: "input-current",
        persistedId: "input-current",
        kind: "image",
        name: "customer.png",
        blob,
        size: blob.size,
      }],
      settings,
      draftNumbers: [],
      blockingIssues: [],
      copyActive: false,
    } as unknown as AppState;
    const progressValues: string[] = [];
    vi.stubGlobal("document", {
      querySelector: (selector: string) =>
        selector === "#recognition-progress"
          ? {
              hidden: false,
              set textContent(value: string | null) {
                progressValues.push(value ?? "");
              },
            }
          : null,
    });

    try {
      const retry = retryPersistedRecognition(state);
      expect(state.recognitionProgress).toBe("正在使用云端模型识别 0/1");
      resolveCloud(cloudResult);
      await retry;

      expect(cloud).toHaveBeenCalledWith(
        "provider-a",
        "session-current",
        ["input-current"],
      );
      expect(state.draftNumbers.map(number => number.canonical)).toEqual(["7"]);
      expect(progressValues).toContain("正在使用云端模型识别 0/1");
      expect(progressValues).toContain("正在使用云端模型识别 1/1");
      expect(state.recognitionProgress).toBeUndefined();
    } finally {
      cloud.mockRestore();
      saveNumbers.mockRestore();
      vi.unstubAllGlobals();
    }
  });

  it("retries more than 12 persisted images in bounded sequential cloud chunks", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
      async (_providerId, _sessionId, inputIds) => ({
        detectedOrderId: null,
        numbers: inputIds.map(inputId => {
          const value = String(Number(inputId.split("-").at(-1)) + 1);
          return {
            original: value.padStart(4, "0"),
            canonical: value,
            confidence: 0.9,
            confirmed: false,
          };
        }),
        rawText: inputIds.join(" "),
        method: "cloud",
      }),
    );
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const state = {
      activeSession: {
        id: "session-chunks",
        taskLabel: "chunks",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-chunks",
      workflowGeneration: 1,
      copyAttemptGeneration: 1,
      inputs: Array.from({ length: 13 }, (_, index) => ({
        id: `input-${index}`,
        persistedId: `input-${index}`,
        kind: "image" as const,
        name: `${index}.png`,
        mime: "image/png",
        size: 1,
      })),
      settings,
      detectedOrderId: null,
      draftNumbers: [],
      numbersConfirmed: false,
      blockingIssues: [],
      copyActive: false,
    } as unknown as AppState;

    try {
      await expect(retryPersistedRecognition(state)).resolves.toBe(true);
      expect(cloud.mock.calls.map(([, , ids]) => ids.length)).toEqual([12, 1]);
      expect(state.draftNumbers.map(number => number.canonical)).toEqual(
        Array.from({ length: 13 }, (_, index) => String(index + 1)),
      );
      expect(saveNumbers).toHaveBeenCalledOnce();
    } finally {
      vi.restoreAllMocks();
    }
  });

  it("splits a cumulative batch over 48 MiB without exceeding a provider call limit", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockResolvedValue({
      detectedOrderId: null,
      numbers: [],
      rawText: "",
      method: "cloud",
    });
    vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const state = persistedRetryState(
      "session-byte-chunks",
      [24 * 1024 * 1024, 24 * 1024 * 1024, 24 * 1024 * 1024],
    );

    try {
      await expect(retryPersistedRecognition(state)).resolves.toBe(true);
      expect(cloud.mock.calls.map(([, , ids]) => ids)).toEqual([
        ["input-0", "input-1"],
        ["input-2"],
      ]);
    } finally {
      vi.restoreAllMocks();
    }
  });

  it("loads metadata-only images one bounded chunk at a time for cloud fallback offline", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockRejectedValue(
      new Error("OFFLINE_FALLBACK_REQUIRED:timeout"),
    );
    const read = vi.spyOn(bridge, "readSessionInput").mockResolvedValue(
      new Uint8Array([1]),
    );
    vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    recognize.mockClear();
    recognize.mockResolvedValue({ data: { text: "0007", confidence: 90 } });
    createWorker.mockResolvedValue({ recognize });
    const state = persistedRetryState(
      "session-fallback-chunks",
      Array.from({ length: 13 }, () => 1),
      { ...settings, cloudFallbackOffline: true },
    );

    try {
      await expect(retryPersistedRecognition(state)).resolves.toBe(true);
      expect(cloud.mock.calls.map(([, , ids]) => ids.length)).toEqual([12, 1]);
      expect(read).toHaveBeenCalledTimes(13);
      expect(recognize).toHaveBeenCalledTimes(13);
      expect(state.draftNumbers.map(number => number.canonical)).toEqual(["7"]);
    } finally {
      vi.restoreAllMocks();
    }
  });

  it("does not apply earlier chunks when a later chunk fails", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud")
      .mockResolvedValueOnce({
        detectedOrderId: "ORDER-PARTIAL",
        numbers: [{
          original: "0007",
          canonical: "7",
          confidence: 0.9,
          confirmed: false,
        }],
        rawText: "0007",
        method: "cloud",
      })
      .mockRejectedValueOnce(new Error("second chunk failed"));
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const state = persistedRetryState(
      "session-chunk-failure",
      Array.from({ length: 13 }, () => 1),
    );
    state.detectedOrderId = "ORDER-MANUAL";
    state.draftNumbers = [{
      original: "人工0042",
      canonical: "42",
      confidence: null,
      confirmed: false,
    }];

    try {
      await expect(retryPersistedRecognition(state)).rejects.toThrow(
        "second chunk failed",
      );
      expect(state.detectedOrderId).toBe("ORDER-MANUAL");
      expect(state.draftNumbers).toEqual([{
        original: "人工0042",
        canonical: "42",
        confidence: null,
        confirmed: false,
      }]);
      expect(saveNumbers).not.toHaveBeenCalled();
    } finally {
      vi.restoreAllMocks();
    }
  });

  it("silently retires a multi-chunk retry when manual editing wins during the second chunk", async () => {
    let resolveSecond!: (
      draft: Awaited<ReturnType<typeof bridge.recognizeCloud>>,
    ) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud")
      .mockResolvedValueOnce({
        detectedOrderId: null,
        numbers: [{
          original: "0007",
          canonical: "7",
          confidence: 0.9,
          confirmed: false,
        }],
        rawText: "0007",
        method: "cloud",
      })
      .mockImplementationOnce(
        () => new Promise(resolve => {
          resolveSecond = resolve;
        }),
      );
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const state = persistedRetryState(
      "session-chunk-race",
      Array.from({ length: 13 }, () => 1),
    );

    try {
      const retry = retryPersistedRecognition(state);
      await vi.waitFor(() => expect(cloud).toHaveBeenCalledTimes(2));
      mutateNumberDraft(
        state,
        numbers => numbers.push({
          original: "人工0099",
          canonical: "99",
          confidence: null,
          confirmed: false,
        }),
        async () => undefined,
      );
      state.recognitionNote = "人工编辑已保存";
      resolveSecond({
        detectedOrderId: "ORDER-STALE",
        numbers: [{
          original: "0008",
          canonical: "8",
          confidence: 0.9,
          confirmed: false,
        }],
        rawText: "0008",
        method: "cloud",
      });

      await expect(retry).resolves.toBe(false);
      expect(state.draftNumbers.map(number => number.canonical)).toEqual(["99"]);
      expect(state.recognitionNote).toBe("人工编辑已保存");
      expect(saveNumbers).not.toHaveBeenCalled();
    } finally {
      vi.restoreAllMocks();
    }
  });

  it.each(["success", "failure"] as const)(
    "confirmation immediately retires an in-flight toolbar retry %s",
    async outcome => {
      let resolveCloud!: (value: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
      let rejectCloud!: (reason: Error) => void;
      const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
        () => new Promise((resolve, reject) => {
          resolveCloud = resolve;
          rejectCloud = reject;
        }),
      );
      const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
      const progressValues: string[] = [];
      const progressPanel = {
        hidden: false,
        set textContent(value: string | null) {
          progressValues.push(value ?? "");
        },
      };
      vi.stubGlobal("document", {
        querySelector: (selector: string) =>
          selector === "#recognition-progress" ? progressPanel : null,
      });
      const rerender = vi.fn();
      const persistConfirmation = vi.fn().mockResolvedValue(true);
      const blob = new Blob(["persisted"], { type: "image/png" });
      const state = {
        activeSession: {
          id: "session-retry-confirm",
          taskLabel: "retry confirm",
          note: null,
          status: "draft",
          createdAt: "now",
          numbersConfirmed: false,
        },
        copyRuntimeSessionId: "session-retry-confirm",
        workflowGeneration: 1,
        copyAttemptGeneration: 1,
        inputs: [{
          id: "input-retry-confirm",
          persistedId: "input-retry-confirm",
          kind: "image",
          name: "retry.png",
          blob,
          size: blob.size,
        }],
        settings,
        detectedOrderId: null,
        draftNumbers: [{
          original: "0099",
          canonical: "99",
          confidence: null,
          confirmed: false,
        }],
        numbersConfirmed: false,
        blockingIssues: [],
        copyActive: false,
      } as unknown as AppState;

      try {
        const handler = handleToolbarRecognitionRetry(
          state,
          retryPersistedRecognition,
          rerender,
        );
        await vi.waitFor(() => expect(cloud).toHaveBeenCalledOnce());
        expect(state.recognitionProgress).toBe("正在使用云端模型识别 0/1");
        expect(state.pendingRecognitionRetry).toBeDefined();

        const confirmation = beginNumberConfirmation(
          state,
          true,
          persistConfirmation,
        )!;
        await confirmation.persistence;
        expect(state.pendingRecognitionRetry).toBeUndefined();
        expect(state.recognitionProgress).toBeUndefined();
        expect(progressValues.at(-1)).toBe("");
        expect(progressPanel.hidden).toBe(true);

        if (outcome === "success") {
          resolveCloud({
            detectedOrderId: "ORDER-OLD",
            numbers: [{
              original: "0007",
              canonical: "7",
              confidence: 0.9,
              confirmed: false,
            }],
            rawText: "0007",
            method: "cloud",
          });
        } else {
          rejectCloud(new Error("old retry failed"));
        }
        await expect(handler).resolves.toBe(false);

        expect(state.draftNumbers.map(number => number.canonical)).toEqual(["99"]);
        expect(state.numbersConfirmed).toBe(true);
        expect(state.recognitionNote).toBeUndefined();
        expect(state.modal).toBeUndefined();
        expect(saveNumbers).not.toHaveBeenCalled();
        expect(rerender).not.toHaveBeenCalled();
        expect(progressValues).not.toContain("正在使用云端模型识别 1/1");
      } finally {
        vi.restoreAllMocks();
        vi.unstubAllGlobals();
      }
    },
  );

  it("uses the original blobs sequentially when cloud falls back offline", async () => {
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockRejectedValue(
      new Error("OFFLINE_FALLBACK_REQUIRED:timeout"),
    );
    recognize.mockResolvedValue({ data: { text: "0007", confidence: 90 } });
    createWorker.mockResolvedValue({ recognize });
    const blob = new Blob(["persisted"], { type: "image/png" });
    const onProgress = vi.fn();
    const onStage = vi.fn();

    const result = await recognizeWithFallback(
      { ...settings, cloudFallbackOffline: true },
      "session-current",
      [{ inputId: "input-current", blob, size: blob.size }],
      onProgress,
      onStage,
    );

    expect(recognize).toHaveBeenCalledWith(blob);
    expect(onProgress).toHaveBeenCalledWith(1, 1);
    expect(onStage.mock.calls.map(([stage]) => stage)).toEqual([
      "cloud",
      "fallback",
    ]);
    expect(result.method).toBe("offline");
    cloud.mockRestore();
  });

  it("never enters offline fallback when an ordinary cloud failure settles after the request became stale", async () => {
    let rejectCloud!: (reason: Error) => void;
    let current = true;
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
      () => new Promise((_, reject) => {
        rejectCloud = reject;
      }),
    );
    const blob = new Blob(["persisted"], { type: "image/png" });
    const onStage = vi.fn();
    const workerCallsBefore = createWorker.mock.calls.length;
    const recognizeCallsBefore = recognize.mock.calls.length;

    try {
      const recognition = recognizeWithFallback(
        { ...settings, cloudFallbackOffline: true },
        "session-stale-failure",
        [{ inputId: "input-stale-failure", blob, size: blob.size }],
        vi.fn(),
        onStage,
        () => current,
      );
      await vi.waitFor(() => expect(cloud).toHaveBeenCalledOnce());

      current = false;
      rejectCloud(new Error("network timeout"));

      await expect(recognition).rejects.toThrow("识别任务已失效");
      expect(onStage.mock.calls.map(([stage]) => stage)).toEqual(["cloud"]);
      expect(createWorker.mock.calls.length).toBe(workerCallsBefore);
      expect(recognize.mock.calls.length).toBe(recognizeCallsBefore);
    } finally {
      cloud.mockRestore();
    }
  });

  it("silently ignores a stale cloud success after switching sessions", async () => {
    let resolveCloud!: (draft: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
      () => new Promise(resolve => { resolveCloud = resolve; }),
    );
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const blob = new Blob(["persisted"], { type: "image/png" });
    const state = {
      activeSession: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      inputs: [{
        id: "input-a",
        persistedId: "input-a",
        kind: "image",
        name: "a.png",
        blob,
        size: blob.size,
      }],
      settings,
      draftNumbers: [],
      blockingIssues: [],
      copyActive: false,
      recognitionNote: "A pending",
    } as unknown as AppState;

    const retry = retryPersistedRecognition(state);
    beginSessionSwitch(state, "session-b");
    state.activeSession = {
      id: "session-b",
      taskLabel: "B",
      note: null,
      status: "draft",
      createdAt: "now",
      numbersConfirmed: false,
    };
    state.inputs = [];
    state.draftNumbers = [{
      original: "0099",
      canonical: "99",
      confidence: null,
      confirmed: false,
    }];
    state.recognitionNote = "B manual";
    resolveCloud({
      detectedOrderId: "OLD-A",
      numbers: [{
        original: "0007",
        canonical: "7",
        confidence: 0.9,
        confirmed: false,
      }],
      rawText: "",
      method: "cloud",
    });

    await expect(retry).resolves.toBe(false);
    expect(state.activeSession.id).toBe("session-b");
    expect(state.draftNumbers.map(number => number.canonical)).toEqual(["99"]);
    expect(state.recognitionNote).toBe("B manual");
    expect(saveNumbers).not.toHaveBeenCalled();
    cloud.mockRestore();
    saveNumbers.mockRestore();
  });

  it("silently ignores a stale cloud failure after same-session manual editing", async () => {
    let rejectCloud!: (error: Error) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud").mockImplementation(
      () => new Promise((_resolve, reject) => { rejectCloud = reject; }),
    );
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const blob = new Blob(["persisted"], { type: "image/png" });
    const state = {
      activeSession: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      inputs: [{
        id: "input-a",
        persistedId: "input-a",
        kind: "image",
        name: "a.png",
        blob,
        size: blob.size,
      }],
      settings,
      draftNumbers: [],
      blockingIssues: [],
      copyActive: false,
      recognitionNote: "retry pending",
    } as unknown as AppState;

    const retry = retryPersistedRecognition(state);
    mutateNumberDraft(
      state,
      numbers => numbers.push({
        original: "0042",
        canonical: "42",
        confidence: null,
        confirmed: false,
      }),
      async () => undefined,
    );
    state.recognitionNote = "manual edit";
    rejectCloud(new Error("old cloud failed"));

    await expect(retry).resolves.toBe(false);
    expect(state.draftNumbers.map(number => number.canonical)).toEqual(["42"]);
    expect(state.recognitionNote).toBe("manual edit");
    expect(state.pendingRecognitionRetry).toBeUndefined();
    expect(saveNumbers).not.toHaveBeenCalled();
    cloud.mockRestore();
    saveNumbers.mockRestore();
  });

  it("lets a newer retry supersede an older retry in the same session", async () => {
    let resolveOld!: (draft: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud")
      .mockImplementationOnce(() => new Promise(resolve => { resolveOld = resolve; }))
      .mockResolvedValueOnce({
        detectedOrderId: "NEW",
        numbers: [{
          original: "0088",
          canonical: "88",
          confidence: 0.9,
          confirmed: false,
        }],
        rawText: "",
        method: "cloud",
      });
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const blob = new Blob(["persisted"], { type: "image/png" });
    const state = {
      activeSession: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      inputs: [{
        id: "input-a",
        persistedId: "input-a",
        kind: "image",
        name: "a.png",
        blob,
        size: blob.size,
      }],
      settings,
      draftNumbers: [],
      blockingIssues: [],
      copyActive: false,
    } as unknown as AppState;

    const oldRetry = retryPersistedRecognition(state);
    const newRetry = retryPersistedRecognition(state);
    await expect(newRetry).resolves.toBe(true);
    resolveOld({
      detectedOrderId: "OLD",
      numbers: [{
        original: "0007",
        canonical: "7",
        confidence: 0.9,
        confirmed: false,
      }],
      rawText: "",
      method: "cloud",
    });

    await expect(oldRetry).resolves.toBe(false);
    expect(state.detectedOrderId).toBe("NEW");
    expect(state.draftNumbers.map(number => number.canonical)).toEqual(["88"]);
    expect(saveNumbers).toHaveBeenCalledOnce();
    cloud.mockRestore();
    saveNumbers.mockRestore();
  });

  it("does not render a stale toolbar retry", async () => {
    const state = {
      copyActive: false,
    } as unknown as AppState;
    const retry = vi.fn().mockResolvedValue(false);
    const rerender = vi.fn();

    await expect(
      handleToolbarRecognitionRetry(state, retry, rerender),
    ).resolves.toBe(false);

    expect(rerender).not.toHaveBeenCalled();
  });

  it("keeps a newer toolbar failure modal when an older modal retry settles stale", async () => {
    let resolveOld!: (draft: Awaited<ReturnType<typeof bridge.recognizeCloud>>) => void;
    const cloud = vi.spyOn(bridge, "recognizeCloud")
      .mockImplementationOnce(() => new Promise(resolve => { resolveOld = resolve; }))
      .mockRejectedValueOnce(new Error("newer cloud failed"));
    const saveNumbers = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const blob = new Blob(["persisted"], { type: "image/png" });
    const state = {
      activeSession: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: false,
      },
      copyRuntimeSessionId: "session-a",
      workflowGeneration: 1,
      inputs: [{
        id: "input-a",
        persistedId: "input-a",
        kind: "image",
        name: "a.png",
        blob,
        size: blob.size,
      }],
      settings,
      draftNumbers: [],
      blockingIssues: [],
      copyActive: false,
      modal: {
        issue: "partial-format",
        title: "旧识别失败",
        message: "old",
        affected: [],
        actions: ["retryRecognition"],
      },
    } as unknown as AppState;
    const dialog = { close: vi.fn() } as unknown as HTMLDialogElement;
    const rerender = vi.fn();

    const oldModalRetry = handleModalRecognitionRetry(
      state,
      dialog,
      retryPersistedRecognition,
      rerender,
    );
    await expect(handleToolbarRecognitionRetry(
      state,
      retryPersistedRecognition,
      rerender,
    )).resolves.toBe(false);
    expect(state.modal?.message).toBe("newer cloud failed");
    expect(rerender).toHaveBeenCalledOnce();

    resolveOld({
      detectedOrderId: "OLD",
      numbers: [{
        original: "0007",
        canonical: "7",
        confidence: 0.9,
        confirmed: false,
      }],
      rawText: "",
      method: "cloud",
    });
    await expect(oldModalRetry).resolves.toBe(false);

    expect(state.modal?.message).toBe("newer cloud failed");
    expect(dialog.close).not.toHaveBeenCalled();
    expect(rerender).toHaveBeenCalledOnce();
    expect(saveNumbers).not.toHaveBeenCalled();
    cloud.mockRestore();
    saveNumbers.mockRestore();
  });
});

describe("screenshot import controls", () => {
  class FakeClassList {
    private readonly values = new Set<string>();

    add(value: string) {
      this.values.add(value);
    }

    remove(value: string) {
      this.values.delete(value);
    }

    contains(value: string) {
      return this.values.has(value);
    }
  }

  class FakeControl extends EventTarget {
    readonly classList = new FakeClassList();
    disabled = false;
    value = "selected";
    files: File[] | null = null;
    click = vi.fn();
  }

  const eventWith = <T extends Event>(
    type: string,
    properties: Record<string, unknown> = {},
  ) => {
    const event = new Event(type, { bubbles: true, cancelable: true });
    for (const [key, value] of Object.entries(properties)) {
      Object.defineProperty(event, key, { value });
    }
    return event as T;
  };

  it("renders one hidden multiple image picker and a visible choose button", () => {
    const markup = renderScreenshotImportControls({
      inputImportBusy: false,
      inputImportStatuses: [],
    });

    expect(markup).toContain('id="screenshot-picker"');
    expect(markup).toContain('type="file"');
    expect(markup).toContain(`accept="${SCREENSHOT_ACCEPT}"`);
    expect(markup).toContain("multiple");
    expect(markup).toContain("hidden");
    expect(markup).toContain('id="choose-screenshots"');
    expect(markup).toContain("选择图片");
  });

  it.each(["Enter", " "])("%s opens the picker", key => {
    const dropzone = new FakeControl();
    const picker = new FakeControl();
    const chooseButton = new FakeControl();

    bindScreenshotImportControls(
      {
        dropzone: dropzone as unknown as HTMLElement,
        picker: picker as unknown as HTMLInputElement,
        chooseButton: chooseButton as unknown as HTMLButtonElement,
      },
      vi.fn(),
      () => false,
    );
    dropzone.dispatchEvent(eventWith<KeyboardEvent>("keydown", { key }));

    expect(picker.click).toHaveBeenCalledOnce();
  });

  it("the visible choose button opens the same picker", () => {
    const dropzone = new FakeControl();
    const picker = new FakeControl();
    const chooseButton = new FakeControl();

    bindScreenshotImportControls(
      {
        dropzone: dropzone as unknown as HTMLElement,
        picker: picker as unknown as HTMLInputElement,
        chooseButton: chooseButton as unknown as HTMLButtonElement,
      },
      vi.fn(),
      () => false,
    );
    chooseButton.dispatchEvent(eventWith("click"));

    expect(picker.click).toHaveBeenCalledOnce();
  });

  it("clicking the dropzone opens the picker", () => {
    const dropzone = new FakeControl();
    const picker = new FakeControl();
    const chooseButton = new FakeControl();

    bindScreenshotImportControls(
      {
        dropzone: dropzone as unknown as HTMLElement,
        picker: picker as unknown as HTMLInputElement,
        chooseButton: chooseButton as unknown as HTMLButtonElement,
      },
      vi.fn(),
      () => false,
    );
    dropzone.dispatchEvent(eventWith("click"));

    expect(picker.click).toHaveBeenCalledOnce();
  });

  it("picker changes and drops pass files through one importer and allow reselection", () => {
    const dropzone = new FakeControl();
    const picker = new FakeControl();
    const chooseButton = new FakeControl();
    const first = new File(["first"], "first.jpg", { type: "image/jpeg" });
    const second = new File(["second"], "second.png", { type: "image/png" });
    const importImageFiles = vi.fn();

    bindScreenshotImportControls(
      {
        dropzone: dropzone as unknown as HTMLElement,
        picker: picker as unknown as HTMLInputElement,
        chooseButton: chooseButton as unknown as HTMLButtonElement,
      },
      importImageFiles,
      () => false,
    );
    picker.files = [first];
    picker.dispatchEvent(eventWith("change"));
    picker.value = "selected-again";
    picker.dispatchEvent(eventWith("change"));
    const drop = eventWith<DragEvent>("drop", {
      dataTransfer: { files: [second] },
    });
    dropzone.dispatchEvent(drop);

    expect(importImageFiles).toHaveBeenNthCalledWith(1, [first]);
    expect(importImageFiles).toHaveBeenNthCalledWith(2, [first]);
    expect(importImageFiles).toHaveBeenNthCalledWith(3, [second]);
    expect(picker.value).toBe("");
    expect(drop.defaultPrevented).toBe(true);
  });

  it("uses drag depth to avoid child flicker and clears highlight on leave and drop", () => {
    const dropzone = new FakeControl();
    const picker = new FakeControl();
    const chooseButton = new FakeControl();

    bindScreenshotImportControls(
      {
        dropzone: dropzone as unknown as HTMLElement,
        picker: picker as unknown as HTMLInputElement,
        chooseButton: chooseButton as unknown as HTMLButtonElement,
      },
      vi.fn(),
      () => false,
    );
    dropzone.dispatchEvent(eventWith("dragenter"));
    dropzone.dispatchEvent(eventWith("dragenter"));
    dropzone.dispatchEvent(eventWith("dragleave"));
    expect(dropzone.classList.contains("is-dragging")).toBe(true);
    dropzone.dispatchEvent(eventWith("dragleave"));
    expect(dropzone.classList.contains("is-dragging")).toBe(false);

    dropzone.dispatchEvent(eventWith("dragenter"));
    dropzone.dispatchEvent(eventWith<DragEvent>("drop", {
      dataTransfer: { files: [] },
    }));
    expect(dropzone.classList.contains("is-dragging")).toBe(false);
  });

  it("prevents browser navigation and ignores duplicate submissions while busy", () => {
    const dropzone = new FakeControl();
    const picker = new FakeControl();
    const chooseButton = new FakeControl();
    const safetyTarget = new EventTarget();
    const importImageFiles = vi.fn();

    bindScreenshotImportControls(
      {
        dropzone: dropzone as unknown as HTMLElement,
        picker: picker as unknown as HTMLInputElement,
        chooseButton: chooseButton as unknown as HTMLButtonElement,
        safetyTarget,
      },
      importImageFiles,
      () => true,
    );
    const outsideDrop = eventWith("drop");
    safetyTarget.dispatchEvent(outsideDrop);
    chooseButton.dispatchEvent(eventWith("click"));
    dropzone.dispatchEvent(eventWith<DragEvent>("drop", {
      dataTransfer: {
        files: [new File(["busy"], "busy.jpg", { type: "image/jpeg" })],
      },
    }));

    expect(outsideDrop.defaultPrevented).toBe(true);
    expect(picker.click).not.toHaveBeenCalled();
    expect(importImageFiles).not.toHaveBeenCalled();
  });

  it("shows the busy count and preserves visible failed statuses", () => {
    const markup = renderScreenshotImportControls({
      inputImportBusy: true,
      inputImportStatuses: [
        { name: "good.jpg", state: "pending" },
        { name: "already-saved.png", state: "saved" },
        { name: "bad.txt", state: "failed", message: "仅支持图片" },
      ],
    });

    expect(markup).toContain("is-busy");
    expect(markup).toContain("正在导入 1 张");
    expect(markup).toContain("already-saved.png");
    expect(markup).toContain("已导入");
    expect(markup).toContain("bad.txt");
    expect(markup).toContain("仅支持图片");
    expect(markup).toContain("导入失败");
  });
});

describe("parseRecognizedText", () => {
  it("keeps originals and canonicalizes leading zeroes", () => {
    const result = parseRecognizedText(
      "订单号 BD-240718\nIMG_01234.JPG\n0781\n0012",
      0.82,
    );

    expect(result.detectedOrderId).toBe("BD-240718");
    expect(result.numbers.map((number) => [number.original, number.canonical])).toEqual([
      ["IMG_01234.JPG", "1234"],
      ["0781", "781"],
      ["0012", "12"],
    ]);
    expect(result.numbers.every((number) => !number.confirmed)).toBe(true);
  });

  it("removes only the order-id span and keeps photo numbers on the same line", () => {
    const result = parseRecognizedText(
      "订单号 BD-240718，选片：0012、0013",
      0.91,
    );

    expect(result.detectedOrderId).toBe("BD-240718");
    expect(result.numbers.map(number => [number.original, number.canonical]))
      .toEqual([
        ["0012", "12"],
        ["0013", "13"],
      ]);
    expect(result.numbers.map(number => number.canonical)).not.toContain(
      "240718",
    );
  });

  it("supports a safe English order label and deduplicates by canonical number", () => {
    const result = parseRecognizedText(
      "Order ID: BD-240718, selections: 0012, 12, IMG_0013.JPG, 013",
    );

    expect(result.detectedOrderId).toBe("BD-240718");
    expect(result.numbers.map(number => [number.original, number.canonical]))
      .toEqual([
        ["0012", "12"],
        ["IMG_0013.JPG", "13"],
      ]);
  });

  it("ignores date, decimal, and compound-identifier fragments without losing valid IDs", () => {
    const result = parseRecognizedText(
      "拍摄日期 2024-07-18\n曝光 12.34\n批次 CLIENT_0012_0034\n客户编号 00781\nIMG_01234.JPG",
    );

    expect(result.numbers.map((number) => [number.original, number.canonical])).toEqual([
      ["00781", "781"],
      ["IMG_01234.JPG", "1234"],
    ]);
  });

  it("uses only packaged OCR paths and leaves OCR results unconfirmed", async () => {
    recognize.mockResolvedValue({ data: { text: "订单号 BD-240718\n0781", confidence: 91.7 } });
    createWorker.mockResolvedValue({ recognize });
    const onProgress = vi.fn();

    const result = await recognizeOffline([new Blob(["image"])], onProgress);

    expect(createWorker).toHaveBeenCalledWith(["chi_sim", "eng"], 1, {
      workerPath: "/ocr/worker.min.js",
      corePath: "/ocr/core",
      langPath: "/ocr/lang",
      gzip: true,
      workerBlobURL: false,
    });
    expect(onProgress).toHaveBeenCalledWith(1, 1);
    expect(result).toMatchObject({
      method: "offline",
      detectedOrderId: "BD-240718",
      numbers: [{ original: "0781", canonical: "781", confidence: 0.917, confirmed: false }],
    });
  });
});

describe("workbench safety gates", () => {
  it("renders cloud results as unconfirmed rows with the order ID only in its cross-check banner", async () => {
    const cloudDraft = {
      detectedOrderId: "BD-240718",
      numbers: [
        {
          original: "IMG_0012.JPG",
          canonical: "12",
          confidence: 0.98,
          confirmed: true,
        },
      ],
      rawText: "{\"order_id\":\"BD-240718\"}",
      method: "cloud" as const,
    };
    const state = {
      page: "workbench",
      activeSession: {
        id: "session-1",
        taskLabel: "客户选片",
        note: null,
        status: "readyToCopy",
        createdAt: "2026-07-25T00:00:00Z",
        numbersConfirmed: true,
      },
      sessions: [],
      inputs: [],
      detectedOrderId: "OLD-ORDER",
      draftNumbers: [{
        original: "IMG_0999.JPG",
        canonical: "999",
        confidence: 0.8,
        confirmed: true,
      }],
      numbersConfirmed: true,
      sourceDir: "D:/photos",
      targetDir: "D:/target",
      matchReport: {
        items: [],
        skippedNumbers: [],
        autoCopyStarted: false,
        requiresSecondConfirmation: true,
        confirmationToken: "old-token",
        copyJob: null,
      },
      blockingIssues: ["missing"],
      copyProgress: {
        jobId: "old-prep",
        currentFile: "old.jpg",
        completedFiles: 0,
        totalFiles: 1,
        copiedBytes: 0,
        totalBytes: 1,
      },
      completionReport: undefined,
      settings: {
        recognitionMode: "cloud",
        defaultProviderId: "provider-1",
        secondConfirmationEnabled: false,
        cloudFallbackOffline: false,
        extensions: [".jpg"],
      },
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
      copyActive: false,
    } as unknown as AppState;

    expect(applyRecognitionDraft(state, cloudDraft)).toBe(true);
    const markup = renderWorkbenchForTest(state);

    expect(state.draftNumbers).toEqual([
      expect.objectContaining({
        original: "IMG_0012.JPG",
        canonical: "12",
        confirmed: false,
      }),
    ]);
    expect(state.numbersConfirmed).toBe(false);
    expect(state.matchReport).toBeUndefined();
    expect(state.blockingIssues).toEqual([]);
    expect(state.copyProgress).toBeUndefined();
    expect(state.draftNumbers.map(number => number.original)).not.toContain("BD-240718");
    expect(markup).toContain("检测订单号 / BD-240718 · 仅核对用");
    expect(markup).toContain('value="IMG_0012.JPG"');
    expect(markup).toMatch(/id="confirm-numbers" type="checkbox"\s*>/);
    expect(markup).toContain('id="choose-source" type="button" disabled');
    expect(markup).not.toContain('id="scan"');
  });

  it("never allows copy before number confirmation", () => {
    expect(canStartCopy({ numbersConfirmed: false, secondConfirmationEnabled: false, blockingIssues: [] })).toBe(false);
  });
  it("blocks exceptions even when second confirmation is disabled", () => {
    expect(canStartCopy({ numbersConfirmed: true, secondConfirmationEnabled: false, blockingIssues: ["ambiguous"] })).toBe(false);
    expect(unresolvedIssues(["ambiguous", "target-conflict"])).toHaveLength(2);
  });
  it("requires confirmed IDs and native-bound directories before scanning", () => {
    expect(canScanDirectory({
      numbersConfirmed: false,
      sourceDir: "D:/photos",
      targetDir: "D:/照片成片/待精修的原片",
      copyActive: false,
    })).toBe(false);
    expect(canScanDirectory({
      numbersConfirmed: true,
      sourceDir: undefined,
      targetDir: undefined,
      copyActive: false,
    })).toBe(false);
    expect(canScanDirectory({
      numbersConfirmed: true,
      sourceDir: "D:/photos",
      targetDir: "D:/照片成片/待精修的原片",
      copyActive: false,
    })).toBe(true);
    expect(canScanDirectory({
      numbersConfirmed: true,
      sourceDir: "D:/photos",
      targetDir: "D:/target",
      copyActive: true,
    })).toBe(false);
  });

  it("shows live filename-scan activity and counts while the background scan runs", () => {
    const state = {
      page: "workbench",
      activeSession: {
        id: "session-1",
        taskLabel: "客户选片",
        note: null,
        status: "readyToScan",
        createdAt: "now",
        numbersConfirmed: true,
      },
      sessions: [],
      inputs: [],
      detectedOrderId: null,
      draftNumbers: [{ original: "7", canonical: "7", confidence: null, confirmed: true }],
      numbersConfirmed: true,
      sourceDir: "Z:/photos",
      targetDir: "Z:/photos/照片成片/待精修的原片",
      blockingIssues: [],
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
      copyActive: false,
      numberWorkflowCommandPending: "scan",
      scanProgress: {
        phase: "scanning",
        checkedFiles: 420,
        matchedFiles: 3,
        scannedDirectories: 2,
        elapsedMs: 1500,
      },
      settings: {
        recognitionMode: "offline",
        defaultProviderId: null,
        cloudFallbackOffline: true,
        secondConfirmationEnabled: true,
        extensions: ["JPG", "CR3"],
      },
    } as AppState & {
      scanProgress: {
        phase: "scanning";
        checkedFiles: number;
        matchedFiles: number;
        scannedDirectories: number;
        elapsedMs: number;
      };
    };

    const markup = renderWorkbenchForTest(state);

    expect(markup).toContain('id="scan-progress"');
    expect(markup).toContain("<progress");
    expect(markup).toContain("已检查 420 个文件");
    expect(markup).toContain("找到 3 个候选");
    expect(markup).toContain("1.5 秒");
  });

  it.each([
    ["an active copy", { copyActive: true, activeCopyJobId: "copy-job" }],
    ["a pending scan", { copyActive: false, numberWorkflowCommandPending: "scan" as const }],
    ["a pending copy launch", { copyActive: false, numberWorkflowCommandPending: "copyLaunch" as const }],
    ["a paused copy", { copyActive: false, pausedCopyJobId: "paused-job" }],
    ["a pending recheck", {
      copyActive: false,
      pendingRecheckRequest: { sessionId: "session-1", generation: 2 },
    }],
    ["a terminal copy event", { copyActive: false, terminalCopyJobId: "terminal-job" }],
    ["a pause without a job ID", {
      copyActive: false,
      modal: {
        issue: "source-disconnected" as const,
        title: "源目录已断开",
        message: "恢复后重试",
        affected: [],
        actions: ["recheck" as const, "cancel" as const],
      },
    }],
  ])("blocks every number mutation during %s without persistence or job retirement", (_label, lock) => {
    const makeState = () => ({
      page: "workbench",
      activeSession: {
        id: "session-1",
        taskLabel: "客户选片",
        note: null,
        status: "readyToCopy",
        createdAt: "2026-07-25T00:00:00Z",
        numbersConfirmed: true,
      },
      sessions: [],
      inputs: [],
      detectedOrderId: "BD-240718",
      draftNumbers: [
        {
          original: "IMG_0012.JPG",
          canonical: "12",
          confidence: 0.98,
          confirmed: true,
        },
      ],
      numbersConfirmed: true,
      sourceDir: "D:/photos",
      targetDir: "D:/target",
      matchReport: {
        items: [],
        skippedNumbers: [],
        autoCopyStarted: false,
        requiresSecondConfirmation: true,
        confirmationToken: "current-token",
        copyJob: null,
      },
      blockingIssues: [],
      settings: {
        recognitionMode: "cloud",
        defaultProviderId: "provider-1",
        secondConfirmationEnabled: true,
        cloudFallbackOffline: false,
        extensions: [".jpg"],
      },
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
      ...lock,
    }) as unknown as AppState;
    const persist = vi.fn(async () => true);

    for (const mutation of [
      (numbers: AppState["draftNumbers"]) => {
        numbers[0] = {
          original: "IMG_0013.JPG",
          canonical: "13",
          confidence: null,
          confirmed: false,
        };
      },
      (numbers: AppState["draftNumbers"]) => {
        numbers.splice(0, 1);
      },
      (numbers: AppState["draftNumbers"]) => {
        numbers.push({
          original: "",
          canonical: "",
          confidence: null,
          confirmed: false,
        });
      },
    ]) {
      const state = makeState();
      const before = structuredClone(state);
      expect(isNumberWorkflowLocked(state)).toBe(true);
      expect(mutateNumberDraft(state, mutation, persist)).toBeUndefined();
      expect(state).toEqual(before);
    }

    const confirmationState = makeState();
    const confirmationPersist = vi.fn(async () => true);
    const confirmationBefore = structuredClone(confirmationState);
    expect(beginNumberConfirmation(
      confirmationState,
      false,
      confirmationPersist,
    )).toBeUndefined();
    expect(confirmationState).toEqual(confirmationBefore);
    expect(persist).not.toHaveBeenCalled();
    expect(confirmationPersist).not.toHaveBeenCalled();

    const recognitionState = makeState();
    const recognitionBefore = structuredClone(recognitionState);
    expect(applyRecognitionDraft(recognitionState, {
      detectedOrderId: "NEW",
      numbers: [{
        original: "IMG_0099.JPG",
        canonical: "99",
        confidence: 0.9,
        confirmed: true,
      }],
      rawText: "",
      method: "cloud",
    })).toBe(false);
    expect(recognitionState).toEqual(recognitionBefore);

    const markup = renderWorkbenchForTest(makeState());
    expect(markup).toMatch(/data-number-index="0"[^>]*disabled/);
    expect(markup).toMatch(/data-delete-number="0"[^>]*disabled/);
    expect(markup).toMatch(/id="add-number"[^>]*disabled/);
    expect(markup).toMatch(/id="confirm-numbers"[^>]*disabled/);
    expect(markup).toMatch(/id="paste-input"[^>]*disabled/);
    expect(markup).toMatch(/id="choose-source"[^>]*disabled/);
  });

  it("keeps the active copy job current when a locked edit is rejected", () => {
    const persist = vi.fn(async () => true);
    const state = {
      activeSession: { id: "session-1", status: "copying" },
      draftNumbers: [{
        original: "IMG_0012.JPG",
        canonical: "12",
        confidence: 0.98,
        confirmed: true,
      }],
      numbersConfirmed: true,
      blockingIssues: [],
      copyActive: true,
      activeCopyJobId: "copy-job",
      copyAttemptGeneration: 7,
      retiredCopyJobIds: [],
    } as unknown as AppState;

    expect(mutateNumberDraft(state, numbers => {
      numbers[0] = {
        original: "IMG_0013.JPG",
        canonical: "13",
        confidence: null,
        confirmed: false,
      };
    }, persist)).toBeUndefined();
    expect(state.copyAttemptGeneration).toBe(7);
    expect(state.retiredCopyJobIds).toEqual([]);
    expect(applyCopyProgress(state, {
      jobId: "copy-job",
      currentFile: "IMG_0012.JPG",
      completedFiles: 1,
      totalFiles: 2,
      copiedBytes: 10,
      totalBytes: 20,
    })).toBe(true);
    expect(state.copyProgress?.currentFile).toBe("IMG_0012.JPG");
    expect(persist).not.toHaveBeenCalled();
  });

  it.each(["scanning", "copying"] as const)(
    "uses persisted session status %s as the only number-workflow lock source",
    status => {
      const state = {
        activeSession: {
          id: "session-1",
          taskLabel: "客户选片",
          note: null,
          status,
          createdAt: "2026-07-25T00:00:00Z",
          numbersConfirmed: true,
        },
        draftNumbers: [{
          original: "IMG_0012.JPG",
          canonical: "12",
          confidence: 0.98,
          confirmed: true,
        }],
        numbersConfirmed: true,
        blockingIssues: [],
        copyActive: false,
      } as unknown as AppState;
      const before = structuredClone(state);
      const editPersist = vi.fn(async () => true);
      const confirmPersist = vi.fn(async () => true);

      expect(state).not.toHaveProperty("numberWorkflowCommandPending");
      expect(state).not.toHaveProperty("activeCopyJobId");
      expect(state).not.toHaveProperty("terminalCopyJobId");
      expect(state).not.toHaveProperty("pausedCopyJobId");
      expect(state).not.toHaveProperty("pendingRecheckRequest");
      expect(state).not.toHaveProperty("matchReport");
      expect(state).not.toHaveProperty("modal");
      expect(isNumberWorkflowLocked(state)).toBe(true);
      expect(mutateNumberDraft(state, numbers => {
        numbers[0] = {
          original: "IMG_0013.JPG",
          canonical: "13",
          confidence: null,
          confirmed: false,
        };
      }, editPersist)).toBeUndefined();
      expect(beginNumberConfirmation(state, false, confirmPersist)).toBeUndefined();
      expect(state).toEqual(before);
      expect(editPersist).not.toHaveBeenCalled();
      expect(confirmPersist).not.toHaveBeenCalled();
    },
  );

  it.each([
    "draft",
    "awaitingNumberConfirmation",
    "readyToScan",
    "readyToCopy",
  ] as const)(
    "keeps idle persisted session status %s editable and confirmable",
    async status => {
      const makeState = () => ({
        activeSession: {
          id: "session-1",
          taskLabel: "客户选片",
          note: null,
          status,
          createdAt: "2026-07-25T00:00:00Z",
          numbersConfirmed: false,
        },
        draftNumbers: [{
          original: "IMG_0012.JPG",
          canonical: "12",
          confidence: 0.98,
          confirmed: false,
        }],
        numbersConfirmed: false,
        blockingIssues: [],
        copyActive: false,
      }) as unknown as AppState;

      const editState = makeState();
      const editPersist = vi.fn(async () => true);
      expect(isNumberWorkflowLocked(editState)).toBe(false);
      const edit = mutateNumberDraft(editState, numbers => {
        numbers[0] = {
          original: "IMG_0013.JPG",
          canonical: "13",
          confidence: null,
          confirmed: false,
        };
      }, editPersist);
      expect(edit).toBeDefined();
      await expect(edit!.persistence).resolves.toBe(true);
      expect(editState.draftNumbers).toEqual([{
        original: "IMG_0013.JPG",
        canonical: "13",
        confidence: null,
        confirmed: false,
      }]);
      expect(editState.numbersConfirmed).toBe(false);
      expect(editPersist).toHaveBeenCalledOnce();

      const confirmState = makeState();
      const confirmPersist = vi.fn(async (
        _state: AppState,
        snapshot: AppState["draftNumbers"],
      ) => {
        expect(snapshot).toEqual([{
          original: "IMG_0012.JPG",
          canonical: "12",
          confidence: 0.98,
          confirmed: true,
        }]);
        return true;
      });
      expect(isNumberWorkflowLocked(confirmState)).toBe(false);
      const confirmation = beginNumberConfirmation(
        confirmState,
        true,
        confirmPersist,
      );
      expect(confirmation).toBeDefined();
      await expect(confirmation!.persistence).resolves.toBe(true);
      expect(confirmState.numbersConfirmed).toBe(true);
      expect(confirmState.draftNumbers.every(number => number.confirmed)).toBe(true);
      expect(confirmPersist).toHaveBeenCalledOnce();
    },
  );

  it("recognizes Windows path-not-found errors as a directory recovery case", () => {
    expect(isUnavailableDirectoryError("无法读取文件或目录：系统找不到指定的路径 (os error 3)")).toBe(true);
    expect(isUnavailableDirectoryError(new Error("照片目录无法读取或已断开"))).toBe(true);
    expect(isUnavailableDirectoryError(new Error("SMB 网络目录不可用，请检查 NAS、局域网和共享地址后重试"))).toBe(true);
    expect(isUnavailableDirectoryError(new Error("模型返回格式错误"))).toBe(false);
  });
  it("treats an explicitly skipped missing number as resolved", () => {
    expect(blockingIssuesFrom({
      items: [{ canonicalNumber: "7", status: "missing", groups: [] }],
      skippedNumbers: ["7"],
      autoCopyStarted: false,
      requiresSecondConfirmation: true,
      confirmationToken: null,
      copyJob: null,
    })).toEqual([]);
  });
  it("accepts only actionable strongly shaped pause events", () => {
    expect(modalFromPausePayload({
      issue: "target-conflict",
      title: "目标冲突",
      message: "处理后重试",
      affected: ["IMG_0007.JPG"],
      actions: ["recheck", "cancel"],
    })).toMatchObject({ issue: "target-conflict", actions: ["recheck", "cancel"] });
    expect(() => modalFromPausePayload({ issue: "unknown" })).toThrow("未知");
  });

  it("keeps target-directory disconnects in the recheck and cancel workflow", () => {
    expect(modalFromPausePayload({
      issue: "target-disconnected",
      title: "目标目录或 NAS 已断开",
      message: "恢复目标目录连接后重新检查。",
      affected: [],
      actions: ["recheck", "cancel"],
    })).toEqual({
      issue: "target-disconnected",
      title: "目标目录或 NAS 已断开",
      message: "恢复目标目录连接后重新检查。",
      affected: [],
      actions: ["recheck", "cancel"],
    });
  });

  it("invalidates every frontend scan artifact as soon as a number changes", () => {
    const state = {
      draftNumbers: [
        {
          original: "IMG_0012.JPG",
          canonical: "12",
          confidence: 0.98,
          confirmed: true,
        },
        {
          original: "IMG_0013.JPG",
          canonical: "13",
          confidence: 0.97,
          confirmed: true,
        },
      ],
      numbersConfirmed: true,
      matchReport: {
        items: [],
        skippedNumbers: [],
        autoCopyStarted: false,
        requiresSecondConfirmation: true,
        confirmationToken: "stale-token",
        copyJob: null,
      },
      blockingIssues: ["missing"],
      copyProgress: {
        jobId: "old",
        currentFile: "old.jpg",
        completedFiles: 0,
        totalFiles: 1,
        copiedBytes: 0,
        totalBytes: 1,
      },
      completionReport: {
        jobId: "old",
        status: "completed",
        copiedCount: 1,
        skippedIdenticalCount: 0,
        skippedUserCount: 0,
        failedCount: 0,
        copiedBytes: 1,
        source: "source",
        target: "target",
        startedAt: null,
        finishedAt: "now",
        message: "done",
      },
      copyActive: false,
    } as unknown as AppState;

    state.draftNumbers[1] = {
      original: "IMG_0014.JPG",
      canonical: "14",
      confidence: null,
      confirmed: false,
    };
    invalidateNumberWorkflow(state);

    expect(state).toMatchObject({
      numbersConfirmed: false,
      blockingIssues: [],
      copyActive: false,
    });
    expect(state.draftNumbers[1]).toMatchObject({
      original: "IMG_0014.JPG",
      canonical: "14",
      confirmed: false,
    });
    expect(state.matchReport).toBeUndefined();
    expect(state.copyProgress).toBeUndefined();
    expect(state.completionReport).toBeUndefined();
    expect(applyCopyProgress(state, {
      jobId: "old",
      currentFile: "late.jpg",
      completedFiles: 0,
      totalFiles: 1,
      copiedBytes: 0,
      totalBytes: 1,
    })).toBe(false);
  });

  it("persists the edited draft through the existing backend number-save command", async () => {
    const save = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const state = {
      activeSession: { id: "session-1" },
      draftNumbers: [
        {
          original: "0012",
          canonical: "12",
          confidence: null,
          confirmed: true,
        },
      ],
    } as unknown as AppState;

    await persistNumberDraft(state);

    expect(save).toHaveBeenCalledWith("session-1", [
      {
        original: "0012",
        canonical: "12",
        confidence: null,
        confirmed: false,
      },
    ]);
    save.mockRestore();
  });

  it("does not issue a queued number save for A after switching to B", async () => {
    const save = vi.spyOn(bridge, "saveConfirmedNumbers").mockResolvedValue();
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
    } as unknown as AppState;
    const request = beginCopyAttempt(state, "session-a");
    const queued = queueNumberSnapshot(state, [{ original: "7", canonical: "7", confidence: null, confirmed: true }], request);

    beginSessionSwitch(state, "session-b");
    await expect(queued).resolves.toBe(false);
    expect(save).not.toHaveBeenCalled();
    save.mockRestore();
  });

  it("invalidates pending scan, start, and recheck reducers on session switch or an edit", () => {
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
    } as unknown as AppState;
    const scan = beginCopyAttempt(state, "session-a");
    beginSessionSwitch(state, "session-b");
    expect(isCurrentCopyAttempt(state, scan)).toBe(false);

    state.activeSession = { id: "session-a" } as AppState["activeSession"];
    state.copyRuntimeSessionId = "session-a";
    const start = beginCopyAttempt(state, "session-a");
    invalidateNumberWorkflow(state);
    expect(isCurrentCopyAttempt(state, start)).toBe(false);

    const recheck = beginCopyAttempt(state, "session-a");
    expect(captureCopyAttempt(state, "session-a")).toEqual(recheck);
    beginSessionSwitch(state, "session-b");
    expect(isCurrentCopyAttempt(state, recheck)).toBe(false);
  });

  it("silently ignores a rejected recheck after its session switches or its same-session attempt is invalidated", async () => {
    let reject!: (error: Error) => void;
    const pendingRecheck = () => new Promise<never>((_, rejectPromise) => { reject = rejectPromise; });
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
    } as unknown as AppState;

    const afterSessionSwitch = recheckCopyAttempt(state, "session-a", pendingRecheck);
    beginSessionSwitch(state, "session-b");
    reject(new Error("A recheck failed"));
    await expect(afterSessionSwitch).resolves.toBe(false);

    state.activeSession = { id: "session-a" } as AppState["activeSession"];
    state.copyRuntimeSessionId = "session-a";
    const afterSameSessionInvalidation = recheckCopyAttempt(state, "session-a", pendingRecheck);
    invalidateNumberWorkflow(state);
    reject(new Error("superseded recheck failed"));
    await expect(afterSameSessionInvalidation).resolves.toBe(false);
  });

  it("keeps a source-disconnected session paused until backend recheck succeeds", async () => {
    let resolve!: (launch: { jobId: string; status: "copying" }) => void;
    const pendingRecheck = () => new Promise<{ jobId: string; status: "copying" }>(
      resolvePromise => { resolve = resolvePromise; },
    );
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
      copyActive: false,
      pausedCopyJobId: "paused-job",
      modal: {
        issue: "source-disconnected",
        title: "源目录或 NAS 已断开",
        message: "恢复连接后重新检查。",
        affected: [],
        actions: ["recheck"],
      },
    } as unknown as AppState;

    const recheck = recheckCopyAttempt(state, "session-a", pendingRecheck);

    expect(state.activeSession?.id).toBe("session-a");
    expect(state.copyActive).toBe(false);
    expect(state.pausedCopyJobId).toBe("paused-job");
    expect(state.modal?.issue).toBe("source-disconnected");

    resolve({ jobId: "resumed-job", status: "copying" });
    await expect(recheck).resolves.toBe(true);
    expect(state.copyActive).toBe(true);
    expect(state.activeCopyJobId).toBe("resumed-job");
  });

  it("keeps the current recheck modal and paused job visible when reconnect fails", async () => {
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
      copyActive: false,
      pausedCopyJobId: "paused-job",
      modal: {
        issue: "source-disconnected",
        title: "源目录或 NAS 已断开",
        message: "恢复连接后重新检查。",
        affected: [],
        actions: ["recheck", "cancel"],
      },
    } as unknown as AppState;
    const messageNode = { textContent: "" };
    const dialog = {
      querySelector: vi.fn(() => messageNode),
    } as unknown as HTMLDialogElement;

    await expect(
      recheckCopyAttempt(state, "session-a", async () => {
        throw new Error("SMB 网络目录不可用，请检查连接后重试。");
      }),
    ).resolves.toBe(false);
    syncPausedModalMessage(dialog, state);

    expect(state.pausedCopyJobId).toBe("paused-job");
    expect(state.modal?.issue).toBe("source-disconnected");
    expect(state.modal?.actions).toEqual(["recheck", "cancel"]);
    expect(state.modal?.message).toBe("SMB 网络目录不可用，请检查连接后重试。");
    expect(messageNode.textContent).toBe("SMB 网络目录不可用，请检查连接后重试。");
  });

  it("deduplicates recheck clicks while the current request is pending", async () => {
    let resolve!: (launch: { jobId: string; status: "copying" }) => void;
    const pendingRecheck = vi.fn(
      () =>
        new Promise<{ jobId: string; status: "copying" }>((resolvePromise) => {
          resolve = resolvePromise;
        }),
    );
    const state = {
      activeSession: { id: "session-a" },
      copyRuntimeSessionId: "session-a",
      pausedCopyJobId: "paused-job",
      modal: {
        issue: "source-disconnected",
        title: "源目录或 NAS 已断开",
        message: "恢复连接后重新检查。",
        affected: [],
        actions: ["recheck"],
      },
    } as unknown as AppState;

    const first = recheckCopyAttempt(state, "session-a", pendingRecheck);
    const duplicate = recheckCopyAttempt(state, "session-a", pendingRecheck);

    expect(pendingRecheck).toHaveBeenCalledTimes(1);
    await expect(duplicate).resolves.toBe(false);
    resolve({ jobId: "resumed-job", status: "copying" });
    await expect(first).resolves.toBe(true);
    expect(state.activeCopyJobId).toBe("resumed-job");
  });

  it("does not let a late command response overwrite an earlier completion event", () => {
    const state = { copyActive: false } as AppState;
    applyCopyCompletion(state, {
      jobId: "job-1",
      status: "completed",
      copiedCount: 1,
      skippedIdenticalCount: 0,
      skippedUserCount: 0,
      failedCount: 0,
      copiedBytes: 10,
      source: "source",
      target: "target",
      startedAt: null,
      finishedAt: "now",
      message: "done",
    });
    applyCopyLaunch(state, { jobId: "job-1", status: "copying" });

    expect(state.copyActive).toBe(false);
    expect(state.completionReport?.status).toBe("completed");
  });

  it("does not let an older terminal event overwrite a newer copy attempt", () => {
    const state = { copyActive: false } as AppState;
    const completion = (jobId: string, message: string) => ({
      jobId,
      status: "completed" as const,
      copiedCount: 1,
      skippedIdenticalCount: 0,
      skippedUserCount: 0,
      failedCount: 0,
      copiedBytes: 10,
      source: "source",
      target: "target",
      startedAt: null,
      finishedAt: "now",
      message,
    });
    applyCopyCompletion(state, completion("old-job", "old"));
    beginCopyAttempt(state);
    applyCopyLaunch(state, { jobId: "new-job", status: "copying" });
    applyCopyCompletion(state, completion("new-job", "new"));

    expect(applyCopyCompletion(state, completion("old-job", "late-old"))).toBe(false);
    expect(state.terminalCopyJobId).toBe("new-job");
    expect(state.completionReport?.message).toBe("new");
  });

  it("accepts a fast auto-copy B completion after failed A while rejecting late A events", () => {
    const state = { copyActive: false, blockingIssues: [] } as unknown as AppState;
    const completion = (jobId: string, status: "completed" | "failed", message: string) => ({
      jobId,
      status,
      copiedCount: status === "completed" ? 1 : 0,
      skippedIdenticalCount: 0,
      skippedUserCount: 0,
      failedCount: status === "failed" ? 1 : 0,
      copiedBytes: status === "completed" ? 10 : 0,
      source: "source",
      target: "target",
      startedAt: null,
      finishedAt: "now",
      message,
    });
    applyCopyCompletion(state, completion("job-a", "failed", "A failed"));

    beginCopyAttempt(state);
    expect(applyCopyCompletion(state, completion("job-b", "completed", "B done"))).toBe(true);
    applyMatchReport(state, {
      items: [],
      skippedNumbers: [],
      autoCopyStarted: true,
      requiresSecondConfirmation: false,
      confirmationToken: null,
      copyJob: { jobId: "job-b", status: "copying" },
    });

    expect(applyCopyProgress(state, {
      jobId: "job-a",
      currentFile: "late-a.jpg",
      completedFiles: 0,
      totalFiles: 1,
      copiedBytes: 0,
      totalBytes: 10,
    })).toBe(false);
    expect(applyCopyCompletion(state, completion("job-a", "failed", "late A"))).toBe(false);
    expect(state.copyActive).toBe(false);
    expect(state.terminalCopyJobId).toBe("job-b");
    expect(state.completionReport?.message).toBe("B done");
  });

  it("scopes every transient copy field to the workflow session", () => {
    const state = {
      activeSession: { id: "old-session" },
      copyActive: true,
      activeCopyJobId: "old-active",
      terminalCopyJobId: "old-terminal",
      pausedCopyJobId: "old-paused",
      copyProgress: { jobId: "old-active" },
      completionReport: { jobId: "old-terminal" },
      modal: { issue: "missing" },
    } as unknown as AppState;

    applyWorkflow(state, {
      session: {
        id: "new-session",
        taskLabel: "new",
        note: null,
        status: "draft",
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: false,
      },
      numbers: [],
      inputs: [],
      bindings: { source: null, target: null },
      snapshot: null,
      preflight: null,
      copyItems: [],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: false,
      targetAvailable: false,
    });

    expect(state).toMatchObject({
      copyActive: false,
      blockingIssues: [],
    });
    expect(state.activeCopyJobId).toBeUndefined();
    expect(state.terminalCopyJobId).toBeUndefined();
    expect(state.pausedCopyJobId).toBeUndefined();
    expect(state.copyProgress).toBeUndefined();
    expect(state.completionReport).toBeUndefined();
    expect(state.modal).toBeUndefined();
    expect(shouldHandleSessionEvent(state, "old-session")).toBe(false);
    expect(shouldHandleSessionEvent(state, "new-session")).toBe(true);
  });

  it("resets stale job state when reapplying a settled workflow for the same session", () => {
    const state = {
      activeSession: { id: "session" },
      copyRuntimeSessionId: "session",
      copyActive: true,
      activeCopyJobId: "stale-job",
      copyProgress: { jobId: "stale-job" },
    } as unknown as AppState;

    applyWorkflow(state, {
      session: {
        id: "session",
        taskLabel: "same",
        note: null,
        status: "draft",
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: false,
      },
      numbers: [],
      inputs: [],
      bindings: { source: null, target: null },
      snapshot: null,
      preflight: null,
      copyItems: [],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: false,
      targetAvailable: false,
    });

    expect(state.copyActive).toBe(false);
    expect(state.activeCopyJobId).toBeUndefined();
    expect(state.copyProgress).toBeUndefined();
  });

  it("ignores the old session while a different session is opening", () => {
    const state = {
      activeSession: { id: "old-session" },
      copyActive: true,
      activeCopyJobId: "old-job",
    } as unknown as AppState;

    beginSessionSwitch(state, "new-session");

    expect(state.activeSession).toBeUndefined();
    expect(state.activeCopyJobId).toBeUndefined();
    expect(shouldHandleSessionEvent(state, "old-session")).toBe(false);
    expect(shouldHandleSessionEvent(state, "new-session")).toBe(true);
  });

  it("keeps a matching completion event that arrives before open-session returns", () => {
    const state = {
      activeSession: { id: "old-session" },
      copyActive: true,
      activeCopyJobId: "old-job",
    } as unknown as AppState;
    beginSessionSwitch(state, "new-session");
    applyCopyCompletion(state, {
      jobId: "new-job",
      status: "completed",
      copiedCount: 1,
      skippedIdenticalCount: 0,
      skippedUserCount: 0,
      failedCount: 0,
      copiedBytes: 10,
      source: "source",
      target: "target",
      startedAt: null,
      finishedAt: "now",
      message: "done",
    });

    applyWorkflow(state, {
      session: {
        id: "new-session",
        taskLabel: "new",
        note: null,
        status: "copying",
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: true,
      },
      numbers: [],
      inputs: [],
      bindings: { source: "/source", target: "/target" },
      snapshot: null,
      preflight: null,
      copyItems: [],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: true,
      targetAvailable: true,
    });

    expect(state.copyActive).toBe(false);
    expect(state.terminalCopyJobId).toBe("new-job");
    expect(state.completionReport?.status).toBe("completed");
  });

  it("rejects an A open response that arrives after a newer B request", () => {
    const state = { copyActive: false } as AppState;
    const requestA = beginSessionSwitch(state, "session-a");
    const requestB = beginSessionSwitch(state, "session-b");
    const workflow = {
      session: {
        id: "session-a",
        taskLabel: "A",
        note: null,
        status: "draft" as const,
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: false,
      },
      numbers: [],
      inputs: [],
      bindings: { source: null, target: null },
      snapshot: null,
      preflight: null,
      copyItems: [],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: false,
      targetAvailable: false,
    };

    expect(applyWorkflow(state, workflow, requestA)).toBe(false);
    expect(state.activeSession).toBeUndefined();
    expect(state.pendingWorkflowSessionId).toBe("session-b");
    expect(applyWorkflow(state, { ...workflow, session: { ...workflow.session, id: "session-b", taskLabel: "B" } }, requestB)).toBe(true);
    expect(state.activeSession?.id).toBe("session-b");
  });

  it("does not hydrate a late A input into the active B session", async () => {
    let resolveA!: (value: Uint8Array<ArrayBuffer>) => void;
    const read = vi.spyOn(bridge, "readSessionInput").mockImplementation((sessionId) => {
      if (sessionId === "session-a") {
        return new Promise(resolve => { resolveA = resolve; });
      }
      return Promise.resolve(new Uint8Array([66]));
    });
    const state = {
      copyActive: false,
      inputs: [],
      candidatePreviewUrls: {},
    } as unknown as AppState;
    const workflow = (id: string) => ({
      session: {
        id,
        taskLabel: id,
        note: null,
        status: "draft" as const,
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: false,
      },
      numbers: [],
      inputs: [],
      bindings: { source: null, target: null },
      snapshot: null,
      preflight: null,
      copyItems: [],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: false,
      targetAvailable: false,
    });
    const input = (id: string) => [{ id, name: `${id}.txt`, kind: "text" as const, mime: "text/plain", size: 1 }];

    const requestA = beginSessionSwitch(state, "session-a");
    applyWorkflow(state, workflow("session-a"), requestA);
    const hydrationA = hydrateSessionInputs(state, requestA, input("a"));
    const requestB = beginSessionSwitch(state, "session-b");
    applyWorkflow(state, workflow("session-b"), requestB);
    await hydrateSessionInputs(state, requestB, input("b"));
    resolveA(new Uint8Array([65]));
    await hydrationA;

    expect(state.activeSession?.id).toBe("session-b");
    expect(state.inputs.map(item => item.id)).toEqual(["b"]);
    expect(state.inputs[0]?.text).toBe("B");
    read.mockRestore();
  });

  it("keeps legacy over-limit image sessions metadata-only", async () => {
    const read = vi.spyOn(bridge, "readSessionInput");
    const state = {
      copyActive: false,
      inputs: [],
      candidatePreviewUrls: {},
    } as unknown as AppState;
    const request = beginSessionSwitch(state, "legacy-session");
    state.activeSession = {
      id: "legacy-session",
      taskLabel: "legacy",
      note: null,
      status: "draft",
      createdAt: "now",
      numbersConfirmed: false,
    };
    const persisted = Array.from({ length: MAX_RECOGNITION_IMAGES + 1 }, (_, index) => ({
      id: `input-${index}`,
      name: `legacy-${index}.png`,
      kind: "image" as const,
      mime: "image/png",
      size: 8,
    }));

    await hydrateSessionInputs(state, request, persisted);

    expect(read).not.toHaveBeenCalled();
    expect(state.inputs).toHaveLength(MAX_RECOGNITION_IMAGES + 1);
    expect(state.inputs.every(input => input.blob == null)).toBe(true);
    expect(state.recognitionNote).toContain("一次最多识别 12 张图片");
    read.mockRestore();
  });

  it("keeps a terminal auto-copy event when the returned launch arrives later", () => {
    const state = {
      copyActive: false,
      blockingIssues: [],
    } as unknown as AppState;
    applyCopyCompletion(state, {
      jobId: "auto-job",
      status: "completed",
      copiedCount: 1,
      skippedIdenticalCount: 0,
      skippedUserCount: 0,
      failedCount: 0,
      copiedBytes: 10,
      source: "source",
      target: "target",
      startedAt: null,
      finishedAt: "now",
      message: "done",
    });

    applyMatchReport(state, {
      items: [],
      skippedNumbers: [],
      autoCopyStarted: true,
      requiresSecondConfirmation: false,
      confirmationToken: null,
      copyJob: { jobId: "auto-job", status: "copying" },
    });

    expect(state.matchReport?.copyJob?.jobId).toBe("auto-job");
    expect(state.copyActive).toBe(false);
    expect(state.completionReport?.status).toBe("completed");
  });

  it("does not let a stale copying refresh reset a newer completion", () => {
    const state = { copyActive: false } as AppState;
    const request = beginSessionSwitch(state, "session");
    const workflow = {
      session: {
        id: "session",
        taskLabel: "session",
        note: null,
        status: "copying" as const,
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: true,
      },
      numbers: [],
      inputs: [],
      bindings: { source: "/source", target: "/target" },
      snapshot: null,
      preflight: null,
      copyItems: [],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: true,
      targetAvailable: true,
    };
    applyWorkflow(state, workflow, request);
    const refresh = captureWorkflowRequest(state, "session");
    applyCopyCompletion(state, {
      jobId: "auto-job",
      status: "completed",
      copiedCount: 1,
      skippedIdenticalCount: 0,
      skippedUserCount: 0,
      failedCount: 0,
      copiedBytes: 10,
      source: "source",
      target: "target",
      startedAt: null,
      finishedAt: "now",
      message: "done",
    });

    expect(applyWorkflow(state, workflow, refresh)).toBe(true);
    expect(state.copyActive).toBe(false);
    expect(state.terminalCopyJobId).toBe("auto-job");
    expect(state.completionReport?.status).toBe("completed");
  });

  it("offers a start action for a ready plan after second confirmation is disabled", () => {
    expect(shouldShowStartCopy({
      sessionStatus: "readyToCopy",
      numbersConfirmed: true,
      blockingIssues: [],
      copyActive: false,
      autoCopyStarted: false,
      requiresSecondConfirmation: false,
    })).toBe(true);
  });

  it("does not render a stale copy action while the persistent session needs attention", () => {
    const state = {
      page: "workbench",
      activeSession: {
        id: "session-needs-attention",
        taskLabel: "stale plan",
        note: null,
        status: "needsAttention",
        createdAt: "now",
        numbersConfirmed: true,
      },
      sessions: [],
      inputs: [],
      detectedOrderId: null,
      draftNumbers: [{
        original: "06613",
        canonical: "6613",
        confidence: null,
        confirmed: true,
      }],
      numbersConfirmed: true,
      sourceDir: "Z:\\source",
      targetDir: "Z:\\target",
      matchReport: {
        items: [],
        skippedNumbers: [],
        autoCopyStarted: false,
        requiresSecondConfirmation: false,
        confirmationToken: null,
        copyJob: null,
      },
      blockingIssues: [],
      settings: {
        recognitionMode: "offline",
        defaultProviderId: null,
        cloudFallbackOffline: true,
        secondConfirmationEnabled: false,
        extensions: [],
      },
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
      copyActive: false,
    } as AppState;

    expect(renderWorkbenchForTest(state)).not.toContain('id="start-copy"');
    state.activeSession!.status = "readyToCopy";
    expect(renderWorkbenchForTest(state)).toContain('id="start-copy"');
  });

  it("offers manual source and target paths after number confirmation", () => {
    const markup = renderWorkbenchForTest({
      page: "workbench",
      activeSession: {
        id: "session-manual-path",
        taskLabel: "manual path",
        note: null,
        status: "draft",
        createdAt: "now",
        numbersConfirmed: true,
      },
      copyRuntimeSessionId: "session-manual-path",
      workflowGeneration: 1,
      copyAttemptGeneration: 1,
      sessions: [],
      inputs: [],
      detectedOrderId: null,
      numbersConfirmed: true,
      draftNumbers: [{
        original: "06613",
        canonical: "6613",
        confidence: null,
        confirmed: true,
      }],
      blockingIssues: [],
      settings: {
        recognitionMode: "offline",
        defaultProviderId: null,
        cloudFallbackOffline: true,
        secondConfirmationEnabled: false,
        extensions: [],
      },
      providers: [],
      providerTemplates: [],
      candidatePreviewUrls: {},
      copyActive: false,
    } as AppState);

    expect(markup).toContain('id="manual-source-path"');
    expect(markup).toContain('id="bind-manual-source"');
    expect(markup).toContain("也可直接粘贴本机、映射盘或 UNC 路径");
    expect(markup).toContain('id="manual-target-base"');
    expect(markup).toContain('id="bind-manual-target-base"');
  });

  it("unchecks the rendered confirmation control immediately after an edit", () => {
    const checkbox = { checked: true };
    const scan = { disabled: false };
    const results = { innerHTML: "" };
    const querySelector = vi.fn((selector: string) => ({
      "#confirm-numbers": checkbox,
      "#scan": scan,
      ".results": results,
    })[selector] ?? null);

    updateInvalidatedNumberUi({ querySelector } as unknown as Document);

    expect(checkbox.checked).toBe(false);
    expect(scan.disabled).toBe(true);
  });

  it("reconstructs skipped counts by reason instead of counting every skip as identical", () => {
    const report = recoveredCompletionReport({
      session: {
        id: "session",
        taskLabel: "task",
        note: null,
        status: "completed",
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: true,
      },
      numbers: [],
      inputs: [],
      bindings: { source: "/source", target: "/target" },
      snapshot: null,
      preflight: null,
      copyItems: [
        {
          id: "identical",
          sessionId: "session",
          canonicalNumber: "1",
          source: "/source/a",
          target: "/target/a",
          plannedHash: "",
          planRevision: 1,
          status: "skipped",
          sourceHash: null,
          skippedReason: "identical-target",
          errorCode: null,
          errorSummary: null,
          createdAt: "2026-01-01T00:00:00Z",
          updatedAt: "2026-01-01T00:00:01Z",
        },
        {
          id: "manual",
          sessionId: "session",
          canonicalNumber: "2",
          source: "/source",
          target: "/target",
          plannedHash: "",
          planRevision: 1,
          status: "skipped",
          sourceHash: null,
          skippedReason: "user-skipped",
          errorCode: null,
          errorSummary: null,
          createdAt: "2026-01-01T00:00:00Z",
          updatedAt: "2026-01-01T00:00:01Z",
        },
      ],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: true,
      targetAvailable: true,
    });

    expect(report).toMatchObject({
      skippedIdenticalCount: 1,
      skippedUserCount: 1,
    });
  });

  it("reconstructs completion from only the latest non-superseded plan revision", () => {
    const item = (
      id: string,
      planRevision: number,
      status: "copied" | "failed" | "planSuperseded",
    ) => ({
      id,
      sessionId: "session",
      canonicalNumber: id,
      source: `/source/${id}`,
      target: `/target/${id}`,
      plannedHash: "",
      planRevision,
      status,
      sourceHash: null,
      skippedReason: null,
      errorCode: null,
      errorSummary: null,
      createdAt: `2026-01-01T00:00:0${planRevision}Z`,
      updatedAt: `2026-01-01T00:00:1${planRevision}Z`,
    });
    const report = recoveredCompletionReport({
      session: {
        id: "session",
        taskLabel: "task",
        note: null,
        status: "failed",
        createdAt: "2026-01-01T00:00:00Z",
        numbersConfirmed: true,
      },
      numbers: [],
      inputs: [],
      bindings: { source: "/source", target: "/target" },
      snapshot: null,
      preflight: null,
      copyItems: [
        item("historical-copy", 1, "copied"),
        item("superseded-copy", 2, "planSuperseded"),
        item("current-failure", 3, "failed"),
      ],
      requiresSecondConfirmation: false,
      confirmationToken: null,
      sourceAvailable: true,
      targetAvailable: true,
    });

    expect(report).toMatchObject({
      copiedCount: 0,
      failedCount: 1,
      startedAt: "2026-01-01T00:00:03Z",
      finishedAt: "2026-01-01T00:00:13Z",
    });
  });

  it("supports keyboard activation for the dropzone", () => {
    expect(shouldActivateDropzone("Enter")).toBe(true);
    expect(shouldActivateDropzone(" ")).toBe(true);
    expect(shouldActivateDropzone("Escape")).toBe(false);
  });
});

describe("confidence safety", () => {
  it("clamps non-finite and out-of-range recognition confidence", () => {
    expect(parseRecognizedText("0012", 250).numbers[0].confidence).toBe(1);
    expect(parseRecognizedText("0012", -20).numbers[0].confidence).toBe(0);
    expect(parseRecognizedText("0012", Number.NaN).numbers[0].confidence).toBe(0);
  });
});

describe("provider draft forms", () => {
  it("tests the currently edited draft fields while keeping the key outside the profile", () => {
    const form = new FormData();
    form.set("id", "saved-provider");
    form.set("name", "edited name");
    form.set("template", "tencent");
    form.set("address", "https://tokenhub.tencentmaas.com/v1");
    form.set("addressMode", "baseUrl");
    form.set("apiFormat", "chatCompletions");
    form.set("model", "edited-model");
    form.set("fallbackModel", "fallback-model");
    form.set("timeoutSeconds", "42");
    form.set("enabled", "on");
    form.set("apiKey", "");

    const draft = providerDraftFromFormData(form);

    expect(draft.profile).toMatchObject({
      id: "saved-provider",
      name: "edited name",
      apiFormat: "chatCompletions",
      model: "edited-model",
      timeoutSeconds: 42,
    });
    expect(draft.apiKey).toBe("");
    expect(draft.profile).not.toHaveProperty("apiKey");
  });
});
