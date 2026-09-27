import "./styles.css";
import "./task8.css";
import { bridge } from "./bridge";
import { applyCopyCompletion, applyCopyPaused, applyCopyProgress, applyScanProgress, modalFromPausePayload, releaseObjectUrls, render, shouldHandleSessionEvent, type AppState } from "./app";

const state: AppState = {
  page: "sessions", sessions: [], inputs: [], detectedOrderId: null,
  draftNumbers: [], numbersConfirmed: false, blockingIssues: [], providers: [],
  providerTemplates: [], candidatePreviewUrls: {}, copyActive: false,
  settings: { recognitionMode: "offline", defaultProviderId: null, cloudFallbackOffline: true, secondConfirmationEnabled: false, extensions: ["CR2", "CR3", "NEF", "ARW", "RAF", "DNG", "RW2", "ORF", "JPG", "JPEG"] },
};

await bridge.onCopyProgress(({ sessionId, payload }) => {
  if (!shouldHandleSessionEvent(state, sessionId)) return;
  if (!applyCopyProgress(state, payload)) return;
  render(state);
});
await bridge.onScanProgress(({ sessionId, payload }) => {
  if (!shouldHandleSessionEvent(state, sessionId)) return;
  if (!applyScanProgress(state, payload)) return;
  render(state);
});
await bridge.onCopyPaused(({ sessionId, payload }) => {
  if (!shouldHandleSessionEvent(state, sessionId)) return;
  let modal;
  try { modal = modalFromPausePayload(payload); } catch (error) {
    modal = { issue: "source-changed" as const, title: "复制已暂停", message: error instanceof Error ? error.message : String(error), affected: [], actions: ["close" as const] };
  }
  if (!applyCopyPaused(state, payload.jobId, modal)) return;
  render(state);
});
await bridge.onCopyComplete(async ({ sessionId, payload }) => {
  if (!shouldHandleSessionEvent(state, sessionId)) return;
  if (!applyCopyCompletion(state, payload)) return;
  releaseObjectUrls(state);
  state.modal = undefined;
  state.sessions = await bridge.listSessions();
  render(state);
});
const [sessions, settings, providers, providerTemplates] = await Promise.all([
  bridge.listSessions(),
  bridge.loadSettings(),
  bridge.listProviders(),
  bridge.providerTemplates(),
]);
state.sessions = sessions;
state.settings = settings;
state.providers = providers;
state.providerTemplates = providerTemplates;
render(state);
