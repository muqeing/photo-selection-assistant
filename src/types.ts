export type PhotoNumber = {
  original: string;
  canonical: string;
  confidence: number | null;
  confirmed: boolean;
};

export type RecognitionDraft = {
  detectedOrderId: string | null;
  numbers: PhotoNumber[];
  rawText: string;
  method: "text" | "offline" | "cloud" | "manual";
};

export type SessionStatus =
  | "draft"
  | "awaitingNumberConfirmation"
  | "readyToScan"
  | "scanning"
  | "needsAttention"
  | "readyToCopy"
  | "copying"
  | "completed"
  | "failed"
  | "cancelled";

export type Session = {
  id: string;
  taskLabel: string;
  note: string | null;
  status: SessionStatus;
  createdAt: string;
  numbersConfirmed: boolean;
};

export type IndexedFile = {
  path: string;
  stem: string;
  extension: string;
  canonicalNumber: string;
  size: number;
  modifiedMs: number;
  familyKey: string;
  fingerprint: FileFingerprint;
};
export type FileIdentity = { device: number; fileIndex: number };
export type FileFingerprint = {
  identity: FileIdentity;
  size: number;
  modifiedMs: number;
  contentHash: string;
};
export type CandidateGroup = { id: string; files: IndexedFile[] };
export type MatchStatus = "complete" | "partial" | "ambiguous" | "missing";
export type MatchItem = {
  canonicalNumber: string;
  status: MatchStatus;
  groups: CandidateGroup[];
};
export type MatchReport = {
  items: MatchItem[];
  skippedNumbers: string[];
  autoCopyStarted: boolean;
  requiresSecondConfirmation: boolean;
  confirmationToken: string | null;
  copyJob: CopyLaunch | null;
};
export type MatchResolution =
  | { kind: "selectGroup"; groupId: string }
  | { kind: "acceptPartial" }
  | { kind: "skip" };
export type ResolutionResult = {
  readyToCopy: boolean;
  confirmationToken: string | null;
  copyJob: CopyLaunch | null;
};
export type BoundPaths = { source: string | null; target: string | null };
export type TargetOperation =
  | "unknown"
  | "open-root"
  | "traverse"
  | "create-directory"
  | "open-target"
  | "create-part"
  | "write"
  | "sync"
  | "metadata"
  | "seek"
  | "hash"
  | "reopen"
  | "commit"
  | "cleanup"
  | "available-space";
export type TargetDisconnectInfo = {
  windowsCode: number | null;
  operation: TargetOperation;
};
export type ScanSnapshot = {
  source: string;
  sourceRootIdentity: FileIdentity;
  target: string;
  items: MatchItem[];
  skippedNumbers: string[];
};
export type PreflightReport = {
  ambiguous: string[];
  partial: string[];
  missing: string[];
  conflicts: string[];
  identical: string[];
  permissionDenied: boolean;
  insufficientSpace: boolean;
  atomicCommitUnsupported: boolean;
  sourceChanged: boolean;
  sourceDisconnected: boolean;
  targetDisconnected: boolean;
  targetDisconnect: TargetDisconnectInfo | null;
  pendingTargetParts: Array<{
    target: string;
    partName: string;
    identity: FileIdentity;
  }>;
};
export type SessionWorkflow = {
  session: Session;
  numbers: PhotoNumber[];
  inputs: PersistedInput[];
  bindings: BoundPaths;
  snapshot: ScanSnapshot | null;
  preflight: PreflightReport | null;
  copyItems: CopyHistoryItem[];
  requiresSecondConfirmation: boolean;
  confirmationToken: string | null;
  sourceAvailable: boolean;
  targetAvailable: boolean;
};
export type CopyItemStatus =
  | "planned"
  | "copying"
  | "copied"
  | "skipped"
  | "failed"
  | "cancelled"
  | "planSuperseded";
export type CopyHistoryItem = {
  id: string;
  sessionId: string;
  canonicalNumber: string;
  source: string;
  target: string;
  plannedHash: string;
  planRevision: number;
  status: CopyItemStatus;
  sourceHash: string | null;
  skippedReason: string | null;
  errorCode: string | null;
  errorSummary: string | null;
  createdAt: string;
  updatedAt: string;
};
export type DirectoryPurpose = "source" | "targetBase";
export type AppSettings = {
  recognitionMode: "offline" | "cloud";
  defaultProviderId: string | null;
  cloudFallbackOffline: boolean;
  secondConfirmationEnabled: boolean;
  extensions: string[];
};
export type CopyProgress = {
  jobId: string;
  currentFile: string;
  completedFiles: number;
  totalFiles: number;
  copiedBytes: number;
  totalBytes: number;
};
export type SessionEvent<T> = { sessionId: string; payload: T };
export type ProviderProfile = {
  id: string;
  name: string;
  template: "custom" | "aliyun" | "tencent" | "volcengine" | "xiaomi";
  address: string;
  addressMode: "baseUrl" | "fullEndpoint";
  apiFormat: "responses" | "chatCompletions";
  model: string;
  fallbackModel: string | null;
  timeoutSeconds: number;
  enabled: boolean;
  isDefault: boolean;
  secretRef: string;
};
export type ProviderTestResult = { ok: boolean; message: string };

export type ProviderTemplate = {
  id: ProviderProfile["template"];
  label: string;
  address: string;
  addressMode: ProviderProfile["addressMode"];
  apiFormat: ProviderProfile["apiFormat"];
  model: string;
};

export type PersistedInput = {
  id: string;
  name: string;
  kind: "text" | "image";
  mime: string;
  size: number;
};

export type InputItem = {
  id: string;
  kind: "text" | "image";
  name: string;
  text?: string;
  blob?: Blob;
  persistedId?: string;
  mime?: string;
  size?: number;
  previewUrl?: string;
};

export type InputImportStatus = {
  name: string;
  state: "pending" | "saved" | "failed";
  message?: string;
};

export type BlockingIssue =
  | "ambiguous"
  | "partial-format"
  | "missing"
  | "target-conflict"
  | "permission-denied"
  | "insufficient-space"
  | "source-changed"
  | "source-disconnected"
  | "target-disconnected";

export type ModalState = {
  issue: BlockingIssue;
  title: string;
  message: string;
  affected: string[];
  actions: ModalAction[];
};

export type ModalAction = "recheck" | "cancel" | "manual" | "retryRecognition" | "close";

export type ScanProgress = {
  phase: "scanning" | "complete";
  checkedFiles: number;
  matchedFiles: number;
  scannedDirectories: number;
  elapsedMs: number;
};

export type CopyPausedPayload = {
  jobId?: string;
  issue: BlockingIssue;
  title: string;
  message: string;
  affected: string[];
  actions: ModalAction[];
  targetDisconnect?: TargetDisconnectInfo;
};

export type CopyCompletionReport = {
  jobId: string;
  status: "completed" | "cancelled" | "failed";
  copiedCount: number;
  skippedIdenticalCount: number;
  skippedUserCount: number;
  failedCount: number;
  copiedBytes: number;
  source: string;
  target: string;
  startedAt: string | null;
  finishedAt: string;
  message: string;
};

export type CopyLaunch = {
  jobId: string;
  status: "copying";
};

export type CandidatePreview = {
  bytes: Uint8Array<ArrayBuffer>;
  mime: "image/jpeg" | "image/png" | "image/webp";
};
