/**
 * Service-worker → client messages for the version-history / merge-conflict
 * subsystem. Lives in its own file so adding new history-related messages
 * doesn't keep colliding with upstream edits to `types/ui.ts`.
 */

export type MergeConflictMessage = {
  type: "merge-conflict";
  path: string;
  conflictContent: string;
  serverHash: string | null;
};
