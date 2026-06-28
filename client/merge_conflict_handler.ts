/**
 * Wires merge-conflict notifications and save() failures into the editor UI.
 *
 * The sync engine writes conflict-marked content to disk and broadcasts a
 * `merge-conflict` SW message. This module:
 *  - Hooks the SW message dispatch to reload the open page when the conflict
 *    is for it, or flash a "go to it" notification otherwise.
 *  - Provides a single `handleMergeConflictOnSave(client, e)` helper that
 *    `client.save()` calls in its catch block when the HTTP layer throws a
 *    MergeConflictError.
 *
 * Living in its own file means client.ts only carries one import + one call,
 * so future upstream edits to client.ts rarely touch this code.
 */
import { MergeConflictError } from "./spaces/merge_conflict.ts";
import type { MergeConflictMessage } from "./service_worker/merge_conflict_messages.ts";

/**
 * Minimal Client surface this handler needs. Avoids tight import-time coupling
 * to client.ts so this file stays small and stable.
 */
export interface MergeConflictClient {
  currentPath(): string | undefined;
  flashNotification(message: string, type?: "info" | "error"): void;
  loadPage(opts: { path: string }): Promise<void> | void;
}

/** Dispatch a `merge-conflict` SW message into the editor UI. */
export function dispatchMergeConflictMessage(
  client: MergeConflictClient,
  message: MergeConflictMessage,
): void {
  console.warn(
    "Merge conflict received for",
    message.path,
    "serverHash:",
    message.serverHash,
  );
  if (client.currentPath() === message.path) {
    client.flashNotification(
      "Merge conflict — resolve the highlighted sections below",
      "error",
    );
    void client.loadPage({ path: message.path });
  } else {
    client.flashNotification(
      `Merge conflict in ${message.path} — open the file to resolve`,
      "error",
    );
  }
}

/**
 * Called from `client.save()`'s catch block. Returns true when the error was a
 * MergeConflictError (and was handled) so the caller can swallow / reject
 * without falling through to the generic retry path.
 */
export function handleMergeConflictOnSave(
  client: MergeConflictClient,
  e: unknown,
): e is MergeConflictError {
  if (!(e instanceof MergeConflictError)) return false;
  client.flashNotification(
    "Merge conflict — resolve the highlighted sections below",
    "error",
  );
  const path = client.currentPath();
  if (path) void client.loadPage({ path });
  return true;
}
